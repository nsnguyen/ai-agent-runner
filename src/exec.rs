// The host's side of the guest protocol (src/proto.rs), under `runner exec`,
// `runner shell` and MCP's exec. Each command is one connection to the VM's
// runner-guest, through Firecracker's vsock socket: write `CONNECT <port>`,
// and Firecracker answers `OK <n>` once a listener in the guest takes it.
//
// Firecracker ends a connection both ways once the host closes its side,
// even only for writing, so the host only does that to hang up.
use crate::proto::{self, Exit, FromGuest, Request, ToGuest, WinSize};
use crate::store::Store;
use crate::systemd::{self, State};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use rmcp::schemars;
use serde::Serialize;
use std::io::{self, IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Who commands in a VM run as unless told otherwise: the base image's user.
pub const USER: &str = "agent";
/// How long a VM's runner-guest gets to answer, for a VM that has just booted.
pub const GUEST_WAIT: Duration = Duration::from_secs(60);
/// The most of each stream MCP's exec gives back.
pub const OUTPUT_CAP: usize = 32 * 1024;
/// `runner shell`: the user's own shell, as a login shell.
pub const LOGIN: [&str; 3] = ["/bin/sh", "-c", r#"exec "${SHELL:-/bin/sh}" -l"#];

// A PTY's size when there's no terminal here to take one from.
const DEFAULT_SIZE: WinSize = WinSize { rows: 24, cols: 80 };
// Firecracker or the guest stalling this long partway through the handshake
// counts as a failed try.
const HANDSHAKE: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_millis(100);
// Stdin goes out in frames of this much, the guest's own read size.
const CHUNK: usize = 32 * 1024;
// After a timeout's kill, how long to wait for the exit: the guest's grace
// before its own SIGKILL, its drain of the last output, and slack.
const KILL_WAIT: Duration = Duration::from_secs(10);
// Without a stop, an Instant past this would overflow.
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error(transparent)]
    Proto(#[from] proto::Error),
    #[error(
        "runner-guest in {name} didn't answer within {}s ({last}): `runner logs {name}` says more",
        GUEST_WAIT.as_secs()
    )]
    NoGuest { name: String, last: io::Error },
    /// Why the guest couldn't run the command.
    #[error("{0}")]
    Guest(String),
    #[error("the connection to runner-guest closed before the command exited")]
    HungUp,
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::Io { what, source }
}

/// Starts the VM unless it's running, then connects to its runner-guest.
pub fn open(store: &Store, name: &str) -> Result<UnixStream> {
    if systemd::status(store, name)?.state != State::Running {
        systemd::start(store, name)?;
    }
    let sock = store.vsock(name)?;
    connect(&sock, proto::PORT, GUEST_WAIT).map_err(|e| match e {
        proto::Error::Io(last) => Error::NoGuest {
            name: name.to_string(),
            last,
        },
        e => e.into(),
    })
}

/// Connects to `port` in the guest through Firecracker's vsock socket at
/// `sock`, and checks the hello. A guest that has just booted takes a moment
/// to listen, so it tries again until `wait` has passed; the error is the
/// last try's. Something that answers but isn't runner-guest fails at once.
pub fn connect(sock: &Path, port: u32, wait: Duration) -> proto::Result<UnixStream> {
    let deadline = Instant::now() + wait;
    loop {
        match try_connect(sock, port) {
            Err(proto::Error::Io(_)) if Instant::now() < deadline => thread::sleep(RETRY),
            done => return done,
        }
    }
}

fn try_connect(sock: &Path, port: u32) -> proto::Result<UnixStream> {
    let mut conn = UnixStream::connect(sock)?;
    conn.set_read_timeout(Some(HANDSHAKE))?;
    conn.write_all(format!("CONNECT {port}\n").as_bytes())?;
    // A byte at a time: the guest's hello comes right after the reply.
    let mut reply = Vec::new();
    while !reply.ends_with(b"\n") && reply.len() < 64 {
        let mut b = [0];
        if conn.read(&mut b)? == 0 {
            break;
        }
        reply.push(b[0]);
    }
    if !(reply.starts_with(b"OK ") && reply.ends_with(b"\n")) {
        let why = format!("nothing in the guest took vsock port {port}");
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, why).into());
    }
    proto::hello(&mut conn)?;
    proto::check_hello(&mut conn)?;
    conn.set_read_timeout(None)?;
    Ok(conn)
}

fn clone(conn: &UnixStream) -> Result<UnixStream> {
    conn.try_clone()
        .map_err(io_err("cloning the connection to runner-guest"))
}

// The host's frames go out from several threads, a whole frame at a time.
type Writer = Arc<Mutex<UnixStream>>;

fn send(writer: &Mutex<UnixStream>, msg: &ToGuest) -> proto::Result<()> {
    proto::write_frame(&mut *writer.lock().unwrap(), msg)
}

/// What a command did, as MCP's exec gives it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Captured {
    /// Its exit code; null if a signal ended it, or the exit never came back.
    pub exit_code: Option<i32>,
    /// The signal that ended it, if one did.
    pub signal: Option<i32>,
    /// The start of its stdout, as UTF-8 with bad bytes replaced.
    pub stdout: String,
    /// True if stdout went on past what's here.
    pub stdout_truncated: bool,
    pub stderr: String,
    pub stderr_truncated: bool,
    /// True if it ran out of time and was killed.
    pub timed_out: bool,
}

/// Runs `req` on `conn` to its end, with `stdin` as its input, then closed,
/// and keeps the first `cap` bytes of each stream. Past `timeout`, the
/// command's process group gets SIGKILL.
pub fn capture(
    conn: UnixStream,
    req: &Request,
    stdin: &[u8],
    timeout: Duration,
    cap: usize,
) -> Result<Captured> {
    let writer: Writer = Arc::new(Mutex::new(clone(&conn)?));
    proto::write_frame(&mut *writer.lock().unwrap(), req)?;
    // Frames are read on their own thread, so the deadline never lands
    // partway through one.
    let (tx, rx) = mpsc::channel();
    let mut reader = clone(&conn)?;
    thread::spawn(move || {
        loop {
            let frame = proto::read_frame::<FromGuest>(&mut reader);
            let output = matches!(frame, Ok(Some(FromGuest::Stdout(_) | FromGuest::Stderr(_))));
            if tx.send(frame).is_err() || !output {
                return;
            }
        }
    });
    // Fed on its own thread too: a command that writes before it reads
    // would otherwise wait on this end forever.
    let feeder = writer.clone();
    let input = stdin.to_vec();
    thread::spawn(move || {
        for chunk in input.chunks(CHUNK) {
            if send(&feeder, &ToGuest::Stdin(chunk.to_vec())).is_err() {
                return;
            }
        }
        let _ = send(&feeder, &ToGuest::CloseStdin);
    });

    let (mut stdout, mut stderr) = (Kept::new(cap), Kept::new(cap));
    let mut deadline = Instant::now() + timeout.min(MAX_TIMEOUT);
    let mut timed_out = false;
    let exit = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ok(Some(FromGuest::Stdout(b)))) => stdout.push(&b),
            Ok(Ok(Some(FromGuest::Stderr(b)))) => stderr.push(&b),
            Ok(Ok(Some(FromGuest::Exit(exit)))) => break Some(exit),
            Ok(Ok(Some(FromGuest::Error(why)))) => return Err(Error::Guest(why)),
            // A hang-up for the kill ends the connection before the exit.
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) if timed_out => break None,
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::HungUp);
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(mpsc::RecvTimeoutError::Timeout) if timed_out => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                if !kill(&writer, &conn) {
                    break None;
                }
                deadline = Instant::now() + KILL_WAIT;
            }
        }
    };
    // Ends the reader and feeder, if they're still going.
    let _ = conn.shutdown(Shutdown::Both);
    Ok(Captured {
        exit_code: match exit {
            Some(Exit::Code(code)) => Some(code),
            _ => None,
        },
        signal: match exit {
            Some(Exit::Signal(sig)) => Some(sig),
            _ => None,
        },
        stdout: stdout.text(),
        stdout_truncated: stdout.cut,
        stderr: stderr.text(),
        stderr_truncated: stderr.cut,
        timed_out,
    })
}

// SIGKILL to the command's group; true if it went, so the exit will come.
// While the feeder holds the writer, stalled behind a command that isn't
// reading its input, it hangs up instead. Once that reaches the guest, it
// sends SIGHUP, then SIGKILL, but Firecracker has ended the connection by
// then, so no exit comes back. Nor does anything reach the guest while its
// stdin queue is full, so in that case the command may live on.
fn kill(writer: &Mutex<UnixStream>, conn: &UnixStream) -> bool {
    if let Ok(mut w) = writer.try_lock()
        && proto::write_frame(&mut *w, &ToGuest::Signal(libc::SIGKILL)).is_ok()
    {
        return true;
    }
    let _ = conn.shutdown(Shutdown::Write);
    false
}

// The start of an output stream.
struct Kept {
    bytes: Vec<u8>,
    cap: usize,
    cut: bool,
}

impl Kept {
    fn new(cap: usize) -> Kept {
        Kept {
            bytes: Vec::new(),
            cap,
            cut: false,
        }
    }

    fn push(&mut self, b: &[u8]) {
        let room = self.cap - self.bytes.len();
        self.cut |= b.len() > room;
        self.bytes.extend_from_slice(&b[..b.len().min(room)]);
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// This terminal's size, from stdin, or 24x80 when stdin isn't one.
pub fn terminal_size() -> WinSize {
    // SAFETY: winsize is plain integers, for which zero is valid.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes one winsize, which lives through the call.
    let got = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if got == -1 || ws.ws_row == 0 || ws.ws_col == 0 {
        return DEFAULT_SIZE;
    }
    WinSize {
        rows: ws.ws_row,
        cols: ws.ws_col,
    }
}

/// Runs `req` on `conn` with this process's stdin, stdout and stderr, until
/// it exits. With a PTY and stdin a terminal, the terminal is raw until this
/// returns, whichever way it does, and the PTY follows its size.
///
/// SIGINT and SIGQUIT go to the command. SIGTERM and SIGHUP hang it up, as
/// closing its terminal would, and a second one cuts the connection. Output
/// that can't be written to stdout hangs it up too, as with `| head`.
pub fn attach(conn: UnixStream, req: &Request) -> Result<Exit> {
    let tty = req.tty.is_some();
    let writer: Writer = Arc::new(Mutex::new(clone(&conn)?));
    proto::write_frame(&mut *writer.lock().unwrap(), req)?;
    let _raw = if tty && io::stdin().is_terminal() {
        let raw = Raw::enter().map_err(|e| io_err("putting the terminal in raw mode")(e.into()))?;
        Some(raw)
    } else {
        None
    };
    forward_signals(&writer, &conn, tty)?;
    let feeder = writer.clone();
    thread::Builder::new()
        .spawn(move || feed_stdin(&feeder, tty))
        .map_err(io_err("starting a thread"))?;

    let mut reader = clone(&conn)?;
    let (mut stdout, mut stderr) = (io::stdout().lock(), io::stderr().lock());
    let mut stdout_open = true;
    loop {
        match proto::read_frame::<FromGuest>(&mut reader)? {
            Some(FromGuest::Stdout(b)) => {
                if stdout_open && stdout.write_all(&b).and_then(|()| stdout.flush()).is_err() {
                    stdout_open = false;
                    let _ = conn.shutdown(Shutdown::Write);
                }
            }
            Some(FromGuest::Stderr(b)) => {
                let _ = stderr.write_all(&b);
            }
            Some(FromGuest::Exit(exit)) => return Ok(exit),
            Some(FromGuest::Error(why)) => return Err(Error::Guest(why)),
            None => return Err(Error::HungUp),
        }
    }
}

// Copies stdin to the command until it ends. A PTY ignores CloseStdin, so
// there the end is the terminal's EOF character, ^D.
fn feed_stdin(writer: &Mutex<UnixStream>, tty: bool) {
    let mut stdin = io::stdin().lock();
    let mut buf = vec![0; CHUNK];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if send(writer, &ToGuest::Stdin(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let end = if tty {
        ToGuest::Stdin(vec![4])
    } else {
        ToGuest::CloseStdin
    };
    let _ = send(writer, &end);
}

// Blocks the signals in this thread, so in every thread it starts after, and
// takes them on two threads of their own. Hang-ups get their own, which never
// waits on the writer, so they land even while stdin is stalled.
fn forward_signals(writer: &Writer, conn: &UnixStream, tty: bool) -> Result<()> {
    let set = |sigs: &[Signal]| {
        let mut set = SigSet::empty();
        sigs.iter().for_each(|s| set.add(*s));
        set
    };
    let pass = set(&[Signal::SIGINT, Signal::SIGQUIT, Signal::SIGWINCH]);
    let hang_up = set(&[Signal::SIGTERM, Signal::SIGHUP]);
    let mut all = pass;
    all.extend(&hang_up);
    all.thread_block()
        .map_err(|e| io_err("blocking signals")(e.into()))?;

    let writer = writer.clone();
    let spawn = |f: Box<dyn FnOnce() + Send>| {
        thread::Builder::new()
            .spawn(f)
            .map(drop)
            .map_err(io_err("starting a thread"))
    };
    spawn(Box::new(move || {
        while let Ok(sig) = pass.wait() {
            let msg = match sig {
                Signal::SIGWINCH if tty => ToGuest::Resize(terminal_size()),
                Signal::SIGWINCH => continue,
                sig => ToGuest::Signal(sig as i32),
            };
            if send(&writer, &msg).is_err() {
                return;
            }
        }
    }))?;
    let conn = clone(conn)?;
    spawn(Box::new(move || {
        let mut how = Shutdown::Write;
        while hang_up.wait().is_ok() {
            let _ = conn.shutdown(how);
            how = Shutdown::Both;
        }
    }))
}

// The terminal in raw mode, so keys like ^C, ^D and ^Z reach the guest's PTY
// as bytes. Dropping it puts the terminal back as it was.
struct Raw {
    saved: Termios,
}

impl Raw {
    fn enter() -> nix::Result<Raw> {
        let saved = termios::tcgetattr(io::stdin())?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(io::stdin(), SetArg::TCSANOW, &raw)?;
        Ok(Raw { saved })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), SetArg::TCSADRAIN, &self.saved);
    }
}
