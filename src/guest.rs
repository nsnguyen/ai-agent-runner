// runner-guest: the agent inside each VM, so the guest needs no sshd and no
// keys. It runs as root under systemd and runs one command per connection,
// as any user, for the host (src/proto.rs has the protocol). Each command
// gets exactly the credentials the host handed over at the agent's start
// (src/creds.rs), which it holds in memory only.
//
// Each connection has a thread that waits for the command, one per output
// stream that forwards it, one that applies the host's frames, and one that
// writes stdin.
use crate::creds::Keyring;
use crate::proto::{self, Exit, FromGuest, Request, ToGuest, WinSize};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, poll};
use nix::pty;
use nix::sys::socket::{SockFlag, accept4};
use nix::sys::stat::{Mode, fchmod};
use nix::sys::wait::{Id, WaitPidFlag, waitid};
use nix::unistd::{Gid, Pid, Uid, User, fchown, geteuid, getgrouplist};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Commands start with this PATH, as under systemd.
pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
// After the command exits, its output is done once it closes, goes quiet for
// QUIET ms, or DRAIN has passed, so a background process left holding it
// can't keep the request open. Sending what the command left unread
// doesn't count toward DRAIN, so a slow host doesn't cost it its last output.
const QUIET: u16 = 100;
const DRAIN: Duration = Duration::from_secs(1);
// Stdin queued for a command that isn't reading it. Past this, the host's
// frames wait, but its hang-up still lands.
const MAX_QUEUED: usize = 1 << 20;
// A command still running this long after the host hangs up is killed.
const GRACE: Duration = Duration::from_secs(2);

/// A connection from the host: vsock in a VM, a Unix socket in tests.
pub trait Conn: Read + Write + AsFd + Send + Sized + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    /// Shuts down both ways, which wakes a thread reading it.
    fn shutdown(&self) -> io::Result<()>;
}

impl Conn for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn shutdown(&self) -> io::Result<()> {
        UnixStream::shutdown(self, std::net::Shutdown::Both)
    }
}

impl Conn for vsock::VsockStream {
    fn try_clone(&self) -> io::Result<Self> {
        vsock::VsockStream::try_clone(self)
    }
    fn shutdown(&self) -> io::Result<()> {
        vsock::VsockStream::shutdown(self, std::net::Shutdown::Both)
    }
}

/// Accepts with close-on-exec set at once. The vsock crate sets it after
/// accepting, when a command starting on another thread could inherit the
/// connection.
pub fn accept_vsock(listener: &vsock::VsockListener) -> io::Result<vsock::VsockStream> {
    let fd = accept4(listener.as_raw_fd(), SockFlag::SOCK_CLOEXEC)?;
    // SAFETY: accept4 just made this fd, and nothing else owns it.
    Ok(unsafe { vsock::VsockStream::from_raw_fd(fd) })
}

/// Serves each connection on its own thread, for as long as they come. Every
/// command gets `keyring`'s credentials.
pub fn serve<C: Conn>(conns: impl Iterator<Item = io::Result<C>>, keyring: Arc<Keyring>) {
    for conn in conns {
        let keyring = keyring.clone();
        let served =
            conn.and_then(|conn| thread::Builder::new().spawn(move || handle(conn, &keyring)));
        if let Err(e) = served {
            eprintln!("runner-guest: accept: {e}");
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn handle<C: Conn>(mut conn: C, keyring: &Keyring) {
    if let Err(e) = session(&mut conn, keyring) {
        eprintln!("runner-guest: {e}");
    }
}

fn session<C: Conn>(conn: &mut C, keyring: &Keyring) -> proto::Result<()> {
    proto::hello(conn)?;
    proto::check_hello(conn)?;
    let Some(req) = proto::read_frame::<Request>(conn)? else {
        return Ok(());
    };
    let out = Mutex::new(conn.try_clone()?);
    let host = conn.try_clone()?;
    let Running {
        mut child,
        group,
        stdin,
        pty,
        outputs,
    } = match start(&req, keyring) {
        Ok(running) => running,
        Err(e) => return send(&out, FromGuest::Error(e)),
    };
    let input = Feed::new(stdin).and_then(|stdin| {
        let group = group.clone();
        thread::Builder::new().spawn(move || take_input(host, stdin, pty, &group))
    });
    let input = match input {
        Ok(input) => input,
        Err(e) => {
            group.signal(libc::SIGKILL);
            let _ = reap(&mut child, &group);
            return send(&out, FromGuest::Error(format!("starting a thread: {e}")));
        }
    };
    // The scope joins the pumps, so all output is sent before the exit.
    let status = thread::scope(|s| {
        for (src, wrap) in outputs {
            let (out, group) = (&out, &*group);
            let pumping =
                thread::Builder::new().spawn_scoped(s, move || pump(src, wrap, out, group));
            if let Err(e) = pumping {
                eprintln!("runner-guest: starting a thread: {e}");
                group.signal(libc::SIGKILL);
            }
        }
        reap(&mut child, &group)
    });
    let last = match status {
        Ok(status) => FromGuest::Exit(match status.code() {
            Some(code) => Exit::Code(code),
            None => Exit::Signal(status.signal().unwrap_or(0)),
        }),
        Err(e) => FromGuest::Error(format!("waiting for {}: {e}", req.argv[0])),
    };
    let sent = send(&out, last);
    let _ = conn.shutdown();
    let _ = input.join();
    sent
}

fn send<C: Conn>(out: &Mutex<C>, msg: FromGuest) -> proto::Result<()> {
    proto::write_frame(&mut *out.lock().unwrap(), &msg)
}

// A started command, and where its input goes and its output comes from.
struct Running {
    child: Child,
    group: Arc<Group>,
    stdin: File,
    pty: Option<File>,
    outputs: Vec<Output>,
}

// An output stream, and the frame its bytes go out in.
type Output = (File, fn(Vec<u8>) -> FromGuest);

fn start(req: &Request, keyring: &Keyring) -> Result<Running, String> {
    let Some((program, args)) = req.argv.split_first() else {
        return Err("nothing to run: argv is empty".into());
    };
    let user = match &req.user {
        Some(name) => Some(
            User::from_name(name)
                .map_err(|e| format!("looking up user {name}: {e}"))?
                .ok_or_else(|| format!("no user named {name}"))?,
        ),
        None => User::from_uid(geteuid()).ok().flatten(),
    };
    let mut cmd = Command::new(program);
    cmd.args(args).env_clear().env("PATH", PATH);
    if let Some(u) = &user {
        cmd.env("HOME", &u.dir)
            .env("USER", &u.name)
            .env("LOGNAME", &u.name)
            .env("SHELL", &u.shell);
    }
    if req.tty.is_some() {
        cmd.env("TERM", "xterm-256color");
    }
    cmd.envs(req.env.iter().map(|(k, v)| (k, v)));
    // Last, so the request can't add or change one. cmd's Debug output now
    // holds their values: it's never printed.
    keyring.apply(&mut cmd);
    let cwd = match &req.cwd {
        Some(cwd) if !Path::new(cwd).is_dir() => return Err(format!("no directory {cwd}")),
        Some(cwd) => PathBuf::from(cwd),
        None => user
            .as_ref()
            .map(|u| u.dir.clone())
            .filter(|home| home.is_dir())
            .unwrap_or_else(|| "/".into()),
    };
    // Entered in the child as the user, so root's access doesn't leak.
    let dir = CString::new(cwd.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let ids = match &user {
        Some(u) if u.uid != geteuid() => Some(Ids::of(u)?),
        _ => None,
    };

    let master = match req.tty {
        Some(size) => {
            let (master, slave) = open_pty(size).map_err(|e| format!("opening a pty: {e}"))?;
            if let Some(ids) = &ids {
                hand_over(&slave, ids).map_err(|e| format!("handing over the pty: {e}"))?;
            }
            let dup = |fd: &OwnedFd| fd.try_clone().map_err(|e| e.to_string());
            cmd.stdin(dup(&slave)?).stdout(dup(&slave)?).stderr(slave);
            let dup = |f: &File| f.try_clone().map_err(|e| e.to_string());
            Some((dup(&master)?, dup(&master)?, master))
        }
        None => {
            let piped = Stdio::piped;
            cmd.stdin(piped())
                .stdout(piped())
                .stderr(piped())
                .process_group(0);
            None
        }
    };
    let tty = master.is_some();
    // SAFETY: in_child makes only async-signal-safe calls.
    unsafe { cmd.pre_exec(move || in_child(tty, ids.as_ref(), &dir)) };
    let spawned = cmd.spawn();
    // Drops the slave ends it holds, so the PTY closes when the command does.
    drop(cmd);
    let in_dir = cwd.display();
    let mut child = spawned.map_err(|e| format!("couldn't run {program} in {in_dir}: {e}"))?;
    let group = Arc::new(Group {
        pgid: child.id() as libc::pid_t,
        exited: Mutex::new(None),
    });

    let (stdin, pty, outputs) = match master {
        Some((stdin, out, master)) => {
            let outputs = vec![(out, FromGuest::Stdout as fn(_) -> _)];
            (stdin, Some(master), outputs)
        }
        None => {
            let file = |fd: OwnedFd| File::from(fd);
            let stdin = file(child.stdin.take().unwrap().into());
            let out = file(child.stdout.take().unwrap().into());
            let err = file(child.stderr.take().unwrap().into());
            let outputs = vec![
                (out, FromGuest::Stdout as fn(_) -> _),
                (err, FromGuest::Stderr),
            ];
            (stdin, None, outputs)
        }
    };
    Ok(Running {
        child,
        group,
        stdin,
        pty,
        outputs,
    })
}

// Who a command runs as, looked up before the fork, after which nothing
// may allocate.
struct Ids {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
}

impl Ids {
    fn of(user: &User) -> Result<Ids, String> {
        let name = CString::new(user.name.as_str()).map_err(|e| e.to_string())?;
        let groups = getgrouplist(&name, user.gid)
            .map_err(|e| format!("looking up the groups of {}: {e}", user.name))?;
        Ok(Ids {
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            groups: groups.iter().map(|g| g.as_raw()).collect(),
        })
    }
}

// Runs in the child between fork and exec, so it makes raw syscalls only.
fn in_child(tty: bool, ids: Option<&Ids>, dir: &CString) -> io::Result<()> {
    let check = |r: libc::c_int| match r {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    };
    // SAFETY: plain syscalls; groups and dir outlive the call.
    unsafe {
        if tty {
            // A new session, with the PTY on stdin as its controlling terminal.
            check(libc::setsid())?;
            check(libc::ioctl(0, libc::TIOCSCTTY, 0))?;
        }
        if let Some(ids) = ids {
            check(libc::setgroups(ids.groups.len(), ids.groups.as_ptr()))?;
            check(libc::setgid(ids.gid))?;
            check(libc::setuid(ids.uid))?;
        }
        check(libc::chdir(dir.as_ptr()))?;
    }
    Ok(())
}

// Both ends close on exec, so one command's PTY never leaks into another
// connection's command.
fn open_pty(size: WinSize) -> nix::Result<(File, OwnedFd)> {
    let flags = OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC;
    let master = pty::posix_openpt(flags)?;
    pty::grantpt(&master)?;
    pty::unlockpt(&master)?;
    let slave = nix::fcntl::open(pty::ptsname_r(&master)?.as_str(), flags, Mode::empty())?;
    let master = File::from(OwnedFd::from(master));
    resize(&master, size)?;
    Ok((master, slave))
}

// Gives the PTY to the user it runs as, group tty, as sshd does: programs
// that reopen it by name need to.
fn hand_over(slave: &OwnedFd, ids: &Ids) -> nix::Result<()> {
    let (gid, mode) = match nix::unistd::Group::from_name("tty")? {
        Some(tty) => (tty.gid, 0o620),
        None => (Gid::from_raw(ids.gid), 0o600),
    };
    fchown(slave, Some(Uid::from_raw(ids.uid)), Some(gid))?;
    fchmod(slave, Mode::from_bits_truncate(mode))
}

fn nonblocking(file: &File) -> nix::Result<()> {
    let flags = OFlag::from_bits_retain(fcntl(file, FcntlArg::F_GETFL)?);
    fcntl(file, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(drop)
}

// The kernel signals SIGWINCH to the PTY's foreground process group.
fn resize(pty: &File, size: WinSize) -> nix::Result<()> {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one winsize, which lives through the call.
    Errno::result(unsafe { libc::ioctl(pty.as_raw_fd(), libc::TIOCSWINSZ, &ws) }).map(drop)
}

// The command's process group. Signals reach the whole group until the
// command is reaped, and never after, so a reused pid can't get one.
struct Group {
    pgid: libc::pid_t,
    exited: Mutex<Option<Instant>>,
}

impl Group {
    fn signal(&self, sig: i32) {
        let exited = self.exited.lock().unwrap();
        if exited.is_none() {
            // SAFETY: kill takes no pointers. A bad signal number is EINVAL.
            unsafe { libc::kill(-self.pgid, sig) };
        }
    }

    fn exited(&self) -> Option<Instant> {
        *self.exited.lock().unwrap()
    }
}

// Waits for the command to exit, stops signals to its group, then reaps it.
fn reap(child: &mut Child, group: &Group) -> io::Result<ExitStatus> {
    let exited = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT;
    let waited = loop {
        match waitid(Id::Pid(Pid::from_raw(group.pgid)), exited) {
            Err(Errno::EINTR) => continue,
            r => break r,
        }
    };
    *group.exited.lock().unwrap() = Some(Instant::now());
    waited?;
    child.wait()
}

// Forwards one output stream to the host until it closes, or until the
// command has exited and the stream has gone quiet.
fn pump<C: Conn>(mut src: File, wrap: fn(Vec<u8>) -> FromGuest, out: &Mutex<C>, group: &Group) {
    let mut buf = vec![0; 32 * 1024];
    // Once the pump sees the exit: when, the bytes then unread, and the time
    // spent sending those, which DRAIN doesn't count.
    let mut drain: Option<(Instant, usize)> = None;
    let mut excused = Duration::ZERO;
    loop {
        // Checked before the wait, so the last wait starts after the exit and
        // still catches output on its way through the PTY.
        let exited = group.exited().is_some();
        if exited && drain.is_none() {
            drain = Some((Instant::now(), unread(&src)));
        }
        if drain.is_some_and(|(since, _)| since.elapsed().saturating_sub(excused) > DRAIN) {
            return;
        }
        let mut fds = [PollFd::new(src.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, QUIET) {
            Ok(0) if exited => return,
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => {}
            Err(e) => return eprintln!("runner-guest: output: {e}"),
        }
        match src.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let start = Instant::now();
                if send(out, wrap(buf[..n].to_vec())).is_err() {
                    return;
                }
                if let Some((_, owed @ 1..)) = &mut drain {
                    excused += start.elapsed();
                    *owed = owed.saturating_sub(n);
                }
            }
            // The PTY is nonblocking for its stdin's sake.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            // A PTY whose command has gone reads EIO.
            Err(_) => return,
        }
    }
}

// Bytes waiting to be read from a pipe or PTY.
fn unread(src: &File) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int, which lives through the call.
    match unsafe { libc::ioctl(src.as_raw_fd(), libc::FIONREAD, &mut n) } {
        -1 => 0,
        _ => n.max(0) as usize,
    }
}

// Applies the host's frames until it hangs up, which hangs up the command,
// as closing a terminal does, and kills it if it ignores that.
fn take_input<C: Conn>(mut host: C, mut stdin: Feed, pty: Option<File>, group: &Group) {
    while stdin.wait_for_room(host.as_fd()) {
        match proto::read_frame::<ToGuest>(&mut host) {
            Ok(Some(ToGuest::Stdin(data))) => stdin.push(data),
            Ok(Some(ToGuest::CloseStdin)) => {
                if pty.is_none() {
                    stdin.close();
                }
            }
            Ok(Some(ToGuest::Resize(size))) => {
                if let Some(pty) = &pty {
                    let _ = resize(pty, size);
                }
            }
            Ok(Some(ToGuest::Signal(sig))) => group.signal(sig),
            Ok(None) => break,
            Err(e) => {
                eprintln!("runner-guest: from the host: {e}");
                break;
            }
        }
    }
    group.signal(libc::SIGHUP);
    let start = Instant::now();
    while group.exited().is_none() {
        if start.elapsed() > GRACE {
            return group.signal(libc::SIGKILL);
        }
        thread::sleep(Duration::from_millis(QUIET.into()));
    }
}

// The command's stdin, written from a queue on its own thread, so the host's
// signals and resizes don't wait behind input the command hasn't read.
struct Feed {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Feed {
    fn new(mut stdin: File) -> io::Result<Feed> {
        nonblocking(&stdin)?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (left, stopped) = (queued.clone(), stop.clone());
        // Closing the Feed closes stdin once the queue is written; dropping
        // it drops the rest.
        thread::Builder::new().spawn(move || {
            let mut open = true;
            for chunk in rx {
                // A command that closed its stdin takes no more.
                open = open && write_until(&mut stdin, &chunk, &stopped);
                left.fetch_sub(chunk.len(), Ordering::Relaxed);
            }
        })?;
        Ok(Feed {
            tx: Some(tx),
            queued,
            stop,
        })
    }

    fn push(&self, data: Vec<u8>) {
        if let Some(tx) = &self.tx {
            self.queued.fetch_add(data.len(), Ordering::Relaxed);
            let _ = tx.send(data);
        }
    }

    fn close(&mut self) {
        self.tx = None;
    }

    // Past the cap, waits for the command to read, but not past a hang-up:
    // false means the host hung up.
    fn wait_for_room(&self, host: BorrowedFd) -> bool {
        // nix doesn't know RDHUP's bit, so it reads as None, not true.
        let rdhup = PollFlags::from_bits_retain(libc::POLLRDHUP);
        while self.queued.load(Ordering::Relaxed) > MAX_QUEUED {
            let mut fds = [PollFd::new(host, rdhup)];
            let _ = poll(&mut fds, QUIET);
            if fds[0].any() != Some(false) {
                return false;
            }
        }
        true
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// Writes all of data unless the command closes its stdin or stop is set
// first. It polls, so a command that never reads can't hold the thread.
fn write_until(stdin: &mut File, mut data: &[u8], stop: &AtomicBool) -> bool {
    while !data.is_empty() && !stop.load(Ordering::Relaxed) {
        match stdin.write(data) {
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLOUT)];
                let _ = poll(&mut fds, QUIET);
            }
            Err(_) => return false,
        }
    }
    data.is_empty()
}
