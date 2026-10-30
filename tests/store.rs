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

// What a test expects of a VM. Space used hangs on the filesystem's block
// size, so it has tests of its own and these compare the rest.
#[derive(Debug)]
struct Shown(&'static str, u64, u64);

impl PartialEq<Shown> for Vm {
    fn eq(&self, s: &Shown) -> bool {
        (self.name.as_str(), self.generation, self.bytes) == (s.0, s.1, s.2)
    }
}

fn vm(name: &'static str, generation: u64, bytes: u64) -> Shown {
    Shown(name, generation, bytes)
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
    assert!(store.list().unwrap().is_empty());
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
    assert!(store.list().unwrap().is_empty());
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

// gc: the current generation, commented checkpoints and the newest few others
// stay; everything else on disk goes.

// A store that keeps everything, for building up generations to collect.
fn hoarder(dir: &TempDir) -> Store {
    Store::new(dir.path().join("root")).keeping(usize::MAX)
}

// `dev` at generation `n`, with every generation from 0 committed.
fn committed(dir: &TempDir, n: u64) -> Store {
    let store = hoarder(dir);
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    for g in 1..=n {
        let run = store.begin_run("dev").unwrap();
        fs::write(&run.disk, format!("v{g}")).unwrap();
        run.commit().unwrap();
    }
    store
}

// dev's disks, by generation.
fn on_disk(dir: &Path) -> Vec<u64> {
    let gens = dir.join("root/vms/dev/gens");
    let mut out: Vec<u64> = fs::read_dir(gens)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".ext4")?.parse().ok()
        })
        .collect();
    out.sort();
    out
}

fn undo_all(store: &Store) -> Vec<u64> {
    let mut seen = Vec::new();
    while let Ok(vm) = store.undo("dev") {
        seen.push(vm.generation);
    }
    seen
}

#[test]
fn gc_keeps_the_current_generation_and_the_newest_others() {
    let dir = tempfile::tempdir().unwrap();
    committed(&dir, 6);
    let store = Store::new(dir.path().join("root")).keeping(2);
    let freed = store.gc("dev", false).unwrap();
    assert_eq!(freed.name, "dev");
    assert_eq!(freed.generations, [0, 1, 2, 3]);
    assert_eq!(on_disk(dir.path()), [4, 5, 6]);
    assert_eq!(store.get("dev").unwrap(), vm("dev", 6, 2));
    // History lost the deleted ones, so undo stops at the oldest disk left.
    assert_eq!(undo_all(&store), [5, 4]);
}

#[test]
fn gc_keeps_every_checkpoint_with_a_comment() {
    let dir = tempfile::tempdir().unwrap();
    committed(&dir, 6);
    let gens = dir.path().join("root/vms/dev/gens");
    fs::write(gens.join("1.comment"), "before the upgrade\n").unwrap();
    let store = Store::new(dir.path().join("root")).keeping(2);
    assert_eq!(store.gc("dev", false).unwrap().generations, [0, 2, 3]);
    assert_eq!(on_disk(dir.path()), [1, 4, 5, 6]);
    assert!(gens.join("1.comment").is_file());
    assert_eq!(undo_all(&store), [5, 4, 1]);
}

#[test]
fn gc_keeps_five_others_unless_told() {
    let dir = tempfile::tempdir().unwrap();
    committed(&dir, 8);
    let store = Store::new(dir.path().join("root"));
    assert_eq!(store.gc("dev", false).unwrap().generations, [0, 1, 2]);
    assert_eq!(on_disk(dir.path()), [3, 4, 5, 6, 7, 8]);
}

#[test]
fn gc_keeps_undo_history_ahead_of_failed_runs_disks() {
    let dir = tempfile::tempdir().unwrap();
    let store = committed(&dir, 1);
    drop(store.begin_run("dev").unwrap());
    drop(store.begin_run("dev").unwrap());
    // Room for the one undo and the newest failed disk.
    let store = Store::new(dir.path().join("root")).keeping(2);
    assert_eq!(store.gc("dev", false).unwrap().generations, [2]);
    assert_eq!(on_disk(dir.path()), [0, 1, 3]);
    let store = store.keeping(1);
    assert_eq!(store.gc("dev", false).unwrap().generations, [3]);
    assert_eq!(undo_all(&store), [0]);
}

#[test]
fn crashed_runs_never_cost_undo_history() {
    let dir = tempfile::tempdir().unwrap();
    let hoard = committed(&dir, 3);
    for _ in 0..5 {
        drop(hoard.begin_run("dev").unwrap());
    }
    let store = Store::new(dir.path().join("root"));
    store.begin_run("dev").unwrap().commit().unwrap();
    assert_eq!(on_disk(dir.path()), [0, 1, 2, 3, 8, 9]);
    assert_eq!(undo_all(&store), [3, 2, 1, 0]);
}

#[test]
fn gc_dry_run_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    committed(&dir, 6);
    let history = dir.path().join("root/vms/dev/history");
    let before = fs::read_to_string(&history).unwrap();
    let store = Store::new(dir.path().join("root")).keeping(2);
    let would = store.gc("dev", true).unwrap();
    assert_eq!(would.generations, [0, 1, 2, 3]);
    assert!(would.bytes > 0);
    assert_eq!(on_disk(dir.path()), [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(fs::read_to_string(&history).unwrap(), before);
    // The real thing frees just what the dry run said.
    assert_eq!(store.gc("dev", false).unwrap(), would);
}

#[test]
fn gc_counts_what_it_frees_in_allocated_blocks() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    committed(&dir, 3);
    let gens = dir.path().join("root/vms/dev/gens");
    let blocks = |g: u64| {
        fs::metadata(gens.join(format!("{g}.ext4")))
            .unwrap()
            .blocks()
            * 512
    };
    let want = blocks(0) + blocks(1);
    let store = Store::new(dir.path().join("root")).keeping(1);
    let freed = store.gc("dev", false).unwrap();
    assert_eq!(freed.generations, [0, 1]);
    assert_eq!(freed.bytes, want);
}

#[test]
fn gc_leaves_alone_a_vm_a_run_holds() {
    let dir = tempfile::tempdir().unwrap();
    let hoard = committed(&dir, 6);
    let run = hoard.begin_run("dev").unwrap();
    fs::write(&run.disk, b"in progress").unwrap();
    let store = Store::new(dir.path().join("root")).keeping(0);
    assert!(matches!(store.gc("dev", false), Err(Error::Busy(n)) if n == "dev"));
    assert!(matches!(store.gc("dev", true), Err(Error::Busy(_))));
    assert_eq!(on_disk(dir.path()), [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(fs::read(&run.disk).unwrap(), b"in progress");
    run.commit().unwrap();
    assert_eq!(
        store.gc("dev", false).unwrap().generations,
        [0, 1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn every_commit_collects_what_the_store_does_not_keep() {
    let (dir, store) = store();
    let store = store.keeping(2);
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    for _ in 0..5 {
        store.begin_run("dev").unwrap().commit().unwrap();
    }
    assert_eq!(on_disk(dir.path()), [3, 4, 5]);
}

#[test]
fn gc_refuses_an_unknown_vm() {
    let (_dir, store) = store();
    assert!(matches!(store.gc("nope", false), Err(Error::NoVm(n)) if n == "nope"));
    assert!(matches!(store.gc("../etc", true), Err(Error::BadName(_))));
}

#[test]
fn used_is_allocated_blocks_of_every_generation_not_apparent_size() {
    use std::os::unix::fs::MetadataExt;
    let (dir, store) = store();
    let src = dir.path().join("sparse.ext4");
    fs::File::create(&src).unwrap().set_len(64 << 20).unwrap();
    store.import("base", &src).unwrap();
    let run = store.begin_run("base").unwrap();
    fs::write(&run.disk, vec![7u8; 1 << 20]).unwrap();
    // Synced, so block counts don't move under the test.
    fs::File::open(&run.disk).unwrap().sync_all().unwrap();
    run.commit().unwrap();
    let got = store.get("base").unwrap();
    let gens = dir.path().join("root/vms/base/gens");
    let want: u64 = fs::read_dir(gens)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().blocks() * 512)
        .sum();
    assert_eq!(got.used, want);
    assert!(got.used < 4 << 20, "64 MB of holes counted: {}", got.used);
    assert_eq!(store.list().unwrap()[0].used, want);
}
