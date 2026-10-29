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
fn a_lock_file_left_by_a_dead_run_does_not_block() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    // A run that crashed leaves the file, but the kernel already dropped its lock.
    fs::write(dir.path().join("root/vms/dev/lock"), "").unwrap();
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
fn a_half_made_vm_is_not_listed() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"v0")).unwrap();
    // A create that died before writing history leaves a bare directory.
    fs::create_dir_all(dir.path().join("root/vms/broken/gens")).unwrap();
    let names: Vec<String> = store.list().unwrap().into_iter().map(|v| v.name).collect();
    assert_eq!(names, ["base"]);
}

mod props {
    use super::*;
    use proptest::prelude::*;
    use std::path::Component;

    proptest! {
        // Anything accepted is safe to join onto the store's directory.
        #[test]
        fn accepted_names_stay_inside_the_store(name in ".{0,40}") {
            if check_name(&name).is_ok() {
                let parts: Vec<_> = Path::new(&name).components().collect();
                prop_assert_eq!(parts.len(), 1);
                prop_assert!(matches!(parts[0], Component::Normal(_)));
            }
        }
    }
}
