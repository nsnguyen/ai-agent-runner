// The host's side of the guest protocol (src/proto.rs), under `runner exec`,
// `runner shell`, MCP's exec and headless runs. Each command is one
// connection to the VM's runner-guest, through Firecracker's vsock socket:
// write `CONNECT <port>`, and Firecracker answers `OK <n>` once a listener in
// the guest takes it.
//
// Firecracker ends a connection both ways once the host closes its side,
// even only for writing, so the host only does that to hang up.
use crate::creds;
use crate::idle::InUse;
use crate::proto::{self, Exit, FromGuest, Request, ToGuest, WinSize};
use crate::store::Store;
use crate::systemd::{self, State};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use rmcp::schemars;
use serde::Serialize;
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Who commands in a VM run as unless told otherwise: the base image's user.
pub const USER: &str = "agent";
/// How long a VM's runner-guest gets to answer, for a VM that has just booted.
pub const GUEST_WAIT: Duration = Duration::from_secs(60);
/// How much of each stream MCP's exec gives back whole; past it, the first
/// and last half of that.
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
// A writer held this long is stalled behind stdin the command isn't reading,
// not partway through a frame.
const STALLED: Duration = Duration::from_millis(100);
// After a timeout's kill, how long to wait for the exit: the guest's grace
// before its own SIGKILL, its drain of the last output, and slack.
const KILL_WAIT: Duration = Duration::from_secs(10);
// How often `stream` asks whether to stop.
const POLL: Duration = Duration::from_millis(100);
/// The longest exec waits on a command. Without a stop, an Instant past
/// this would overflow.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error(transparent)]
    Proto(#[from] proto::Error),
    #[error(transparent)]
    Refused(#[from] creds::Refused),
    #[error("runner-guest in {name} didn't answer ({last}): `runner logs {name}` says more")]
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

/// A connection to a VM's runner-guest, from a command using the VM: its
/// boot won't put it to sleep for want of use until both are dropped.
#[derive(Debug)]
pub struct Conn {
    pub stream: UnixStream,
    pub in_use: InUse,
}

/// Starts the VM unless it's running, waking it if it's sleeping, then
/// connects to its runner-guest.
pub fn open(store: &Store, name: &str) -> Result<Conn> {
    // Before the look, so the VM can't go to sleep between it and the connect.
    let in_use = InUse::take(store, name)?;
    if systemd::status(store, name)?.state != State::Running {
        systemd::start(store, name)?;
    }
    let stream = reach(store, name)?;
    Ok(Conn { stream, in_use })
}

/// Connects to the VM's runner-guest if the VM is up, waking it if it's
/// sleeping; None if it's stopped, and it stays that way.
pub fn open_running(store: &Store, name: &str) -> Result<Option<Conn>> {
    let in_use = InUse::take(store, name)?;
    match systemd::status(store, name)?.state {
        State::Running => {}
        State::Sleeping => {
            systemd::wake(store, name)?;
        }
        _ => return Ok(None),
    }
    let stream = reach(store, name)?;
    Ok(Some(Conn { stream, in_use }))
}

/// Holds the VM in use, starting it first unless it's running, until
/// `conn`'s far end closes: what the laptop's `runner proxy` runs on the host
/// beside its tunnel, so the VM doesn't idle-sleep under the proxy. Over SSH,
/// `conn` is stdout, a pipe sshd closes once the connection goes. It reads
/// nothing, so stdin closing isn't it. A signal ends it too, and the lock goes
/// with the process.
pub fn hold(store: &Store, name: &str, conn: impl AsFd) -> Result<()> {
    let _in_use = InUse::take(store, name)?;
    if systemd::status(store, name)?.state != State::Running {
        systemd::start(store, name)?;
    }
    hung_up(conn.as_fd()).map_err(io_err("waiting for the connection to close"))
}

// Returns once `fd`'s far end has closed. It asks poll for no events, so
// only a hang-up or an error ends the wait: POLLERR for a pipe, POLLHUP for
// a socket or a terminal.
fn hung_up(fd: BorrowedFd) -> io::Result<()> {
    let mut fds = [PollFd::new(fd, PollFlags::empty())];
    loop {
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// Connects to the VM's runner-guest, waiting for one that's booting.
pub fn reach(store: &Store, name: &str) -> Result<UnixStream> {
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
/// last try's. An error that waiting won't clear, like something answering
/// that isn't runner-guest, fails at once.
pub fn connect(sock: &Path, port: u32, wait: Duration) -> proto::Result<UnixStream> {
    let deadline = Instant::now() + wait;
    loop {
        match try_connect(sock, port) {
            Err(proto::Error::Io(e)) if passing(&e) && Instant::now() < deadline => {
                thread::sleep(RETRY)
            }
            done => return done,
        }
    }
}

// What a VM that's still booting, or a guest agent restarting, looks like.
fn passing(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(
        e.kind(),
        NotFound | ConnectionRefused | ConnectionReset | ConnectionAborted | WouldBlock | TimedOut
    )
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

// Every request leaves the host here, so none sets a credential's variable,
// whichever command built it.
fn send_request(writer: &Mutex<UnixStream>, req: &Request) -> Result<()> {
    creds::refuse(req.env.iter().map(|(k, _)| k.as_str()))?;
    Ok(proto::write_frame(&mut *writer.lock().unwrap(), req)?)
}

fn send(writer: &Mutex<UnixStream>, msg: &ToGuest) -> proto::Result<()> {
    proto::write_frame(&mut *writer.lock().unwrap(), msg)
}

// Sends unless the writer is stalled; None if it is. Then nothing reaches the
// guest until the command reads its stdin, and Firecracker likely won't pass
// on a hang-up either: it stops reading this end while the guest is full.
fn send_unless_stalled(writer: &Mutex<UnixStream>, msg: &ToGuest) -> Option<proto::Result<()>> {
    let start = Instant::now();
    loop {
        match writer.try_lock() {
            Ok(mut w) => return Some(proto::write_frame(&mut *w, msg)),
            Err(TryLockError::Poisoned(_)) => return None,
            Err(TryLockError::WouldBlock) if start.elapsed() > STALLED => return None,
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

// Shuts the connection down both ways when dropped, which ends any thread
// still using it, whichever way the caller returns.
struct Cut(UnixStream);

impl Drop for Cut {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

// A command under way on its connection, with its stdin fed and closed.
struct Begun {
    _cut: Cut,
    writer: Writer,
    /// The guest's frames, up to and including the first that isn't output.
    frames: mpsc::Receiver<proto::Result<Option<FromGuest>>>,
}

// Sends `req`, then `stdin` and its close. Frames are read on their own
// thread, so a deadline never lands partway through one, and stdin is fed on
// its own thread too: a command that writes before it reads would otherwise
// wait on this end forever.
fn begin(conn: &UnixStream, req: &Request, stdin: &[u8]) -> Result<Begun> {
    let cut = Cut(clone(conn)?);
    let writer: Writer = Arc::new(Mutex::new(clone(conn)?));
    send_request(&writer, req)?;
    let (tx, frames) = mpsc::channel();
    let mut reader = clone(conn)?;
    thread::spawn(move || {
        loop {
            let frame = proto::read_frame::<FromGuest>(&mut reader);
            let output = matches!(frame, Ok(Some(FromGuest::Stdout(_) | FromGuest::Stderr(_))));
            if tx.send(frame).is_err() || !output {
                return;
            }
        }
    });
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
    Ok(Begun {
        _cut: cut,
        writer,
        frames,
    })
}

/// What a command did, as MCP's exec gives it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Captured {
    /// Its exit code; null if a signal ended it, or the exit never came back.
    pub exit_code: Option<i32>,
    /// The signal that ended it, if one did.
    pub signal: Option<i32>,
    /// Its stdout, as UTF-8 with bad bytes replaced: whole up to the cap,
    /// and past it the first and last half of that, with a line between
    /// saying how many bytes were cut.
    pub stdout: String,
    /// True if stdout went on past the cap, so some of it was cut.
    pub stdout_truncated: bool,
    pub stderr: String,
    pub stderr_truncated: bool,
    /// True if it ran out of time and was killed.
    pub timed_out: bool,
}

/// Runs `req` on `conn` to its end, with `stdin` as its input, then closed,
/// and keeps each stream as `Kept` does with `cap`. Past `timeout`, the
/// command's process group gets SIGKILL.
pub fn capture(
    conn: UnixStream,
    req: &Request,
    stdin: &[u8],
    timeout: Duration,
    cap: usize,
) -> Result<Captured> {
    let Begun {
        _cut,
        writer,
        frames: rx,
    } = begin(&conn, req, stdin)?;
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
        stdout_truncated: stdout.cut(),
        stderr: stderr.text(),
        stderr_truncated: stderr.cut(),
        timed_out,
    })
}

/// How a command `stream` ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Streamed {
    /// None if the exit never came back, after a stop that had to hang up.
    pub exit: Option<Exit>,
    /// True if `stop` said to stop it.
    pub stopped: bool,
}

/// Runs `req` on `conn` to its end, with `stdin` as its input, then closed.
/// Its stdout goes to `out` and its stderr to `err` as they come. `stop` is
/// asked every tenth of a second; once it says yes, the command's process group
/// gets SIGTERM, and SIGKILL `grace` later.
pub fn stream(
    conn: UnixStream,
    req: &Request,
    stdin: &[u8],
    out: &mut impl Write,
    err: &mut impl Write,
    stop: impl Fn() -> bool,
    grace: Duration,
) -> Result<Streamed> {
    let Begun {
        _cut,
        writer,
        frames: rx,
    } = begin(&conn, req, stdin)?;
    let write = |to: &mut dyn Write, b: &[u8]| {
        to.write_all(b)
            .and_then(|()| to.flush())
            .map_err(io_err("writing the command's output"))
    };
    // When SIGTERM went, then when SIGKILL did.
    let (mut termed, mut killed): (Option<Instant>, Option<Instant>) = (None, None);
    let mut asked = Instant::now();
    let exit = loop {
        match rx.recv_timeout(POLL) {
            Ok(Ok(Some(FromGuest::Stdout(b)))) => write(out, &b)?,
            Ok(Ok(Some(FromGuest::Stderr(b)))) => write(err, &b)?,
            Ok(Ok(Some(FromGuest::Exit(exit)))) => break Some(exit),
            Ok(Ok(Some(FromGuest::Error(why)))) => return Err(Error::Guest(why)),
            // A hang-up for the kill ends the connection before the exit.
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) if termed.is_some() => {
                break None;
            }
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::HungUp);
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if termed.is_none() && asked.elapsed() >= POLL {
            asked = Instant::now();
            if stop() {
                termed = Some(Instant::now());
                let sigterm = ToGuest::Signal(libc::SIGTERM);
                if !matches!(send_unless_stalled(&writer, &sigterm), Some(Ok(()))) {
                    let _ = conn.shutdown(Shutdown::Both);
                    break None;
                }
            }
        }
        match (termed, killed) {
            (Some(t), None) if t.elapsed() > grace => {
                if !kill(&writer, &conn) {
                    break None;
                }
                killed = Some(Instant::now());
            }
            (_, Some(k)) if k.elapsed() > KILL_WAIT => {
                let _ = conn.shutdown(Shutdown::Both);
                break None;
            }
            _ => {}
        }
    };
    Ok(Streamed {
        exit,
        stopped: termed.is_some(),
    })
}

// SIGKILL to the command's group; true if it went, so the exit will come.
// With the writer stalled it hangs up instead, which may never reach the
// guest, and no exit comes back after it.
fn kill(writer: &Mutex<UnixStream>, conn: &UnixStream) -> bool {
    let sigkill = ToGuest::Signal(libc::SIGKILL);
    if let Some(Ok(())) = send_unless_stalled(writer, &sigkill) {
        return true;
    }
    let _ = conn.shutdown(Shutdown::Both);
    false
}

/// An output stream kept to `cap` bytes: whole up to there, and past it its
/// first and last half (the first gets an odd byte), so the error a long
/// command prints last isn't lost.
pub struct Kept {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    /// Every byte pushed.
    seen: u64,
}

impl Kept {
    pub fn new(cap: usize) -> Kept {
        Kept {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_cap: cap - cap / 2,
            tail_cap: cap / 2,
            seen: 0,
        }
    }

    pub fn push(&mut self, b: &[u8]) {
        self.seen += b.len() as u64;
        let (head, rest) = b.split_at(b.len().min(self.head_cap - self.head.len()));
        self.head.extend_from_slice(head);
        let rest = &rest[rest.len().saturating_sub(self.tail_cap)..];
        let over = (self.tail.len() + rest.len()).saturating_sub(self.tail_cap);
        self.tail.drain(..over);
        self.tail.extend(rest);
    }

    /// True if bytes between the two ends were dropped.
    pub fn cut(&self) -> bool {
        self.seen > (self.head.len() + self.tail.len()) as u64
    }

    /// The stream as text. Cut, a line between its ends says how many bytes
    /// went, and a character the cut splits goes whole, rather than leaving
    /// U+FFFD; it's counted in those bytes.
    pub fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if !self.cut() {
            return String::from_utf8_lossy(&[&self.head[..], &tail].concat()).into_owned();
        }
        let mut head = &self.head[..];
        if let Err(e) = std::str::from_utf8(head)
            && e.error_len().is_none()
        {
            head = &head[..e.valid_up_to()];
        }
        // A UTF-8 character has at most three continuation bytes.
        let split = tail
            .iter()
            .take(3)
            .take_while(|b| *b & 0xC0 == 0x80)
            .count();
        let tail = &tail[split..];
        let gone = self.seen - (head.len() + tail.len()) as u64;
        let s = if gone == 1 { "" } else { "s" };
        format!(
            "{}\n[runner cut {gone} byte{s} here]\n{}",
            String::from_utf8_lossy(head),
            String::from_utf8_lossy(tail),
        )
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
/// closing its terminal would, and so does SIGINT or SIGQUIT when stdin is
/// stalled and it can't get through. So does stdout closing, as with `| head`.
/// A hang-up returns at once, as if the signal, or SIGPIPE, had killed the
/// command: the exit can't come back after it.
pub fn attach(conn: UnixStream, req: &Request) -> Result<Exit> {
    let tty = req.tty.is_some();
    let writer: Writer = Arc::new(Mutex::new(clone(&conn)?));
    send_request(&writer, req)?;
    let hang_up = Arc::new(HangUp {
        conn: clone(&conn)?,
        by: AtomicI32::new(0),
    });
    // Signals first, so none can kill this process with the terminal raw.
    forward_signals(&writer, &hang_up, tty)?;
    let _raw = if tty && io::stdin().is_terminal() {
        let raw = Raw::enter().map_err(|e| io_err("putting the terminal in raw mode")(e.into()))?;
        Some(raw)
    } else {
        None
    };
    let feeder = writer.clone();
    thread::Builder::new()
        .spawn(move || feed_stdin(&feeder, tty))
        .map_err(io_err("starting a thread"))?;

    let mut reader = clone(&conn)?;
    let (mut stdout, mut stderr) = (io::stdout().lock(), io::stderr().lock());
    loop {
        let frame = match (proto::read_frame::<FromGuest>(&mut reader), hang_up.by()) {
            (Ok(Some(FromGuest::Exit(exit))), _) => return Ok(exit),
            (_, Some(sig)) => return Ok(Exit::Signal(sig)),
            (frame, None) => frame?,
        };
        match frame {
            Some(FromGuest::Stdout(b)) => {
                if stdout.write_all(&b).and_then(|()| stdout.flush()).is_err() {
                    hang_up.up(libc::SIGPIPE);
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

// Ending the session from this side. The guest hangs the command up once it
// hears, but Firecracker ends the connection both ways first, so attach
// stops reading and reports the signal that did it.
struct HangUp {
    conn: UnixStream,
    by: AtomicI32,
}

impl HangUp {
    fn up(&self, sig: i32) {
        let _ = self
            .by
            .compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
        let _ = self.conn.shutdown(Shutdown::Both);
    }

    fn by(&self) -> Option<i32> {
        Some(self.by.load(Ordering::SeqCst)).filter(|&sig| sig != 0)
    }
}

// Copies stdin to the command until it ends. A PTY ignores CloseStdin, so
// there the end is the terminal's EOF character, ^D, twice after a partial
// line, since the first only sends the line.
fn feed_stdin(writer: &Mutex<UnixStream>, tty: bool) {
    let mut stdin = io::stdin().lock();
    let mut buf = vec![0; CHUNK];
    let mut line_done = true;
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                line_done = buf[n - 1] == b'\n';
                if send(writer, &ToGuest::Stdin(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let end = match (tty, line_done) {
        (true, true) => ToGuest::Stdin(vec![4]),
        (true, false) => ToGuest::Stdin(vec![4, 4]),
        (false, _) => ToGuest::CloseStdin,
    };
    let _ = send(writer, &end);
}

// Blocks the signals in this thread, so in every thread it starts after, and
// takes them on two threads of their own. Hang-ups get their own, which never
// waits on the writer, so they're taken even while stdin is stalled.
fn forward_signals(writer: &Writer, hang_up: &Arc<HangUp>, tty: bool) -> Result<()> {
    let set = |sigs: &[Signal]| {
        let mut set = SigSet::empty();
        sigs.iter().for_each(|s| set.add(*s));
        set
    };
    let pass = set(&[Signal::SIGINT, Signal::SIGQUIT, Signal::SIGWINCH]);
    let hang_ups = set(&[Signal::SIGTERM, Signal::SIGHUP]);
    let mut all = pass;
    all.extend(&hang_ups);
    all.thread_block()
        .map_err(|e| io_err("blocking signals")(e.into()))?;

    let (writer, stalled) = (writer.clone(), hang_up.clone());
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
            match send_unless_stalled(&writer, &msg) {
                Some(Ok(())) => {}
                Some(Err(_)) => return,
                // A resize can wait for the next one.
                None if sig == Signal::SIGWINCH => {}
                None => stalled.up(sig as i32),
            }
        }
    }))?;
    let hang_up = hang_up.clone();
    spawn(Box::new(move || {
        while let Ok(sig) = hang_ups.wait() {
            hang_up.up(sig as i32);
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
