// runner-guest end to end, on a Unix socket in place of vsock: no VM needed.
use runner::proto::{self, Exit, FromGuest, Request, ToGuest, WinSize};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Guest {
    child: Child,
    sock: PathBuf,
    dir: TempDir,
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The agent's own environment carries a marker that commands must not see.
fn guest() -> Guest {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("guest.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_runner-guest"))
        .arg("--unix")
        .arg(&sock)
        .env("GUEST_ONLY", "leaked")
        .spawn()
        .unwrap();
    // bind makes the file before listen, so wait for a connect to work.
    let start = Instant::now();
    while UnixStream::connect(&sock).is_err() {
        assert!(start.elapsed() < Duration::from_secs(10), "not listening");
        std::thread::sleep(Duration::from_millis(10));
    }
    Guest { child, sock, dir }
}

struct Session {
    conn: UnixStream,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Guest {
    // Connects as the host does: hello both ways, then the request.
    fn start(&self, req: Request) -> Session {
        let mut conn = UnixStream::connect(&self.sock).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        proto::hello(&mut conn).unwrap();
        proto::check_hello(&mut conn).unwrap();
        proto::write_frame(&mut conn, &req).unwrap();
        Session {
            conn,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn run(&self, req: Request, input: &[ToGuest]) -> (String, String, Exit) {
        let mut s = self.start(req);
        input.iter().for_each(|m| s.send(m));
        let exit = s.exit();
        (text(&s.stdout), text(&s.stderr), exit)
    }
}

impl Session {
    fn send(&mut self, msg: &ToGuest) {
        proto::write_frame(&mut self.conn, msg).unwrap();
    }

    // The next frame that isn't output, keeping the output.
    fn next_end(&mut self) -> FromGuest {
        loop {
            match proto::read_frame(&mut self.conn).unwrap() {
                Some(FromGuest::Stdout(b)) => self.stdout.extend(b),
                Some(FromGuest::Stderr(b)) => self.stderr.extend(b),
                Some(end) => return end,
                None => panic!("closed without an exit"),
            }
        }
    }

    fn exit(&mut self) -> Exit {
        match self.next_end() {
            FromGuest::Exit(exit) => exit,
            other => panic!("wanted an exit, got {other:?}"),
        }
    }

    fn wait_for_stdout(&mut self, want: &str) {
        while !text(&self.stdout).contains(want) {
            match proto::read_frame(&mut self.conn).unwrap() {
                Some(FromGuest::Stdout(b)) => self.stdout.extend(b),
                other => panic!("waiting for {want:?}, got {other:?}"),
            }
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn cmd(argv: &[&str]) -> Request {
    Request {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        cwd: None,
        user: None,
        tty: None,
    }
}

fn sh(script: &str) -> Request {
    cmd(&["sh", "-c", script])
}

fn with_tty(req: Request, rows: u16, cols: u16) -> Request {
    let tty = Some(WinSize { rows, cols });
    Request { tty, ..req }
}

#[test]
fn exit_codes_come_back() {
    let g = guest();
    assert_eq!(g.run(sh("exit 3"), &[]).2, Exit::Code(3));
    assert_eq!(g.run(cmd(&["true"]), &[]).2, Exit::Code(0));
}

#[test]
fn stderr_stays_apart_from_stdout() {
    let g = guest();
    let got = g.run(sh("echo out; echo err >&2; echo out2"), &[]);
    let want = ("out\nout2\n".into(), "err\n".into(), Exit::Code(0));
    assert_eq!(got, want);
}

// cat only exits once its stdin closes.
#[test]
fn stdin_reaches_the_command_and_eof_ends_it() {
    let g = guest();
    let input = [
        ToGuest::Stdin(b"hello ".to_vec()),
        ToGuest::Stdin(b"world".to_vec()),
        ToGuest::CloseStdin,
    ];
    let got = g.run(cmd(&["cat"]), &input);
    assert_eq!(got, ("hello world".into(), "".into(), Exit::Code(0)));
}

#[test]
fn a_pty_command_sees_a_tty_of_the_requested_size() {
    let g = guest();
    let req = with_tty(
        sh("test -t 0 && test -t 1 && test -t 2 && stty size"),
        24,
        80,
    );
    let got = g.run(req, &[]);
    assert_eq!(got, ("24 80\r\n".into(), "".into(), Exit::Code(0)));
}

#[test]
fn a_pty_is_the_command_s_controlling_terminal() {
    let g = guest();
    let got = g.run(with_tty(sh("exec </dev/tty && echo ok"), 24, 80), &[]);
    assert_eq!(got.0, "ok\r\n");
}

// The kernel signals SIGWINCH on a resize; the trap reads the new size.
#[test]
fn a_resize_reaches_a_pty_command() {
    let g = guest();
    let script = r#"trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done"#;
    let mut s = g.start(with_tty(sh(script), 24, 80));
    s.wait_for_stdout("ready");
    s.send(&ToGuest::Resize(WinSize {
        rows: 50,
        cols: 132,
    }));
    assert_eq!(s.exit(), Exit::Code(0));
    assert!(
        text(&s.stdout).ends_with("50 132\r\n"),
        "{:?}",
        text(&s.stdout)
    );
}

#[test]
fn a_killed_command_reports_its_signal() {
    let g = guest();
    assert_eq!(g.run(sh("kill -9 $$"), &[]).2, Exit::Signal(9));
}

#[test]
fn the_host_can_signal_the_command() {
    let g = guest();
    let got = g.run(cmd(&["sleep", "30"]), &[ToGuest::Signal(15)]);
    assert_eq!(got.2, Exit::Signal(15));
}

// Signals reach the whole process group, not only the shell.
#[test]
fn a_signal_reaches_the_command_s_children() {
    let g = guest();
    let file = g.dir.path().join("pid");
    let script = format!("sleep 30 & echo $! >{}; echo ready; wait", file.display());
    let mut s = g.start(sh(&script));
    let child = pid_of(&mut s, &file);
    s.send(&ToGuest::Signal(9));
    assert_eq!(s.exit(), Exit::Signal(9));
    assert!(gone_soon(child), "{child} still running");
}

#[test]
fn env_and_cwd_are_the_request_s_and_nothing_leaks() {
    let g = guest();
    let req = Request {
        env: vec![("FOO".into(), "bar baz".into())],
        cwd: Some(g.dir.path().to_str().unwrap().into()),
        ..sh(r#"echo "$FOO|$(pwd)|${GUEST_ONLY-unset}|$PATH""#)
    };
    let want = format!(
        "bar baz|{}|unset|/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n",
        g.dir.path().display()
    );
    assert_eq!(g.run(req, &[]).0, want);
}

#[test]
fn a_pty_command_gets_a_term() {
    let g = guest();
    let got = g.run(with_tty(sh("echo $TERM"), 24, 80), &[]);
    assert_eq!(got.0, "xterm-256color\r\n");
}

// A daemon left holding stdout doesn't keep the request open.
#[test]
fn exit_doesn_t_wait_for_a_background_process() {
    let g = guest();
    let start = Instant::now();
    let got = g.run(sh("sleep 30 & echo started"), &[]);
    assert_eq!(got, ("started\n".into(), "".into(), Exit::Code(0)));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn exit_doesn_t_wait_for_a_chatty_background_process() {
    let g = guest();
    let start = Instant::now();
    let mut s = g.start(sh("(while :; do echo x; sleep 0.05; done) & echo started"));
    assert_eq!(s.exit(), Exit::Code(0));
    assert!(text(&s.stdout).starts_with("started\n"));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_command_that_can_t_start_is_an_error() {
    let g = guest();
    let mut s = g.start(cmd(&["no-such-command"]));
    match s.next_end() {
        FromGuest::Error(e) => assert!(e.contains("no-such-command"), "{e}"),
        other => panic!("wanted an error, got {other:?}"),
    }
}

#[test]
fn an_unknown_user_is_an_error() {
    let g = guest();
    let mut s = g.start(Request {
        user: Some("no-such-user".into()),
        ..cmd(&["true"])
    });
    assert_eq!(
        s.next_end(),
        FromGuest::Error("no user named no-such-user".into())
    );
}

// Switching users needs root, which the agent has in a VM. Tests that do
// are skipped, and say so, anywhere else.
fn root() -> bool {
    let root = nix::unistd::geteuid().is_root();
    if !root {
        eprintln!("skipped: needs root");
    }
    root
}

#[test]
fn runs_as_the_named_user_in_their_home() {
    if !root() {
        return;
    }
    let g = guest();
    let req = Request {
        user: Some("nobody".into()),
        ..sh(r#"echo "$(id -un) $(id -u) $USER $HOME $(pwd)""#)
    };
    let nobody = nix::unistd::User::from_name("nobody").unwrap().unwrap();
    let home = nobody.dir.display().to_string();
    let cwd = if nobody.dir.is_dir() {
        home.as_str()
    } else {
        "/"
    };
    let want = format!("nobody {} nobody {home} {cwd}\n", nobody.uid);
    assert_eq!(g.run(req, &[]).0, want);
}

#[test]
fn a_host_on_another_version_is_refused() {
    let g = guest();
    let mut conn = UnixStream::connect(&g.sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut hello = proto::MAGIC.to_vec();
    hello.extend((proto::VERSION + 1).to_be_bytes());
    std::io::Write::write_all(&mut conn, &hello).unwrap();
    proto::check_hello(&mut conn).unwrap();
    let _ = proto::write_frame(&mut conn, &cmd(&["true"]));
    let got = proto::read_frame::<FromGuest>(&mut conn);
    assert!(
        matches!(got, Ok(None) | Err(proto::Error::Io(_))),
        "{got:?}"
    );
}

// One connection per command, several at once.
#[test]
fn commands_run_side_by_side() {
    let g = guest();
    let mut a = g.start(cmd(&["cat"]));
    let mut b = g.start(sh("echo b"));
    assert_eq!(b.exit(), Exit::Code(0));
    a.send(&ToGuest::Stdin(b"a".to_vec()));
    a.send(&ToGuest::CloseStdin);
    assert_eq!(a.exit(), Exit::Code(0));
    assert_eq!(
        (text(&a.stdout), text(&b.stdout)),
        ("a".into(), "b\n".into())
    );
}

// The command's pid, from a script that writes it to a file before "ready".
fn pid_of(s: &mut Session, file: &std::path::Path) -> i32 {
    s.wait_for_stdout("ready");
    std::fs::read_to_string(file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn gone(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
}

fn gone_soon(pid: i32) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

// As closing a terminal does.
#[test]
fn a_hang_up_hangs_up_the_command() {
    let g = guest();
    let file = g.dir.path().join("pid");
    let mut s = g.start(sh(&format!(
        "echo $$ >{}; echo ready; exec sleep 30",
        file.display()
    )));
    let pid = pid_of(&mut s, &file);
    drop(s);
    assert!(gone_soon(pid), "{pid} still running");
}

// Stdin the command never reads, more than a pipe holds.
#[test]
fn a_signal_lands_while_stdin_is_backed_up() {
    let g = guest();
    let mut s = g.start(cmd(&["sleep", "30"]));
    s.send(&ToGuest::Stdin(vec![b'x'; 300_000]));
    s.send(&ToGuest::Signal(15));
    assert_eq!(s.exit(), Exit::Signal(15));
}

// Past what the agent queues, it stops reading frames, but still hears
// the host hang up.
#[test]
fn a_hang_up_lands_while_stdin_is_backed_up() {
    let g = guest();
    let file = g.dir.path().join("pid");
    let mut s = g.start(sh(&format!(
        "echo $$ >{}; echo ready; exec sleep 30",
        file.display()
    )));
    let pid = pid_of(&mut s, &file);
    let mut conn = s.conn.try_clone().unwrap();
    let flood = std::thread::spawn(move || {
        for _ in 0..64 {
            let chunk = ToGuest::Stdin(vec![b'x'; 64 * 1024]);
            if proto::write_frame(&mut conn, &chunk).is_err() {
                return;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!gone(pid), "{pid} died before the hang-up");
    s.conn.shutdown(std::net::Shutdown::Both).unwrap();
    drop(s);
    flood.join().unwrap();
    assert!(gone_soon(pid), "{pid} still running");
}

// Programs that reopen the terminal by name, like ssh-agent's pinentry.
#[test]
fn a_pty_belongs_to_the_user_it_runs_as() {
    if !root() {
        return;
    }
    let g = guest();
    let req = Request {
        user: Some("nobody".into()),
        ..with_tty(
            sh(r#"exec 3<>"$(tty)" && stat -c '%U %a' "$(tty)""#),
            24,
            80,
        )
    };
    assert_eq!(g.run(req, &[]).0, "nobody 620\r\n");
}

// The directory is entered as the user, not as root.
#[test]
fn a_directory_the_user_can_t_enter_is_an_error() {
    if !root() {
        return;
    }
    let g = guest();
    let private = g.dir.path().join("private");
    std::fs::create_dir(&private).unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o700);
    std::fs::set_permissions(&private, mode).unwrap();
    let mut s = g.start(Request {
        user: Some("nobody".into()),
        cwd: Some(private.to_str().unwrap().into()),
        ..cmd(&["pwd"])
    });
    match s.next_end() {
        FromGuest::Error(e) => assert!(e.contains("Permission denied"), "{e}"),
        other => panic!("wanted an error, got {other:?}"),
    }
}
