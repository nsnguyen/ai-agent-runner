// `runner exec` and `runner shell` end to end, and the host's side of the
// guest protocol under them: the runner binary with systemctl faked, and
// runner-guest on a Unix socket behind a fake of Firecracker's vsock socket.
mod common;

use common::{FakeSystemd, FakeVsock, Guest, me, pty, set_size, text};
use nix::sys::termios::Termios;
use proptest::prelude::*;
use runner::exec::{self, Captured, Kept};
use runner::proto::{self, Request};
use runner::store::Store;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// A VM named dev whose guest is up, as far as the runner can tell.
struct Vm {
    dir: TempDir,
    sd: FakeSystemd,
    vsock: FakeVsock,
    _guest: Guest,
}

fn vm() -> Vm {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    let guest = Guest::spawn(&dir.path().join("guest.sock"));
    let vsock = FakeVsock::serve(&vsock_of(dir.path()), &guest.sock);
    Vm {
        dir,
        sd,
        vsock,
        _guest: guest,
    }
}

fn vsock_of(root: &Path) -> PathBuf {
    root.join("vms/dev/vsock.sock")
}

impl Vm {
    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.dir.path())
            .env("PATH", self.sd.path())
            .env("RUNNER_USER", me())
            .env_remove("TERM")
            .args(args);
        cmd
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        run_with(self.runner(args), stdin)
    }

    fn sock(&self) -> PathBuf {
        vsock_of(self.dir.path())
    }
}

fn run_with(mut cmd: Command, stdin: &[u8]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    // From a thread, so a command that doesn't read can't hold up the test.
    std::thread::spawn(move || input.write_all(&stdin));
    child.wait_with_output().unwrap()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

#[test]
fn exec_exits_with_the_command_s_code() {
    let vm = vm();
    let out = vm.run(&["exec", "dev", "--", "sh", "-c", "exit 3"], b"");
    assert_eq!(code(&out), 3, "{}", text(&out.stderr));
    assert_eq!(code(&vm.run(&["exec", "dev", "--", "true"], b"")), 0);
    // Killed by a signal: 128 plus its number, as a shell says.
    let out = vm.run(&["exec", "dev", "--", "sh", "-c", "kill -9 $$"], b"");
    assert_eq!(code(&out), 137);
}

#[test]
fn exec_takes_the_command_without_a_double_dash_too() {
    let vm = vm();
    let out = vm.run(&["exec", "dev", "echo", "-n", "hi"], b"");
    assert_eq!(text(&out.stdout), "hi", "{}", text(&out.stderr));
}

#[test]
fn exec_passes_stdin_through_and_keeps_stderr_apart() {
    let vm = vm();
    let out = vm.run(&["exec", "dev", "--", "cat"], b"hello\nworld");
    assert_eq!(text(&out.stdout), "hello\nworld");
    let script = "echo out; echo err >&2; echo out2";
    let out = vm.run(&["exec", "dev", "--", "sh", "-c", script], b"");
    assert_eq!(text(&out.stdout), "out\nout2\n");
    assert_eq!(text(&out.stderr), "err\n");
}

// Big enough to fill every buffer between here and the command.
#[test]
fn exec_streams_more_than_fits_in_a_buffer() {
    let vm = vm();
    let input = vec![b'x'; 4 << 20];
    let out = vm.run(&["exec", "dev", "--", "cat"], &input);
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    assert_eq!(out.stdout.len(), input.len());
}

#[test]
fn exec_takes_env_and_a_directory() {
    let vm = vm();
    let dir = vm.dir.path().to_str().unwrap();
    let script = r#"echo "$FOO|$X|$(pwd)""#;
    let args = [
        "exec",
        "dev",
        "--env",
        "FOO=bar baz",
        "--env",
        "X=a=b",
        "--dir",
        dir,
        "--",
        "sh",
        "-c",
        script,
    ];
    let out = vm.run(&args, b"");
    assert_eq!(text(&out.stdout), format!("bar baz|a=b|{dir}\n"));
    let out = vm.run(&["exec", "dev", "--env", "FOO", "--", "true"], b"");
    assert_ne!(code(&out), 0);
    assert!(text(&out.stderr).contains("K=V"), "{}", text(&out.stderr));
}

// Refused before the VM is looked at, so nothing runs and nothing boots.
#[test]
fn exec_refuses_a_credential_variable() {
    let vm = vm();
    let ran = vm.dir.path().join("ran");
    let touch = format!("touch {}", ran.display());
    for var in ["GH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
        let set = format!("{var}=sekret-value");
        let args = [
            "exec", "dev", "-e", "FOO=1", "-e", &set, "--", "sh", "-c", &touch,
        ];
        let out = vm.run(&args, b"");
        assert_eq!(code(&out), 255, "{var}");
        // Named, and its value never printed.
        let want = format!(
            "runner: {var} is a credential: credentials reach a VM only through `runner creds`\n"
        );
        assert_eq!(text(&out.stderr), want);
        assert!(!ran.exists(), "{var} ran the command");
    }
    assert!(vm.sd.calls().is_empty(), "{:?}", vm.sd.calls());
    // Only those names: one that merely starts like them passes through.
    let args = [
        "exec",
        "dev",
        "-e",
        "GH_TOKEN_FILE=f",
        "--",
        "sh",
        "-c",
        "echo $GH_TOKEN_FILE",
    ];
    let out = vm.run(&args, b"");
    assert_eq!(text(&out.stdout), "f\n", "{}", text(&out.stderr));
}

#[test]
fn exec_runs_as_the_user_in_their_home() {
    let vm = vm();
    let me = nix::unistd::User::from_name(&me()).unwrap().unwrap();
    let out = vm.run(&["exec", "dev", "--", "sh", "-c", "echo $USER; pwd"], b"");
    let home = me.dir.display();
    assert_eq!(text(&out.stdout), format!("{}\n{home}\n", me.name));
}

// The base image's user, which this machine doesn't have.
#[test]
fn exec_runs_as_agent_unless_told_otherwise() {
    let vm = vm();
    let mut cmd = vm.runner(&["exec", "dev", "--", "true"]);
    cmd.env_remove("RUNNER_USER");
    let out = run_with(cmd, b"");
    assert_eq!(code(&out), 255);
    let err = text(&out.stderr);
    assert!(err.contains("no user named agent"), "{err}");
}

#[test]
fn exec_has_a_tty_only_when_asked() {
    let vm = vm();
    let out = vm.run(&["exec", "dev", "--", "test", "-t", "0"], b"");
    assert_eq!(code(&out), 1);
    // No terminal here to take a size from, so the PTY is 24x80.
    let script = "test -t 0 && test -t 1 && stty size && echo $TERM";
    let out = vm.run(&["exec", "dev", "--tty", "--", "sh", "-c", script], b"");
    assert_eq!(text(&out.stdout), "24 80\r\nxterm-256color\r\n");
}

#[test]
fn exec_passes_term_on_to_a_tty() {
    let vm = vm();
    let mut cmd = vm.runner(&["exec", "dev", "--tty", "--", "sh", "-c", "echo $TERM"]);
    cmd.env("TERM", "screen");
    assert_eq!(text(&run_with(cmd, b"").stdout), "screen\r\n");
}

#[test]
fn a_stopped_vm_is_started_first() {
    let vm = vm();
    let show = "systemctl show --property=ActiveState --value firecracker@dev.service";
    let out = vm.run(&["exec", "dev", "--", "true"], b"");
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    let start = "systemctl start firecracker@dev.service";
    // The rest are shows: of dev, and of each VM, for the host's room.
    let verbs = || -> Vec<String> {
        let calls = vm.sd.calls().into_iter();
        calls.filter(|c| c != show).collect()
    };
    assert_eq!(vm.sd.calls()[0], show);
    assert_eq!(verbs(), [start]);
    let before = vm.sd.calls().len();

    let out = vm.run(&["exec", "dev", "--", "true"], b"");
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    assert_eq!(vm.sd.calls()[before..], [show]);
    assert_eq!(vm.vsock.connects(), ["CONNECT 10022", "CONNECT 10022"]);
}

#[test]
fn a_vm_that_doesn_t_exist_never_reaches_systemd() {
    let vm = vm();
    let out = vm.run(&["exec", "nope", "--", "true"], b"");
    assert_eq!(code(&out), 255);
    assert_eq!(text(&out.stderr), "runner: no VM named nope\n");
    assert!(vm.sd.calls().is_empty());
}

#[test]
fn a_command_that_can_t_start_says_why() {
    let vm = vm();
    let out = vm.run(&["exec", "dev", "--", "no-such-command"], b"");
    assert_eq!(code(&out), 255);
    let err = text(&out.stderr);
    assert!(
        err.starts_with("runner: couldn't run no-such-command"),
        "{err}"
    );
}

#[test]
fn shell_is_a_login_shell_as_the_user_in_their_home() {
    let vm = vm();
    let me = nix::unistd::User::from_name(&me()).unwrap().unwrap();
    let input = "echo \"[$USER@$PWD]\" \"[$(tr '\\0' ' ' </proc/$$/cmdline)]\"; exit 4\n";
    let out = vm.run(&["shell", "dev"], input.as_bytes());
    let got = text(&out.stdout);
    assert_eq!(code(&out), 4, "{got}{}", text(&out.stderr));
    assert!(
        got.contains(&format!("[{}@{}]", me.name, me.dir.display())),
        "{got}"
    );
    // The user's own shell, with -l.
    assert!(
        got.contains(&format!("[{} -l ]", me.shell.display())),
        "{got}"
    );
}

// No terminal here, so none there either: the input's end ends the shell.
#[test]
fn shell_without_a_terminal_ends_with_its_input() {
    let vm = vm();
    let out = vm.run(&["shell", "dev"], b"test -t 0 || echo no tty\n");
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "no tty\n");
}

fn same_modes(a: &Termios, b: &Termios) -> bool {
    (
        a.input_flags,
        a.output_flags,
        a.local_flags,
        a.control_flags,
    ) == (
        b.input_flags,
        b.output_flags,
        b.local_flags,
        b.control_flags,
    )
}

#[test]
fn a_tty_puts_the_terminal_in_raw_mode_and_back() {
    let vm = vm();
    let t = pty(30, 100);
    let before = t.modes();
    let script = "stty size; echo ready; read x; echo \"got $x\"";
    let mut child = t.spawn(vm.runner(&["exec", "dev", "--tty", "--", "sh", "-c", script]));
    t.wait_for("ready");
    assert!(t.raw(), "not raw while the command runs");
    // Raw, so ^C would go through as a byte; Enter is \r, as a terminal sends it.
    t.master.try_clone().unwrap().write_all(b"yes\r").unwrap();
    assert!(child.wait().unwrap().success(), "{}", t.seen());
    // Read from the PTY on another thread, which may still be catching up.
    t.wait_for("got yes\r\n");
    assert!(t.seen().contains("30 100\r\n"), "{}", t.seen());
    assert!(same_modes(&t.modes(), &before), "terminal not restored");
}

#[test]
fn shell_on_a_terminal_gets_one_of_its_size() {
    let vm = vm();
    let t = pty(30, 100);
    let before = t.modes();
    let mut child = t.spawn(vm.runner(&["shell", "dev"]));
    let input = "echo \"[$(stty size)]\"; tty >/dev/null && exit 5\r";
    t.master
        .try_clone()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(5), "{}", t.seen());
    t.wait_for("[30 100]");
    assert!(same_modes(&t.modes(), &before), "terminal not restored");
}

#[test]
fn sigterm_hangs_up_and_still_restores_the_terminal() {
    let vm = vm();
    let t = pty(24, 80);
    let before = t.modes();
    let script = "echo ready; sleep 30";
    let mut child = t.spawn(vm.runner(&["exec", "dev", "--tty", "--", "sh", "-c", script]));
    t.wait_for("ready");
    let start = Instant::now();
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::SIGTERM).unwrap();
    let status = child.wait().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    // Not killed by the signal: it handled it, and exits as if it had been.
    assert_eq!(status.code(), Some(143), "{}", t.seen());
    assert!(same_modes(&t.modes(), &before), "terminal not restored");
}

// The guest stops reading once a command leaves 1 MiB of its input unread,
// and then neither a signal nor a hang-up gets through to it. The runner
// still gets out at once, as if killed by the signal.
#[test]
fn a_signal_gets_out_while_stdin_is_stalled() {
    let vm = vm();
    for (sig, code) in [(libc::SIGTERM, 143), (libc::SIGINT, 130)] {
        let mut child = vm
            .runner(&["exec", "dev", "--", "sleep", "5"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        std::thread::spawn(move || input.write_all(&vec![b'x'; 20 << 20]));
        std::thread::sleep(Duration::from_millis(500));
        let start = Instant::now();
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        let sig = nix::sys::signal::Signal::try_from(sig).unwrap();
        nix::sys::signal::kill(pid, sig).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(code), "{sig}");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{sig}: {:?}",
            start.elapsed()
        );
    }
}

// As a command writing into `| head` does locally: quietly, as if SIGPIPE
// had killed it.
#[test]
fn exec_into_a_closed_pipe_exits_as_sigpipe() {
    let vm = vm();
    let mut child = vm
        .runner(&["exec", "dev", "--", "yes"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut some = [0; 10];
    child.stdout.take().unwrap().read_exact(&mut some).unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(141));
    assert_eq!(text(&out.stderr), "");
}

// A ^D after a partial line only sends the line, so the end takes two.
#[test]
fn a_tty_s_piped_input_ends_after_a_partial_line_too() {
    let vm = vm();
    let out = vm.run(&["exec", "dev", "--tty", "--", "cat"], b"abc");
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).contains("abc"), "{}", text(&out.stdout));
}

// The kernel sends SIGWINCH only to a terminal's foreground group, which the
// runner isn't here, so the test sends it.
#[test]
fn a_resize_follows_the_terminal() {
    let vm = vm();
    let t = pty(24, 80);
    // Gives up after five seconds.
    let script = r#"trap 'stty size; exit 0' WINCH; echo ready; for _ in $(seq 100); do sleep 0.05; done; exit 1"#;
    let mut child = t.spawn(vm.runner(&["exec", "dev", "--tty", "--", "sh", "-c", script]));
    t.wait_for("ready");
    set_size(&t.master, 50, 132);
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::SIGWINCH).unwrap();
    assert!(child.wait().unwrap().success(), "{}", t.seen());
    t.wait_for("50 132\r\n");
}

#[test]
fn sigint_reaches_the_command() {
    let vm = vm();
    let t = pty(24, 80);
    let mut child = t.spawn(vm.runner(&["exec", "dev", "--", "sh", "-c", "echo ready; sleep 30"]));
    t.wait_for("ready");
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::SIGINT).unwrap();
    // The command died of it, so the runner exits 128 + 2.
    assert_eq!(child.wait().unwrap().code(), Some(130), "{}", t.seen());
}

// The host's side, as MCP's exec uses it.

fn open(vm: &Vm) -> std::os::unix::net::UnixStream {
    exec::connect(&vm.sock(), proto::PORT, Duration::from_secs(5)).unwrap()
}

fn req(argv: &[&str]) -> Request {
    Request {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        cwd: None,
        user: None,
        tty: None,
    }
}

fn capture(vm: &Vm, argv: &[&str], stdin: &[u8], timeout: Duration, cap: usize) -> Captured {
    exec::capture(open(vm), &req(argv), stdin, timeout, cap).unwrap()
}

const LONG: Duration = Duration::from_secs(30);

#[test]
fn capture_returns_the_exit_and_both_streams() {
    let vm = vm();
    let got = capture(
        &vm,
        &["sh", "-c", "echo out; echo err >&2; exit 3"],
        b"",
        LONG,
        100,
    );
    assert_eq!(got.exit_code, Some(3));
    assert_eq!(got.signal, None);
    assert_eq!(
        (got.stdout.as_str(), got.stderr.as_str()),
        ("out\n", "err\n")
    );
    assert!(!got.stdout_truncated && !got.stderr_truncated && !got.timed_out);
}

#[test]
fn capture_feeds_stdin_then_closes_it() {
    let vm = vm();
    let got = capture(&vm, &["cat"], b"some input", LONG, 100);
    assert_eq!(
        (got.exit_code, got.stdout.as_str()),
        (Some(0), "some input")
    );
    // With none, stdin is closed from the start, so cat doesn't wait.
    let got = capture(&vm, &["cat"], b"", LONG, 100);
    assert_eq!((got.exit_code, got.stdout.as_str()), (Some(0), ""));
}

#[test]
fn capture_keeps_both_ends_of_each_stream() {
    let vm = vm();
    let script = "printf start; head -c 5000000 /dev/zero | tr '\\0' a; printf end; printf bbb >&2";
    let got = capture(&vm, &["sh", "-c", script], b"", LONG, 1000);
    assert_eq!(got.exit_code, Some(0));
    let (head, tail) = ("a".repeat(495), "a".repeat(497));
    let want = format!("start{head}\n[runner cut 4999008 bytes here]\n{tail}end");
    assert_eq!(got.stdout, want);
    assert!(got.stdout_truncated);
    assert_eq!((got.stderr.as_str(), got.stderr_truncated), ("bbb", false));
    // Exactly the cap isn't cut.
    let got = capture(&vm, &["printf", "abc"], b"", LONG, 3);
    assert_eq!((got.stdout.as_str(), got.stdout_truncated), ("abc", false));
    // An odd cap's spare byte goes to the start.
    let got = capture(&vm, &["printf", "abcd"], b"", LONG, 3);
    let want = "ab\n[runner cut 1 byte here]\nd";
    assert_eq!((got.stdout.as_str(), got.stdout_truncated), (want, true));
    // A character the cut splits goes, rather than leaving U+FFFD, and it's
    // counted in what was cut.
    let got = capture(&vm, &["printf", "aé"], b"", LONG, 2);
    let want = "a\n[runner cut 2 bytes here]\n";
    assert_eq!((got.stdout.as_str(), got.stdout_truncated), (want, true));
    let got = capture(&vm, &["printf", "éa"], b"", LONG, 2);
    let want = "\n[runner cut 2 bytes here]\na";
    assert_eq!((got.stdout.as_str(), got.stdout_truncated), (want, true));
}

// Every request the host sends is checked, whoever built it.
#[test]
fn capture_refuses_a_credential_variable() {
    let vm = vm();
    let ran = vm.dir.path().join("ran");
    let mut request = req(&["touch", ran.to_str().unwrap()]);
    request.env = vec![("ANTHROPIC_API_KEY".into(), "x".into())];
    let err = exec::capture(open(&vm), &request, b"", LONG, 100).unwrap_err();
    let want =
        "ANTHROPIC_API_KEY is a credential: credentials reach a VM only through `runner creds`";
    assert_eq!(err.to_string(), want);
    assert!(!ran.exists());
}

proptest! {
    // Whatever the chunks, it's the whole output up to the cap, and past it
    // the first and last halves, with the bytes between counted.
    #[test]
    fn kept_is_both_ends_of_the_whole(
        out in proptest::collection::vec(0u8..128, 0..300),
        cuts in proptest::collection::vec(0usize..300, 0..8),
        cap in 0usize..100,
    ) {
        let mut kept = Kept::new(cap);
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(out.len())).collect();
        cuts.sort();
        let mut from = 0;
        for to in cuts.into_iter().chain([out.len()]) {
            kept.push(&out[from..to]);
            from = to;
        }
        let whole = String::from_utf8(out.clone()).unwrap();
        if out.len() <= cap {
            prop_assert!(!kept.cut());
            prop_assert_eq!(kept.text(), whole);
        } else {
            let (head, tail) = (cap - cap / 2, cap / 2);
            let gone = out.len() - cap;
            let s = if gone == 1 { "" } else { "s" };
            let want = format!(
                "{}\n[runner cut {gone} byte{s} here]\n{}",
                &whole[..head],
                &whole[out.len() - tail..],
            );
            prop_assert!(kept.cut());
            prop_assert_eq!(kept.text(), want);
        }
    }

    // Valid UTF-8 in, valid UTF-8 out: a character cut in two at either end
    // goes whole.
    #[test]
    fn kept_never_splits_a_character(out in "[^\\x{FFFD}]{0,80}", cap in 0usize..40) {
        let mut kept = Kept::new(cap);
        kept.push(out.as_bytes());
        let text = kept.text();
        prop_assert!(!text.contains(char::REPLACEMENT_CHARACTER), "{:?}", text);
    }
}

#[test]
fn capture_survives_input_and_output_bigger_than_any_buffer() {
    let vm = vm();
    let input = vec![b'x'; 4 << 20];
    let got = capture(&vm, &["cat"], &input, LONG, 10);
    assert_eq!(got.exit_code, Some(0));
    assert!(got.stdout_truncated);
}

#[test]
fn a_timeout_kills_the_command() {
    let vm = vm();
    let start = Instant::now();
    let got = capture(&vm, &["sleep", "30"], b"", Duration::from_millis(300), 100);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(got.timed_out);
    assert_eq!((got.exit_code, got.signal), (None, Some(9)));
}

// The feed is stuck behind a command that isn't reading its input, and so
// is the kill, so the timeout hangs up and returns at once, with no exit.
#[test]
fn a_timeout_returns_even_when_the_command_isn_t_reading_its_input() {
    let vm = vm();
    let input = vec![b'x'; 8 << 20];
    let start = Instant::now();
    // It may live on in the guest, so it's short.
    let got = capture(
        &vm,
        &["sleep", "3"],
        &input,
        Duration::from_millis(500),
        100,
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(got.timed_out);
    assert_eq!((got.exit_code, got.signal), (None, None));
}

#[test]
fn connect_waits_for_the_guest_to_listen() {
    let dir = tempfile::tempdir().unwrap();
    let guest_sock = dir.path().join("guest.sock");
    let uds = dir.path().join("vsock.sock");
    let vsock = FakeVsock::serve(&uds, &guest_sock);
    let later = guest_sock.clone();
    let booting = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        Guest::spawn(&later)
    });
    let conn = exec::connect(&uds, proto::PORT, Duration::from_secs(10));
    let _guest = booting.join().unwrap();
    assert!(conn.is_ok(), "{:?}", conn.err());
    assert!(vsock.connects().len() > 1, "{:?}", vsock.connects());
}

#[test]
fn connect_gives_up_after_the_wait() {
    let dir = tempfile::tempdir().unwrap();
    let uds = dir.path().join("vsock.sock");
    let _vsock = FakeVsock::serve(&uds, &dir.path().join("never.sock"));
    let start = Instant::now();
    let got = exec::connect(&uds, proto::PORT, Duration::from_millis(300));
    assert!(got.is_err());
    assert!(start.elapsed() < Duration::from_secs(5));
}

// A path too long for a socket won't get shorter.
#[test]
fn connect_fails_at_once_on_an_error_that_won_t_clear() {
    let long = PathBuf::from(format!("/tmp/{}/vsock.sock", "x".repeat(200)));
    let start = Instant::now();
    let got = exec::connect(&long, proto::PORT, Duration::from_secs(10));
    assert!(got.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

// Whatever answers isn't runner-guest, and waiting won't change that.
#[test]
fn connect_refuses_a_stranger_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let stranger = dir.path().join("stranger.sock");
    let listener = UnixListener::bind(&stranger).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let _ = conn.unwrap().write_all(b"SSH-2.0-OpenSSH_9.6\r\n");
        }
    });
    let uds = dir.path().join("vsock.sock");
    let _vsock = FakeVsock::serve(&uds, &stranger);
    let start = Instant::now();
    let got = exec::connect(&uds, proto::PORT, Duration::from_secs(10));
    assert!(matches!(got, Err(proto::Error::NotPeer)), "{got:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}
