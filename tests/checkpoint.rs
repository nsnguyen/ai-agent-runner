// Checkpoints and restores of a running VM, end to end through the CLI, with
// systemctl faked, a stand-in for its boot holding the disk generation it
// runs, a fake of Firecracker's API, and runner-guest on a Unix socket behind
// a fake of its vsock socket. No KVM needed; tests/kvm.rs has the real thing.
mod common;

use common::{FakeApi, FakeSystemd, FakeVsock, Guest, script, text};
use runner::store::Store;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const LIVE: &[u8] = b"written while running";

// dev, imported from a disk holding "at import", running generation 1, a copy
// of it that the guest has since written LIVE to. As under the unit, `systemctl
// stop` runs `runner halt dev`, whose Ctrl-Alt-Del has the stand-in commit
// generation 1, as a clean shutdown does.
struct Running {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
    api: FakeApi,
    vsock: Arc<FakeVsock>,
    /// The disk the boot runs.
    live: PathBuf,
    /// How many connections the guest had taken when the VM was paused.
    synced: Arc<Mutex<Option<usize>>>,
    _guest: Guest,
}

fn running() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"at import").unwrap();
    store.import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state("active");
    script(
        &sd.bin.join("on-stop"),
        &format!("exec '{}' halt dev\n", env!("CARGO_BIN_EXE_runner")),
    );
    let guest = Guest::spawn(&dir.path().join("guest.sock"));
    let vsock = Arc::new(FakeVsock::serve(
        &dir.path().join("vms/dev/vsock.sock"),
        &guest.sock,
    ));
    let run = store.begin_run("dev").unwrap();
    let live = run.disk.clone();
    fs::write(&live, LIVE).unwrap();
    let run = Mutex::new(Some(run));
    let synced = Arc::new(Mutex::new(None));
    let (seen, guest_conns) = (synced.clone(), vsock.clone());
    let api = FakeApi::serve(&dir.path().join("vms/dev/api.sock"), move |req| {
        if req.body == json!({"state": "Paused"}) {
            *seen.lock().unwrap() = Some(guest_conns.connects().len());
        }
        if req.body == json!({"action_type": "SendCtrlAltDel"})
            && let Some(run) = run.lock().unwrap().take()
        {
            run.commit().unwrap();
        }
        (204, None)
    });
    Running {
        dir,
        store,
        sd,
        api,
        vsock,
        live,
        synced,
        _guest: guest,
    }
}

impl Running {
    fn runner(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", self.dir.path())
            .env("PATH", self.sd.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.runner(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.runner(args);
        assert!(!out.status.success(), "{args:?}: {}", text(&out.stdout));
        text(&out.stderr)
    }

    fn disk(&self, generation: u64) -> Vec<u8> {
        fs::read(
            self.dir
                .path()
                .join(format!("vms/dev/gens/{generation}.ext4")),
        )
        .unwrap()
    }

    fn stops_and_starts(&self) -> Vec<String> {
        let calls = self.sd.calls().into_iter();
        let verbs = calls.filter_map(|c| {
            let verb = c.strip_prefix("systemctl ")?.split(' ').next()?.to_string();
            (verb == "stop" || verb == "start").then_some(verb)
        });
        verbs.collect()
    }

    fn api_calls(&self) -> Vec<(String, Value)> {
        self.api.calls()
    }
}

#[test]
fn a_running_vm_s_checkpoint_is_its_live_disk_copied_while_paused() {
    let vm = running();
    let out = vm.ok(&["checkpoint", "dev", "-m", "mid-run"]);
    assert!(out.starts_with("  2  "), "{out}");
    assert!(
        out.ends_with("  checkpoint  21 bytes  \"mid-run\"\n"),
        "{out}"
    );
    assert_eq!(vm.disk(2), LIVE);
    // The guest synced its disks, then the VM was paused for the copy only.
    assert_eq!(vm.vsock.connects(), ["CONNECT 10022"]);
    assert_eq!(*vm.synced.lock().unwrap(), Some(1));
    assert_eq!(
        vm.api_calls(),
        [
            ("PATCH /vm".to_string(), json!({"state": "Paused"})),
            ("PATCH /vm".to_string(), json!({"state": "Resumed"})),
        ]
    );
    // The VM carries on from its own disk: the boot's generation isn't
    // current until it shuts down, and the copy is listed apart from it.
    assert_eq!(vm.store.get("dev").unwrap().generation, 0);
    let listed = vm.ok(&["checkpoints", "dev"]);
    let ids: Vec<&str> = listed
        .lines()
        .map(|l| l.split_whitespace().find(|w| *w != "*").unwrap())
        .collect();
    assert_eq!(ids, ["2", "0"], "{listed}");
    assert!(vm.stops_and_starts().is_empty());
}

#[test]
fn checkpoints_of_a_running_vm_take_turns() {
    let vm = running();
    let vm = Arc::new(vm);
    let both: Vec<_> = (0..2)
        .map(|_| {
            let vm = vm.clone();
            std::thread::spawn(move || vm.ok(&["checkpoint", "dev"]))
        })
        .collect();
    let mut ids: Vec<String> = both
        .into_iter()
        .map(|t| {
            t.join()
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    ids.sort();
    assert_eq!(ids, ["2", "3"]);
    assert_eq!((vm.disk(2), vm.disk(3)), (LIVE.to_vec(), LIVE.to_vec()));
    let states: Vec<Value> = vm.api_calls().into_iter().map(|(_, b)| b).collect();
    let paused = json!({"state": "Paused"});
    let resumed = json!({"state": "Resumed"});
    assert_eq!(states, [paused.clone(), resumed.clone(), paused, resumed]);
}

#[test]
fn a_vm_booting_or_stopping_isn_t_checkpointed() {
    let vm = running();
    vm.sd.set_state("activating");
    let why = vm.fails(&["checkpoint", "dev"]);
    assert!(why.contains("dev is booting"), "{why}");
    vm.sd.set_state("deactivating");
    let why = vm.fails(&["checkpoint", "dev"]);
    assert!(why.contains("dev is stopping"), "{why}");
    assert!(vm.api_calls().is_empty());
}

#[test]
fn restoring_a_running_vm_stops_it_restores_and_starts_it_again() {
    let vm = running();
    vm.ok(&["checkpoint", "dev", "-m", "before"]);
    // The VM writes on after its checkpoint.
    fs::write(&vm.live, b"after the checkpoint").unwrap();

    // Its stop commits generation 1, and the copy of the checkpoint goes on
    // top of that.
    let out = vm.ok(&["restore", "dev", "2"]);
    assert_eq!(out, "dev  gen 3  21 bytes\n");
    assert_eq!(vm.disk(3), LIVE);
    assert_eq!(vm.stops_and_starts(), ["stop", "start"]);
    let calls = vm.api_calls();
    assert_eq!(
        calls.last().unwrap(),
        &(
            "PUT /actions".to_string(),
            json!({"action_type": "SendCtrlAltDel"})
        )
    );

    // undo goes back to the disk as the VM left it.
    let out = vm.ok(&["undo", "dev"]);
    assert_eq!(out, "dev  gen 1  20 bytes\n");
    assert_eq!(vm.disk(1), b"after the checkpoint");
}

#[test]
fn restoring_a_missing_checkpoint_leaves_a_running_vm_running() {
    let vm = running();
    let why = vm.fails(&["restore", "dev", "9"]);
    assert!(why.contains("dev has no checkpoint 9"), "{why}");
    assert!(vm.stops_and_starts().is_empty());
    assert!(vm.api_calls().is_empty());
}

#[test]
fn restore_is_refused_while_a_run_is_going() {
    let vm = running();
    vm.ok(&["checkpoint", "dev"]);
    // What `runner drive` holds for as long as its run goes on.
    let runs = vm.dir.path().join("vms/dev/runs");
    fs::create_dir_all(&runs).unwrap();
    let lock = File::create(runs.join("lock")).unwrap();
    lock.lock().unwrap();
    let why = vm.fails(&["restore", "dev", "2"]);
    assert!(why.contains("dev already has a run"), "{why}");
    assert!(vm.stops_and_starts().is_empty());
    // A checkpoint doesn't stop the run, so it goes ahead.
    vm.ok(&["checkpoint", "dev"]);
}
