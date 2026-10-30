// Boot tests: real Firecracker, real KVM. Skipped unless asked for, since
// cloud sessions have no /dev/kvm. On the host:
//
//   RUNNER_TEST_DISK=/path/rootfs.ext4 cargo test --test kvm -- --ignored
//
// The rootfs's init must be systemd or another that takes Ctrl-Alt-Del, with
// a login prompt on the serial console.
// `boot_and_halt_by_hand` and `a_booted_vm_answers_at_its_address` need
// Firecracker on PATH, RUNNER_TEST_KERNEL, and root (or CAP_NET_ADMIN) for
// the VM's tap: run them with sudo -E.
// `start_logs_and_stop_under_systemd` needs `runner unit` installed and the
// host's kernel in place; it uses the unit's root, RUNNER_ROOT or
// /var/lib/runner. `exec_starts_the_vm_and_runs_in_the_guest` needs the same,
// with the base image (images/base) as RUNNER_TEST_DISK, for its
// runner-guest and agent user.
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
    assert!(status.ends_with("  running\n"), "{status}");
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
