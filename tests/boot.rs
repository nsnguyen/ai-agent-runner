// `runner boot` and `runner halt`, as the unit runs them, with a fake
// Firecracker whose API socket this test serves. No KVM needed.
mod common;

use common::{FakeApi, FakeFirecracker, FakeVsock, Reply, Request, script, text, wait_for};
use runner::boot;
use runner::calls::{self, Answer, Call};
use runner::creds::{self, Cred, Keyring};
use runner::firecracker::{self, BOOT_ARGS};
use runner::idle::{Idle, InUse};
use runner::net::{self, HOST_RESOLV_CONFS};
use runner::proto;
use runner::sessions;
use runner::sleep;
use runner::store::{Config, Error, Store};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    let args = format!("{BOOT_ARGS} {}", net.boot_arg_with("dev", &dns));
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

// The unit gives boot CAP_NET_ADMIN for the tap and CAP_NET_BIND_SERVICE
// for the VM's DNS; Firecracker, which a guest could break into, mustn't
// keep either. Run as root:
//   cargo test --test boot -- --ignored
#[test]
#[ignore = "needs root, to raise an ambient capability with setpriv"]
fn firecracker_gets_no_capabilities_from_boot() {
    let h = Host::new();
    let mut cmd = Command::new("setpriv");
    let caps = "+net_admin,+net_bind_service";
    cmd.args(["--inh-caps", caps, "--ambient-caps", caps])
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
    // The fake ip makes no tap, so boot can't serve the VM's DNS on its
    // gateway: it says why, and the guest got the host's resolvers instead
    // (boot_calls).
    let why = match net::dns(&HOST_RESOLV_CONFS.map(Path::new)).is_empty() {
        true => "dev boots without DNS",
        false => {
            "dev's DNS goes straight to the host's resolvers: boot can't serve it on 172.16.0."
        }
    };
    assert!(text(&out.stderr).contains(why), "{}", text(&out.stderr));
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
        idle: None,
        url: None,
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

// The guest's own restore, as `runner-guest restore` asks for it.
fn guest_restores(h: &Host, id: u64) -> Answer {
    let mut conn = UnixStream::connect(calls_socket(h)).unwrap();
    calls::ask(&mut conn, &Call::Restore { id }).unwrap()
}

fn calls_socket(h: &Host) -> PathBuf {
    h.vm_dir("dev").join(format!("vsock.sock_{}", calls::PORT))
}

#[test]
fn a_guest_s_restore_commits_its_boot_puts_the_copy_on_top_and_boots_again() {
    let h = Host::new();
    let clean = h.store.checkpoint("dev", Some("clean")).unwrap();
    let boot = h.boot("dev");
    let _api = h.serve("dev", guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    assert!(calls_socket(&h).exists(), "no calls socket while it runs");

    let answer = guest_restores(&h, clean.id);
    let Answer::Restoring { id, generation } = answer else {
        panic!("{answer:?}")
    };
    assert_eq!((id, generation), (clean.id, 3));
    let out = finish(boot);
    let notes = text(&out.stderr);
    // The unit's RestartForceExitStatus boots it again.
    assert_eq!(out.status.code(), Some(boot::RESTART.into()), "{notes}");
    assert!(notes.contains("to restore checkpoint 1"), "{notes}");
    // The boot's own disk was committed first, as at any clean shutdown, so
    // `undo` goes back to it.
    let listed = h.store.checkpoints("dev").unwrap();
    let top = &listed[0];
    let what = (top.id, top.current, top.auto, top.comment.as_deref());
    assert_eq!(what, (3, true, true, Some("restore of 1")));
    assert_eq!(
        fs::read(h.vm_dir("dev").join("gens/3.ext4")).unwrap(),
        b"rootfs"
    );
    assert_eq!(h.store.undo("dev").unwrap().generation, 2);
    assert!(!calls_socket(&h).exists(), "calls socket left behind");
}

#[test]
fn a_guest_s_restore_is_dropped_if_its_vm_crashes() {
    let h = Host::new();
    let clean = h.store.checkpoint("dev", Some("clean")).unwrap();
    let boot = h.boot("dev");
    let fc = h.fc.clone();
    let _api = h.serve("dev", move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            fc.exit_with(1);
        }
        (204, None)
    });
    assert_eq!(h.ready(), "READY=1");
    let answer = guest_restores(&h, clean.id);
    let Answer::Restoring { generation, .. } = answer else {
        panic!("{answer:?}")
    };
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{notes}");
    assert!(
        notes.contains("restore of checkpoint 1 is dropped"),
        "{notes}"
    );
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    let copy = h.vm_dir("dev").join(format!("gens/{generation}.ext4"));
    assert!(!copy.exists(), "the staged copy was kept");
    let ids: Vec<u64> = h
        .store
        .checkpoints("dev")
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, [1, 0]);
}

// Sleep and wake (src/sleep.rs), which boot does for its VM when asked on its
// socket. Each wake starts a fresh Firecracker, whose API socket the test
// serves once it has started.

// Firecracker's answers while the VM is up: a snapshot writes its two files,
// as the real one does, and Ctrl-Alt-Del shuts the guest down.
fn firecracker_that_sleeps(fc: &FakeFirecracker) -> impl Fn(&Request) -> Reply + Send + 'static {
    let fc = fc.clone();
    move |req| {
        if req.path == "/snapshot/create" {
            for file in ["snapshot_path", "mem_file_path"] {
                fs::write(req.body[file].as_str().unwrap(), file).unwrap();
            }
        }
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            fc.exit_with(0);
        }
        (204, None)
    }
}

// The next Firecracker boot starts, once it has: its API on the test's
// `respond`, and the guest behind its vsock socket, which records what it's
// asked to run and runs nothing.
fn next_firecracker(h: &Host, respond: impl Fn(&Request) -> Reply + Send + 'static) -> Up {
    let argv = h.fc.next();
    let dev = h.vm_dir("dev");
    let guest = h.dir.path().join(format!("guest-{}.sock", h.fc.pid()));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(&guest).unwrap();
    let seen = asked.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let _ = proto::hello(&mut conn);
            if proto::check_hello(&mut conn).is_err() {
                continue;
            }
            if let Ok(Some(req)) = proto::read_frame::<proto::Request>(&mut conn) {
                seen.lock().unwrap().push(req);
                let _ =
                    proto::write_frame(&mut conn, &proto::FromGuest::Exit(proto::Exit::Code(0)));
            }
        }
    });
    // Firecracker binds the vsock socket as it loads, before its API answers.
    let vsock = FakeVsock::serve(&dev.join("vsock.sock"), &guest);
    let api = FakeApi::serve(&dev.join("api.sock"), respond);
    Up {
        argv,
        api,
        asked,
        _vsock: vsock,
    }
}

struct Up {
    argv: Vec<String>,
    api: FakeApi,
    /// What the host asked the guest to run.
    asked: Arc<Mutex<Vec<proto::Request>>>,
    _vsock: FakeVsock,
}

// dev booted, its guest up: it has asked for its credentials.
fn booted(h: &Host) -> (Child, Up) {
    let boot = h.boot("dev");
    let up = next_firecracker(h, firecracker_that_sleeps(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    guest_up(h);
    (boot, up)
}

// Boot notes the handoff just after it.
fn guest_up(h: &Host) {
    guest_asks(h, "dev");
    wait_for("the handoff's note", || {
        h.store.handed("dev").unwrap().is_some()
    });
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn after(up: &Up, n: usize) -> Vec<(String, Value)> {
    up.api.calls()[n..].to_vec()
}

#[test]
fn sleep_snapshots_the_vm_and_stops_firecracker_but_not_boot() {
    let h = Host::new();
    let (mut boot, up) = booted(&h);
    let booted = up.api.calls().len();
    let fc = h.fc.pid();
    let dev = h.vm_dir("dev");

    sleep::sleep(&h.store, "dev").unwrap();
    let snap = dev.join("snapshot");
    assert_eq!(
        after(&up, booted),
        [
            ("PATCH /vm".to_string(), json!({"state": "Paused"})),
            (
                "PUT /snapshot/create".to_string(),
                json!({"snapshot_type": "Full", "snapshot_path": snap.join("vmstate"),
                       "mem_file_path": snap.join("mem"), "sync_snapshot_files": false})
            ),
        ]
    );
    assert!(!alive(fc), "firecracker still running");
    assert!(!dev.join("api.sock").exists() && !dev.join("vsock.sock").exists());
    // Pinned to the disk its boot runs, which stays uncommitted and held.
    let slept = h.store.sleeping("dev").unwrap().expect("not sleeping");
    assert_eq!(slept.generation, 1);
    assert!(boot.try_wait().unwrap().is_none(), "boot ended");
    assert!(matches!(h.store.undo("dev"), Err(Error::Busy(_))));
    assert!(matches!(h.store.gc("dev", false), Err(Error::Busy(_))));
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    // The guest's own sockets stay up for when it wakes.
    assert!(dev.join(format!("vsock.sock_{}", creds::PORT)).exists());
    assert!(calls_socket(&h).exists());

    // Asleep already: nothing to do.
    sleep::sleep(&h.store, "dev").unwrap();
    assert_eq!(up.api.calls().len(), booted + 2);
    boot.kill().unwrap();
    boot.wait().unwrap();
}

#[test]
fn wake_loads_the_snapshot_into_a_new_firecracker_and_sets_the_guest_s_clock() {
    let h = Host::new();
    let (boot, _up) = booted(&h);
    sleep::sleep(&h.store, "dev").unwrap();
    let dev = h.vm_dir("dev");
    let snap = dev.join("snapshot");

    let store = h.store.clone();
    let waking = std::thread::spawn(move || sleep::wake(&store, "dev"));
    let api_sock = dev.join("api.sock");
    // The same command, so the same sockets, as the boot's first.
    let up = next_firecracker(&h, firecracker_that_sleeps(&h.fc));
    waking.join().unwrap().unwrap();
    let woke = chrono::Utc::now();
    assert_eq!(
        up.argv,
        ["--api-sock", api_sock.to_str().unwrap(), "--id", "dev"]
    );
    assert_eq!(
        up.api.calls(),
        [(
            "PUT /snapshot/load".to_string(),
            json!({"snapshot_path": snap.join("vmstate"),
                   "mem_backend": {"backend_type": "File", "backend_path": snap.join("mem")},
                   "resume_vm": true})
        )]
    );
    // Its clock stood still while it slept: the host sets it, as root.
    let asked = up.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let argv = &asked[0].argv;
    assert_eq!(argv[..3], ["date", "-u", "-s"], "{argv:?}");
    assert_eq!(asked[0].user, None);
    let secs: f64 = argv[3].strip_prefix('@').unwrap().parse().unwrap();
    assert!((woke.timestamp() as f64 - secs).abs() < 5.0, "{argv:?}");
    // Awake, with nothing left of the snapshot.
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    assert!(!snap.join("vmstate").exists() && !snap.join("mem").exists());
    // Awake already: nothing to do.
    sleep::wake(&h.store, "dev").unwrap();
    assert_eq!(up.api.calls().len(), 1);

    // It shuts down as any VM does, committing the disk it ran all along.
    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
}

// Asleep from the moment it starts to sleep, so a wake asked meanwhile waits
// for the sleep and wakes it, rather than looking for a guest that's going.
#[test]
fn a_vm_is_listed_asleep_while_its_snapshot_is_written() {
    let h = Host::new();
    let boot = h.boot("dev");
    let store = h.store.clone();
    let during = Arc::new(Mutex::new(None));
    let seen = during.clone();
    let writes = firecracker_that_sleeps(&h.fc);
    let _up = next_firecracker(&h, move |req| {
        if req.path == "/snapshot/create" {
            *seen.lock().unwrap() = Some(store.sleeping("dev").unwrap().is_some());
        }
        writes(req)
    });
    assert_eq!(h.ready(), "READY=1");
    guest_up(&h);
    sleep::sleep(&h.store, "dev").unwrap();
    assert_eq!(*during.lock().unwrap(), Some(true));
    let mut boot = boot;
    boot.kill().unwrap();
    boot.wait().unwrap();
}

// Upgraded while the VM slept: its snapshot is for the old Firecracker.
#[test]
fn a_wake_checks_firecracker_s_version_again() {
    let h = Host::new();
    let (mut boot, _up) = booted(&h);
    sleep::sleep(&h.store, "dev").unwrap();
    fs::write(h.fc.dir.join("version"), "v1.0.0").unwrap();
    let err = sleep::wake(&h.store, "dev").unwrap_err();
    assert!(err.to_string().contains("pinned to"), "{err}");
    assert!(err.to_string().contains("sleeps on"), "{err}");
    assert!(!h.fc.started(), "a firecracker started");
    assert!(h.store.sleeping("dev").unwrap().is_some());
    boot.kill().unwrap();
    boot.wait().unwrap();
}

// A load that got no answer may have run the guest, which may have written
// to its disk since the snapshot: waking it again could load old memory
// over a newer disk, so the snapshot goes, and the boot with it.
#[test]
fn a_load_with_no_answer_drops_the_snapshot() {
    let h = Host::new();
    let (boot, _up) = booted(&h);
    sleep::sleep(&h.store, "dev").unwrap();
    let store = h.store.clone();
    let waking = std::thread::spawn(move || sleep::wake(&store, "dev"));
    h.fc.next();
    // Each request read, and its connection dropped without a reply.
    let api = UnixListener::bind(h.vm_dir("dev").join("api.sock")).unwrap();
    std::thread::spawn(move || {
        for conn in api.incoming() {
            let Ok(mut conn) = conn else { return };
            let _ = std::io::Read::read(&mut conn, &mut [0; 512]);
        }
    });
    let began = std::time::Instant::now();
    let err = waking.join().unwrap().unwrap_err();
    assert!(err.to_string().contains("snapshot is dropped"), "{err}");
    // Not a timeout: the answer never came.
    assert!(began.elapsed() < Duration::from_secs(5), "{err}");
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{notes}");
    assert!(notes.contains("generation 1 is kept for a look"), "{notes}");
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    assert!(!h.vm_dir("dev").join("snapshot/mem").exists());
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
}

#[test]
fn halting_a_sleeping_vm_wakes_it_to_shut_down_cleanly() {
    let h = Host::new();
    let (boot, _up) = booted(&h);
    sleep::sleep(&h.store, "dev").unwrap();
    let halt = h
        .runner(&["halt", "dev"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let up = next_firecracker(&h, firecracker_that_sleeps(&h.fc));
    let out = halt.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let calls: Vec<String> = up.api.calls().into_iter().map(|c| c.0).collect();
    assert_eq!(calls, ["PUT /snapshot/load", "PATCH /vm", "PUT /actions"]);
    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
}

// A snapshot Firecracker won't load: the VM sleeps on, and a halt, which
// can't wake it either, drops it, and its disk rolls back as after a kill.
#[test]
fn a_snapshot_that_won_t_load_sleeps_on_until_a_halt_drops_it() {
    let h = Host::new();
    let (boot, _up) = booted(&h);
    sleep::sleep(&h.store, "dev").unwrap();
    let refuse = |req: &Request| {
        if req.path == "/snapshot/load" {
            let fault = json!({"fault_message": "Cannot restore microvm state"});
            return (400, Some(fault.to_string()));
        }
        (204, None)
    };
    let store = h.store.clone();
    let waking = std::thread::spawn(move || sleep::wake(&store, "dev"));
    let _up = next_firecracker(&h, refuse);
    let err = waking.join().unwrap().unwrap_err();
    assert!(
        err.to_string().contains("Cannot restore microvm state"),
        "{err}"
    );
    assert!(
        !alive(h.fc.pid()),
        "the failed firecracker is still running"
    );
    assert!(
        h.store.sleeping("dev").unwrap().is_some(),
        "not sleeping on"
    );

    let halt = h
        .runner(&["halt", "dev"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _up = next_firecracker(&h, refuse);
    let halted = halt.wait_with_output().unwrap();
    assert!(halted.status.success(), "{}", text(&halted.stderr));
    assert!(
        text(&halted.stderr).contains("dropped"),
        "{}",
        text(&halted.stderr)
    );
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{notes}");
    assert!(notes.contains("generation 1 is kept for a look"), "{notes}");
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
}

#[test]
fn a_failed_snapshot_leaves_the_vm_running() {
    let h = Host::new();
    let boot = h.boot("dev");
    let fc = h.fc.clone();
    let up = next_firecracker(&h, move |req| {
        if req.path == "/snapshot/create" {
            return (
                400,
                Some(json!({"fault_message": "No space left"}).to_string()),
            );
        }
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            fc.exit_with(0);
        }
        (204, None)
    });
    assert_eq!(h.ready(), "READY=1");
    guest_up(&h);
    let booted = up.api.calls().len();
    let err = sleep::sleep(&h.store, "dev").unwrap_err();
    assert!(err.to_string().contains("No space left"), "{err}");
    let calls: Vec<Value> = after(&up, booted).into_iter().map(|c| c.1).collect();
    assert_eq!(calls[0], json!({"state": "Paused"}));
    assert_eq!(calls[2], json!({"state": "Resumed"}));
    assert!(alive(h.fc.pid()));
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    assert!(finish(boot).status.success());
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
}

// Before its guest is up, a snapshot could catch the kernel booting, which
// Firecracker says may not resume.
#[test]
fn sleep_waits_for_the_guest_to_be_up() {
    let h = Host::new();
    let boot = h.boot("dev");
    let up = next_firecracker(&h, firecracker_that_sleeps(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    let booted = up.api.calls().len();
    let err = sleep::sleep(&h.store, "dev").unwrap_err();
    assert!(err.to_string().contains("isn't up yet"), "{err}");
    assert_eq!(up.api.calls().len(), booted);
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

// A run streams over vsock, which a sleep would cut.
#[test]
fn sleep_is_refused_while_a_run_holds_the_vm() {
    let h = Host::new();
    let (boot, up) = booted(&h);
    let booted = up.api.calls().len();
    let run = runner::runs::hold(&h.store, "dev").unwrap();
    let err = sleep::sleep(&h.store, "dev").unwrap_err();
    assert!(
        err.to_string().contains("a run is starting in dev"),
        "{err}"
    );
    assert_eq!(up.api.calls().len(), booted);
    drop(run);
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

#[test]
fn sleep_is_refused_once_a_halt_has_begun() {
    let h = Host::new();
    let boot = h.boot("dev");
    // A guest slow to shut down.
    let up = next_firecracker(&h, |_| (204, None));
    assert_eq!(h.ready(), "READY=1");
    guest_up(&h);
    let halt = h.runner(&["halt", "dev"]).spawn().unwrap();
    wait_for("Ctrl-Alt-Del", || {
        up.api
            .calls()
            .iter()
            .any(|c| c.1 == json!({"action_type": "SendCtrlAltDel"}))
    });
    let err = sleep::sleep(&h.store, "dev").unwrap_err();
    assert!(err.to_string().contains("dev is shutting down"), "{err}");
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
    let halt = halt.wait_with_output().unwrap();
    assert!(halt.status.success());
}

// A boot killed while its VM slept left a snapshot of a disk that was
// never committed: no later boot can wake it.
#[test]
fn a_boot_clears_a_snapshot_an_earlier_one_left() {
    let h = Host::new();
    let snap = h.store.snapshot("dev").unwrap();
    snap.make_room().unwrap();
    fs::write(snap.vmstate(), "state").unwrap();
    fs::write(snap.mem(), "memory").unwrap();
    let at = chrono::Utc::now();
    snap.note(&runner::store::Slept {
        generation: 1,
        at,
        parked: false,
    })
    .unwrap();
    let boot = h.boot("dev");
    let _up = next_firecracker(&h, firecracker_that_sleeps(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    assert!(!snap.mem().exists());
    h.fc.exit_with(0);
    assert!(finish(boot).status.success());
}

#[test]
fn a_stopped_vm_has_no_boot_to_ask() {
    let h = Host::new();
    let err = sleep::sleep(&h.store, "dev").unwrap_err();
    assert!(matches!(err, sleep::Error::NoBoot(_)), "{err}");
    assert!(err.to_string().contains("dev has no boot to ask"), "{err}");
}

// Parking (src/sleep.rs): what a host shutdown does to a sleeping VM, through
// `runner park`, which runner-park.service runs before the VMs' units stop.

// dev booted, put to sleep, and parked: its boot has ended.
fn parked(h: &Host) {
    let (boot, _up) = booted(h);
    sleep::sleep(&h.store, "dev").unwrap();
    let park = h.runner(&["park", "--all"]).output().unwrap();
    assert!(park.status.success(), "{}", text(&park.stderr));
    // base has no boot to ask.
    assert_eq!(text(&park.stdout), "dev  parked\n");
    let out = finish(boot);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let notes = text(&out.stderr);
    assert!(notes.contains("dev is parked"), "{notes}");
}

#[test]
fn parking_a_sleeping_vm_leaves_its_snapshot_and_disk_and_commits_nothing() {
    let h = Host::new();
    let dev = h.vm_dir("dev");
    parked(&h);
    let slept = h.store.sleeping("dev").unwrap().expect("no snapshot");
    assert!(slept.parked, "{slept:?}");
    assert_eq!(slept.generation, 1);
    let snap = dev.join("snapshot");
    assert!(snap.join("vmstate").exists() && snap.join("mem").exists());
    assert!(dev.join("gens/1.ext4").exists());
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    // Its guest still holds what it was handed.
    assert!(h.store.handed("dev").unwrap().is_some());
    // The boot let go of all it held, the tap last but the lock.
    assert!(!dev.join("boot.sock").exists());
    assert!(!dev.join(format!("vsock.sock_{}", creds::PORT)).exists());
    let net = h.store.net("dev").unwrap();
    assert_eq!(
        h.ip_calls().last().unwrap(),
        &format!("link del dev {}", net.tap)
    );
    assert_eq!(h.held_at_tap_removal(), "held");
    // Asleep, with nothing to hold it: busy, as a sleeping VM is.
    assert!(matches!(h.store.undo("dev"), Err(Error::Busy(_))));
    assert!(matches!(h.store.gc("dev", false), Err(Error::Busy(_))));
    assert!(matches!(h.store.delete("dev"), Err(Error::Busy(_))));
    // The unit's ExecStop, which runs after a main process that exited on
    // its own, finds nothing to do.
    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    assert!(h.store.sleeping("dev").unwrap().unwrap().parked);
}

#[test]
fn the_next_boot_loads_a_parked_snapshot_onto_its_disk() {
    let h = Host::new();
    parked(&h);
    let dev = h.vm_dir("dev");
    let snap = dev.join("snapshot");
    let boot = h.boot("dev");
    // Unparked before the load, which runs the guest: a boot that died then
    // mustn't resume this snapshot onto a disk the guest has written since.
    let store = h.store.clone();
    let at_load = Arc::new(Mutex::new(None));
    let seen = at_load.clone();
    let writes = firecracker_that_sleeps(&h.fc);
    let up = next_firecracker(&h, move |req| {
        if req.path == "/snapshot/load" {
            *seen.lock().unwrap() = Some(store.sleeping("dev").unwrap());
        }
        writes(req)
    });
    assert_eq!(h.ready(), "READY=1");
    assert_eq!(*at_load.lock().unwrap(), Some(None));
    let resumed = chrono::Utc::now();
    // No boot of its own: the snapshot, with the disk, tap and sockets it
    // slept with.
    assert_eq!(
        up.api.calls(),
        [(
            "PUT /snapshot/load".to_string(),
            json!({"snapshot_path": snap.join("vmstate"),
                   "mem_backend": {"backend_type": "File", "backend_path": snap.join("mem")},
                   "resume_vm": true})
        )]
    );
    let net = h.store.net("dev").unwrap();
    assert!(
        h.ip_calls()
            .contains(&format!("tuntap add dev {} mode tap", net.tap))
    );
    // Its clock is set, as at a wake.
    let asked = up.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].argv[..3], ["date", "-u", "-s"]);
    let secs: f64 = asked[0].argv[3].strip_prefix('@').unwrap().parse().unwrap();
    assert!((resumed.timestamp() as f64 - secs).abs() < 5.0);
    // The snapshot, credentials and all, goes at the resume, and nothing
    // new was cloned.
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    assert!(!snap.join("vmstate").exists() && !snap.join("mem").exists());
    assert!(!dev.join("gens/2.ext4").exists());
    // Its guest took its credentials before it slept, and isn't asked
    // again: it can sleep again.
    assert!(h.store.handed("dev").unwrap().is_some());
    sleep::sleep(&h.store, "dev").unwrap();
    let woke = std::thread::spawn({
        let store = h.store.clone();
        move || sleep::wake(&store, "dev")
    });
    let up = next_firecracker(&h, firecracker_that_sleeps(&h.fc));
    woke.join().unwrap().unwrap();

    // A clean shutdown commits the disk it ran all along.
    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert!(out.status.success(), "{notes}");
    assert!(notes.contains("dev resumed generation 1"), "{notes}");
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
    assert_eq!(up.api.calls().last().unwrap().0, "PUT /actions");
}

// After a Firecracker upgrade, say. The snapshot and its credentials go, the
// disk it was parked on stays, and the VM boots its last committed disk.
#[test]
fn a_parked_vm_that_can_t_resume_boots_its_last_committed_disk() {
    let h = Host::new();
    parked(&h);
    let dev = h.vm_dir("dev");
    let boot = h.boot("dev");
    let refuse = |req: &Request| {
        if req.path == "/snapshot/load" {
            let fault = json!({"fault_message": "Cannot restore microvm state"});
            return (400, Some(fault.to_string()));
        }
        (204, None)
    };
    let _refused = next_firecracker(&h, refuse);
    let up = next_firecracker(&h, guest_that_shuts_down(&h.fc));
    assert_eq!(h.ready(), "READY=1");
    let calls = up.api.calls();
    assert_eq!(calls.first().unwrap().0, "PUT /boot-source");
    let drive = calls.iter().find(|c| c.0 == "PUT /drives/rootfs").unwrap();
    assert_eq!(
        drive.1["path_on_host"],
        json!(dev.join("gens/2.ext4")),
        "not a fresh copy of the current disk"
    );
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    assert!(!dev.join("snapshot/mem").exists());
    assert!(dev.join("gens/1.ext4").exists());
    // A cold boot: its guest asks for its credentials again.
    assert_eq!(h.store.handed("dev").unwrap(), None);

    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert!(out.status.success(), "{notes}");
    assert!(notes.contains("dev couldn't resume"), "{notes}");
    assert!(notes.contains("Cannot restore microvm state"), "{notes}");
    assert!(
        notes.contains("generation 1, where it was parked, is kept for a look"),
        "{notes}"
    );
    assert_eq!(h.store.get("dev").unwrap().generation, 2);
}

// A Firecracker that never comes up loaded nothing: the boot fails, and the
// VM stays parked for the next start.
#[test]
fn a_parked_vm_stays_parked_if_firecracker_doesn_t_come_up() {
    let h = Host::new();
    parked(&h);
    let boot = h.boot("dev");
    h.fc.next();
    h.fc.exit_with(1);
    let out = finish(boot);
    let notes = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{notes}");
    assert!(notes.contains("stays parked"), "{notes}");
    let slept = h.store.sleeping("dev").unwrap().expect("no snapshot");
    assert!(slept.parked);
    assert!(h.vm_dir("dev").join("snapshot/mem").exists());
    assert!(h.vm_dir("dev").join("gens/1.ext4").exists());
    assert!(!h.vm_dir("dev").join("gens/2.ext4").exists());
    assert!(h.store.handed("dev").unwrap().is_some());
}

// Only a sleeping VM is parked. An awake one is left as it was, and its
// unit's stop shuts it down cleanly, as ever.
#[test]
fn an_awake_vm_isn_t_parked_and_shuts_down_as_ever() {
    let h = Host::new();
    let (boot, _up) = booted(&h);
    let park = h.runner(&["park", "dev"]).output().unwrap();
    assert!(!park.status.success());
    let err = text(&park.stderr);
    assert!(err.contains("dev is awake"), "{err}");
    let all = h.runner(&["park", "--all"]).output().unwrap();
    assert!(all.status.success(), "{}", text(&all.stderr));
    assert!(text(&all.stdout).starts_with("dev  not parked: dev is awake"));
    let halt = h.runner(&["halt", "dev"]).output().unwrap();
    assert!(halt.status.success(), "{}", text(&halt.stderr));
    assert!(finish(boot).status.success());
    assert_eq!(h.store.get("dev").unwrap().generation, 1);
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
}

// Sleep when idle (src/idle.rs). Boot looks as often as the VM's idle time,
// and at least every 10 seconds, so these give it a second.

fn slept(h: &Host, up: &Up) -> bool {
    h.store.sleeping("dev").unwrap().is_some()
        && up.api.calls().iter().any(|c| c.0 == "PUT /snapshot/create")
}

#[test]
fn an_idle_vm_sleeps_on_its_own_once_no_terminal_is_on_it() {
    let h = Host::new();
    h.store.set_idle("dev", Idle::After(1)).unwrap();
    let (mut boot, up) = booted(&h);
    let fc = h.fc.pid();
    wait_for("dev to sleep", || slept(&h, &up));
    wait_for("its firecracker to stop", || !alive(fc));
    // The guest was asked, as root, whether a terminal is on a session.
    let asked = up.asked.lock().unwrap().clone();
    let attached = |r: &proto::Request| r.argv == sessions::ATTACHED && r.user.is_none();
    assert!(asked.iter().any(attached), "{asked:?}");
    assert!(boot.try_wait().unwrap().is_none(), "boot ended");
    boot.kill().unwrap();
    let out = boot.wait_with_output().unwrap();
    let notes = text(&out.stderr);
    assert!(notes.contains("dev went unused for 1s"), "{notes}");
}

// A command holds the VM from before it looks at it until it's done, however
// long that is.
#[test]
fn a_vm_in_use_stays_awake_until_it_is_let_go() {
    let h = Host::new();
    h.store.set_idle("dev", Idle::After(1)).unwrap();
    let used = InUse::take(&h.store, "dev").unwrap();
    let (mut boot, up) = booted(&h);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!slept(&h, &up), "slept while in use");
    drop(used);
    wait_for("dev to sleep", || slept(&h, &up));
    boot.kill().unwrap();
    boot.wait().unwrap();
}

#[test]
fn a_vm_that_never_idles_stays_awake() {
    let h = Host::new();
    h.store.set_idle("dev", Idle::Never).unwrap();
    let (mut boot, up) = booted(&h);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!slept(&h, &up), "slept");
    // Read at each look, so a new idle time counts from the next, which
    // never puts 10 seconds off.
    h.store.set_idle("dev", Idle::After(1)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !slept(&h, &up) {
        assert!(Instant::now() < deadline, "still awake");
        std::thread::sleep(Duration::from_millis(50));
    }
    boot.kill().unwrap();
    boot.wait().unwrap();
}

// A run is use, and a sleep is refused while one goes besides: either way,
// the VM stays awake until it's over.
#[test]
fn a_run_keeps_the_vm_awake() {
    let h = Host::new();
    h.store.set_idle("dev", Idle::After(1)).unwrap();
    let (mut boot, up) = booted(&h);
    let run = runner::runs::hold(&h.store, "dev").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(!slept(&h, &up), "slept while a run held it");
    drop(run);
    wait_for("dev to sleep", || slept(&h, &up));
    boot.kill().unwrap();
    boot.wait().unwrap();
}
