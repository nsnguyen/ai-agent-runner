// Stand-ins for what the host provides: Firecracker's API socket, a
// `firecracker` binary, systemctl and journalctl on PATH, and ext4 disks
// built with e2fsprogs. Each test file uses its own share of them.
#![allow(dead_code)]

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Polls until `done` holds, or panics after a few seconds.
pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub type Reply = (u16, Option<String>);

/// Firecracker's API on a Unix socket: records every request and answers
/// with what `respond` says. 204 with no body is Firecracker's "done".
pub struct FakeApi {
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeApi {
    pub fn serve(sock: &Path, respond: impl Fn(&Request) -> Reply + Send + 'static) -> FakeApi {
        let listener = UnixListener::bind(sock).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut writer = conn;
                // Several requests may share one connection, as with keep-alive.
                while let Some(req) = read_request(&mut reader) {
                    seen.lock().unwrap().push(req.clone());
                    let (status, body) = respond(&req);
                    let body = body.unwrap_or_default();
                    let head = if body.is_empty() {
                        format!("HTTP/1.1 {status} \r\nServer: Firecracker API\r\n\r\n")
                    } else {
                        format!(
                            "HTTP/1.1 {status} \r\nServer: Firecracker API\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        )
                    };
                    if writer
                        .write_all(format!("{head}{body}").as_bytes())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        FakeApi { requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Each request as `METHOD path body`, for comparing a whole conversation.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.requests()
            .into_iter()
            .map(|r| (format!("{} {}", r.method, r.path), r.body))
            .collect()
    }
}

fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

/// A `firecracker` that prints the version it's told to (FAKE_FC_VERSION,
/// or what's in its `version` file, as if upgraded), records its pid and argv
/// and environment, writes a line of "console", then exits with whatever number is written
/// to its `exit` fifo. The test serves its API socket.
#[derive(Clone)]
pub struct FakeFirecracker {
    pub dir: PathBuf,
    pub bin: PathBuf,
}

impl FakeFirecracker {
    pub fn new(dir: &Path) -> FakeFirecracker {
        fs::create_dir_all(dir).unwrap();
        let bin = dir.join("firecracker");
        script(
            &bin,
            r#"if [ "$1" = --version ]; then
  v=$FAKE_FC_VERSION; [ -f "$FAKE_FC/version" ] && v=$(cat "$FAKE_FC/version")
  printf 'Firecracker %s\n\n' "$v"; exit 0
fi
grep '^CapAmb' /proc/$$/status > "$FAKE_FC/caps"
env > "$FAKE_FC/env"
echo $$ > "$FAKE_FC/pid"
printf '%s\n' "$@" > "$FAKE_FC/argv.tmp" && mv "$FAKE_FC/argv.tmp" "$FAKE_FC/argv"
echo "fake console: Linux version 6.1"
read -r code < "$FAKE_FC/exit"
exit "$code"
"#,
        );
        let fifo = dir.join("exit");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        FakeFirecracker {
            dir: dir.to_path_buf(),
            bin,
        }
    }

    /// Its argv, once it has started.
    pub fn argv(&self) -> Vec<String> {
        let path = self.dir.join("argv");
        wait_for("firecracker to start", || path.exists());
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    pub fn started(&self) -> bool {
        self.dir.join("argv").exists()
    }

    /// The pid of the latest one started.
    pub fn pid(&self) -> u32 {
        let pid = fs::read_to_string(self.dir.join("pid")).unwrap();
        pid.trim().parse().unwrap()
    }

    /// Waits for the next one to start, and forgets it started, so the one
    /// after can be waited for too.
    pub fn next(&self) -> Vec<String> {
        let argv = self.argv();
        fs::remove_file(self.dir.join("argv")).unwrap();
        argv
    }

    /// Makes it exit with `code`, from another thread so a reply isn't held up.
    pub fn exit_with(&self, code: i32) {
        let fifo = self.dir.join("exit");
        std::thread::spawn(move || fs::write(fifo, format!("{code}\n")).unwrap());
    }
}

/// systemctl and journalctl on PATH. systemctl logs its argv to `calls`,
/// keeps a unit's ActiveState in `state`, replaced whole so a `show` racing a
/// `start` never reads it empty, or one VM's in `state.<unit>` if a test set it, and fails with the text in `fail` if that
/// file exists. A `stop` runs `on-stop` first if a test put one there, as the
/// unit runs its ExecStop. journalctl logs its argv and prints one line.
pub struct FakeSystemd {
    pub bin: PathBuf,
}

impl FakeSystemd {
    pub fn new(dir: &Path) -> FakeSystemd {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let d = bin.display();
        script(
            &bin.join("systemctl"),
            &format!(
                r#"echo "systemctl $*" >> "{d}/calls"
if [ -f "{d}/fail" ]; then cat "{d}/fail" >&2; exit 1; fi
case "$1" in
  start) echo active > "{d}/state.$$" && mv "{d}/state.$$" "{d}/state" ;;
  stop) if [ -x "{d}/on-stop" ]; then "{d}/on-stop" || exit 1; fi
    echo inactive > "{d}/state.$$" && mv "{d}/state.$$" "{d}/state" ;;
  show) cat "{d}/state.$4" 2>/dev/null || cat "{d}/state" 2>/dev/null || echo inactive ;;
esac
"#
            ),
        );
        script(
            &bin.join("journalctl"),
            &format!(
                "echo \"journalctl $*\" >> \"{d}/calls\"\necho 'fake console: Linux version 6.1'\n"
            ),
        );
        FakeSystemd { bin }
    }

    /// PATH with the fakes first.
    pub fn path(&self) -> String {
        format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap())
    }

    pub fn set_state(&self, state: &str) {
        fs::write(self.bin.join("state"), format!("{state}\n")).unwrap();
    }

    /// One VM's unit's ActiveState, over `set_state`'s for every unit.
    pub fn set_vm_state(&self, vm: &str, state: &str) {
        let unit = format!("state.firecracker@{vm}.service");
        fs::write(self.bin.join(unit), format!("{state}\n")).unwrap();
    }

    pub fn fail_with(&self, stderr: &str) {
        fs::write(self.bin.join("fail"), stderr).unwrap();
    }

    pub fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.bin.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

/// runner-guest on a Unix socket, standing in for the one in a VM.
pub struct Guest {
    child: std::process::Child,
    pub sock: PathBuf,
}

impl Guest {
    pub fn spawn(sock: &Path) -> Guest {
        let child = Command::new(env!("CARGO_BIN_EXE_runner-guest"))
            .arg("--unix")
            .arg(sock)
            .spawn()
            .unwrap();
        // bind makes the file before listen, so wait for a hello to come back.
        let hello = |sock: &Path| -> runner::proto::Result<()> {
            let mut conn = std::os::unix::net::UnixStream::connect(sock)?;
            runner::proto::hello(&mut conn)?;
            runner::proto::check_hello(&mut conn)
        };
        wait_for("runner-guest to listen", || hello(sock).is_ok());
        let sock = sock.to_path_buf();
        Guest { child, sock }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Firecracker's host side of vsock, on a VM's `vsock.sock`. It takes
/// `CONNECT <port>` and, if the guest socket answers, replies `OK <n>` and
/// joins the two. If not, it hangs up, as Firecracker does when nothing in
/// the guest listens on the port. As in Firecracker, the host closing its
/// side, even only for writing, ends the connection both ways.
pub struct FakeVsock {
    connects: Arc<Mutex<Vec<String>>>,
}

impl FakeVsock {
    pub fn serve(uds: &Path, guest: &Path) -> FakeVsock {
        use std::net::Shutdown;
        use std::os::unix::net::UnixStream;
        let listener = UnixListener::bind(uds).unwrap();
        let connects = Arc::new(Mutex::new(Vec::new()));
        let seen = connects.clone();
        let guest = guest.to_path_buf();
        std::thread::spawn(move || {
            for host in listener.incoming() {
                let Ok(mut host) = host else { return };
                let mut line = Vec::new();
                let mut b = [0];
                while !line.ends_with(b"\n") {
                    match std::io::Read::read(&mut host, &mut b) {
                        Ok(1) => line.push(b[0]),
                        _ => break,
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(text(&line).trim_end().to_string());
                let Ok(vm) = UnixStream::connect(&guest) else {
                    continue;
                };
                host.write_all(b"OK 1073741824\n").unwrap();
                let (mut from_host, mut to_vm) =
                    (host.try_clone().unwrap(), vm.try_clone().unwrap());
                let (vm_end, host_end) = (vm.try_clone().unwrap(), host.try_clone().unwrap());
                // An error is the guest gone, whose last frames the other
                // thread still passes on.
                std::thread::spawn(move || {
                    if std::io::copy(&mut from_host, &mut to_vm).is_ok() {
                        let _ = vm_end.shutdown(Shutdown::Both);
                        let _ = host_end.shutdown(Shutdown::Both);
                    }
                });
                let (mut from_vm, mut to_host) = (vm, host);
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from_vm, &mut to_host);
                    let _ = to_host.shutdown(Shutdown::Write);
                });
            }
        });
        FakeVsock { connects }
    }

    /// Each connection's first line, as the host sent it.
    pub fn connects(&self) -> Vec<String> {
        self.connects.lock().unwrap().clone()
    }
}

/// The user running the tests, whom commands in the fake guest run as.
pub fn me() -> String {
    nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap()
        .name
}

/// A terminal for the runner to run on, as ssh -t gives it on the host.
pub struct Pty {
    pub master: std::fs::File,
    pub slave: std::os::fd::OwnedFd,
    seen: Arc<Mutex<Vec<u8>>>,
}

pub fn pty(rows: u16, cols: u16) -> Pty {
    use nix::fcntl::OFlag;
    use nix::pty;
    use std::io::Read;
    use std::os::fd::OwnedFd;
    let master = pty::posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).unwrap();
    pty::grantpt(&master).unwrap();
    pty::unlockpt(&master).unwrap();
    let name = pty::ptsname_r(&master).unwrap();
    let flags = OFlag::O_RDWR | OFlag::O_NOCTTY;
    let slave = nix::fcntl::open(name.as_str(), flags, nix::sys::stat::Mode::empty()).unwrap();
    let master = std::fs::File::from(OwnedFd::from(master));
    set_size(&master, rows, cols);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reader = master.try_clone().unwrap();
    let into = seen.clone();
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        // EIO once nothing holds the slave open.
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            into.lock().unwrap().extend(&buf[..n]);
        }
    });
    Pty {
        master,
        slave,
        seen,
    }
}

pub fn set_size(master: &std::fs::File, rows: u16, cols: u16) {
    use std::os::fd::AsRawFd;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one winsize, which lives through the call.
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) },
        0
    );
}

impl Pty {
    pub fn spawn(&self, mut cmd: Command) -> std::process::Child {
        use std::process::Stdio;
        let fd = || Stdio::from(self.slave.try_clone().unwrap());
        cmd.stdin(fd()).stdout(fd()).stderr(fd()).spawn().unwrap()
    }

    /// Types into the terminal, as keys would.
    pub fn send(&self, keys: &str) {
        let mut master = &self.master;
        master.write_all(keys.as_bytes()).unwrap();
    }

    pub fn seen(&self) -> String {
        text(&self.seen.lock().unwrap())
    }

    pub fn wait_for(&self, want: &str) {
        wait_for(want, || self.seen().contains(want));
    }

    pub fn modes(&self) -> nix::sys::termios::Termios {
        nix::sys::termios::tcgetattr(&self.slave).unwrap()
    }

    pub fn raw(&self) -> bool {
        use nix::sys::termios::LocalFlags;
        !self.modes().local_flags.contains(LocalFlags::ICANON)
    }
}

// A directory tree from (path, contents) pairs, parents made as needed.
pub fn tree(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
    fs::create_dir(dir).unwrap();
    for (path, text) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir.to_path_buf()
}

fn ok(cmd: &mut Command) {
    let out = cmd
        .output()
        .expect("e2fsprogs (mkfs.ext4, debugfs) is installed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{cmd:?}: {err}");
}

// An ext4 disk holding a copy of `tree`, the way images/base builds one.
pub fn image(tree: &Path) -> PathBuf {
    let disk = tree.with_extension("ext4");
    ok(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .args([tree, &disk])
        .arg("16M"));
    disk
}

// Edits a disk in place, as a run's guest would: one request per line.
pub fn debugfs(disk: &Path, requests: &str) {
    let script = disk.with_extension("debugfs");
    fs::write(&script, requests).unwrap();
    ok(Command::new("debugfs")
        .arg("-w")
        .arg("-f")
        .args([&script, disk]));
}

/// A VM's boot as src/sleep.rs asks it, standing in for `runner boot`: it
/// holds the VM as a boot does, notes a snapshot when asked to sleep and
/// clears it when asked to wake, and at a halt clears it and commits its run,
/// as waking and shutting down cleanly would. It records each ask, and
/// refuses with the text in `refuse` while there is one.
pub struct StandIn {
    asked: Arc<Mutex<Vec<runner::sleep::Ask>>>,
    pub refuse: Arc<Mutex<Option<String>>>,
    _asks: runner::sleep::Asks,
}

impl StandIn {
    pub fn serve(store: &runner::store::Store, name: &str) -> StandIn {
        use runner::sleep::{Ask, Asks, Reply};
        let run = store.begin_run(name).unwrap();
        let generation = run.generation;
        let run = Mutex::new(Some(run));
        let snap = store.snapshot(name).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let refuse = Arc::new(Mutex::new(None::<String>));
        let (seen, refusing) = (asked.clone(), refuse.clone());
        let answer = move |ask| {
            seen.lock().unwrap().push(ask);
            if let Some(why) = refusing.lock().unwrap().clone() {
                return Reply::Refused(why);
            }
            match ask {
                Ask::Sleep => {
                    snap.make_room().unwrap();
                    let at = chrono::Utc::now();
                    snap.note(&runner::store::Slept { generation, at }).unwrap();
                }
                Ask::Wake => snap.clear().unwrap(),
                Ask::Halt => {
                    snap.clear().unwrap();
                    if let Some(run) = run.lock().unwrap().take() {
                        run.commit().unwrap();
                    }
                }
            }
            Reply::Done
        };
        let sock = store.boot_socket(name).unwrap();
        let asks = Asks::serve(&sock, answer).unwrap();
        StandIn {
            asked,
            refuse,
            _asks: asks,
        }
    }

    pub fn asked(&self) -> Vec<runner::sleep::Ask> {
        self.asked.lock().unwrap().clone()
    }
}
