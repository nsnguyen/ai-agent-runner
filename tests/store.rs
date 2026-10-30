// The VM store on a temp directory: no KVM needed.
use runner::store::{Error, Store, Vm, check_name};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root"));
    (dir, store)
}

fn disk(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("disk.ext4");
    fs::write(&path, bytes).unwrap();
    path
}

fn vm(name: &str, generation: u64, bytes: u64) -> Vm {
    let name = name.to_string();
    Vm {
        name,
        generation,
        bytes,
    }
}

#[test]
fn import_makes_a_vm_at_generation_zero() {
    let (dir, store) = store();
    let got = store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    assert_eq!(got, vm("base", 0, 6));
    assert_eq!(store.list().unwrap(), vec![vm("base", 0, 6)]);
}

#[test]
fn list_is_empty_before_anything_exists() {
    let (_dir, store) = store();
    assert_eq!(store.list().unwrap(), vec![]);
}

#[test]
fn fork_copies_the_current_disk_and_shares_nothing_after() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    assert_eq!(store.fork("base", "dev").unwrap(), vm("dev", 0, 6));

    let run = store.begin_run("dev").unwrap();
    fs::write(&run.disk, b"changed by dev").unwrap();
    run.commit().unwrap();
    assert_eq!(store.get("base").unwrap(), vm("base", 0, 6));
    assert_eq!(store.get("dev").unwrap(), vm("dev", 1, 14));
}

#[test]
fn names_are_taken_once() {
    let (dir, store) = store();
    let d = disk(dir.path(), b"rootfs");
    store.import("base", &d).unwrap();
    assert!(matches!(store.import("base", &d), Err(Error::Exists(n)) if n == "base"));
    assert!(matches!(store.fork("base", "base"), Err(Error::Exists(_))));
}

#[test]
fn missing_vms_are_named_in_the_error() {
    let (_dir, store) = store();
    assert!(matches!(store.fork("nope", "dev"), Err(Error::NoVm(n)) if n == "nope"));
    assert!(matches!(store.get("nope"), Err(Error::NoVm(_))));
    assert!(matches!(store.begin_run("nope"), Err(Error::NoVm(_))));
}

#[test]
fn a_committed_run_becomes_current() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    assert_eq!(run.generation, 1);
    assert_eq!(fs::read(&run.disk).unwrap(), b"v0");
    fs::write(&run.disk, b"v1!").unwrap();
    assert_eq!(run.commit().unwrap(), vm("dev", 1, 3));
}

#[test]
fn an_abandoned_run_leaves_the_current_disk_alone() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    {
        let run = store.begin_run("dev").unwrap();
        fs::write(&run.disk, b"half-written").unwrap();
    }
    assert_eq!(store.get("dev").unwrap(), vm("dev", 0, 2));
    // The failed generation stays for a look, and the next run gets a new number.
    let run = store.begin_run("dev").unwrap();
    assert_eq!(run.generation, 2);
    assert_eq!(fs::read(&run.disk).unwrap(), b"v0");
}

#[test]
fn one_run_at_a_time() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    assert!(matches!(store.begin_run("dev"), Err(Error::Busy(n)) if n == "dev"));
    assert!(matches!(store.undo("dev"), Err(Error::Busy(_))));
    run.commit().unwrap();
    store.begin_run("dev").unwrap();
}

#[test]
fn another_process_holds_the_lock_until_it_dies() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let lock = dir.path().join("root/vms/dev/lock");
    // --no-fork: the process we kill is the one holding the lock.
    let mut holder = std::process::Command::new("flock")
        .arg("--no-fork")
        .arg(&lock)
        .args(["sleep", "60"])
        .spawn()
        .unwrap();
    let held = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        matches!(store.begin_run("dev"), Err(Error::Busy(_)))
    });
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(held, "the other process's lock was never seen");
    // It died without unlocking; the kernel let go for it.
    store.begin_run("dev").unwrap();
}

#[test]
fn undo_steps_back_one_committed_generation() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    for body in [&b"v1"[..], b"v2!"] {
        let run = store.begin_run("dev").unwrap();
        fs::write(&run.disk, body).unwrap();
        run.commit().unwrap();
    }
    assert_eq!(store.undo("dev").unwrap(), vm("dev", 1, 2));
    assert_eq!(store.undo("dev").unwrap(), vm("dev", 0, 2));
    assert!(matches!(store.undo("dev"), Err(Error::NothingToUndo(_))));
}

#[test]
fn names_must_be_one_plain_path_part() {
    for good in ["base", "dev-2", "a", "0day"] {
        check_name(good).unwrap();
    }
    let long = "a".repeat(33);
    for bad in [
        "",
        "-x",
        "Dev",
        "a/b",
        "..",
        ".",
        "a b",
        "x.ext4",
        long.as_str(),
    ] {
        assert!(matches!(check_name(bad), Err(Error::BadName(_))), "{bad:?}");
    }
    let (dir, store) = store();
    let d = disk(dir.path(), b"v0");
    assert!(matches!(
        store.import("../escape", &d),
        Err(Error::BadName(_))
    ));
}

#[test]
fn an_interrupted_create_does_not_take_the_name() {
    let (dir, store) = store();
    // What a create killed partway leaves: its staging directory, from a dead pid.
    let staged = dir.path().join("root/vms/.new-dev-999999999");
    fs::create_dir_all(staged.join("gens")).unwrap();
    assert_eq!(store.list().unwrap(), vec![]);
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    assert!(!staged.exists(), "stale staging directory left behind");
}

#[test]
fn a_broken_vm_does_not_hide_the_others() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"v0")).unwrap();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    fs::write(dir.path().join("root/vms/dev/history"), "").unwrap();
    let names: Vec<String> = store.list().unwrap().into_iter().map(|v| v.name).collect();
    assert_eq!(names, ["base"]);
}

#[test]
fn copies_keep_holes_and_are_writable() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (dir, store) = store();
    let src = dir.path().join("sparse.ext4");
    let f = fs::File::create(&src).unwrap();
    f.set_len(64 << 20).unwrap();
    drop(f);
    fs::set_permissions(&src, fs::Permissions::from_mode(0o444)).unwrap();
    store.import("base", &src).unwrap();
    let run = store.begin_run("base").unwrap();
    let meta = fs::metadata(&run.disk).unwrap();
    assert_eq!(meta.len(), 64 << 20);
    assert!(
        meta.blocks() * 512 < 1 << 20,
        "64 MB of holes became {} bytes",
        meta.blocks() * 512
    );
    fs::write(&run.disk, b"agent was here").unwrap();
}

mod props {
    use super::*;
    use proptest::prelude::*;
    use std::path::Component;

    proptest! {
        // Anything accepted is safe to join onto the store's directory.
        #[test]
        // Half the inputs look like names, so the accepted branch runs often.
        fn accepted_names_stay_inside_the_store(
            name in prop_oneof!["[a-z0-9-]{1,33}", "[a-zA-Z0-9./ -]{0,40}"]
        ) {
            if check_name(&name).is_ok() {
                let parts: Vec<_> = Path::new(&name).components().collect();
                prop_assert_eq!(parts.len(), 1);
                prop_assert!(matches!(parts[0], Component::Normal(_)));
            }
        }

        #[test]
        fn plain_names_are_accepted(name in "[a-z0-9][a-z0-9-]{0,31}") {
            prop_assert!(check_name(&name).is_ok());
        }
    }
}

#[test]
fn delete_removes_the_vm_and_every_generation() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    let run = store.begin_run("dev").unwrap();
    run.commit().unwrap();
    // A failed run's disk is left on disk too.
    drop(store.begin_run("dev").unwrap());

    assert_eq!(store.delete("dev").unwrap(), vm("dev", 1, 6));
    assert_eq!(store.list().unwrap(), vec![vm("base", 0, 6)]);
    let vms = dir.path().join("root/vms");
    let left: Vec<_> = fs::read_dir(vms).unwrap().collect();
    assert_eq!(left.len(), 1, "{left:?}");
    // The name is free again.
    assert_eq!(store.fork("base", "dev").unwrap(), vm("dev", 0, 6));
}

#[test]
fn delete_leaves_forks_alone() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    store.delete("base").unwrap();
    assert_eq!(store.get("dev").unwrap(), vm("dev", 0, 6));
}

#[test]
fn delete_refuses_a_busy_vm_and_an_unknown_one() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    let run = store.begin_run("base").unwrap();
    assert!(matches!(store.delete("base"), Err(Error::Busy(n)) if n == "base"));
    drop(run);
    assert!(matches!(store.delete("nope"), Err(Error::NoVm(n)) if n == "nope"));
    assert!(matches!(store.delete("../etc"), Err(Error::BadName(_))));
    assert_eq!(store.get("base").unwrap(), vm("base", 0, 6));
}

#[test]
fn a_create_sweeps_a_delete_that_died_but_not_a_live_one() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    let vms = dir.path().join("root/vms");
    let dead = vms.join(".del-gone-1-4000000000");
    let live = vms.join(format!(".del-live-1-{}", std::process::id()));
    fs::create_dir(&dead).unwrap();
    fs::create_dir(&live).unwrap();
    store.fork("base", "dev").unwrap();
    assert!(!dead.exists());
    assert!(live.exists());
    assert_eq!(store.list().unwrap().len(), 2);
}

#[test]
fn deleting_twice_in_one_process_works() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    for _ in 0..2 {
        store.fork("base", "dev").unwrap();
        store.delete("dev").unwrap();
    }
}

fn slot(store: &Store, name: &str) -> u32 {
    store.net(name).unwrap().slot
}

#[test]
fn every_vm_gets_its_own_slot_and_a_fork_does_not_share_its_source_s() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    store.fork("dev", "dev-2").unwrap();
    assert_eq!(slot(&store, "base"), 0);
    assert_eq!(slot(&store, "dev"), 1);
    assert_eq!(slot(&store, "dev-2"), 2);
    let net = store.net("dev").unwrap();
    assert_eq!(
        (net.tap.as_str(), net.address.to_string()),
        ("fctap1", "172.16.0.6".into())
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("root/vms/dev/slot")).unwrap(),
        "1\n"
    );
    // A slot sticks: undo, runs and reading it again don't move it.
    store.begin_run("dev").unwrap().commit().unwrap();
    store.undo("dev").unwrap();
    assert_eq!(slot(&store, "dev"), 1);
}

#[test]
fn delete_frees_a_slot_for_the_next_create() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    for name in ["a", "b", "c"] {
        store.fork("base", name).unwrap();
    }
    store.delete("b").unwrap();
    store.fork("base", "d").unwrap();
    assert_eq!(slot(&store, "d"), 2);
    store.fork("base", "e").unwrap();
    assert_eq!(slot(&store, "e"), 4);
}

#[test]
fn creates_at_once_never_share_a_slot() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    std::thread::scope(|s| {
        for i in 0..8 {
            let store = &store;
            s.spawn(move || store.fork("base", &format!("vm-{i}")).unwrap());
        }
    });
    let mut slots: Vec<u32> = (0..8).map(|i| slot(&store, &format!("vm-{i}"))).collect();
    slots.sort();
    assert_eq!(slots, (1..=8).collect::<Vec<_>>());
}

#[test]
fn a_create_in_flight_holds_its_slot() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    // A create still copying its disk, in this live process.
    let staged = dir
        .path()
        .join(format!("root/vms/.new-dev-{}", std::process::id()));
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("slot"), "1\n").unwrap();
    store.fork("base", "other").unwrap();
    assert_eq!(slot(&store, "other"), 2);
}

#[test]
fn the_subnet_is_host_config_and_a_full_one_refuses_a_create() {
    let (dir, store) = store();
    fs::create_dir_all(dir.path().join("root")).unwrap();
    fs::write(dir.path().join("root/subnet"), "10.9.0.0/29\n").unwrap();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    assert_eq!(store.net("dev").unwrap().address.to_string(), "10.9.0.6");
    let err = store.fork("base", "more").unwrap_err();
    assert!(matches!(err, Error::NoSlot(_)), "{err:?}");
    assert!(err.to_string().contains("10.9.0.0/29"), "{err}");
    // The refused create left nothing behind.
    assert!(matches!(store.get("more"), Err(Error::NoVm(_))));
    let vms: Vec<_> = fs::read_dir(dir.path().join("root/vms")).unwrap().collect();
    assert_eq!(vms.len(), 2, "{vms:?}");

    fs::write(dir.path().join("root/subnet"), "10.9.0.1/29\n").unwrap();
    let err = store.net("dev").unwrap_err();
    assert!(err.to_string().contains("subnet"), "{err}");
}

#[test]
fn a_vm_made_before_slots_existed_gets_one_when_asked() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    fs::remove_file(dir.path().join("root/vms/base/slot")).unwrap();
    assert_eq!(slot(&store, "base"), 0);
    fs::remove_file(dir.path().join("root/vms/dev/slot")).unwrap();
    fs::remove_file(dir.path().join("root/vms/base/slot")).unwrap();
    assert_eq!(slot(&store, "dev"), 0);
    assert_eq!(slot(&store, "base"), 1);
    assert!(matches!(store.net("nope"), Err(Error::NoVm(_))));
}

mod slot_props {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        // However creates and deletes interleave, live VMs never share a slot,
        // and a create takes the lowest one free.
        #[test]
        fn live_vms_never_share_a_slot(ops in proptest::collection::vec((any::<bool>(), 0u8..6), 1..16)) {
            let (dir, store) = store();
            let d = disk(dir.path(), b"rootfs");
            let mut live: BTreeMap<String, u32> = BTreeMap::new();
            for (create, i) in ops {
                let name = format!("vm-{i}");
                if create && !live.contains_key(&name) {
                    store.import(&name, &d).unwrap();
                    let got = slot(&store, &name);
                    let lowest = (0..).find(|s| !live.values().any(|v| v == s)).unwrap();
                    prop_assert_eq!(got, lowest);
                    live.insert(name, got);
                } else if !create && live.remove(&name).is_some() {
                    store.delete(&name).unwrap();
                }
                for (name, s) in &live {
                    prop_assert_eq!(slot(&store, name), *s);
                }
            }
        }
    }
}
