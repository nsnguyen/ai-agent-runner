// Boot tests: real Firecracker, real KVM. Skipped unless asked for, since
// cloud sessions have no /dev/kvm. On the host, `host/check.sh` runs them
// with every other #[ignore]d test, as docs/host.md (Boot tests) says, or
// only those matching a filter: `host/check.sh <name>`.
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
// `a_resized_disk_is_that_big_in_the_guest`,
// `a_running_vm_s_export_boots_elsewhere_and_it_carries_on`,
// `an_agent_s_checkpoint_from_inside_its_vm_shows_up_in_runner_checkpoints`,
// `an_agent_s_restore_reboots_its_vm_onto_its_checkpoint`,
// `best_of_two_keeps_the_winner_on_dev`,
// `a_process_lives_through_sleep_and_wake`,
// `a_sleeping_vm_stops_cleanly_and_keeps_what_it_wrote`,
// `a_sleeping_vm_lives_through_a_host_stop`,
// `an_idle_vm_sleeps_and_exec_wakes_it`,
// `an_open_proxy_keeps_its_vm_awake`,
// `a_session_outlives_a_dropped_connection`,
// `a_service_answers_again_after_a_stop_and_start`,
// `an_imported_export_boots_with_its_files_and_services`,
// `an_old_vm_rebased_onto_base_gets_its_guest_and_keeps_its_home`,
// `a_service_answers_through_its_url_after_the_vm_slept`,
// `a_vm_reaches_the_internet_but_not_the_host`,
// `a_listed_vm_reaches_only_its_names`,
// `a_headless_run_on_a_listed_vm_reaches_claude_code_s_api`,
// `claude_keeps_running_after_a_detach`,
// `claude_code_in_a_vm_has_its_updater_off`,
// `agent_runs_docker_in_a_vm_without_sudo`,
// `a_compose_stack_lives_through_sleep_and_comes_back_at_boot`,
// `a_headless_run_edits_a_file_and_commits_it`,
// `a_run_taken_over_carries_on_in_claude_s_session` and
// `a_cancelled_run_taken_over_gets_its_disk_and_session_back`) need a host set up as
// docs/host.md says, with `runner doctor` all ok: they fork its base, in the
// unit's root (RUNNER_ROOT or /var/lib/runner), and run `runner` as that
// root's owner. The unit boots them with the installed runner, so rerun
// host/install.sh after a build. The headless runs need the host's Claude
// Code token in /etc/credstore (docs/host.md, step 6). The agent's own
// checkpoints need a base built with this runner-guest, and the updater and
// Docker tests need a base built from this images/base (images/base/build.sh).
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

/// What the take-over tests ask a run to do, so it's still going when they
/// take it over. Not `sleep 300`: Claude Code blocks a standalone sleep and
/// has the model run it in the background, and the run ends at once
/// (2026-11-08's check). A command past the Bash tool's timeout, two minutes
/// by default, goes to the background too, so the run lasts about that long.
const LONG_PROMPT: &str = "the shell command timeout 300 tail -f /dev/null in the foreground";

/// Presses Enter through an interactive claude's first-run screens until
/// the take-over tests' prompt shows in claude's pane, for up to a minute,
/// and Down where Enter would end claude. Giving up, it says what the pane
/// shows, how its capture ended, and the end of what the taking-over
/// terminal drew, without its escapes.
fn enter_to_the_prompt(t: &common::Pty, root: &Path, capture: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let out = runner(root, capture);
        let shown = String::from_utf8_lossy(&out.stdout);
        // Claude Code wraps the prompt to the pane: matched as one line.
        let flat = shown.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.contains(LONG_PROMPT) {
            return;
        }
        if Instant::now() >= deadline {
            let drawn = plain(&t.seen());
            let end = drawn.len() - drawn.len().min(4000);
            let end = (end..drawn.len())
                .find(|&i| drawn.is_char_boundary(i))
                .unwrap_or(end);
            panic!(
                "gave up waiting for the run's prompt in claude's pane, which shows:\n{shown}\n\
                 The capture: {}, {}\nThe terminal drew, at the end:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
                &drawn[end..]
            );
        }
        let key = key_for(&shown);
        t.send(key);
        if key == DOWN {
            // Down wraps from Yes back to No, so no key goes until the pane
            // shows this one has moved off No.
            let moved = Instant::now() + Duration::from_secs(5);
            while Instant::now() < moved {
                let out = runner(root, capture);
                if key_for(&String::from_utf8_lossy(&out.stdout)) != DOWN {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

const DOWN: &str = "\x1b[B";

/// The key for a first-run screen: Enter takes its default, but on Claude
/// Code's trust screen the default is "No, exit", so Down moves to "Yes, I
/// trust this folder" first.
fn key_for(shown: &str) -> &'static str {
    // The capture holds the scrollback: only the newest trust screen counts.
    let newest = shown.lines().rfind(|l| l.contains("No, exit"));
    if newest.is_some_and(|l| l.contains('\u{276f}')) {
        DOWN
    } else {
        "\r"
    }
}

/// What a terminal drew, as text: its escape sequences and control
/// characters dropped, but for newlines and tabs.
fn plain(drawn: &str) -> String {
    let mut out = String::new();
    let mut chars = drawn.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte from @ to ~. A cursor
                // move starts a row, so the screen's rows don't run together.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            if matches!(c, 'H' | 'f') && !out.ends_with('\n') {
                                out.push('\n');
                            }
                            break;
                        }
                    }
                }
                // OSC: up to BEL, or ESC and a backslash.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // A character set, like ESC ( B.
                Some('(' | ')') => {
                    chars.next();
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

// Claude Code 2.1's trust screen, as tmux captured it on 2026-11-08.
const TRUST: &str = " Quick safety check: Is this a project you created or one you trust?
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 \u{276f} No, exit
   Yes, I trust this folder
 Enter to confirm \u{b7} Esc to cancel
";

#[test]
fn the_trust_screen_gets_down_until_yes_is_picked() {
    assert_eq!(key_for(TRUST), "\x1b[B");
    let yes = TRUST
        .replace(" \u{276f} No, exit", "   No, exit")
        .replace("   Yes, I trust", " \u{276f} Yes, I trust");
    assert_eq!(key_for(&yes), "\r");
    assert_eq!(key_for(" Press Enter to continue\n"), "\r");
    // An old frame in the scrollback, above the newest one on Yes.
    assert_eq!(key_for(&format!("{TRUST}{yes}")), "\r");
}

#[test]
fn plain_drops_a_terminal_s_escapes() {
    let drawn = "\x1b[2J\x1b[1;1HTrust?\x1b]10;?\x1b\\ \x1b]11;rgb:1e/1e/2e\x07No,\x1b(B exit\r\n";
    assert_eq!(plain(drawn), "\nTrust? No, exit\n");
    assert_eq!(plain("a\x1b[2;1Hb\x1b[3;1H"), "a\nb\n");
}

/// The state in a `runner status` line: its last word before any creds.
fn state_in(status: &str) -> &str {
    let line = status.trim_end();
    let line = line
        .split_once("  creds ")
        .map_or(line, |(before, _)| before);
    line.rsplit("  ").next().unwrap_or_default()
}

#[test]
fn state_in_reads_past_the_creds() {
    let line = "dev  gen 0  21474836480 bytes  cpus 2  mem 2G  sleeping  creds claude\n";
    assert_eq!(state_in(line), "sleeping");
    assert_eq!(
        state_in("dev  gen 1  10 bytes  cpus 2  mem 2G  stopped\n"),
        "stopped"
    );
}

/// Polls `check` for up to a minute.
fn eventually(what: &str, check: impl FnMut() -> bool) {
    eventually_within(what, Duration::from_secs(60), check);
}

/// `eventually`, polling for up to `wait`.
fn eventually_within(what: &str, wait: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + wait;
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

// An old VM onto the current base: one forked from a copy of base whose
// runner-guest has a byte on its end (it still runs) and that lacks Claude
// Code's managed settings gets base's of both from `runner rebase`, and
// keeps its files, their owner, and its service. undo brings the old guest
// back.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn an_old_vm_rebased_onto_base_gets_its_guest_and_keeps_its_home() {
    use std::io::Write;
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let (old, name) = (
        format!("rebase-base-{}", std::process::id()),
        format!("rebase-test-{}", std::process::id()),
    );
    let store = runner::store::Store::new(&root);
    let base = store.get("base").unwrap().generation;
    let base_disk = store.disk("base", base).unwrap();
    // Where the root's owner can read the old base to import it.
    let scratch = tempfile::tempdir_in("/var/tmp").unwrap();
    std::fs::set_permissions(scratch.path(), Permissions::from_mode(0o755)).unwrap();
    let (disk, guest) = (
        scratch.path().join("old-base.ext4"),
        scratch.path().join("runner-guest"),
    );
    let done = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{cmd:?}: {err}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let sum_of = |path: &Path| {
        let said = done(Command::new("sha256sum").arg(path));
        said.split_whitespace().next().unwrap().to_string()
    };
    done(
        Command::new("cp")
            .args(["--reflink=auto", "--sparse=always", "--"])
            .args([&base_disk, &disk]),
    );
    let dump = format!("dump /usr/local/bin/runner-guest {}", guest.display());
    done(
        Command::new(runner::host::DEBUGFS)
            .args(["-R", &dump])
            .arg(&disk),
    );
    let new_guest = sum_of(&guest);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&guest)
        .unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let old_guest = sum_of(&guest);
    common::debugfs(
        &disk,
        &format!(
            "rm /usr/local/bin/runner-guest\nwrite {} /usr/local/bin/runner-guest\n\
             sif /usr/local/bin/runner-guest mode 0100755\n\
             sif /usr/local/bin/runner-guest uid 0\nsif /usr/local/bin/runner-guest gid 0\n\
             rm /etc/claude-code/managed-settings.json\n",
            guest.display()
        ),
    );
    std::fs::set_permissions(&disk, Permissions::from_mode(0o644)).unwrap();
    ok(runner(&root, &["import", &old, disk.to_str().unwrap()]));
    let _old_cleanup = Cleanup {
        root: root.clone(),
        name: old.clone(),
    };
    ok(runner(&root, &["create", &name, "--from", &old]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    let guest_sum = || {
        let said = sh("sha256sum /usr/local/bin/runner-guest");
        said.split_whitespace().next().unwrap().to_string()
    };
    let settings = "test -f /etc/claude-code/managed-settings.json && echo there || echo gone";
    let address = ok(runner(&root, &["ip", &name])).trim().to_string();
    let answers = || http_status(&address, 8080).is_some_and(|l| l.ends_with(" 200 OK"));

    // The old guest, booted.
    assert_eq!(guest_sum(), old_guest);
    assert_eq!(sh(settings), "gone\n");
    sh("echo kept > /home/agent/work/kept.txt");
    let mut add = vec!["service", "add", &name, "web", "--"];
    add.extend(["python3", "-m", "http.server", "8080"]);
    ok(runner(&root, &add));
    eventually("the service to answer", answers);
    ok(runner(&root, &["stop", &name]));

    let said = ok(runner(&root, &["rebase", &name]));
    assert!(
        said.starts_with(&format!("{name} is on base's generation {base} now: ")),
        "{said}"
    );
    assert!(
        said.contains("Kept /home, /root, /etc/machine-id and 1 service (web).\n"),
        "{said}"
    );
    assert_eq!(guest_sum(), new_guest);
    assert_eq!(sh(settings), "there\n");
    assert_eq!(sh("cat /home/agent/work/kept.txt"), "kept\n");
    assert_eq!(sh("stat -c %U /home/agent/work/kept.txt"), "agent\n");
    eventually("the service to answer after the rebase", answers);

    // The boot after the rebase committed a generation of its own, so two
    // undos go back to before it.
    ok(runner(&root, &["stop", &name]));
    ok(runner(&root, &["undo", &name]));
    ok(runner(&root, &["undo", &name]));
    assert_eq!(guest_sum(), old_guest);
    assert_eq!(sh("cat /home/agent/work/kept.txt"), "kept\n");
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

// What a GET of / at the address gets back in full: its status line and its
// body.
fn http_get(address: &str, port: u16) -> Option<(String, String)> {
    use std::io::{Read, Write};
    let to = std::net::SocketAddr::new(address.parse().ok()?, port);
    let mut conn = std::net::TcpStream::connect_timeout(&to, Duration::from_secs(2)).ok()?;
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    conn.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
    let mut response = String::new();
    conn.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    Some((head.lines().next()?.to_string(), body.to_string()))
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

// Exported and imported under another name on the same host, as a move to
// another host would: the copy boots with the file, and its service answers
// at the copy's own address.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn an_imported_export_boots_with_its_files_and_services() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("export-test-{}", std::process::id());
    let copy = format!("import-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let _copy_cleanup = Cleanup {
        root: root.clone(),
        name: copy.clone(),
    };
    let write = "echo kept > /home/agent/work/kept.txt";
    ok(runner(&root, &["exec", &name, "--", "sh", "-c", write]));
    let mut add = vec!["service", "add", &name, "web", "--"];
    add.extend(["python3", "-m", "http.server", "8080"]);
    ok(runner(&root, &add));
    let address = ok(runner(&root, &["ip", &name])).trim().to_string();
    let answers = |at: &str| http_status(at, 8080).is_some_and(|l| l.ends_with(" 200 OK"));
    eventually("the service to answer", || answers(&address));
    ok(runner(&root, &["stop", &name]));

    // Through a file root writes and reads, as the laptop's pipe would carry it.
    let tar = tempfile::NamedTempFile::new_in("/var/tmp").unwrap();
    let exported = cli(&root)
        .args(["export", &name])
        .stdout(tar.reopen().unwrap())
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let imported = cli(&root)
        .args(["import", &copy, "-"])
        .stdin(tar.reopen().unwrap())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&imported.stderr);
    assert!(imported.status.success(), "{said}");
    assert!(
        said.contains(&format!("imported {name}'s export as {copy}")),
        "{said}"
    );

    let there = ok(runner(&root, &["ip", &copy])).trim().to_string();
    assert_ne!(there, address);
    let kept = ok(runner(
        &root,
        &["exec", &copy, "--", "cat", "/home/agent/work/kept.txt"],
    ));
    assert_eq!(kept, "kept\n");
    eventually("the copy's service to answer at its address", || {
        answers(&there)
    });
    assert!(!answers(&address), "the original is up");
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

// The host drops what a VM opens to it, but the VM still has the internet
// through the host, and DNS from its boot, on its gateway.
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
    // The kernel's /proc/net/pnp, from the boot argument.
    let resolv = ok(runner(
        &root,
        &["exec", &name, "--", "cat", "/etc/resolv.conf"],
    ));
    let servers: Vec<&str> = resolv
        .lines()
        .filter_map(|line| line.strip_prefix("nameserver "))
        .collect();
    assert_eq!(servers, [gateway.as_str()], "{resolv}");
    // Which looks up example.com.
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

// A list made on a running VM holds it to its names within a second, and
// the names under them: other names aren't answered, an address it didn't
// look up is dropped, and so is another resolver. A name taken off goes the
// same way, and dropping the list opens it again, with no reboot.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and the internet"]
fn a_listed_vm_reaches_only_its_names() {
    use std::net::ToSocketAddrs;
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("net-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let in_vm = |argv: &[&str]| {
        let exec = ["exec", name.as_str(), "--"];
        let all: Vec<&str> = exec.iter().chain(argv).copied().collect();
        runner(&root, &all)
    };
    // curl exits 6 when it can't resolve the host.
    let curl = |url: &str| in_vm(&["curl", "-fsS", "-m", "20", "-o", "/dev/null", url]);
    // Dropped, so it times out; refused or answered would be at once.
    let connect = "import socket, sys
try:
    socket.create_connection((sys.argv[1], 443), 5)
    print('connected')
except TimeoutError:
    print('timed out')
except OSError as e:
    print(e)
";
    // One plain question for example.com's A record, straight to a resolver.
    let ask = "import socket, sys
q = bytes.fromhex('123401000001000000000000076578616d706c6503636f6d0000010001')
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(5)
s.sendto(q, (sys.argv[1], 53))
try:
    s.recvfrom(512)
    print('answered')
except TimeoutError:
    print('no answer')
";
    let boot_id = ["cat", "/proc/sys/kernel/random/boot_id"];
    let booted = ok(in_vm(&boot_id));
    // Open, it reaches all of it: so what fails below, the list does.
    ok(curl("https://example.com"));
    assert_eq!(ok(in_vm(&["python3", "-c", ask, "1.1.1.1"])), "answered\n");
    // Looked up here, on the host, so never by the VM's own DNS.
    let example = ("example.com", 443)
        .to_socket_addrs()
        .unwrap()
        .find(std::net::SocketAddr::is_ipv4)
        .expect("example.com has an IPv4 address")
        .ip()
        .to_string();

    let set = ok(runner(&root, &["net", &name, "allow", "github.com"]));
    assert_eq!(
        set,
        format!("{name}  allows github.com, and always api.anthropic.com\n")
    );
    eventually("example.com to stop resolving", || {
        curl("https://example.com").status.code() == Some(6)
    });
    ok(curl("https://github.com"));
    // A name under one on the list is on it too.
    ok(curl("https://api.github.com"));
    let reached = ok(in_vm(&["python3", "-c", connect, &example]));
    assert_eq!(reached, "timed out\n", "{example}:443");
    assert_eq!(ok(in_vm(&["python3", "-c", ask, "1.1.1.1"])), "no answer\n");

    // Taken off while it runs, github.com goes as example.com did.
    let only = ok(runner(&root, &["net", &name, "deny", "github.com"]));
    assert_eq!(only, format!("{name}  allows only api.anthropic.com\n"));
    eventually("github.com to stop resolving", || {
        curl("https://github.com").status.code() == Some(6)
    });

    let open = ok(runner(&root, &["net", &name, "open"]));
    assert_eq!(open, format!("{name}  open: the whole internet\n"));
    eventually("example.com to answer again", || {
        curl("https://example.com").status.success()
    });
    assert_eq!(ok(in_vm(&boot_id)), booted, "it rebooted");
}

// Claude Code works on a VM whose list has nothing on it but what every
// list has: a real `claude -p` edits a file and exits 0.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and its token"]
fn a_headless_run_on_a_listed_vm_reaches_claude_code_s_api() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("net-run-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    ok(runner(&root, &["net", &name, "allow", "github.com"]));
    let only = ok(runner(&root, &["net", &name, "deny", "github.com"]));
    assert_eq!(only, format!("{name}  allows only api.anthropic.com\n"));

    let prompt = "Create a file named hello.txt in the current directory holding \
                  exactly the text hi, then stop.";
    let id = ok(runner(&root, &["run", &name, prompt]));
    // Returns once the run is over, its VM shut down and all.
    let stream = ok(runner(&root, &["output", id.trim(), "--follow"]));
    let rows = ok(runner(&root, &["runs", &name]));
    assert!(rows.contains("  exited 0, committed  "), "{rows}{stream}");
    let hello = ok(runner(
        &root,
        &["exec", &name, "--", "cat", "/home/agent/work/hello.txt"],
    ));
    assert_eq!(hello.trim(), "hi");
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

// Claude Code keeps the version base was built with: the image's managed
// settings turn its updater off, and `claude doctor` says what it sees. No
// token needed: doctor doesn't log in.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn claude_code_in_a_vm_has_its_updater_off() {
    const OFF: &str = "Auto-updates: disabled";
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("updater-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let in_vm = |argv: &[&str]| {
        let exec = ["exec", name.as_str(), "--"];
        let all: Vec<&str> = exec.iter().chain(argv).copied().collect();
        ok(runner(&root, &all))
    };

    // As a run starts it: from runner-guest, with no terminal and systemd's
    // environment, as exec does.
    let doctor = in_vm(&["claude", "doctor"]);
    assert!(doctor.contains(OFF), "{doctor}");

    // As `runner claude` does, in its tmux session. That closes when doctor
    // exits, unless the agent's tmux server keeps a pane past its command.
    let keep = "tmux new-session -d -s keep && tmux set -g remain-on-exit on";
    in_vm(&["sh", "-c", keep]);
    let t = common::pty(50, 120);
    let mut cmd = cli(&root);
    cmd.env_remove("TERM")
        .args(["claude", &name, "--", "doctor"]);
    let mut claude = t.spawn(cmd);
    // Empty until `runner claude` has made the session.
    let capture = ["exec", &name, "--", "tmux", "capture-pane", "-p", "-S", "-"];
    let screen = || {
        let out = runner(&root, &[&capture[..], &["-t", "=claude:"]].concat());
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    eventually("claude doctor's report", || {
        screen().contains("Auto-updates:")
    });
    let shown = screen();
    assert!(shown.contains(OFF), "{shown}");
    in_vm(&["tmux", "detach-client", "-s", "=claude"]);
    assert_eq!(claude.wait().unwrap().code(), Some(0), "{}", t.seen());
    in_vm(&["tmux", "kill-server"]);
}

// Docker in a fork of base, as agent with no sudo, as exec runs commands
// right after `runner create`: dockerd runs on the guest kernel, pulls from
// Docker Hub, and starts a container on its bridge.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and the internet"]
fn agent_runs_docker_in_a_vm_without_sudo() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("docker-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let in_vm = |argv: &[&str]| {
        let exec = ["exec", name.as_str(), "--"];
        let all: Vec<&str> = exec.iter().chain(argv).copied().collect();
        ok(runner(&root, &all))
    };

    let who = in_vm(&["id", "-un"]);
    assert_eq!(who.trim(), "agent");
    let hello = in_vm(&["docker", "run", "--rm", "hello-world"]);
    assert!(hello.contains("Hello from Docker!"), "{hello}");
    // Ubuntu's build prints its version without upstream's "v".
    let compose = in_vm(&["docker", "compose", "version", "--short"]);
    let version = compose.trim().trim_start_matches('v');
    assert!(version.starts_with("2."), "{compose}");
}

// A compose stack in a fork of base behaves like the VM's own services: its
// web container, brought up by agent with no sudo, answers on the VM's
// address. Asleep and woken, the same container answers, in the same
// process. Stopped and started, its restart policy brings it back once
// dockerd is up, which is after runner-guest.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and the internet"]
fn a_compose_stack_lives_through_sleep_and_comes_back_at_boot() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("compose-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let dir = "/home/agent/work/web";
    let docker = |args: &[&str]| {
        let exec = ["exec", name.as_str(), "-d", dir, "--", "docker"];
        let all: Vec<&str> = exec.iter().chain(args).copied().collect();
        ok(runner(&root, &all))
    };
    // Whether it runs, its process, and when that started, as dockerd has it.
    let state = |id: &str| {
        let format = "{{.State.Status}} {{.State.Pid}} {{.State.StartedAt}}";
        docker(&["inspect", "-f", format, id]).trim().to_string()
    };
    let address = ok(runner(&root, &["ip", &name])).trim().to_string();
    // httpd serves the container's hostname, its id's first 12 characters, so
    // a GET says which container answered.
    let answers = |id: &str| {
        http_get(&address, 8080)
            .is_some_and(|(status, body)| status.ends_with(" 200 OK") && body.trim() == &id[..12])
    };

    // A container's PID 1 ignores a SIGTERM it has no handler for, so sh
    // traps it: the stop at shutdown ends it at once, not 10 seconds on.
    let serve = "mkdir -p /www; hostname >/www/index.html; \
                 trap 'exit 0' TERM; httpd -f -p 8080 -h /www & wait";
    let command = format!("    command: [sh, -c, \"{serve}\"]");
    let compose = [
        "services:",
        "  web:",
        "    image: busybox:1.37",
        "    restart: unless-stopped",
        "    ports: [\"8080:8080\"]",
        &command,
        "",
    ]
    .join("\n");
    // Boots it.
    let put = "mkdir -p \"$1\" && printf %s \"$2\" > \"$1/compose.yaml\"";
    ok(runner(
        &root,
        &["exec", &name, "--", "sh", "-c", put, "sh", dir, &compose],
    ));
    docker(&["compose", "up", "-d"]);
    let id = docker(&["compose", "ps", "-q", "web"]).trim().to_string();
    assert_eq!(id.len(), 64, "not one container's id: {id:?}");
    eventually("the container to answer", || answers(&id));
    let before = state(&id);
    assert!(before.starts_with("running "), "{before}");

    let slept = ok(runner(&root, &["sleep", &name]));
    assert!(slept.contains("  sleeping"), "{slept}");
    let woke = ok(runner(&root, &["wake", &name]));
    assert!(woke.contains("  running"), "{woke}");
    eventually("the container to answer after the wake", || answers(&id));
    assert_eq!(state(&id), before);

    ok(runner(&root, &["stop", &name]));
    assert!(!answers(&id));
    let began = Instant::now();
    ok(runner(&root, &["start", &name]));
    let wait = Duration::from_secs(120);
    eventually_within(
        "the container to answer after a stop and start",
        wait,
        || answers(&id),
    );
    let took = began.elapsed();
    // The same container, started again: a new start time.
    let after = state(&id);
    assert!(after.starts_with("running "), "{after}");
    assert_ne!(
        after.rsplit(' ').next(),
        before.rsplit(' ').next(),
        "{after}"
    );
    eprintln!("runner start to the container's answer took {took:?}");
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
    // Claude Code's own init event names a UUID session, which runs shows
    // before the prompt.
    let row = rows
        .lines()
        .find(|r| r.starts_with(&format!("{}  ", id.trim())));
    let row = row.unwrap_or_else(|| panic!("{rows}"));
    let (before, _) = row.split_once("  \"").unwrap();
    let session = before.rsplit("  ").next().unwrap();
    let uuid = session.len() == 36 && session.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit());
    assert!(uuid, "no session in {row}");
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

// A run that's going, taken over on a terminal: its claude -p stops, and its
// session goes on in the VM's claude session, the prompt it was given and
// all. The VM the run booted stays up, and its next clean shutdown commits
// the generation the run booted.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and its token"]
fn a_run_taken_over_carries_on_in_claude_s_session() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("takeover-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let prompt = format!("Run {LONG_PROMPT} and wait for it, then say done.");
    let id = ok(runner(&root, &["run", &name, &prompt]));
    let id = id.trim().to_string();
    let row = || {
        let rows = ok(runner(&root, &["runs", &name]));
        let row = rows.lines().find(|r| r.starts_with(&format!("{id}  ")));
        row.unwrap_or_else(|| panic!("{rows}")).to_string()
    };
    // The column before the prompt, once it's a UUID.
    let session = |row: &str| {
        let (before, _) = row.split_once("  \"")?;
        let s = before.rsplit("  ").next()?;
        let uuid = s.len() == 36 && s.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit());
        uuid.then(|| s.to_string())
    };
    eventually("the run's session in runs", || session(&row()).is_some());
    let uuid = session(&row()).unwrap();
    let in_vm = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    // Claude Code has written the prompt to its session by the time it runs
    // the tool.
    eventually("claude -p to run tail", || {
        let tailing = r#"for c in /proc/[0-9]*/cmdline; do
            [ "$(tr '\0' ' ' <"$c" 2>/dev/null)" = 'tail -f /dev/null ' ] && echo found
        done; true"#;
        in_vm(tailing).contains("found")
    });

    let t = common::pty(50, 160);
    let mut cmd = cli(&root);
    cmd.env_remove("TERM").args(["claude", "--run", &id]);
    let mut taking = t.spawn(cmd);
    eventually("the takeover", || {
        t.seen().contains(&format!("{id} is taken over"))
    });
    let capture = [
        "exec",
        &name,
        "--",
        "tmux",
        "capture-pane",
        "-p",
        "-S",
        "-",
        "-t",
        "=claude:",
    ];
    let screen = || String::from_utf8_lossy(&runner(&root, &capture).stdout).into_owned();
    // A fork of base has no ~/.claude.json, so an interactive claude shows
    // its first-run screens, and then may ask to trust the directory: Enter
    // takes each one's default. At the prompt, an empty Enter does nothing.
    enter_to_the_prompt(&t, &root, &capture);
    let shown = screen();
    assert!(!shown.contains("No conversation found"), "{shown}");
    // As tmux started it, whatever claude makes of its own argv.
    let pane = [
        "exec",
        &name,
        "--",
        "tmux",
        "display",
        "-p",
        "-t",
        "=claude:",
        "#{pane_start_command}|#{pane_current_path}",
    ];
    let ran = ok(runner(&root, &pane));
    assert!(ran.contains("--resume"), "{ran}");
    assert!(ran.contains(&uuid), "{ran}");
    assert!(ran.ends_with("|/home/agent/work\n"), "{ran}");
    let taken = row();
    assert!(taken.contains("Z  taken over  "), "{taken}");
    let result = ok(runner(&root, &["result", &id]));
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    let booted = result["generation"].as_u64().expect("the run's generation");

    in_vm("tmux detach-client -s =claude");
    assert_eq!(taking.wait().unwrap().code(), Some(0), "{}", t.seen());
    ok(runner(&root, &["stop", &name]));
    let status = ok(runner(&root, &["status", &name]));
    assert!(status.ends_with("  stopped\n"), "{status}");
    assert!(status.contains(&format!("  gen {booted}  ")), "{status}");
}

// A run that's over, taken over: one cancelled once it has written
// work/started.txt rolls back, so the takeover puts a copy of its disk on
// top of the VM's history and boots onto it, and the session goes on there,
// the prompt it was given and all.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, a host set up with its base, and its token"]
fn a_cancelled_run_taken_over_gets_its_disk_and_session_back() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("pickup-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let prompt = format!(
        "Create a file named started.txt in the current directory holding exactly the \
         text started. Then run {LONG_PROMPT} and wait for it, then say done."
    );
    let id = ok(runner(&root, &["run", &name, &prompt]));
    let id = id.trim().to_string();
    let row = || {
        let rows = ok(runner(&root, &["runs", &name]));
        let row = rows.lines().find(|r| r.starts_with(&format!("{id}  ")));
        row.unwrap_or_else(|| panic!("{rows}")).to_string()
    };
    let in_vm = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    // A wait each for the boot and Claude Code's start, then its two turns.
    eventually("the run's session in runs", || {
        !row().contains("Z  running  -  ")
    });
    // The file is there, and Claude Code has written the prompt to its
    // session, by the time it runs tail.
    eventually("claude -p to write started.txt", || {
        in_vm("[ -f /home/agent/work/started.txt ] && echo found || true").contains("found")
    });
    eventually("claude -p to run tail", || {
        let tailing = r#"for c in /proc/[0-9]*/cmdline; do
            [ "$(tr '\0' ' ' <"$c" 2>/dev/null)" = 'tail -f /dev/null ' ] && echo found
        done; true"#;
        in_vm(tailing).contains("found")
    });
    // It can still be measuring the run's disk when cancel gives up waiting.
    runner(&root, &["cancel", &id]);
    eventually("the run to end rolled back", || {
        row().contains("Z  cancelled, rolled back  ")
    });
    let result = ok(runner(&root, &["result", &id]));
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    let booted = result["generation"].as_u64().expect("the run's generation");

    let t = common::pty(50, 160);
    let mut cmd = cli(&root);
    cmd.env_remove("TERM").args(["claude", "--run", &id]);
    let mut taking = t.spawn(cmd);
    eventually("the takeover", || {
        t.seen().contains(&format!("{id}'s session goes on"))
    });
    let seen = t.seen();
    let put = format!("put {id}'s disk, generation {booted}, back on {name} as generation ");
    assert!(seen.contains(&put), "{seen}");
    let capture = [
        "exec",
        &name,
        "--",
        "tmux",
        "capture-pane",
        "-p",
        "-S",
        "-",
        "-t",
        "=claude:",
    ];
    let screen = || String::from_utf8_lossy(&runner(&root, &capture).stdout).into_owned();
    // Enter through an interactive claude's first-run screens, as in
    // a_run_taken_over_carries_on_in_claude_s_session.
    enter_to_the_prompt(&t, &root, &capture);
    let shown = screen();
    assert!(!shown.contains("No conversation found"), "{shown}");
    // The run's disk, put back: dev's own from before the run never had it.
    let started = in_vm("cat /home/agent/work/started.txt");
    assert_eq!(started.trim(), "started");
    let listed = ok(runner(&root, &["checkpoints", &name]));
    assert!(
        listed.contains(&format!("  \"takeover of {id}\"")),
        "{listed}"
    );

    in_vm("tmux detach-client -s =claude");
    assert_eq!(taking.wait().unwrap().code(), Some(0), "{}", t.seen());
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

// A disk grown while the VM is stopped is that big in the guest at its next
// boot, with what it held: 10 GiB past base's, which is 30 GiB for a base
// built as it comes, whatever size base was built at.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_resized_disk_is_that_big_in_the_guest() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("resize-test-{}", std::process::id());
    // "<name>  gen 0  <bytes> bytes"
    let made = ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    let bytes: u64 = made.split_whitespace().nth(3).unwrap().parse().unwrap();
    let gib = bytes.div_ceil(1 << 30) + 10;

    sh("echo kept > ~/before-resize");
    ok(runner(&root, &["stop", &name]));
    let disk = format!("{gib}G");
    let said = ok(runner(&root, &["resize", &name, "--disk", &disk]));
    assert!(said.contains(&format!("  {} bytes  ", gib << 30)), "{said}");
    let size = sh("df -B1 --output=size / | tail -n 1");
    let size: u64 = size
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("df said {size:?}"));
    assert!(
        size > (gib - 1) << 30,
        "/ is {size} bytes, want about {gib} GiB"
    );
    assert_eq!(sh("cat ~/before-resize"), "kept\n");
}

// A running VM's export, through `tar -xO` into an import under another
// name: the VM carries on, its process too, and the copy boots with what it
// wrote a moment before, never synced by hand: the export syncs it.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_running_vm_s_export_boots_elsewhere_and_it_carries_on() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("export-test-{}", std::process::id());
    let copy = format!("export-copy-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let _copy = Cleanup {
        root: root.clone(),
        name: copy.clone(),
    };
    let sh = |vm: &str, script: &str| ok(runner(&root, &["exec", vm, "--", "sh", "-c", script]));

    // Boots it. In a session of its own, so the exec's end doesn't end it.
    let pid = sh(
        &name,
        "setsid sleep 1000 </dev/null >/dev/null 2>&1 & echo $!",
    );
    let pid = pid.trim().to_string();
    sh(&name, "echo kept > ~/exported");

    let began = Instant::now();
    let mut export = cli(&root)
        .args(["export", &name, "--no-checkpoints"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut tar = Command::new("tar")
        .args(["-xOf", "-", &format!("{name}/disk.ext4")])
        .stdin(export.stdout.take().unwrap())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let import = cli(&root)
        .args(["import", &copy, "-"])
        .stdin(tar.stdout.take().unwrap())
        .output()
        .unwrap();
    let exported = export.wait_with_output().unwrap();
    let took = began.elapsed();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(tar.wait().unwrap().success());
    ok(import);

    let status = ok(runner(&root, &["status", &name]));
    assert!(status.contains("  running"), "{status}");
    assert_eq!(sh(&name, &format!("cat /proc/{pid}/comm")), "sleep\n");
    assert_eq!(sh(&copy, "cat ~/exported"), "kept\n");
    eprintln!("export of a running VM, through tar into an import, took {took:?}");
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

// What a host stop does to a sleeping VM, without stopping the host: parked,
// as runner-park.service parks it, its boot ends and its unit stops, and its
// next start resumes it, every process and session where it was.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn a_sleeping_vm_lives_through_a_host_stop() {
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("park-test-{}", std::process::id());
    ok(runner(&root, &["create", &name]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let sh = |script: &str| ok(runner(&root, &["exec", &name, "--", "sh", "-c", script]));
    let unit = format!("firecracker@{name}.service");
    let active = || {
        let out = Command::new("systemctl")
            .args(["is-active", &unit])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let pid = sh("setsid sleep 1000 </dev/null >/dev/null 2>&1 & echo $!");
    let pid = pid.trim().to_string();
    sh("tmux new-session -d -s parked 'sleep 1000'");
    ok(runner(&root, &["sleep", &name]));
    let parked = ok(runner(&root, &["park", &name]));
    assert_eq!(parked, format!("{name}  parked\n"));
    eventually("its unit to stop", || active() == "inactive");
    // A sleeping or running VM's status ends with the creds its guest took.
    let status = ok(runner(&root, &["status", &name]));
    assert_eq!(state_in(&status), "sleeping", "{status}");

    let began = Instant::now();
    let started = ok(runner(&root, &["start", &name]));
    let took = began.elapsed();
    assert_eq!(state_in(&started), "running", "{started}");
    assert_eq!(sh(&format!("cat /proc/{pid}/comm")), "sleep\n");
    let sessions = ok(runner(&root, &["sessions", &name]));
    assert!(
        sessions.lines().any(|l| l.starts_with("parked  sleep  ")),
        "{sessions}"
    );
    let logs = ok(runner(&root, &["logs", &name]));
    let notes: Vec<&str> = logs.lines().filter(|l| l.contains(" resumed ")).collect();
    assert_eq!(notes.len(), 1, "{logs}");
    eprintln!("runner start of a parked VM took {took:?}; boot's note: {notes:?}");
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

// The laptop's `runner proxy` (client/runner), run on this host: its ssh runs
// each command here as the root's owner, as the host's sshd runs it for the
// runner user. A tunnel's command gets a session of its own, with its stdout
// read by ssh, so that end closes when ssh dies, as sshd's does when the
// connection goes. The forwards ssh is asked for go in FORWARDS, for the test
// to make.
fn proxy_laptop(root: &Path) -> tempfile::TempDir {
    let laptop = tempfile::tempdir().unwrap();
    let bin = laptop.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    common::script(
        &bin.join("ssh"),
        r#"for cmd; do :; done
for arg; do
  case $arg in localhost:*) printf '%s\n' "${arg#localhost:}" >>"$FORWARDS" ;; esac
done
for arg; do
  if [ "$arg" = -L ]; then
    rm -f "$CONN" && mkfifo "$CONN" || exit 255
    setsid -w sh -c "$cmd" >"$CONN" </dev/null &
    while read -r _; do :; done <"$CONN"
    wait $!
    exit
  fi
done
exec sh -c "$cmd"
"#,
    );
    // The client asks the aws.sh beside it about a host it can't reach: this
    // AWS CLI takes nothing, so the test never reaches a real account.
    common::script(
        &bin.join("aws"),
        "echo 'aws: no AWS in tests' >&2\nexit 1\n",
    );
    let cmd = cli(root);
    let quote = |arg: &std::ffi::OsStr| {
        let arg = arg.to_str().unwrap();
        assert!(!arg.contains('\''), "{arg}");
        format!("'{arg}'")
    };
    let mut line = format!("cd / || exit\nRUNNER_ROOT={} exec", quote(root.as_os_str()));
    for arg in std::iter::once(cmd.get_program()).chain(cmd.get_args()) {
        line = format!("{line} {}", quote(arg));
    }
    common::script(&bin.join("runner"), &format!("{line} \"$@\"\n"));
    laptop
}

// Forwards each connection `listener` takes to `to`: the test's stand-in for
// the -L the proxy asked ssh for, since the stand-in ssh forwards nothing.
fn forward(listener: std::net::TcpListener, to: std::net::SocketAddr) {
    use std::net::{Shutdown, TcpStream};
    let pipe = |mut from: TcpStream, mut to: TcpStream| {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut from, &mut to);
            let _ = to.shutdown(Shutdown::Write);
        });
    };
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            if let Ok(back) = TcpStream::connect(to) {
                pipe(conn.try_clone().unwrap(), back.try_clone().unwrap());
                pipe(back, conn);
            }
        }
    });
}

// A proxy a failed test left open still ends, before Cleanup's stop. One
// that has ended is left be: its group's id may be another's by then.
struct Proxy(Child);

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let group = nix::unistd::Pid::from_raw(self.0.id() as i32);
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::SIGKILL);
        }
    }
}

// An open proxy keeps its VM awake past its idle time, and the VM answers
// at the forward the proxy asked for; once Ctrl-C ends the proxy, the idle
// time starts again and the VM sleeps.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn an_open_proxy_keeps_its_vm_awake() {
    use std::os::unix::process::CommandExt;
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("proxy-test-{}", std::process::id());
    ok(runner(&root, &["create", &name, "--idle", "90s"]));
    let _cleanup = Cleanup {
        root: root.clone(),
        name: name.clone(),
    };
    let status = || ok(runner(&root, &["status", &name]));
    // A service alone lets the VM sleep, so what keeps it up is the proxy.
    let web = ["python3", "-m", "http.server", "8080"];
    let mut add = vec!["service", "add", &name, "web", "--"];
    add.extend(web);
    ok(runner(&root, &add));
    let address = ok(runner(&root, &["ip", &name])).trim().to_string();
    eventually("the service to answer", || {
        http_status(&address, 8080).is_some_and(|l| l.ends_with(" 200 OK"))
    });

    let laptop = proxy_laptop(&root);
    let forwards = laptop.path().join("forwards");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap().port();
    let path = format!(
        "{}:{}",
        laptop.path().join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let client = Path::new(env!("CARGO_MANIFEST_DIR")).join("client/runner");
    let mut proxy = Proxy(
        Command::new("sh")
            .arg(client)
            .args(["proxy", &name, &format!("{local}:8080")])
            .env("PATH", path)
            .env("RUNNER_HOST", "runner@this-host")
            .env("FORWARDS", &forwards)
            .env("CONN", laptop.path().join("conn"))
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let group = nix::unistd::Pid::from_raw(proxy.0.id() as i32);
    eventually("the proxy's forward", || {
        std::fs::read_to_string(&forwards).is_ok_and(|f| !f.is_empty())
    });
    let asked = std::fs::read_to_string(&forwards).unwrap();
    assert_eq!(asked, format!("{local}:{address}:8080\n"));
    forward(listener, format!("{address}:8080").parse().unwrap());

    // Its idle time, a look's 10 seconds and then some, still running.
    let opened = Instant::now();
    while opened.elapsed() < Duration::from_secs(120) {
        assert!(proxy.0.try_wait().unwrap().is_none(), "the proxy ended");
        let now = status();
        assert!(
            now.contains("  running"),
            "slept under the proxy after {:?}: {now}",
            opened.elapsed()
        );
        std::thread::sleep(Duration::from_secs(5));
    }
    let through = http_status("127.0.0.1", local);
    assert!(
        through.as_deref().is_some_and(|l| l.ends_with(" 200 OK")),
        "{through:?}"
    );

    // Ctrl-C, as a terminal sends it to its job.
    nix::sys::signal::killpg(group, nix::sys::signal::SIGINT).unwrap();
    proxy.0.wait().unwrap();
    let ended = Instant::now();
    while !status().contains("  sleeping") {
        assert!(
            ended.elapsed() < Duration::from_secs(120),
            "still awake {:?} after the proxy ended: {}",
            ended.elapsed(),
            status()
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    let slept = ended.elapsed();
    // Its idle time started again as the proxy ended, not when it opened.
    assert!(slept >= Duration::from_secs(85), "slept after {slept:?}");
    eprintln!("slept {slept:?} after the proxy ended");
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

// Best of two under the unit, with no token: RUNNER_CLAUDE is a script put
// in dev that writes the VM's hostname, its name, to work/who.txt and exits 0
// only in the fork ending -2. Both tries boot at once; the first rolls back,
// the second commits, and keep puts its disk on dev and deletes both.
#[test]
#[ignore = "boots a VM: needs /dev/kvm and a host set up with its base"]
fn best_of_two_keeps_the_winner_on_dev() {
    const SESSION: &str = "4e1f7a2b-9c3d-4e5f-8a6b-7c8d9e0f1a2b";
    let root = std::env::var_os("RUNNER_ROOT").map_or("/var/lib/runner".into(), PathBuf::from);
    let name = format!("keep-test-{}", std::process::id());
    let tries = [format!("{name}-1"), format!("{name}-2")];
    ok(runner(&root, &["create", &name]));
    // Each try too, should the keep not delete it.
    let _cleanup = [&name, &tries[0], &tries[1]].map(|n| Cleanup {
        root: root.clone(),
        name: n.clone(),
    });
    let init = format!(r#"echo '{{"type":"system","subtype":"init","session_id":"{SESSION}"}}'"#);
    let claude = [
        "#!/bin/sh",
        "cat >/dev/null",
        &init,
        "me=$(cat /proc/sys/kernel/hostname)",
        "echo \"$me\" > who.txt && sync",
        "case $me in *-2) exit 0 ;; esac",
        "exit 1\n",
    ]
    .join("\n");
    let put = "printf %s \"$1\" > ~/claude && chmod 755 ~/claude";
    ok(runner(
        &root,
        &["exec", &name, "--", "sh", "-c", put, "sh", &claude],
    ));
    // Stopped, which commits the script, so both tries fork it.
    ok(runner(&root, &["stop", &name]));

    let started = ok(cli(&root)
        .env("RUNNER_CLAUDE", "/home/agent/claude")
        .args(["run", &name, "--n", "2", "pick one"])
        .output()
        .unwrap());
    assert_eq!(started, format!("{}.1\n{}.1\n", tries[0], tries[1]));
    let waited = ok(runner(&root, &["tries", &name, "--wait", "--json"]));
    let now: serde_json::Value = serde_json::from_str(&waited).unwrap();
    let outcome = |i: usize| now["tries"][i]["run"]["outcome"].as_str().map(String::from);
    assert_eq!(outcome(0).as_deref(), Some("rolled_back"), "{now}");
    assert_eq!(outcome(1).as_deref(), Some("committed"), "{now}");
    let who = serde_json::json!({"path": "/home/agent/work/who.txt", "change": "added"});
    let paths = now["tries"][1]["result"]["changed"]["paths"].as_array();
    assert!(paths.is_some_and(|p| p.contains(&who)), "{now}");

    let kept = ok(runner(&root, &["keep", &tries[1]]));
    assert!(
        kept.starts_with(&format!("kept {} on {name}: ", tries[1])),
        "{kept}"
    );
    assert!(kept.contains(&format!("--resume {SESSION} ")), "{kept}");
    let cat = ["exec", &name, "--", "cat", "/home/agent/work/who.txt"];
    assert_eq!(ok(runner(&root, &cat)), format!("{}\n", tries[1]));
    let list = ok(runner(&root, &["list"]));
    let names: Vec<&str> = list.lines().filter_map(|l| l.split(' ').next()).collect();
    assert!(names.contains(&name.as_str()), "{list}");
    assert!(!tries.iter().any(|t| names.contains(&t.as_str())), "{list}");
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
