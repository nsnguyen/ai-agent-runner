// Boot tests: real Firecracker, real KVM. Skipped unless asked for, since
// cloud sessions have no /dev/kvm. On the host, as docs/host.md says:
//
//   CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' \
//   RUNNER_TEST_DISK=$PWD/images/base/out/base.ext4 \
//   RUNNER_TEST_KERNEL=$PWD/images/base/out/vmlinux \
//   cargo test --test kvm -- --ignored --test-threads=1
//
// The rootfs's init must be systemd or another that takes Ctrl-Alt-Del, with
// a login prompt on the serial console.
// `boot_and_halt_by_hand` and `a_booted_vm_answers_at_its_address` need
// Firecracker on PATH, RUNNER_TEST_KERNEL, and root (or CAP_NET_ADMIN) for
// the VM's tap: run them with sudo -E. So does
// `the_token_reaches_the_vm_s_memory_and_never_its_disk`, with the base
// image as RUNNER_TEST_DISK; a real token in CLAUDE_CODE_OAUTH_TOKEN lets
// claude get further than a login error.
// The tests under the unit (`start_logs_and_stop_under_systemd`,
// `exec_starts_the_vm_and_runs_in_the_guest`,
// `a_running_vm_s_checkpoint_brings_a_deleted_file_back`,
// `an_agent_s_checkpoint_from_inside_its_vm_shows_up_in_runner_checkpoints`,
// `an_agent_s_restore_reboots_its_vm_onto_its_checkpoint`,
// `a_process_lives_through_sleep_and_wake`,
// `a_sleeping_vm_stops_cleanly_and_keeps_what_it_wrote`,
// `an_idle_vm_sleeps_and_exec_wakes_it`,
// `a_session_outlives_a_dropped_connection`,
// `a_service_answers_again_after_a_stop_and_start`,
// `a_service_answers_through_its_url_after_the_vm_slept`,
// `a_vm_reaches_the_internet_but_not_the_host`,
// `claude_keeps_running_after_a_detach` and
// `a_headless_run_edits_a_file_and_commits_it`) need a host set up as
// docs/host.md says, with `runner doctor` all ok: they fork its base, in the
// unit's root (RUNNER_ROOT or /var/lib/runner), and run `runner` as that
// root's owner. The unit boots them with the installed runner, so rerun
// host/install.sh after a build. The headless run needs the host's Claude
// Code token in /etc/credstore (docs/host.md, step 6). The agent's own
// checkpoints need a base built with this runner-guest (images/base/build.sh).
mod common;

use std::fs::Permissions;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn env_path(name: &str) -> PathBuf {
    let path = std::env::var_os(name).unwrap_or_else(|| panic!("set {name}"));
    PathBuf::from(path)
}

/// `runner` on `root`, as the user who owns it, as over SSH. Run as root, the
/// tests would make VMs only root can open, which the unit, running as the
/// root's owner, then fails to boot: "lock: Permission denied". That user
/// can't reach this build in a home it can't read, so it gets a copy.
fn cli(root: &Path) -> Command {
    let owner = std::fs::metadata(root).unwrap();
    let mut cmd = if nix::unistd::geteuid().is_root() && owner.uid() != 0 {
        let mut cmd = Command::new("setpriv");
        cmd.arg(format!("--reuid={}", owner.uid()))
            .arg(format!("--regid={}", owner.gid()))
            .args(["--init-groups", "--"])
            .arg(runner_copy())
            .current_dir("/");
        cmd
    } else {
        Command::new(env!("CARGO_BIN_EXE_runner"))
    };
    cmd.env("RUNNER_ROOT", root);
    cmd
}

/// This build, where any user can run it: one copy, replaced each run. In
/// /var/tmp, not TMPDIR, which sudo -E passes on: the invoking user's, which
/// the root's owner may not be able to read.
fn runner_copy() -> &'static Path {
    static COPY: OnceLock<PathBuf> = OnceLock::new();
    COPY.get_or_init(|| {
        let dir = Path::new("/var/tmp/runner-kvm");
        match std::fs::create_dir(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => panic!("{dir:?}: {e}"),
            _ => {}
        }
        // Root's own directory, not one or a link someone else put there.
        let meta = std::fs::symlink_metadata(dir).unwrap();
        assert!(meta.is_dir() && meta.uid() == 0, "{dir:?} isn't root's");
        std::fs::set_permissions(dir, Permissions::from_mode(0o755)).unwrap();
        // Renamed over the old copy, which a run may still be executing.
        let new = dir.join(format!("runner.{}", std::process::id()));
        std::fs::copy(env!("CARGO_BIN_EXE_runner"), &new).unwrap();
        std::fs::set_permissions(&new, Permissions::from_mode(0o755)).unwrap();
        let copy = dir.join("runner");
        std::fs::rename(&new, &copy).unwrap();
        copy
    })
}

fn runner(root: &Path, args: &[&str]) -> Output {
    cli(root).args(args).output().unwrap()
}

fn ok(out: Output) -> String {
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}{err}");
    text
}

/// Halts the VM `boot` booted by hand, and waits for boot to end for as long
/// as the unit's stop timeout would. A guest that ignores Ctrl-Alt-Del fails
/// the test there instead of hanging it: its Firecracker, whose socket is
/// under `root`, is killed, and the console's i8042 lines say why.
fn halt_by_hand(root: &Path, mut boot: Child, console: &Path) -> Output {
    let mut halt = cli(root)
        .args(["halt", "dev"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Firecracker by its socket, the one process of this VM's that boot and
    // halt wait on: a pattern for the whole root, /var/lib/runner say, would
    // match every VM there.
    let sock = root.join("vms/dev/api.sock");
    let kill = |boot: &mut Child, halt: &mut Child| {
        let _ = Command::new("pkill")
            .args(["-KILL", "-f"])
            .arg(&sock)
            .status();
        // A moment for boot to take its tap down once Firecracker's gone.
        let gone = Instant::now() + Duration::from_secs(2);
        while boot.try_wait().unwrap().is_none() && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = boot.kill();
        let _ = halt.kill();
        let _ = boot.wait();
        let _ = halt.wait();
    };
    let limit = Duration::from_secs(runner::systemd::STOP_TIMEOUT_SECS.into());
    let deadline = Instant::now() + limit;
    while boot.try_wait().unwrap().is_none() {
        if let Some(status) = halt.try_wait().unwrap()
            && !status.success()
        {
            let mut err = String::new();
            std::io::Read::read_to_string(&mut halt.stderr.take().unwrap(), &mut err).unwrap();
            kill(&mut boot, &mut halt);
            panic!("runner halt failed: {err}");
        }
        if Instant::now() >= deadline {
            kill(&mut boot, &mut halt);
            let log = std::fs::read_to_string(console).unwrap_or_default();
            let keyboard: Vec<&str> = log.lines().filter(|l| l.contains("i8042")).collect();
            panic!(
                "the guest ignored Ctrl-Alt-Del for {limit:?}. Its keyboard:\n{}",
                keyboard.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let halted = halt.wait_with_output().unwrap();
    assert!(
        halted.status.success(),
        "{}",
        String::from_utf8_lossy(&halted.stderr)
    );
    boot.wait_with_output().unwrap()
}

// No VM boots here: a halt that fails at once fails the test at once, with
// halt's own words, rather than 30 seconds later blaming the guest.
#[test]
fn a_halt_that_fails_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let boot = Command::new("sleep").arg("60").spawn().unwrap();
    let started = Instant::now();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        halt_by_hand(dir.path(), boot, &dir.path().join("console.log"))
    }));
    let why = failed.expect_err("halted a VM that isn't there");
    let why = why.downcast_ref::<String>().expect("a formatted panic");
    assert!(
        why.contains("runner halt failed") && why.contains("dev"),
        "{why}"
    );
    assert!(started.elapsed() < Duration::from_secs(10), "{why}");
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
    let out = halt_by_hand(root, boot, &console);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(ok(runner(root, &["list"])).starts_with("dev  gen 1  "));
}

// The guest's side (resolving a name, fetching a page, and not reaching its
// gateway) is `a_vm_reaches_the_internet_but_not_the_host`. From the host: the guest kernel took its address
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
    let out = halt_by_hand(root, boot, &console);
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
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn start_logs_and_stop_under_systemd() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("boot-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
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
    // journald keeps a line until its newline, and getty's prompt has none,
    // so `login:` never reaches the journal. Getty starting is the last line
    // that does, and by then init takes Ctrl-Alt-Del.
    eventually("getty on the serial console", || {
        logs()
            .lines()
            .any(|l| l.contains("Started") && l.contains("serial-getty@ttyS0"))
    });

    ok(runner(&root, &["stop", &name]));
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.ends_with("  stopped\n"), "{status}");
    // Stopped cleanly, so the run was committed.
    assert!(status.contains("  gen 1  "), "{status}\n{}", logs());
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn exec_starts_the_vm_and_runs_in_the_guest() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("exec-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
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
    let mut piped = cli(&root)
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
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_session_outlives_a_dropped_connection() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("session-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    ok(runner(&root, &["start", &name]));
    let on_a_terminal = |args: &[&str]| {
        let mut cmd = cli(&root);
        cmd.env_remove("TERM").args(args);
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

// What a GET of / at the address gets back first, if anything.
fn http_status(address: &str, port: u16) -> Option<String> {
    http_status_within(address, port, Duration::from_secs(5))
}

// `http_status`, waiting up to `wait` for the answer.
fn http_status_within(address: &str, port: u16, wait: Duration) -> Option<String> {
    use std::io::{BufRead, Write};
    let to = std::net::SocketAddr::new(address.parse().ok()?, port);
    let mut conn = std::net::TcpStream::connect_timeout(&to, Duration::from_secs(2)).ok()?;
    conn.set_read_timeout(Some(wait)).ok()?;
    conn.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut line = String::new();
    std::io::BufReader::new(conn).read_line(&mut line).ok()?;
    Some(line.trim_end().to_string())
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_service_answers_again_after_a_stop_and_start() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("service-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let address = ok(runner(&root, &["ip", &name])).trim().to_string();
    let answers = || http_status(&address, 8080).is_some_and(|l| l.ends_with(" 200 OK"));

    // Stopped, so add boots it.
    let web = ["python3", "-m", "http.server", "8080"];
    let mut add = vec!["service", "add", &name, "web", "--"];
    add.extend(web);
    let added = ok(runner(&root, &add));
    let row = "web  active/running  /home/agent/work  python3 -m http.server 8080\n";
    assert_eq!(added, row);
    eventually("the service to answer", answers);

    ok(runner(&root, &["stop", &name]));
    assert!(!answers());
    ok(runner(&root, &["start", &name]));
    eventually("the service to answer after a stop and start", answers);
    assert_eq!(ok(runner(&root, &["service", "list", &name])), row);
    let logs = ok(runner(
        &root,
        &["service", "logs", &name, "web", "-n", "50"],
    ));
    assert!(logs.contains("\"GET / HTTP/1.0\" 200"), "{logs}");

    // A program that isn't there fails add, and leaves no unit.
    let missing = runner(
        &root,
        &["service", "add", &name, "nope", "--", "no-such-program"],
    );
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(!missing.status.success());
    assert!(err.contains("nope didn't start in"), "{err}");
    assert!(err.contains("no-such-program"), "{err}");
    let taken = runner(&root, &["service", "add", &name, "web", "--", "true"]);
    let err = String::from_utf8_lossy(&taken.stderr);
    assert!(err.contains("already has a service named web"), "{err}");

    ok(runner(&root, &["service", "rm", &name, "web"]));
    assert_eq!(
        ok(runner(&root, &["service", "list", &name])),
        "no services\n"
    );
    assert!(!answers());
}

// The URL, from the host itself through its tailnet address, so Tailscale has
// to be up: it answers while the VM runs, and a GET to it once it's asleep
// wakes it and is answered.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and Tailscale up"]
fn a_service_answers_through_its_url_after_the_vm_slept() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("url-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let url = ok(runner(&root, &["url", &name])).trim().to_string();
    let (host, port) = url
        .strip_prefix("http://")
        .and_then(|at| at.rsplit_once(':'))
        .unwrap_or_else(|| panic!("not a URL: {url}"));
    assert!(host.starts_with("100."), "{url}");
    let port: u16 = port.parse().unwrap();
    let answers = || http_status(host, port).is_some_and(|l| l.ends_with(" 200 OK"));

    let web = [
        "service",
        "add",
        &name,
        "web",
        "--",
        "python3",
        "-m",
        "http.server",
        "8080",
    ];
    ok(runner(&root, &web));
    eventually("the service to answer through the URL", answers);

    let slept = ok(runner(&root, &["sleep", &name]));
    assert!(slept.contains("  sleeping"), "{slept}");
    // One GET, held through the wake, for as long as the URL would hold it.
    let began = Instant::now();
    let first = http_status_within(host, port, Duration::from_secs(30));
    let took = began.elapsed();
    assert!(
        first.as_deref().is_some_and(|l| l.ends_with(" 200 OK")),
        "{first:?} after {took:?}"
    );
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.contains("  running"), "{status}");
    let logs = ok(runner(&root, &["logs", &name]));
    assert!(logs.contains(&format!("{name}'s URL is {url}")), "{logs}");
    eprintln!("a GET to the sleeping VM's URL was answered in {took:?}");

    // No VM gets into another, or its own, through a URL: the host drops what
    // a VM opens to it, its tailnet address too, before the URL sees it.
    // curl's 28 is a timeout; one the URL refused would end sooner.
    let address = ok(runner(&root, &["ip", &name])).trim().to_string();
    let inside = runner(&root, &["exec", &name, "--", "curl", "-s", "-m", "5", &url]);
    assert_eq!(
        inside.status.code(),
        Some(28),
        "{}",
        String::from_utf8_lossy(&inside.stdout)
    );
    let logs = ok(runner(&root, &["logs", &name]));
    assert!(
        !logs.contains(&format!("{name}'s URL refused {address}:")),
        "{logs}"
    );

    // Off, it doesn't answer; on, it does again.
    ok(runner(&root, &["url", &name, "off"]));
    eventually("the URL to stop answering", || !answers());
    ok(runner(&root, &["url", &name, "on"]));
    eventually("the URL to answer again", answers);
}

// The host drops what a VM opens to it, but the VM still has the internet,
// DNS included, through the host.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and the internet"]
fn a_vm_reaches_the_internet_but_not_the_host() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("host-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let address = ok(runner(&root, &["ip", &name]));
    let address: std::net::Ipv4Addr = address.trim().parse().unwrap();
    // The host's end of the VM's /30.
    let gateway = std::net::Ipv4Addr::from(u32::from(address) - 1).to_string();
    // Dropped, so it times out: sshd would answer at once, and a closed port
    // refuse at once. The host still answers ARP, which nftables' ip tables
    // don't see.
    let probe = "import errno, socket, sys
try:
    socket.create_connection((sys.argv[1], 22), 5)
    print('connected')
except TimeoutError:
    print('timed out')
except OSError as e:
    print('unreachable' if e.errno == errno.EHOSTUNREACH else e)
";
    let ssh = ok(runner(
        &root,
        &["exec", &name, "--", "python3", "-c", probe, &gateway],
    ));
    assert_eq!(ssh, "timed out\n", "{gateway}:22");
    // Nor over IPv6, at the host's link-local address on the VM's tap, which
    // the VM could work out from its gateway's MAC. The tap is named for the
    // VM's /30. A host without IPv6 has no such address. The ip6 input chain
    // drops the VM's neighbor solicitations too, so the guest kernel may give
    // up on the neighbor and fail the connect as unreachable before it times
    // out: either means nothing got through.
    let tap = format!("fc{:08x}", u32::from(address) - 2);
    let shown = Command::new("ip")
        .args(["-6", "-o", "addr", "show", "dev", &tap, "scope", "link"])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout);
    match shown
        .split_whitespace()
        .skip_while(|w| *w != "inet6")
        .nth(1)
    {
        Some(cidr) => {
            let link_local = format!("{}%eth0", cidr.split('/').next().unwrap());
            let ssh6 = ok(runner(
                &root,
                &["exec", &name, "--", "python3", "-c", probe, &link_local],
            ));
            assert!(
                matches!(ssh6.as_str(), "timed out\n" | "unreachable\n"),
                "[{link_local}]:22: {ssh6}"
            );
        }
        None => eprintln!("no IPv6 on {tap}, so no IPv6 to try"),
    }
    let web = [
        "curl",
        "-fsS",
        "-m",
        "20",
        "-o",
        "/dev/null",
        "https://example.com",
    ];
    let mut exec = vec!["exec", &name, "--"];
    exec.extend(web);
    ok(runner(&root, &exec));
}

// No token needed: a logged-out claude waits at its login screen.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn claude_keeps_running_after_a_detach() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let src = format!("claude-src-{}", std::process::id());
    let name = format!("claude-test-{}", std::process::id());
    ok(runner(&root, &["create", &src]));
    let _cleanup = [&src, &name].map(|vm| Cleanup {
        root: root.clone(),
        name: vm.clone(),
    });
    let on_a_terminal = |args: &[&str]| {
        let mut cmd = cli(&root);
        cmd.env_remove("TERM").args(args);
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

// A real `claude -p` under the unit: the run boots the VM, Claude Code edits
// a file and exits 0, and the VM shuts down, committing the file.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and its token"]
fn a_headless_run_edits_a_file_and_commits_it() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("run-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };

    let prompt = "Create a file named hello.txt in the current directory holding \
                  exactly the text hi, then stop.";
    let id = ok(runner(&root, &["run", &name, prompt]));
    assert_eq!(id.trim(), format!("{name}.1"));
    // Returns once the run is over, its VM shut down and all.
    let stream = ok(runner(&root, &["output", id.trim(), "--follow"]));
    let rows = ok(runner(&root, &["runs", &name]));
    assert!(rows.contains("  exited 0, committed  "), "{rows}{stream}");
    assert!(stream.contains(r#""type":"result""#), "{stream}");
    // The runner read the file off the run's disk itself.
    let result = ok(runner(&root, &["result", id.trim()]));
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["outcome"], "committed", "{result}");
    let added = serde_json::json!({"path": "/home/agent/work/hello.txt", "change": "added"});
    let paths = result["changed"]["paths"].as_array();
    assert!(paths.is_some_and(|p| p.contains(&added)), "{result}");
    // And none of the boot's: it wrote its journal at least, which is only
    // counted. The agent writes without root only in its home and the temp
    // directories, so a path anywhere else is the boot's, and belongs on
    // changes::BOOT_ONLY. So are systemd's and X11's in /tmp. This is a first
    // boot: the daily timers' writes a later boot catches up on aren't here.
    let left_out = result["changed"]["boot_only"].as_u64();
    assert!(left_out.is_some_and(|n| n > 0), "{result}");
    let systemd = [
        "/tmp/.",
        "/tmp/systemd-private-",
        "/var/tmp/systemd-private-",
    ];
    let agents = |p: &&str| {
        let temp = ["/tmp/", "/var/tmp/"].iter().any(|d| p.starts_with(d));
        p.starts_with("/home/") || temp && !systemd.iter().any(|d| p.starts_with(d))
    };
    let boots: Vec<&str> = paths
        .into_iter()
        .flatten()
        .filter_map(|c| c["path"].as_str())
        .filter(|p| !agents(p))
        .collect();
    assert!(
        boots.is_empty(),
        "the boot's, not on BOOT_ONLY: {boots:?}\n{result}"
    );
    assert_eq!(result["agent_says"]["untrusted"], true);
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.ends_with("  stopped\n"), "{status}");

    // The next boot starts from the committed generation, file and all.
    let hello = ok(runner(
        &root,
        &["exec", &name, "--", "cat", "/home/agent/work/hello.txt"],
    ));
    assert_eq!(hello.trim(), "hi");
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_running_vm_s_checkpoint_brings_a_deleted_file_back() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("checkpoint-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));

    // Written, never synced by hand: the checkpoint syncs the guest first.
    sh("echo kept > ~/checkpointed");
    let made = Instant::now();
    let row = ok(runner(&root, &["checkpoint", &name, "-m", "with the file"]));
    let took = made.elapsed();
    let id = row.split_whitespace().next().unwrap().to_string();
    assert!(row.contains("  checkpoint  "), "{row}");
    // Still the same boot, which carried on.
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.contains("  running"), "{status}");
    sh("rm ~/checkpointed; sync");

    ok(runner(&root, &["restore", &name, &id]));
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.contains("  running"), "restarted: {status}");
    assert_eq!(sh("cat ~/checkpointed"), "kept\n");
    // Says how long the whole checkpoint took, pause and all: on XFS, well
    // under a second of it is the pause.
    eprintln!("checkpoint of a running VM took {took:?}");
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_process_lives_through_sleep_and_wake() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("sleep-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    let status = || ok(runner(&root, &["status", &name]));

    // Boots it. In a session of its own, so the exec's end doesn't end it.
    let pid = sh("setsid sleep 1000 </dev/null >/dev/null 2>&1 & echo $!");
    let pid = pid.trim().to_string();
    let slept = ok(runner(&root, &["sleep", &name]));
    assert!(slept.contains("  sleeping"), "{slept}");
    assert!(status().contains("  sleeping"), "{}", status());
    // Long enough asleep for a clock that stood still to show.
    std::thread::sleep(Duration::from_secs(5));

    let began = Instant::now();
    let woke = ok(runner(&root, &["wake", &name]));
    let took = began.elapsed();
    assert!(woke.contains("  running"), "{woke}");
    assert_eq!(sh(&format!("cat /proc/{pid}/comm")), "sleep\n");
    // Set to the host's at the wake.
    let guest: i64 = sh("date +%s").trim().parse().unwrap();
    let host = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (host - guest).abs() <= 2,
        "guest clock {guest}, host {host}"
    );

    // Asleep again, exec wakes it first, as start does.
    ok(runner(&root, &["sleep", &name]));
    assert_eq!(sh(&format!("cat /proc/{pid}/comm")), "sleep\n");
    let logs = ok(runner(&root, &["logs", &name]));
    let notes: Vec<&str> = logs.lines().filter(|l| l.contains(" woke in ")).collect();
    // Says how long the wake took, from the CLI and as boot measured it.
    eprintln!("runner wake took {took:?}; boot's notes: {notes:?}");
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_sleeping_vm_stops_cleanly_and_keeps_what_it_wrote() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("sleep-stop-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    // Written, never synced: it's in the guest's memory when it sleeps.
    sh("echo kept > ~/before-sleep");
    ok(runner(&root, &["sleep", &name]));
    // Woken to shut down, so the shutdown is clean and commits.
    ok(runner(&root, &["stop", &name]));
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.ends_with("  stopped\n"), "{status}");
    assert!(status.contains("  gen 1  "), "{status}");
    assert_eq!(sh("cat ~/before-sleep"), "kept\n");
}

// Sleep when idle: a minute unused and its boot puts it to sleep; an exec
// wakes it and runs.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn an_idle_vm_sleeps_and_exec_wakes_it() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("idle-test-{}", std::process::id());
    ok(runner(&root, &["create", &name, "--idle", "1"]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    let status = || ok(runner(&root, &["status", &name]));
    // Boots it. A detached process doesn't keep it awake, and lives on.
    let pid = sh("setsid sleep 1000 </dev/null >/dev/null 2>&1 & echo $!");
    let pid = pid.trim().to_string();
    let unused = Instant::now();
    assert!(status().contains("  running"), "{}", status());
    // A minute, and up to a look's 10 seconds and the sleep itself.
    let deadline = unused + Duration::from_secs(120);
    while !status().contains("  sleeping") {
        assert!(Instant::now() < deadline, "still awake: {}", status());
        std::thread::sleep(Duration::from_secs(1));
    }
    let slept = unused.elapsed();
    assert!(slept >= Duration::from_secs(60), "slept after {slept:?}");

    assert_eq!(sh(&format!("cat /proc/{pid}/comm")), "sleep\n");
    assert!(status().contains("  running"), "{}", status());
    let logs = ok(runner(&root, &["logs", &name]));
    assert!(logs.contains("went unused for 1m"), "{logs}");
    eprintln!("slept after {slept:?} unused");
}

// The id a checkpoint row starts with, after the mark for the current one.
fn row_id(row: &str) -> String {
    let row = row.trim_start_matches('*');
    row.split_whitespace().next().unwrap().to_string()
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn an_agent_s_checkpoint_from_inside_its_vm_shows_up_in_runner_checkpoints() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("agent-checkpoint-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };

    // As the agent user, the way Claude Code in the VM would run it.
    let made = ok(runner(
        &root,
        &[
            "exec",
            &name,
            "--",
            "runner-guest",
            "checkpoint",
            "-m",
            "test",
        ],
    ));
    let id = row_id(&made);
    let listed = ok(runner(&root, &["checkpoints", &name]));
    let row = listed.lines().find(|l| row_id(l) == id);
    let row = row.unwrap_or_else(|| panic!("no checkpoint {id}: {listed}"));
    assert!(
        row.contains("  agent  ") && row.ends_with("  \"test\""),
        "{listed}"
    );
    let from_inside = ok(runner(
        &root,
        &["exec", &name, "--", "runner-guest", "checkpoints"],
    ));
    assert_eq!(from_inside, listed);
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.contains("  running"), "{status}");
}

#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn an_agent_s_restore_reboots_its_vm_onto_its_checkpoint() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("agent-restore-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));

    sh("echo kept > ~/checkpointed");
    let made = ok(runner(
        &root,
        &[
            "exec",
            &name,
            "--",
            "runner-guest",
            "checkpoint",
            "-m",
            "with the file",
        ],
    ));
    let id = row_id(&made);
    sh("rm ~/checkpointed; sync");
    let gen_of = |status: &str| {
        status
            .split("  gen ")
            .nth(1)
            .map(|s| s.split(' ').next().unwrap().to_string())
    };
    let before = gen_of(&ok(runner(&root, &["status", &name])));

    let said = runner(
        &root,
        &["exec", &name, "--", "runner-guest", "restore", &id],
    );
    let (out, err) = (
        String::from_utf8_lossy(&said.stdout),
        String::from_utf8_lossy(&said.stderr),
    );
    assert!(
        out.contains(&format!("restoring checkpoint {id}")),
        "{out}{err}"
    );
    // Down, then booted again by the unit, onto the restored disk.
    eventually("the VM to boot again onto the restore", || {
        let status = ok(runner(&root, &["status", &name]));
        status.contains("  running") && gen_of(&status) != before
    });
    assert_eq!(sh("cat ~/checkpointed"), "kept\n");
    let listed = ok(runner(&root, &["checkpoints", &name]));
    let top = listed.lines().next().unwrap();
    assert!(
        top.starts_with("* ") && top.ends_with(&format!("\"restore of {id}\"")),
        "{listed}"
    );
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
    let out = halt_by_hand(root, boot, &console);
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
