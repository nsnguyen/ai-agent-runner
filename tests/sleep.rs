// `runner sleep` and `wake`, and what a sleeping VM means to the rest of the
// CLI, with systemctl faked and a stand-in for the VM's boot on its socket.
// tests/boot.rs has boot's own side of sleep and wake, and tests/kvm.rs the
// real thing.
mod common;

use common::{FakeSystemd, StandIn, script, text};
use runner::sleep::Ask;
use runner::store::{Slept, Store};
use std::fs;
use std::process::Output;
use tempfile::TempDir;

struct Host {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
}

// dev, imported, and its unit active. As under the unit, `systemctl stop`
// runs `runner halt dev`.
fn host() -> Host {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    store.import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state("active");
    script(
        &sd.bin.join("on-stop"),
        &format!("exec '{}' halt dev\n", env!("CARGO_BIN_EXE_runner")),
    );
    Host { dir, store, sd }
}

impl Host {
    fn runner(&self, args: &[&str]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_runner"))
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

    // A whole snapshot of the generation its boot runs.
    fn asleep(&self, generation: u64) {
        let snap = self.store.snapshot("dev").unwrap();
        snap.make_room().unwrap();
        let at = chrono::Utc::now();
        snap.note(&Slept { generation, at }).unwrap();
    }
}

#[test]
fn sleep_and_wake_go_through_the_vm_s_boot() {
    let h = host();
    let boot = StandIn::serve(&h.store, "dev");
    assert!(h.ok(&["status", "dev"]).ends_with("  running\n"));

    let slept = h.ok(&["sleep", "dev"]);
    assert_eq!(slept, "dev  gen 0  6 bytes  sleeping\n");
    assert!(h.ok(&["status", "dev"]).ends_with("  sleeping\n"));
    let woke = h.ok(&["wake", "dev"]);
    assert_eq!(woke, "dev  gen 0  6 bytes  running\n");
    assert_eq!(boot.asked(), [Ask::Sleep, Ask::Wake]);
    // Neither touches systemd's state.
    let verbs: Vec<String> =
        h.sd.calls()
            .into_iter()
            .filter(|c| !c.contains(" show "))
            .collect();
    assert!(verbs.is_empty(), "{verbs:?}");
}

#[test]
fn asleep_already_or_awake_already_asks_nothing() {
    let h = host();
    let boot = StandIn::serve(&h.store, "dev");
    assert!(h.ok(&["wake", "dev"]).ends_with("  running\n"));
    h.asleep(1);
    assert!(h.ok(&["sleep", "dev"]).ends_with("  sleeping\n"));
    assert_eq!(boot.asked(), []);
}

#[test]
fn a_vm_that_isn_t_up_neither_sleeps_nor_wakes() {
    let h = host();
    h.sd.set_state("inactive");
    let err = h.fails(&["sleep", "dev"]);
    assert!(
        err.contains("dev is stopped: only a running VM sleeps"),
        "{err}"
    );
    let err = h.fails(&["wake", "dev"]);
    assert!(
        err.contains("dev is stopped, not sleeping: `runner start dev` boots it"),
        "{err}"
    );
    h.sd.set_state("deactivating");
    let err = h.fails(&["sleep", "dev"]);
    assert!(err.contains("dev is stopping"), "{err}");
}

#[test]
fn a_refusal_is_the_error() {
    let h = host();
    let boot = StandIn::serve(&h.store, "dev");
    *boot.refuse.lock().unwrap() = Some("run dev.3 is going in dev".to_string());
    let err = h.fails(&["sleep", "dev"]);
    assert!(err.contains("run dev.3 is going in dev"), "{err}");
}

#[test]
fn start_wakes_a_sleeping_vm() {
    let h = host();
    let boot = StandIn::serve(&h.store, "dev");
    h.asleep(1);
    let started = h.ok(&["start", "dev"]);
    assert_eq!(started, "dev  gen 0  6 bytes  running\n");
    assert_eq!(boot.asked(), [Ask::Wake]);
}

// Its boot holds the VM, as while it runs.
#[test]
fn undo_and_gc_refuse_a_sleeping_vm() {
    let h = host();
    let _boot = StandIn::serve(&h.store, "dev");
    h.asleep(1);
    let err = h.fails(&["undo", "dev"]);
    assert!(
        err.contains("dev is busy: it's running or sleeping"),
        "{err}"
    );
    let err = h.fails(&["gc", "dev"]);
    assert!(
        err.contains("dev is busy: it's running or sleeping"),
        "{err}"
    );
}

// The snapshot runs on the disk its boot runs, which a restore replaces: the
// VM is woken to shut down cleanly, restored, and started again, awake.
#[test]
fn restore_drops_the_snapshot_and_starts_the_vm_again() {
    let h = host();
    let boot = StandIn::serve(&h.store, "dev");
    h.asleep(1);
    let restored = h.ok(&["restore", "dev", "0"]);
    assert_eq!(restored, "dev  gen 2  6 bytes\n");
    assert_eq!(boot.asked(), [Ask::Halt]);
    assert_eq!(h.store.sleeping("dev").unwrap(), None);
    let verbs: Vec<String> =
        h.sd.calls()
            .into_iter()
            .filter(|c| !c.contains(" show "))
            .collect();
    assert_eq!(
        verbs,
        [
            "systemctl stop firecracker@dev.service",
            "systemctl start firecracker@dev.service"
        ]
    );
    // The boot's disk was committed at the stop, so undo goes back to it.
    assert_eq!(h.store.undo("dev").unwrap().generation, 1);
}
