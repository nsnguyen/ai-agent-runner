// `runner boot` and `runner halt`, as the unit runs them, with a fake
// Firecracker whose API socket this test serves. No KVM needed.
mod common;

use common::{FakeApi, FakeFirecracker, Reply, Request, script, text, wait_for};
use runner::creds::{self, Cred, Keyring};
use runner::firecracker::{self, BOOT_ARGS};
use runner::net::{self, HOST_RESOLV_CONFS};
use runner::store::{Config, Error, Store};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;
use tempfile::TempDir;

struct Host {
    dir: TempDir,
    store: Store,
    fc: FakeFirecracker,
    notify: UnixDatagram,
}

impl Host {
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let store = Store::new(&root);
        let disk = dir.path().join("rootfs.ext4");
        fs::write(&disk, b"rootfs").unwrap();
        store.import("base", &disk).unwrap();
        store.fork("base", "dev").unwrap();
        fs::create_dir_all(root.join("kernel")).unwrap();
        fs::write(root.join("kernel/vmlinux"), b"kernel").unwrap();
        let fc = FakeFirecracker::new(&dir.path().join("fc"));
        // An `ip` that logs what it's asked and fails a step named in ip.fail.
        // Removing the tap, it notes whether dev is still held, as it must
        // be: a halt that saw it free could let systemd kill boot first.
        let (log, fail) = (dir.path().join("ip.log"), dir.path().join("ip.fail"));
        let (lock, held) = (root.join("vms/dev/lock"), dir.path().join("held"));
        script(
            &dir.path().join("ip"),
            &format!(
                "echo \"$*\" >> '{}'\n\
                 [ \"$1 $2\" = 'link del' ] && {{ flock -n '{}' true && echo free || echo held; }} > '{}'\n\
                 [ \"$1 $2\" = \"$(cat '{}' 2>/dev/null)\" ] && \
                 {{ echo 'RTNETLINK answers: Operation not permitted' >&2; exit 2; }}\nexit 0\n",
                log.display(),
                lock.display(),
                held.display(),
                fail.display()
            ),
        );
        let notify = UnixDatagram::bind(dir.path().join("notify")).unwrap();
        notify
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Host {
            dir,
            store,
            fc,
            notify,
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    fn vm_dir(&self, name: &str) -> PathBuf {
        self.root().join("vms").join(name)
    }

    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        self.env(&mut cmd).args(args);
        cmd
    }

    fn env<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        cmd.env("RUNNER_ROOT", self.root())
            .env("RUNNER_IP", self.dir.path().join("ip"))
            .env("RUNNER_FIRECRACKER", &self.fc.bin)
            .env("FAKE_FC", &self.fc.dir)
            .env("FAKE_FC_VERSION", firecracker::VERSION)
            .env("NOTIFY_SOCKET", self.dir.path().join("notify"))
            // Only the credentials a test gives it.
            .env_remove("CREDENTIALS_DIRECTORY")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GH_TOKEN")
    }

    /// Whether dev was still held when its tap was removed.
    fn held_at_tap_removal(&self) -> String {
        let held = fs::read_to_string(self.dir.path().join("held"));
        held.expect("the tap wasn't removed").trim().to_string()
    }

    /// What `ip` was asked to do, one line per call.
    fn ip_calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.dir.path().join("ip.log")).unwrap_or_default();
        log.lines().map(String::from).collect()
    }

    fn boot(&self, name: &str) -> Child {
        self.boot_with(name, &[])
    }

    // With these variables, the way the host gets its credentials outside
    // systemd.
    fn boot_with(&self, name: &str, env: &[(&str, &str)]) -> Child {
        self.runner(&["boot", name])
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Serves the VM's API socket once the fake Firecracker is up, as the
    /// real one would.
    fn serve(&self, name: &str, respond: impl Fn(&Request) -> Reply + Send + 'static) -> FakeApi {
        self.fc.argv();
        FakeApi::serve(&self.vm_dir(name).join("api.sock"), respond)
    }

    fn ready(&self) -> String {
        let mut buf = [0; 64];
        let n = self
            .notify
            .recv(&mut buf)
            .expect("no READY from runner boot");
        text(&buf[..n])
    }
}

fn finish(child: Child) -> Output {
    child.wait_with_output().unwrap()
}

// Firecracker's answer to everything; Ctrl-Alt-Del makes the guest shut down.
fn guest_that_shuts_down(fc: &FakeFirecracker) -> impl Fn(&Request) -> Reply + Send + 'static {
    let fc = fc.clone();
    move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            fc.exit_with(0);
        }
        (204, None)
    }
}

fn boot_calls(root: &Path, kernel: &Path, cpus: u32, mem: u32) -> Vec<(String, Value)> {
    let dev = root.join("vms/dev");
    let call = |p: &str, b| (p.to_string(), b);
    // The DNS servers are this machine's, as boot reads them.
    let net = Store::new(root).net("dev").unwrap();
    let dns = net::dns(&HOST_RESOLV_CONFS.map(Path::new));
    let args = format!("{BOOT_ARGS} {}", net.boot_arg("dev", &dns));
    vec![
        call(
            "PUT /boot-source",
            json!({"kernel_image_path": kernel, "boot_args": args}),
        ),
        call(
            "PUT /drives/rootfs",
            json!({"drive_id": "rootfs", "path_on_host": dev.join("gens/1.ext4"),
                   "is_root_device": true, "is_read_only": false}),
        ),
        call(
            "PUT /machine-config",
            json!({"vcpu_count": cpus, "mem_size_mib": mem}),
        ),
        call(
            "PUT /vsock",
            json!({"guest_cid": 3, "uds_path": dev.join("vsock.sock")}),
        ),
        call(
            "PUT /network-interfaces/eth0",
            json!({"iface_id": "eth0", "host_dev_name": net.tap, "guest_mac": net.mac}),
        ),
        call("PUT /actions", json!({"action_type": "InstanceStart"})),
    ]
}

#[test]
fn the_tap_is_up_while_the_vm_runs_and_goes_before_the_vm_is_let_go() {
    let h = Host::new();
    let net = h.store.net("dev").unwrap();
    let boot = h.boot("dev");
    let _api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    let tap = &net.tap;
    let up = [
        format!("tuntap del dev {tap} mode tap"),
        format!("tuntap add dev {tap} mode tap"),
        format!("addr add {}/30 dev {tap}", net.gateway),
        format!("link set dev {tap} up"),
    ];
    assert_eq!(h.ip_calls(), up);
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
    assert_eq!(h.ip_calls().last().unwrap(), &format!("link del dev {tap}"));
    assert_eq!(h.ip_calls().len(), 5);
    assert_eq!(h.held_at_tap_removal(), "held");
}

#[test]
fn a_tap_that_can_t_be_made_stops_the_boot_before_firecracker() {
    let h = Host::new();
    fs::write(h.dir.path().join("ip.fail"), "tuntap add").unwrap();
    let out = finish(h.boot("dev"));
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("Operation not permitted"), "{err}");
    assert!(!h.fc.started());
    assert!(!h.vm_dir("dev").join("gens/1.ext4").exists());
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    // Free again.
    h.store.delete("dev").unwrap();
}

// The unit gives boot CAP_NET_ADMIN for the tap; Firecracker, which a guest
// could break into, mustn't keep it. Run as root:
//   cargo test --test boot -- --ignored
#[test]
#[ignore = "needs root, to raise an ambient capability with setpriv"]
fn firecracker_gets_no_capabilities_from_boot() {
    let h = Host::new();
    let mut cmd = Command::new("setpriv");
    cmd.args(["--inh-caps", "+net_admin", "--ambient-caps", "+net_admin"])
        .arg(env!("CARGO_BIN_EXE_runner"))
        .args(["boot", "dev"]);
    let boot = h
        .env(&mut cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    let caps = fs::read_to_string(h.fc.dir.join("caps")).unwrap();
    h.fc.exit_with(0);
    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        caps.split_whitespace().collect::<Vec<_>>(),
        ["CapAmb:", "0000000000000000"]
    );
}

#[test]
fn boot_runs_the_next_generation_and_a_clean_halt_commits_it() {
    let h = Host::new();
    let dev = h.vm_dir("dev");
    // Left by a Firecracker that crashed: binding over them would fail.
    fs::write(dev.join("api.sock"), "").unwrap();
    fs::write(dev.join("vsock.sock"), "").unwrap();

    let boot = h.boot("dev");
    let api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    let api_sock = dev.join("api.sock");
    assert_eq!(
        h.fc.argv(),
        ["--api-sock", api_sock.to_str().unwrap(), "--id", "dev"]
    );
    // Ready only once the guest is started.
    let kernel = h.root().join("kernel/vmlinux");
    assert_eq!(api.calls(), boot_calls(&h.root(), &kernel, 2, 2048));
    assert!(!dev.join("vsock.sock").exists(), "stale vsock socket kept");

    // While it runs, the VM is held.
    assert!(matches!(h.store.delete("dev"), Err(Error::Busy(_))));
    assert!(matches!(h.store.undo("dev"), Err(Error::Busy(_))));
    assert_eq!(h.store.get("dev").unwrap().generation, 0);

    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    // halt returns only after boot has committed and let go.
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
    let calls = api.calls();
    assert_eq!(
        calls.last().unwrap().1,
        json!({"action_type": "SendCtrlAltDel"})
    );

    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("fake console: Linux version"));
    assert!(
        text(&out.stderr).contains("generation 1"),
        "{}",
        text(&out.stderr)
    );
    assert!(!api_sock.exists(), "API socket left behind");
    h.store.delete("dev").unwrap();
}

#[test]
fn a_vm_boots_with_its_own_sizes_and_kernel() {
    let h = Host::new();
    let kernel = h.dir.path().join("my-vmlinux");
    fs::write(&kernel, b"kernel").unwrap();
    let config = Config {
        cpus: Some(4),
        mem_mib: Some(4096),
        kernel: Some(kernel.clone()),
    };
    h.store.delete("dev").unwrap();
    h.store.fork_with("base", "dev", &config).unwrap();

    let boot = h.boot("dev");
    let api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(api.calls(), boot_calls(&h.root(), &kernel, 4, 4096));
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

#[test]
fn a_crash_keeps_the_old_generation_current() {
    let h = Host::new();
    let boot = h.boot("dev");
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    h.fc.exit_with(1);
    let out = finish(boot);
    assert_eq!(h.held_at_tap_removal(), "held");
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("exit status: 1"), "{err}");
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    // The crashed disk stays for a look, and the VM is free again.
    assert!(h.vm_dir("dev").join("gens/1.ext4").exists());
    h.store.delete("dev").unwrap();
}

#[test]
fn an_api_fault_stops_the_boot_and_drops_its_disk() {
    let h = Host::new();
    let boot = h.boot("dev");
    let fault = json!({"fault_message": "The kernel file cannot be opened"}).to_string();
    let _api = h.serve("dev", move |req| {
        if req.path == "/boot-source" {
            (400, Some(fault.clone()))
        } else {
            (204, None)
        }
    });
    let out = finish(boot);
    assert_eq!(h.held_at_tap_removal(), "held");
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("The kernel file cannot be opened"), "{err}");
    h.notify
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(
        h.notify.recv(&mut [0; 64]).is_err(),
        "READY for a failed boot"
    );
    // The guest never ran, so its copy of the disk is gone.
    assert!(!h.vm_dir("dev").join("gens/1.ext4").exists());
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    h.store.delete("dev").unwrap();
}

#[test]
fn firecracker_dying_before_its_api_is_up_is_reported() {
    let h = Host::new();
    let boot = h.boot("dev");
    h.fc.argv();
    h.fc.exit_with(1);
    let out = finish(boot);
    assert_eq!(h.held_at_tap_removal(), "held");
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("before its API"), "{err}");
    assert!(!h.vm_dir("dev").join("gens/1.ext4").exists());
}

#[test]
fn another_firecracker_version_is_refused_before_anything_starts() {
    let h = Host::new();
    let out = h
        .runner(&["boot", "dev"])
        .env("FAKE_FC_VERSION", "v1.0.0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("v1.0.0") && err.contains(firecracker::VERSION),
        "{err}"
    );
    assert!(!h.fc.started());
    assert!(!h.vm_dir("dev").join("gens/1.ext4").exists());
}

#[test]
fn a_missing_kernel_is_named() {
    let h = Host::new();
    fs::remove_file(h.root().join("kernel/vmlinux")).unwrap();
    let out = h.runner(&["boot", "dev"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("kernel/vmlinux"), "{err}");
    assert!(!h.fc.started());
}

#[test]
fn a_held_vm_does_not_boot() {
    let h = Host::new();
    let _run = h.store.begin_run("dev").unwrap();
    let out = h.runner(&["boot", "dev"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("dev is busy"),
        "{}",
        text(&out.stderr)
    );
    assert!(!h.fc.started());
}

#[test]
fn halt_with_nothing_booted_returns_at_once() {
    let h = Host::new();
    let out = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    // And the VM can still be booted afterwards.
    let boot = h.boot("dev");
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
}

#[test]
fn ready_waits_for_the_guest_to_start() {
    let h = Host::new();
    let boot = h.boot("dev");
    // InstanceStart is held until the test lets it through.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let api = h.serve("dev", move |req| {
        if req.body == json!({"action_type": "InstanceStart"}) {
            held.lock().unwrap().recv().unwrap();
        }
        (204, None)
    });
    wait_for("InstanceStart", || {
        api.requests().iter().any(|r| r.path == "/actions")
    });
    h.notify
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(
        h.notify.recv(&mut [0; 64]).is_err(),
        "READY before the guest started"
    );
    release.send(()).unwrap();
    h.notify
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(h.ready(), "READY=1");
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

#[test]
fn firecracker_holds_the_vm_even_if_boot_dies() {
    let h = Host::new();
    let mut boot = h.boot("dev");
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    // What a `runner boot` killed outside systemd leaves: a guest still running.
    boot.kill().unwrap();
    boot.wait().unwrap();
    assert!(matches!(h.store.delete("dev"), Err(Error::Busy(_))));
    assert!(matches!(h.store.undo("dev"), Err(Error::Busy(_))));
    h.fc.exit_with(0);
    // Once it's gone the VM is free, with nobody left to commit the run.
    wait_for("firecracker to let go", || {
        !matches!(h.store.undo("dev"), Err(Error::Busy(_)))
    });
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
}

// A checkpoint under way when the VM stops finishes first, and one whose
// process died with the VM paused left it paused: halt resumes it, or the
// guest couldn't take Ctrl-Alt-Del.
#[test]
fn halt_waits_out_a_checkpoint_and_resumes_the_vm_first() {
    let h = Host::new();
    let boot = h.boot("dev");
    let api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    let freezer = h.store.hold_checkpoints("dev").unwrap();
    let booted = api.calls().len();
    let mut halt = h
        .runner(&["halt", "dev"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(halt.try_wait().unwrap().is_none(), "halt didn't wait");
    assert_eq!(api.calls().len(), booted);
    drop(freezer);
    let out = halt.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        api.calls()[booted..],
        [
            ("PATCH /vm".to_string(), json!({"state": "Resumed"})),
            (
                "PUT /actions".to_string(),
                json!({"action_type": "SendCtrlAltDel"})
            ),
        ]
    );
    assert!(finish(boot).status.success());
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
}

#[test]
fn halt_waits_even_when_ctrl_alt_del_fails() {
    let h = Host::new();
    let boot = h.boot("dev");
    let fault = json!({"fault_message": "The keyboard buffer is full"}).to_string();
    let _api = h.serve("dev", move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            (400, Some(fault.clone()))
        } else {
            (204, None)
        }
    });
    assert_eq!(h.ready(), "READY=1");
    let mut halt = h
        .runner(&["halt", "dev"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Still waiting: the guest may be shutting down anyway, and the unit's
    // stop timeout is what ends the wait.
    std::thread::sleep(Duration::from_millis(300));
    assert!(halt.try_wait().unwrap().is_none(), "halt gave up at once");
    h.fc.exit_with(0);
    let out = halt.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("The keyboard buffer is full"));
    assert!(finish(boot).status.success());
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
}

const TOKEN: &str = "fake-claude-token-for-boot-tests";

// What runner-guest does each time it starts in the VM: asks the host for
// its credentials. Firecracker passes the guest's connection to host port P
// on to `vsock.sock_P`.
fn guest_asks(h: &Host, name: &str) -> Keyring {
    let sock = h.vm_dir(name).join(format!("vsock.sock_{}", creds::PORT));
    wait_for("boot to offer credentials", || sock.exists());
    let mut conn = UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    Keyring::receive(&mut conn).unwrap()
}

fn vars(keyring: &Keyring) -> Vec<(String, String)> {
    let vars = keyring.vars().map(|(k, v)| (k.to_string(), v.to_string()));
    vars.collect()
}

#[test]
fn the_guest_is_handed_what_the_allowlist_names_each_time_it_asks() {
    let h = Host::new();
    let env = [("CLAUDE_CODE_OAUTH_TOKEN", TOKEN), ("GH_TOKEN", "ghp-fake")];
    let boot = h.boot_with("dev", &env);
    let _api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(h.store.handed("dev").unwrap(), None, "before it asked");

    // The default allowlist: Claude's token, not GitHub's.
    let claude = vec![("CLAUDE_CODE_OAUTH_TOKEN".to_string(), TOKEN.to_string())];
    assert_eq!(vars(&guest_asks(&h, "dev")), claude);
    wait_for("the handoff to be noted", || {
        h.store.handed("dev").unwrap().is_some()
    });
    assert_eq!(h.store.handed("dev").unwrap(), Some(vec![Cred::Claude]));
    // A runner-guest that restarts asks again, and gets the same.
    assert_eq!(vars(&guest_asks(&h, "dev")), claude);

    h.fc.exit_with(0);
    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(h.store.handed("dev").unwrap(), None, "once it's down");
    let sock = h.vm_dir("dev").join(format!("vsock.sock_{}", creds::PORT));
    assert!(!sock.exists(), "credentials socket left behind");
    let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(said.contains("handed dev's guest claude"), "{said}");
    assert!(!said.contains(TOKEN), "a value in the journal: {said}");
}

#[test]
fn a_vm_given_github_gets_both() {
    let h = Host::new();
    let env = [("CLAUDE_CODE_OAUTH_TOKEN", TOKEN), ("GH_TOKEN", "ghp-fake")];
    h.store
        .set_creds("dev", &[Cred::Claude, Cred::Github])
        .unwrap();
    let boot = h.boot_with("dev", &env);
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(guest_asks(&h, "dev").creds(), [Cred::Claude, Cred::Github]);
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

#[test]
fn a_vm_given_nothing_is_handed_nothing() {
    let h = Host::new();
    let env = [("CLAUDE_CODE_OAUTH_TOKEN", TOKEN), ("GH_TOKEN", "ghp-fake")];
    h.store.set_creds("dev", &[]).unwrap();
    let boot = h.boot_with("dev", &env);
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(vars(&guest_asks(&h, "dev")), []);
    wait_for("the handoff to be noted", || {
        h.store.handed("dev").unwrap().is_some()
    });
    assert_eq!(h.store.handed("dev").unwrap(), Some(vec![]));
    h.fc.exit_with(0);
    let out = finish(boot);
    assert!(out.status.success());
    // Nothing missing to warn about: it may have none.
    assert!(
        !text(&out.stderr).contains("has none"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn boot_takes_credentials_from_systemd_s_directory_first() {
    let h = Host::new();
    let dir = h.dir.path().join("credentials");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("CLAUDE_CODE_OAUTH_TOKEN"), format!("{TOKEN}\n")).unwrap();
    let dir = dir.to_str().unwrap();
    let env = [
        ("CREDENTIALS_DIRECTORY", dir),
        ("CLAUDE_CODE_OAUTH_TOKEN", "from-the-environment"),
    ];
    let boot = h.boot_with("dev", &env);
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    let got = vars(&guest_asks(&h, "dev"));
    assert_eq!(got, [("CLAUDE_CODE_OAUTH_TOKEN".into(), TOKEN.into())]);
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

// A host with no token still boots the VM, and the journal says what's
// missing and where it goes.
#[test]
fn a_credential_the_host_lacks_is_named_and_the_vm_boots_anyway() {
    let h = Host::new();
    let boot = h.boot("dev");
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(vars(&guest_asks(&h, "dev")), []);
    h.fc.exit_with(0);
    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(
        err.contains("dev may have claude, but this host has none")
            && err.contains("/etc/credstore/CLAUDE_CODE_OAUTH_TOKEN"),
        "{err}"
    );
}

// Firecracker is what a guest would break into first, so it holds no
// credential, even one the VM's allowlist leaves out.
#[test]
fn firecracker_gets_no_credentials() {
    let h = Host::new();
    let dir = h.dir.path().join("credentials");
    fs::create_dir(&dir).unwrap();
    let env = [
        ("CREDENTIALS_DIRECTORY", dir.to_str().unwrap()),
        ("CLAUDE_CODE_OAUTH_TOKEN", TOKEN),
        ("ANTHROPIC_API_KEY", "sk-ant-api-fake"),
        ("GH_TOKEN", "ghp-fake"),
    ];
    let boot = h.boot_with("dev", &env);
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    let seen = fs::read_to_string(h.fc.dir.join("env")).unwrap();
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
    for (var, _) in env {
        let set = seen.lines().any(|l| l.starts_with(&format!("{var}=")));
        assert!(!set, "firecracker has {var}");
    }
}

// Left by a boot that was killed: its record goes before the guest can ask.
#[test]
fn a_stale_handoff_record_is_cleared_at_boot() {
    let h = Host::new();
    fs::write(h.vm_dir("dev").join("handed"), "github\n").unwrap();
    let boot = h.boot("dev");
    let _api = h.serve("dev", |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(h.store.handed("dev").unwrap(), None);
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

#[test]
fn a_run_s_rollback_ask_keeps_a_clean_shutdown_from_committing() {
    let h = Host::new();
    let boot = h.boot("dev");
    let _api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    h.store.ask_rollback("dev", "run dev.4").unwrap();

    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert!(out.status.success(), "{notes}");
    // The generation it booted from stays current, and the run's is kept
    // for a look.
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    assert!(h.vm_dir("dev").join("gens/1.ext4").is_file());
    assert!(notes.contains("run dev.4 asked to roll back"), "{notes}");
    assert!(
        !h.vm_dir("dev").join("rollback").exists(),
        "the ask was kept"
    );
}

#[test]
fn a_rollback_ask_no_boot_took_is_dropped_at_the_next_boot() {
    let h = Host::new();
    h.store.ask_rollback("dev", "run dev.1").unwrap();
    let boot = h.boot("dev");
    let _api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    assert!(finish(boot).status.success());
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
}
