// Boot tests: real Firecracker, real KVM. Skipped unless asked for, since
// cloud sessions have no /dev/kvm. On the host:
//
//   RUNNER_TEST_DISK=/path/rootfs.ext4 cargo test --test kvm -- --ignored
//
// The rootfs's init must be systemd or another that takes Ctrl-Alt-Del, with
// a login prompt on the serial console.
// `boot_and_halt_by_hand` and `a_booted_vm_answers_at_its_address` need
// Firecracker on PATH, RUNNER_TEST_KERNEL, and root (or CAP_NET_ADMIN) for
// the VM's tap: run them with sudo -E. So does
// `the_token_reaches_the_vm_s_memory_and_never_its_disk`, with the base
// image as RUNNER_TEST_DISK; a real token in CLAUDE_CODE_OAUTH_TOKEN lets
// claude get further than a login error.
// `start_logs_and_stop_under_systemd` needs `runner unit` installed and the
// host's kernel in place; it uses the unit's root, RUNNER_ROOT or
// /var/lib/runner. `exec_starts_the_vm_and_runs_in_the_guest` needs the same,
// with the base image (images/base) as RUNNER_TEST_DISK, for its
// runner-guest and agent user, and so does
// `a_session_outlives_a_dropped_connection`, for its tmux, and
// `claude_keeps_running_after_a_detach`, for Claude Code.
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn env_path(name: &str) -> PathBuf {
    let path = std::env::var_os(name).unwrap_or_else(|| panic!("set {name}"));
    PathBuf::from(path)
}

fn runner(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .args(args)
        .output()
        .unwrap()
}

fn ok(out: Output) -> String {
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}{err}");
    text
}

/// Polls `check` for up to a minute.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm, Firecracker, a kernel and a rootfs"]
fn boot_and_halt_by_hand() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    ok(runner(
        root,
        &[
            "import",
            "dev",
            env_path("RUNNER_TEST_DISK").to_str().unwrap(),
        ],
    ));
    let kernel = env_path("RUNNER_TEST_KERNEL");
    std::fs::create_dir_all(root.join("kernel")).unwrap();
    std::fs::copy(kernel, root.join("kernel/vmlinux")).unwrap();

    let console = root.join("console.log");
    let boot = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .args(["boot", "dev"])
        .stdout(std::fs::File::create(&console).unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    eventually("the kernel on the console", || {
        std::fs::read_to_string(&console).is_ok_and(|c| c.contains("Linux version"))
    });
    // Ctrl-Alt-Del before init is up goes nowhere.
    eventually("a login prompt", || {
        std::fs::read_to_string(&console).is_ok_and(|c| c.contains("login:"))
    });
    ok(runner(root, &["halt", "dev"]));
    let out = boot.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(ok(runner(root, &["list"])).starts_with("dev  gen 1  "));
}

// The guest's side (pinging its gateway, resolving a name, fetching a page)
// waits for `runner exec`. From the host: the guest kernel took its address
// and DNS from the boot argument, it answers there, and its tap goes with it.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, Firecracker, a kernel, a rootfs and root"]
fn a_booted_vm_answers_at_its_address() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    ok(runner(
        root,
        &[
            "import",
            "dev",
            env_path("RUNNER_TEST_DISK").to_str().unwrap(),
        ],
    ));
    std::fs::create_dir_all(root.join("kernel")).unwrap();
    std::fs::copy(env_path("RUNNER_TEST_KERNEL"), root.join("kernel/vmlinux")).unwrap();
    let address = ok(runner(root, &["ip", "dev"])).trim().to_string();
    // Named for its /30, which starts two below the guest's address.
    let ip: std::net::Ipv4Addr = address.parse().unwrap();
    let tap = Path::new("/sys/class/net").join(format!("fc{:08x}", u32::from(ip) - 2));

    let console = root.join("console.log");
    let boot = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .args(["boot", "dev"])
        .stdout(std::fs::File::create(&console).unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let read = || std::fs::read_to_string(&console).unwrap_or_default();
    eventually("a login prompt", || read().contains("login:"));
    let tap_up = tap.exists();
    let pinged = Command::new("ping")
        .args(["-c", "1", "-W", "5", &address])
        .output()
        .unwrap();
    ok(runner(root, &["halt", "dev"]));
    let out = boot.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The kernel prints what its ip= argument set: address, gateway, DNS.
    let log = read();
    assert!(log.contains("IP-Config: Complete"), "no IP-Config:\n{log}");
    assert!(log.contains(&format!("ipaddr={address},")), "{log}");
    assert!(tap_up, "no {} while the VM ran", tap.display());
    assert!(pinged.status.success(), "{address} didn't answer a ping");
    assert!(!tap.exists(), "{} left behind", tap.display());
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and the firecracker@ unit installed"]
fn start_logs_and_stop_under_systemd() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("boot-test-{}", std::process::id());
    ok(runner(
        &root,
        &[
            "import",
            &name,
            env_path("RUNNER_TEST_DISK").to_str().unwrap(),
        ],
    ));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };

    let started = runner(&root, &["start", &name]);
    let status = ok(runner(&root, &["status", &name]));
    let logs = || ok(runner(&root, &["logs", &name]));
    assert!(started.status.success(), "start: {:?}\n{}", started, logs());
    // Then the credentials its guest took, if it has runner-guest.
    assert!(status.contains("  running"), "{status}");
    eventually("the kernel in the journal", || {
        logs().contains("Linux version")
    });
    eventually("a login prompt", || logs().contains("login:"));

    ok(runner(&root, &["stop", &name]));
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.ends_with("  stopped\n"), "{status}");
    // Stopped cleanly, so the run was committed.
    assert!(status.contains("  gen 1  "), "{status}\n{}", logs());
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm, the firecracker@ unit and the base image"]
fn exec_starts_the_vm_and_runs_in_the_guest() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("exec-test-{}", std::process::id());
    ok(runner(
        &root,
        &[
            "import",
            &name,
            env_path("RUNNER_TEST_DISK").to_str().unwrap(),
        ],
    ));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };

    // Stopped, so exec boots it, then waits for its guest agent.
    let uname = ok(runner(&root, &["exec", &name, "--", "uname", "-a"]));
    let logs = ok(runner(&root, &["logs", &name]));
    assert!(uname.starts_with("Linux "), "{uname}");
    // The release the guest kernel printed on the console at boot.
    let release = uname.split_whitespace().nth(2).unwrap();
    assert!(
        logs.contains(&format!("Linux version {release} ")),
        "{uname}\n{logs}"
    );
    let who = ok(runner(
        &root,
        &["exec", &name, "--", "sh", "-c", "whoami; pwd"],
    ));
    assert_eq!(who, "agent\n/home/agent\n");
    let mut piped = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", &root)
        .args(["exec", &name, "--", "sh", "-c", "cat; exit 7"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Written, then closed, which ends cat.
    let mut stdin = piped.stdin.take().unwrap();
    std::io::Write::write_all(&mut stdin, b"over vsock").unwrap();
    drop(stdin);
    let out = piped.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "over vsock");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm, the firecracker@ unit and the base image"]
fn a_session_outlives_a_dropped_connection() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("session-test-{}", std::process::id());
    let disk = env_path("RUNNER_TEST_DISK");
    ok(runner(&root, &["import", &name, disk.to_str().unwrap()]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    ok(runner(&root, &["start", &name]));
    let on_a_terminal = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", &root).env_remove("TERM").args(args);
        cmd
    };

    let t = common::pty(24, 80);
    let mut shell = t.spawn(on_a_terminal(&["shell", &name, "--session", "work"]));
    t.send("echo before-$((1+1))\r");
    t.wait_for("before-2");
    // What sshd does when the connection drops.
    let pid = nix::unistd::Pid::from_raw(shell.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::SIGHUP).unwrap();
    assert_eq!(shell.wait().unwrap().code(), Some(129), "{}", t.seen());
    eventually("the session to detach", || {
        ok(runner(&root, &["sessions", &name]))
            .lines()
            .any(|l| l.starts_with("work  bash  ") && l.ends_with("  detached"))
    });

    let t = common::pty(24, 80);
    let mut again = t.spawn(on_a_terminal(&["attach", &name, "work"]));
    t.wait_for("before-2");
    let detach = ["exec", &name, "--", "tmux", "detach-client", "-s", "=work"];
    ok(runner(&root, &detach));
    assert_eq!(again.wait().unwrap().code(), Some(0), "{}", t.seen());
    ok(runner(&root, &["kill", &name, "work"]));
}

// No token needed: a logged-out claude waits at its login screen.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, the firecracker@ unit and the base image"]
fn claude_keeps_running_after_a_detach() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let src = format!("claude-src-{}", std::process::id());
    let name = format!("claude-test-{}", std::process::id());
    let disk = env_path("RUNNER_TEST_DISK");
    ok(runner(&root, &["import", &src, disk.to_str().unwrap()]));
    let _cleanup = [&src, &name].map(|vm| Cleanup {
        root: root.clone(),
        name: vm.clone(),
    });
    let on_a_terminal = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", &root).env_remove("TERM").args(args);
        cmd
    };
    let in_vm = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    let detach = [
        "exec",
        &name,
        "--",
        "tmux",
        "detach-client",
        "-s",
        "=claude",
    ];
    let claude_sessions = || {
        let listed = ok(runner(&root, &["sessions", &name]));
        listed.lines().filter(|l| l.starts_with("claude  ")).count()
    };

    // Made from src, booted, and claude started in the claude session.
    let t = common::pty(24, 80);
    let mut first = t.spawn(on_a_terminal(&["claude", &name, "--from", &src]));
    t.wait_for(&format!("runner: made {name} from {src}"));
    eventually("claude's session", || claude_sessions() == 1);
    std::thread::sleep(Duration::from_secs(5));
    ok(runner(&root, &detach));
    assert_eq!(first.wait().unwrap().code(), Some(0), "{}", t.seen());

    // A pane closes with what it runs, so a pane still there runs claude.
    std::thread::sleep(Duration::from_secs(5));
    let pane = r#"pid=$(tmux list-panes -t =claude: -F '#{pane_pid}') &&
        echo "$pid" && tr '\0' ' ' </proc/$pid/cmdline && echo && readlink /proc/$pid/cwd"#;
    let before = in_vm(pane);
    let lines: Vec<&str> = before.lines().collect();
    assert_eq!(lines.len(), 3, "{before}");
    assert!(lines[1].contains("claude"), "{before}");
    assert_eq!(lines[2], "/home/agent/work", "{before}");

    // Again, with arguments: the same claude, and a note that they're unused.
    let t = common::pty(24, 80);
    let mut again = t.spawn(on_a_terminal(&["claude", &name, "--", "--continue"]));
    t.wait_for("already has a claude session");
    std::thread::sleep(Duration::from_secs(2));
    ok(runner(&root, &detach));
    assert_eq!(again.wait().unwrap().code(), Some(0), "{}", t.seen());
    assert_eq!(claude_sessions(), 1);
    assert_eq!(in_vm(pane), before);
}

// Booted by hand, so the token comes from this process's environment, the
// way the unit's comes from /etc/credstore. Commands reach the guest through
// the VM's vsock directly, since systemd doesn't know this VM is up.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, Firecracker, a kernel, the base image and root"]
fn the_token_reaches_the_vm_s_memory_and_never_its_disk() {
    use runner::proto::{PORT, Request};
    let token = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("sk-ant-oat01-runner-test-{}-{nanos}", std::process::id())
    });
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let disk = env_path("RUNNER_TEST_DISK");
    ok(runner(root, &["import", "dev", disk.to_str().unwrap()]));
    std::fs::create_dir_all(root.join("kernel")).unwrap();
    std::fs::copy(env_path("RUNNER_TEST_KERNEL"), root.join("kernel/vmlinux")).unwrap();

    let console = root.join("console.log");
    let boot = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .env("CLAUDE_CODE_OAUTH_TOKEN", &token)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GH_TOKEN")
        .env_remove("CREDENTIALS_DIRECTORY")
        .args(["boot", "dev"])
        .stdout(std::fs::File::create(&console).unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let vsock = root.join("vms/dev/vsock.sock");
    let run = |script: &str, timeout: u64| {
        let conn = runner::exec::connect(&vsock, PORT, runner::exec::GUEST_WAIT).unwrap();
        let req = Request {
            argv: ["sh", "-c", script].map(String::from).to_vec(),
            env: Vec::new(),
            cwd: None,
            user: Some(runner::exec::USER.to_string()),
            tty: None,
        };
        let timeout = Duration::from_secs(timeout);
        runner::exec::capture(conn, &req, b"", timeout, 1 << 20).unwrap()
    };
    let seen = run(r#"printf %s "$CLAUDE_CODE_OAUTH_TOKEN""#, 30);
    // As a session would: Claude Code runs, and writes what it writes.
    let claude = run("claude -p 'Reply with the word ok' </dev/null; sync", 300);
    let handed = runner::store::Store::new(root).handed("dev").unwrap();
    ok(runner(root, &["halt", "dev"]));
    let out = boot.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert_eq!(seen.stdout, token, "the guest didn't have the token");
    assert_eq!(
        handed,
        Some(vec![runner::creds::Cred::Claude]),
        "{claude:?}"
    );
    assert!(!err.contains(&token), "boot's journal holds the token");

    // Every disk the VM booted from, and its console, which the unit sends
    // to the host's journal. The imported one never ran.
    let mut held = Vec::new();
    for entry in std::fs::read_dir(root.join("vms/dev/gens")).unwrap() {
        let path = entry.unwrap().path();
        let ran = path.extension().is_some_and(|e| e == "ext4") && !path.ends_with("0.ext4");
        if ran && holds(&path, &token) {
            held.push(path.display().to_string());
        }
    }
    if holds(&console, &token) {
        held.push(console.display().to_string());
    }
    assert!(
        held.is_empty(),
        "the token is in {held:?}; claude said {claude:?}"
    );
}

// GNU grep, which skips a sparse disk's holes and is far faster than a scan
// here. -z splits records at NULs, so a disk of zeros isn't one huge line;
// a token holds none. It goes in on stdin, not argv, where `ps` shows it.
fn holds(path: &Path, token: &str) -> bool {
    use std::io::Write;
    let mut grep = Command::new("grep")
        .args(["-F", "-a", "-z", "-q", "-f", "-", "--"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = grep.stdin.take().unwrap();
    stdin.write_all(token.as_bytes()).unwrap();
    drop(stdin);
    match grep.wait().unwrap().code() {
        Some(0) => true,
        Some(1) => false,
        code => panic!("grep {} exited {code:?}", path.display()),
    }
}

// A failed test still leaves the host's VMs as it found them.
struct Cleanup {
    root: PathBuf,
    name: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        runner(&self.root, &["stop", &self.name]);
        runner(&self.root, &["delete", &self.name]);
    }
}
