// The VM store on a temp directory: no KVM needed.
use runner::store::{Checkpoint, Error, Store, Vm, check_name};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

// An flock belongs to the open file, and a child that another test's thread
// forks to run `cp` shares it until it execs, so a lock a test just dropped
// can still look held. The runner does store work on one thread, so the tests
// take turns, as it does.
static TURN: Mutex<()> = Mutex::new(());

struct Scratch {
    dir: TempDir,
    _turn: MutexGuard<'static, ()>,
}

impl Scratch {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn store() -> (Scratch, Store) {
    // A test that failed holding the turn shouldn't fail the rest.
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root"));
    (Scratch { dir, _turn: turn }, store)
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

fn gen_disk(dir: &Path, name: &str, generation: u64) -> PathBuf {
    dir.join(format!("root/vms/{name}/gens/{generation}.ext4"))
}

fn commit_run(store: &Store, name: &str, body: &[u8]) -> Vm {
    let run = store.begin_run(name).unwrap();
    fs::write(&run.disk, body).unwrap();
    run.commit().unwrap()
}

// Everything but the time: (id, auto, comment, current).
fn summary(cs: &[Checkpoint]) -> Vec<(u64, bool, Option<&str>, bool)> {
    cs.iter()
        .map(|c| (c.id, c.auto, c.comment.as_deref(), c.current))
        .collect()
}

#[test]
fn a_checkpoint_is_a_frozen_copy_with_its_comment() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let c = store.checkpoint("dev", Some("before upgrade")).unwrap();
    assert_eq!(
        summary(std::slice::from_ref(&c)),
        [(1, false, Some("before upgrade"), false)]
    );
    assert_eq!(c.bytes, 2);
    // The VM carries on from its own disk, and what it writes later leaves
    // the copy alone.
    assert_eq!(store.get("dev").unwrap(), vm("dev", 0, 2));
    commit_run(&store, "dev", b"v1!");
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 1)).unwrap(), b"v0");
    assert_eq!(store.checkpoint("dev", None).unwrap().comment, None);
}

#[test]
fn checkpoints_list_newest_first_with_what_runs_made_marked_auto() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    commit_run(&store, "dev", b"v1");
    store.checkpoint("dev", Some("a")).unwrap();
    commit_run(&store, "dev", b"v2");
    store.checkpoint("dev", None).unwrap();
    // A failed run's disk is no checkpoint.
    drop(store.begin_run("dev").unwrap());
    let listed = store.checkpoints("dev").unwrap();
    assert_eq!(
        summary(&listed),
        [
            (4, false, None, false),
            (3, true, Some("run"), true),
            (2, false, Some("a"), false),
            (1, true, Some("run"), false),
            (0, true, Some("import"), false),
        ]
    );
    assert!(listed.windows(2).all(|w| w[0].time >= w[1].time));
    store.fork("dev", "dev-2").unwrap();
    let forked = store.checkpoints("dev-2").unwrap();
    assert_eq!(summary(&forked), [(0, true, Some("fork of dev"), true)]);
}

#[test]
fn restoring_an_old_checkpoint_makes_it_current() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    store.checkpoint("dev", Some("clean")).unwrap();
    commit_run(&store, "dev", b"broken");
    assert_eq!(store.restore("dev", 1).unwrap(), vm("dev", 3, 2));
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 3)).unwrap(), b"v0");
    assert_eq!(
        summary(&store.checkpoints("dev").unwrap())[..2],
        [
            (3, true, Some("restore of 1"), true),
            (2, true, Some("run"), false)
        ]
    );
    // The VM gets a copy, so what it writes after a restore can't reach the
    // checkpoint, and restoring it again gives the same disk.
    fs::write(gen_disk(dir.path(), "dev", 3), b"scribbled").unwrap();
    assert_eq!(store.restore("dev", 1).unwrap(), vm("dev", 4, 2));
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 4)).unwrap(), b"v0");
    // What a run or an import made restores too.
    store.restore("dev", 2).unwrap();
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 5)).unwrap(), b"broken");
}

#[test]
fn undo_after_a_restore_brings_back_the_disk_before_it() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    store.checkpoint("dev", None).unwrap();
    commit_run(&store, "dev", b"v1!");
    store.restore("dev", 1).unwrap();
    assert_eq!(store.undo("dev").unwrap(), vm("dev", 2, 3));
    let ids: Vec<u64> = store
        .checkpoints("dev")
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, [2, 1, 0]);
}

#[test]
fn restoring_a_missing_id_fails() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let missing = |id| matches!(store.restore("dev", id), Err(Error::NoCheckpoint(n, i)) if n == "dev" && i == id);
    assert!(missing(7));
    // Neither a failed run's disk nor a generation undo stepped back from is one.
    drop(store.begin_run("dev").unwrap());
    assert!(missing(1));
    commit_run(&store, "dev", b"v2");
    store.undo("dev").unwrap();
    assert!(missing(2));
    assert!(matches!(store.restore("nope", 0), Err(Error::NoVm(_))));
    assert!(matches!(
        store.checkpoint("nope", None),
        Err(Error::NoVm(_))
    ));
    assert!(matches!(store.checkpoints("nope"), Err(Error::NoVm(_))));
    assert_eq!(store.get("dev").unwrap(), vm("dev", 0, 2));
}

#[test]
fn checkpoint_and_restore_refuse_a_busy_vm() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    store.checkpoint("dev", None).unwrap();
    let run = store.begin_run("dev").unwrap();
    assert!(matches!(store.checkpoint("dev", None), Err(Error::Busy(n)) if n == "dev"));
    assert!(matches!(store.restore("dev", 1), Err(Error::Busy(_))));
    // Reading the list doesn't need the lock.
    assert_eq!(store.checkpoints("dev").unwrap().len(), 2);
    drop(run);
}

#[test]
fn generations_without_notes_still_list() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    // As a VM made before generations had notes.
    fs::remove_file(dir.path().join("root/vms/dev/gens/0.json")).unwrap();
    let listed = store.checkpoints("dev").unwrap();
    assert_eq!(summary(&listed), [(0, true, None, true)]);
}
