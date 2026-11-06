// The VM store on a temp directory: no KVM needed.
use chrono::SubsecRound;
use runner::creds::{Cred, DEFAULT};
use runner::dns::{Hostname, List};
use runner::store::{
    Booted, Checkpoint, Config, Error, KEEP_CHECKPOINTS, Machine, Maker, Slept, Source, Store, Vm,
    check_name, parse_mem,
};
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
    // The host's address range comes from a file in the temp directory, not
    // the real /etc/runner/net, which these tests never create.
    let store = Store::new(dir.path().join("root")).with_net_file(dir.path().join("net"));
    (Scratch { dir, _turn: turn }, store)
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

// So a headless run, in another process, knows which disk it ran on.
#[test]
fn begin_run_notes_the_generations_of_the_boot() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    assert_eq!(store.booted("dev").unwrap(), None);
    let run = store.begin_run("dev").unwrap();
    let booted = Booted {
        from: 0,
        generation: 1,
    };
    assert_eq!(store.booted("dev").unwrap(), Some(booted));
    assert_eq!(store.disk("dev", 1).unwrap(), run.disk);
    run.commit().unwrap();
    let next = store.begin_run("dev").unwrap();
    assert_eq!((next.from, next.generation), (1, 2));
    let booted = Booted {
        from: 1,
        generation: 2,
    };
    assert_eq!(store.booted("dev").unwrap(), Some(booted));
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

#[test]
fn a_vm_without_sizes_takes_the_hosts_defaults() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"v0")).unwrap();
    let want = Machine {
        cpus: 2,
        mem_mib: 2048,
        kernel: dir.path().join("root/kernel/vmlinux"),
    };
    assert_eq!(store.machine("base").unwrap(), want);
    assert_eq!(store.config("base").unwrap(), Config::default());
}

#[test]
fn sizes_are_stored_and_a_fork_starts_from_its_parents() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"v0")).unwrap();
    let kernel = dir.path().join("vmlinux");
    fs::write(&kernel, b"kernel").unwrap();
    let big = Config {
        cpus: Some(8),
        mem_mib: None,
        kernel: Some(kernel.clone()),
        idle: None,
        url: None,
        allow: None,
    };
    store.fork_with("base", "big", &big).unwrap();
    assert_eq!(store.config("big").unwrap(), big);
    let more = Config {
        mem_mib: Some(16384),
        ..Config::default()
    };
    store.fork_with("big", "bigger", &more).unwrap();
    let want = Machine {
        cpus: 8,
        mem_mib: 16384,
        kernel,
    };
    assert_eq!(store.machine("bigger").unwrap(), want);
    // The parent is unchanged, and a plain fork copies it as it is.
    assert_eq!(store.config("big").unwrap(), big);
    store.fork("big", "big-2").unwrap();
    assert_eq!(store.config("big-2").unwrap(), big);
}

#[test]
fn sizes_firecracker_would_refuse_are_refused_at_create() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"v0")).unwrap();
    let bad = [
        Config {
            cpus: Some(0),
            ..Config::default()
        },
        Config {
            cpus: Some(33),
            ..Config::default()
        },
        Config {
            mem_mib: Some(64),
            ..Config::default()
        },
        Config {
            kernel: Some("relative/vmlinux".into()),
            ..Config::default()
        },
        Config {
            kernel: Some(dir.path().join("missing")),
            ..Config::default()
        },
    ];
    for config in bad {
        let got = store.fork_with("base", "x", &config);
        assert!(
            matches!(got, Err(Error::BadConfig(_))),
            "{config:?}: {got:?}"
        );
    }
    assert!(matches!(store.get("x"), Err(Error::NoVm(_))));
    // Odd counts are fine: only SMT, which runner never turns on, needs even ones.
    for cpus in [1, 2, 3, 32] {
        let config = Config {
            cpus: Some(cpus),
            ..Config::default()
        };
        store
            .fork_with("base", &format!("c{cpus}"), &config)
            .unwrap();
    }
}

#[test]
fn a_broken_config_is_an_error_not_a_default() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"v0")).unwrap();
    fs::write(dir.path().join("root/vms/base/config.json"), "{cpus: 4").unwrap();
    let err = store.machine("base").unwrap_err();
    assert!(err.to_string().contains("config.json"), "{err}");
}

#[test]
fn memory_sizes_read_like_people_write_them() {
    for (s, mib) in [
        ("2048", 2048),
        ("512M", 512),
        ("512MiB", 512),
        ("4G", 4096),
        ("4g", 4096),
        ("4GiB", 4096),
        ("1T", 1 << 20),
    ] {
        assert_eq!(parse_mem(s).unwrap(), mib, "{s}");
    }
    for bad in ["", "G", "1.5G", "-1G", "4 G", "4GB", "lots", "99999999999G"] {
        assert!(
            matches!(parse_mem(bad), Err(Error::BadConfig(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn a_discarded_run_leaves_no_disk() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    let path = run.disk.clone();
    run.discard();
    assert!(!path.exists());
    assert_eq!(store.get("dev").unwrap(), vm("dev", 0, 2));
    assert_eq!(store.begin_run("dev").unwrap().generation, 1);
}

#[test]
fn wait_idle_returns_once_the_run_lets_go() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    let waiter = {
        let store = store.clone();
        std::thread::spawn(move || store.wait_idle("dev"))
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!waiter.is_finished(), "wait_idle didn't wait");
    run.commit().unwrap();
    waiter.join().unwrap().unwrap();
    // It holds nothing once it returns.
    store.begin_run("dev").unwrap();
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

        #[test]
        fn every_unit_means_the_same_memory(mib in 1u32..(1 << 20)) {
            prop_assert_eq!(parse_mem(&mib.to_string()).unwrap(), mib);
            prop_assert_eq!(parse_mem(&format!("{mib}M")).unwrap(), mib);
            if mib <= 1 << 12 {
                prop_assert_eq!(parse_mem(&format!("{mib}G")).unwrap(), mib << 10);
            }
        }

        // Whatever comes in, the answer is a size or an error, never a panic or a zero.
        #[test]
        fn any_text_is_a_size_or_an_error(s in "[0-9a-zA-Z.-]{0,12}") {
            if let Ok(mib) = parse_mem(&s) {
                prop_assert!(mib > 0);
            }
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

// gc: the current generation, my checkpoints, the newest of runs' and the
// agent's, and the newest few others stay; everything else on disk goes.

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
fn gc_keeps_every_one_of_my_checkpoints_and_drops_notes_with_their_disks() {
    let dir = tempfile::tempdir().unwrap();
    committed(&dir, 6);
    let store = Store::new(dir.path().join("root")).keeping(2);
    let named = store.checkpoint("dev", Some("before the upgrade")).unwrap();
    let plain = store.checkpoint("dev", None).unwrap();
    assert_eq!((named.id, plain.id), (7, 8));
    assert_eq!(store.gc("dev", false).unwrap().generations, [0, 1, 2, 3]);
    assert_eq!(on_disk(dir.path()), [4, 5, 6, 7, 8]);
    let gens = dir.path().join("root/vms/dev/gens");
    assert!(!gens.join("0.json").exists());
    assert!(gens.join("7.json").is_file());
    let ids: Vec<u64> = store
        .checkpoints("dev")
        .unwrap()
        .iter()
        .filter(|c| !c.auto)
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, [8, 7]);
    assert_eq!(undo_all(&store), [5, 4]);
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

// The checkpoints runs take before joining a running VM, and the agent's
// own, pile up unasked, so gc keeps only the newest KEEP_CHECKPOINTS of the
// two together. Every one I make stays, and so does the current disk, older
// than them all.
#[test]
fn gc_keeps_the_newest_runs_and_agent_checkpoints_and_every_one_of_mine() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let mine: Vec<u64> = (0..3)
        .map(|_| store.checkpoint("dev", Some("mine")).unwrap().id)
        .collect();
    let mut made = Vec::new();
    for n in 1..=25 {
        let before = format!("before run dev.{n}");
        let run = store.checkpoint_by("dev", Some(&before), Maker::Run);
        made.push(run.unwrap().id);
        let agent = store.checkpoint_by("dev", None, Maker::Agent);
        made.push(agent.unwrap().id);
    }
    let all = on_disk(dir.path());
    assert_eq!(all.len(), 1 + 3 + 50);
    let store = store.keeping(0);
    let would = store.gc("dev", true).unwrap();
    assert_eq!(would.generations, made[..50 - KEEP_CHECKPOINTS]);
    assert_eq!(on_disk(dir.path()), all);
    assert_eq!(store.gc("dev", false).unwrap(), would);

    let listed = store.checkpoints("dev").unwrap();
    let ids: Vec<u64> = listed.iter().map(|c| c.id).collect();
    let mut want = vec![0];
    want.extend(&mine);
    want.extend(&made[50 - KEEP_CHECKPOINTS..]);
    want.reverse();
    assert_eq!(ids, want);
    assert!(listed.last().unwrap().current);
    let runs: Vec<&Checkpoint> = listed.iter().filter(|c| c.run).collect();
    let agent = listed.iter().filter(|c| c.agent).count();
    assert_eq!((runs.len(), agent), (10, 10));
    assert!(runs.iter().all(|c| !c.auto && !c.agent));
    assert!(runs[0].row().contains("  run  "), "{}", runs[0].row());
    assert_eq!(runs[0].comment.as_deref(), Some("before run dev.25"));
    for c in &listed[KEEP_CHECKPOINTS..KEEP_CHECKPOINTS + 3] {
        assert!(c.row().contains("  checkpoint  "), "{}", c.row());
    }
}

// A run that joined a running VM names the checkpoint it took in its
// result, for a restore by hand, so gc keeps that one while a run holds the
// VM, however many newer ones there are.
#[test]
fn gc_keeps_the_checkpoint_of_a_run_going_on_the_vm() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let old = store.checkpoint_by("dev", Some("before run dev.1"), Maker::Run);
    let going = runner::runs::hold(&store, "dev").unwrap();
    let taken = store.checkpoint_by("dev", Some("before run dev.2"), Maker::Run);
    for _ in 0..KEEP_CHECKPOINTS {
        store.checkpoint_by("dev", None, Maker::Agent).unwrap();
    }
    let freed = store.gc("dev", false).unwrap();
    assert_eq!(freed.generations, [old.unwrap().id]);
    drop(going);
    let taken = taken.unwrap().id;
    assert_eq!(store.gc("dev", false).unwrap().generations, [taken]);
    assert_eq!(on_disk(dir.path()).len(), 1 + KEEP_CHECKPOINTS);
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

// A running VM's boot holds its lock and runs a generation of its own, so
// its checkpoint is a copy of that one, made with the boot still holding it.
#[test]
fn a_running_vm_s_checkpoint_copies_the_disk_its_boot_runs() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    fs::write(&run.disk, b"live").unwrap();
    let c = store
        .hold_checkpoints("dev")
        .unwrap()
        .freeze(Some("mid-run"))
        .unwrap();
    assert_eq!(
        summary(std::slice::from_ref(&c)),
        [(2, false, Some("mid-run"), false)]
    );
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 2)).unwrap(), b"live");
    // The boot's own generation goes on as it would have, and commits.
    fs::write(&run.disk, b"later").unwrap();
    assert_eq!(run.commit().unwrap(), vm("dev", 1, 5));
    let listed = store.checkpoints("dev").unwrap();
    assert_eq!(
        summary(&listed),
        [
            (2, false, Some("mid-run"), false),
            (1, true, Some("run"), true),
            (0, true, Some("import"), false),
        ]
    );
    store.restore("dev", 2).unwrap();
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 3)).unwrap(), b"live");
}

// The agent in the VM checkpoints through its boot (src/calls.rs), and only
// its own count toward its cap.
#[test]
fn an_agent_s_checkpoint_is_noted_as_its_own_and_counted_apart() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    let since = chrono::Utc::now() - chrono::TimeDelta::seconds(1);
    let freezer = store.hold_checkpoints("dev").unwrap();
    let mine = freezer
        .freeze_by(Some("before the migration"), Maker::Agent)
        .unwrap();
    let host = freezer.freeze(Some("from the host")).unwrap();
    assert!(mine.agent && !mine.auto);
    assert!(!host.agent);
    assert_eq!(freezer.agent_since(since).unwrap(), [mine.time]);
    let later = chrono::Utc::now() + chrono::TimeDelta::seconds(2);
    assert!(freezer.agent_since(later).unwrap().is_empty());
    assert!(mine.row().contains("  agent  "), "{}", mine.row());
    assert!(host.row().contains("  checkpoint  "), "{}", host.row());
    drop(freezer);
    drop(run);
    store.clone().keeping(0).gc("dev", false).unwrap();
    let listed = store.checkpoints("dev").unwrap();
    let agent: Vec<_> = listed.iter().filter(|c| c.agent).map(|c| c.id).collect();
    assert_eq!(agent, [mine.id]);
}

// A guest's own restore is staged while its VM runs, and goes into history
// with the boot's commit, on top of it.
#[test]
fn a_commit_with_a_staged_restore_puts_the_copy_on_top() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    store.checkpoint("dev", Some("clean")).unwrap();
    let run = store.begin_run("dev").unwrap();
    fs::write(&run.disk, b"live").unwrap();
    let staged = store
        .hold_checkpoints("dev")
        .unwrap()
        .stage_restore(1)
        .unwrap();
    assert_eq!(run.commit_then(Some(&staged)).unwrap(), vm("dev", 3, 2));
    let listed = store.checkpoints("dev").unwrap();
    assert_eq!(
        summary(&listed),
        [
            (3, true, Some("restore of 1"), true),
            (2, true, Some("run"), false),
            (1, false, Some("clean"), false),
            (0, true, Some("import"), false),
        ]
    );
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 3)).unwrap(), b"v0");
    assert_eq!(store.undo("dev").unwrap().generation, 2);
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 2)).unwrap(), b"live");
}

#[test]
fn a_running_checkpoint_killed_partway_is_never_listed_and_the_next_clears_it() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let run = store.begin_run("dev").unwrap();
    // What one killed mid-copy leaves: its note, and its copy under its
    // working name.
    let gens = dir.path().join("root/vms/dev/gens");
    fs::write(gens.join("2.json"), fs::read(gens.join("0.json")).unwrap()).unwrap();
    fs::write(gens.join("2.part"), b"li").unwrap();
    let ids = |store: &Store| -> Vec<u64> {
        let listed = store.checkpoints("dev").unwrap();
        listed.iter().map(|c| c.id).collect()
    };
    assert_eq!(ids(&store), [0]);
    // Nor does a boot take its number.
    drop(run);
    let run = store.begin_run("dev").unwrap();
    assert_eq!(run.generation, 3);
    let c = store.hold_checkpoints("dev").unwrap().freeze(None).unwrap();
    assert_eq!(c.id, 4);
    assert!(!gens.join("2.part").exists());
    assert!(!gens.join("2.json").exists());
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 4)).unwrap(), b"v0");
    assert_eq!(ids(&store), [4, 0]);
    drop(run);
}

// A running VM's restore copies the checkpoint while the VM still runs, and
// puts the copy in history only once it's stopped.
#[test]
fn a_staged_restore_goes_into_history_only_once_finished() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    store.checkpoint("dev", Some("clean")).unwrap();
    let run = store.begin_run("dev").unwrap();
    let freezer = store.hold_checkpoints("dev").unwrap();
    assert!(matches!(
        freezer.stage_restore(9),
        Err(Error::NoCheckpoint(n, 9)) if n == "dev"
    ));
    let staged = freezer.stage_restore(1).unwrap();
    assert_eq!((staged.from, staged.generation), (1, 3));
    assert_eq!(fs::read(gen_disk(dir.path(), "dev", 3)).unwrap(), b"v0");
    assert!(matches!(
        store.finish_restore("dev", &staged),
        Err(Error::Busy(_))
    ));
    run.commit().unwrap();
    assert_eq!(
        store.finish_restore("dev", &staged).unwrap(),
        vm("dev", 3, 2)
    );
    let listed = store.checkpoints("dev").unwrap();
    assert_eq!(summary(&listed)[0], (3, true, Some("restore of 1"), true));
    assert_eq!(store.undo("dev").unwrap().generation, 2);

    // One that didn't go ahead leaves nothing behind.
    let staged = freezer.stage_restore(1).unwrap();
    let generation = staged.generation;
    store.drop_restore("dev", staged).unwrap();
    assert!(!gen_disk(dir.path(), "dev", generation).exists());
    assert!(
        store
            .checkpoints("dev")
            .unwrap()
            .iter()
            .all(|c| c.id != generation)
    );
}

#[test]
fn a_staged_disk_goes_on_top_of_history_once_finished() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"old")).unwrap();
    let new = dir.path().join("new.ext4");
    fs::write(&new, b"the new one").unwrap();
    let staged = store
        .stage_disk("base", Source::File(&new), "import")
        .unwrap();
    assert_eq!((staged.from, staged.generation), (0, 1));
    assert_eq!(
        fs::read(gen_disk(dir.path(), "base", 1)).unwrap(),
        b"the new one"
    );
    // Not current until finished, and kept meanwhile, as a restore's copy is.
    assert_eq!(store.get("base").unwrap(), vm("base", 0, 3));
    let spare = store.clone().keeping(0);
    assert!(spare.gc("base", true).unwrap().generations.is_empty());
    assert_eq!(
        store.finish_restore("base", &staged).unwrap(),
        vm("base", 1, 11)
    );
    let listed = store.checkpoints("base").unwrap();
    assert_eq!(summary(&listed)[0], (1, true, Some("import"), true));
    assert_eq!(store.undo("base").unwrap(), vm("base", 0, 3));

    // From a stream, the same, and one dropped leaves nothing behind.
    let mut stream: &[u8] = b"streamed";
    let staged = store
        .stage_disk("base", Source::Stream(&mut stream), "import")
        .unwrap();
    let generation = staged.generation;
    assert_eq!(
        fs::read(gen_disk(dir.path(), "base", generation)).unwrap(),
        b"streamed"
    );
    store.drop_restore("base", staged).unwrap();
    assert!(!gen_disk(dir.path(), "base", generation).exists());
    let listed = store.checkpoints("base").unwrap();
    assert!(listed.iter().all(|c| c.id != generation));
    assert!(matches!(
        store.stage_disk("nope", Source::File(&new), "import"),
        Err(Error::NoVm(n)) if n == "nope"
    ));
}

#[test]
fn a_staged_disk_is_refused_at_its_finish_while_the_vm_is_held() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"old")).unwrap();
    let new = dir.path().join("new.ext4");
    fs::write(&new, b"new").unwrap();
    let staged = store
        .stage_disk("base", Source::File(&new), "import")
        .unwrap();
    let run = store.begin_run("base").unwrap();
    assert!(matches!(
        store.finish_restore("base", &staged),
        Err(Error::Busy(_))
    ));
    run.discard();
}

#[test]
fn a_vm_that_never_booted_has_no_running_disk_to_checkpoint() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    let froze = store.hold_checkpoints("dev").unwrap().freeze(None);
    assert!(matches!(froze, Err(Error::NeverBooted(n)) if n == "dev"));
    assert!(matches!(
        store.hold_checkpoints("nope"),
        Err(Error::NoVm(_))
    ));
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

#[test]
fn a_note_left_without_its_disk_is_never_read_as_another() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"v0")).unwrap();
    store.checkpoint("dev", Some("old")).unwrap();
    // As a cleanup by hand that took the disk and missed its note.
    fs::remove_file(gen_disk(dir.path(), "dev", 1)).unwrap();
    assert!(matches!(
        store.restore("dev", 1),
        Err(Error::NoCheckpoint(_, 1))
    ));
    // A run that fails gets a number of its own, not the note's.
    let run = store.begin_run("dev").unwrap();
    assert_eq!(run.generation, 2);
    drop(run);
    assert_eq!(
        summary(&store.checkpoints("dev").unwrap()),
        [(0, true, Some("import"), true)]
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

#[test]
fn a_vm_gets_the_default_credentials_until_told_otherwise() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    assert_eq!(store.creds("base").unwrap(), DEFAULT);
    let both = [Cred::Github, Cred::Claude, Cred::Github];
    assert_eq!(
        store.set_creds("base", &both).unwrap(),
        vec![Cred::Claude, Cred::Github]
    );
    assert_eq!(
        store.creds("base").unwrap(),
        vec![Cred::Claude, Cred::Github]
    );
    assert_eq!(store.set_creds("base", &[]).unwrap(), vec![]);
    assert_eq!(store.creds("base").unwrap(), vec![]);
}

// A fork is a new VM: it starts from the default, not from what its source was given.
#[test]
fn a_fork_does_not_inherit_credentials() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.set_creds("base", &[Cred::Github]).unwrap();
    store.fork("base", "dev").unwrap();
    assert_eq!(store.creds("dev").unwrap(), DEFAULT);
}

#[test]
fn the_allowlist_holds_names_only() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store
        .set_creds("base", &[Cred::Claude, Cred::Github])
        .unwrap();
    let file = dir.path().join("root/vms/base/creds");
    assert_eq!(fs::read_to_string(file).unwrap(), "claude\ngithub\n");
}

#[test]
fn credentials_need_a_vm_and_a_readable_allowlist() {
    let (dir, store) = store();
    assert!(matches!(store.creds("nope"), Err(Error::NoVm(_))));
    assert!(matches!(
        store.set_creds("nope", DEFAULT),
        Err(Error::NoVm(_))
    ));
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    fs::write(dir.path().join("root/vms/base/creds"), "claude\naws\n").unwrap();
    assert!(matches!(store.creds("base"), Err(Error::Io { .. })));
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
        ("fcac100004", "172.16.0.6".into())
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
    // As host/install.sh writes it.
    fs::write(dir.path().join("net"), "10.9.0.0/29\n").unwrap();
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

    fs::write(dir.path().join("net"), "10.9.0.1/29\n").unwrap();
    let err = store.net("dev").unwrap_err();
    let net = dir.path().join("net");
    assert!(err.to_string().contains(net.to_str().unwrap()), "{err}");
    assert!(err.to_string().contains("10.9.0.0/29"), "{err}");
}

#[test]
fn a_slot_that_can_t_be_read_stops_a_create_rather_than_be_reused() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    let bad = dir.path().join("root/vms/dev/slot");
    fs::write(&bad, "one\n").unwrap();
    let err = store.fork("base", "more").unwrap_err();
    assert!(err.to_string().contains(bad.to_str().unwrap()), "{err}");
    assert!(matches!(store.get("more"), Err(Error::NoVm(_))));
    // A stray file among the VMs isn't a VM with a slot, and stops nothing.
    fs::write(&bad, "1\n").unwrap();
    fs::write(dir.path().join("root/vms/stray"), "").unwrap();
    store.fork("base", "more").unwrap();
    assert_eq!(slot(&store, "more"), 2);
}

#[test]
fn a_vm_whose_directory_is_a_symlink_keeps_its_slot() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.fork("base", "dev").unwrap();
    let real = dir.path().join("dev-elsewhere");
    let vm = dir.path().join("root/vms/dev");
    fs::rename(&vm, &real).unwrap();
    std::os::unix::fs::symlink(&real, &vm).unwrap();
    assert_eq!(slot(&store, "dev"), 1);
    store.fork("base", "more").unwrap();
    assert_eq!(slot(&store, "more"), 2);
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

// A sleeping VM (src/sleep.rs) is one with a whole snapshot: Firecracker's
// two files, then the note naming the generation they run on.
#[test]
fn a_vm_sleeps_once_its_snapshot_is_noted_and_not_once_it_s_cleared() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    assert_eq!(store.sleeping("dev").unwrap(), None);
    let snap = store.snapshot("dev").unwrap();
    snap.make_room().unwrap();
    // The memory holds the guest's credentials: only the runner user reads it.
    use std::os::unix::fs::PermissionsExt;
    let dir = snap.mem().parent().unwrap().to_path_buf();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::write(snap.vmstate(), b"state").unwrap();
    fs::write(snap.mem(), b"memory").unwrap();
    // Not noted: not sleeping.
    assert_eq!(store.sleeping("dev").unwrap(), None);
    let slept = Slept {
        generation: 1,
        at: chrono::Utc::now().trunc_subsecs(0),
        parked: false,
    };
    snap.note(&slept).unwrap();
    assert_eq!(store.sleeping("dev").unwrap(), Some(slept));

    snap.clear().unwrap();
    assert_eq!(store.sleeping("dev").unwrap(), None);
    assert!(!snap.vmstate().exists() && !snap.mem().exists());
    // Clearing nothing is fine, and so is making room over a cleared one.
    snap.clear().unwrap();
    snap.make_room().unwrap();
}

// Firecracker maps the memory file into a guest it woke, so a new snapshot
// mustn't write into that file: it gets a new one.
#[test]
fn making_room_for_a_snapshot_unlinks_the_last_one() {
    use std::os::unix::fs::MetadataExt;
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    let snap = store.snapshot("dev").unwrap();
    snap.make_room().unwrap();
    fs::write(snap.mem(), b"memory").unwrap();
    let mapped = fs::File::open(snap.mem()).unwrap();
    snap.make_room().unwrap();
    assert!(!snap.mem().exists());
    // The open file is still what it was.
    assert_eq!(mapped.metadata().unwrap().len(), 6);
    assert_eq!(mapped.metadata().unwrap().nlink(), 0);
}

// dev's boot, run 1, with dev asleep: its snapshot written, as a sleep
// leaves it (src/boot.rs).
fn asleep(store: &Store) -> runner::store::Run {
    let run = store.begin_run("dev").unwrap();
    let snap = run.snapshot();
    snap.make_room().unwrap();
    let at = chrono::Utc::now().trunc_subsecs(0);
    snap.note(&Slept {
        generation: run.generation,
        at,
        parked: false,
    })
    .unwrap();
    fs::write(snap.vmstate(), b"state").unwrap();
    fs::write(snap.mem(), b"memory").unwrap();
    run
}

// A host shutdown parks a sleeping VM: its boot ends without committing, and
// the next boot gets the same run back, on the same disk, to resume it.
#[test]
fn a_parked_vm_s_next_boot_gets_its_run_back() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    let run = asleep(&store);
    assert_eq!(run.parked(), None);
    run.park().unwrap();
    let slept = store.sleeping("dev").unwrap().unwrap();
    assert!(slept.parked);
    assert_eq!(slept.generation, 1);
    drop(run);
    assert_eq!(store.get("dev").unwrap().generation, 0);

    let again = store.begin_run("dev").unwrap();
    assert_eq!((again.from, again.generation), (0, 1));
    assert_eq!(again.disk, store.disk("dev", 1).unwrap());
    assert_eq!(again.parked(), Some(slept));
    // Nothing new was cloned, and what it booted is as it was.
    assert!(!store.disk("dev", 2).unwrap().exists());
    let booted = store.booted("dev").unwrap().unwrap();
    assert_eq!(
        booted,
        Booted {
            from: 0,
            generation: 1
        }
    );
    // Its disk is the guest's, even if this boot never got going.
    again.discard();
    assert!(store.disk("dev", 1).unwrap().exists());
    assert!(store.sleeping("dev").unwrap().unwrap().parked);

    // Resumed and shut down cleanly, it commits that disk.
    let again = store.begin_run("dev").unwrap();
    again.snapshot().clear().unwrap();
    assert_eq!(again.commit().unwrap().generation, 1);
}

// One that can't resume boots its last committed disk instead, under the
// same lock, and the disk it was parked on stays for a look.
#[test]
fn a_parked_run_that_can_t_resume_gives_way_to_a_fresh_one() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    let run = asleep(&store);
    run.park().unwrap();
    drop(run);
    let mut fresh = store.begin_run("dev").unwrap();
    fresh.unpark().unwrap();
    assert_eq!((fresh.from, fresh.generation), (0, 2));
    assert_eq!(fresh.parked(), None);
    assert_eq!(store.sleeping("dev").unwrap(), None);
    assert!(!store.snapshot("dev").unwrap().mem().exists());
    assert_eq!(fs::read(&fresh.disk).unwrap(), b"rootfs");
    assert!(store.disk("dev", 1).unwrap().exists());
    assert!(matches!(store.undo("dev"), Err(Error::Busy(_))));
    let booted = store.booted("dev").unwrap().unwrap();
    assert_eq!(booted.generation, 2);
    assert_eq!(fresh.commit().unwrap().generation, 2);
}

// With no boot up, nothing holds a parked VM's lock, so the store refuses
// it as busy itself: undo, gc and the rest would take its disk from under
// its snapshot.
#[test]
fn a_parked_vm_is_busy_with_no_boot_up() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    store.checkpoint("dev", None).unwrap();
    let run = asleep(&store);
    run.park().unwrap();
    drop(run);
    let busy = |r: Result<(), Error>| assert!(matches!(r, Err(Error::Busy(_))), "{r:?}");
    busy(store.undo("dev").map(drop));
    busy(store.gc("dev", false).map(drop));
    busy(store.checkpoint("dev", None).map(drop));
    busy(store.restore("dev", 1).map(drop));
    busy(store.open_disks("dev", true).map(drop));
    busy(store.lock("dev").map(drop));
    busy(store.delete("dev").map(drop));
    assert!(store.disk("dev", 2).unwrap().exists());
    // An old runner's note says nothing of parking: it's not parked.
    let snap = store.snapshot("dev").unwrap();
    let note = snap.mem().with_file_name("slept.json");
    fs::write(&note, r#"{"generation":2,"at":"2026-11-06T00:00:00Z"}"#).unwrap();
    assert!(!store.sleeping("dev").unwrap().unwrap().parked);
    let undone = store.undo("dev");
    assert!(matches!(undone, Err(Error::NothingToUndo(_))), "{undone:?}");
}

// A note that won't read isn't parked, so it can't hold the VM busy for good:
// its next boot clears it, and a delete takes it.
#[test]
fn an_unreadable_note_is_not_parked() {
    let (dir, store) = store();
    store.import("dev", &disk(dir.path(), b"rootfs")).unwrap();
    let run = asleep(&store);
    run.park().unwrap();
    drop(run);
    let note = store
        .snapshot("dev")
        .unwrap()
        .mem()
        .with_file_name("slept.json");
    fs::write(&note, b"{torn").unwrap();
    let undone = store.undo("dev");
    assert!(matches!(undone, Err(Error::NothingToUndo(_))), "{undone:?}");
    let run = store.begin_run("dev").unwrap();
    assert_eq!((run.generation, run.parked()), (2, None));
    drop(run);
    store.delete("dev").unwrap();
}

fn hosts(names: &[&str]) -> Vec<Hostname> {
    names.iter().map(|n| n.parse().unwrap()).collect()
}

fn shown(list: Option<List>) -> Option<Vec<String>> {
    list.map(|l| l.names().iter().map(|h| h.to_string()).collect())
}

#[test]
fn a_vm_s_list_of_names_reads_back_as_set_and_always_has_api_anthropic_com() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    // No list: every name.
    assert_eq!(store.allowed("base").unwrap(), None);
    let set = store
        .allow("base", &hosts(&["registry.npmjs.org", "GitHub.com."]))
        .unwrap();
    let want = ["api.anthropic.com", "github.com", "registry.npmjs.org"];
    assert_eq!(shown(Some(set)), Some(want.map(String::from).to_vec()));
    assert_eq!(
        shown(store.allowed("base").unwrap()),
        Some(want.map(String::from).to_vec())
    );
    // Beside the rest of its config, which stays.
    let config = fs::read_to_string(dir.path().join("root/vms/base/config.json")).unwrap();
    assert!(config.contains("\"allow\""), "{config}");
    store.set_idle("base", runner::idle::Idle::Never).unwrap();
    assert_eq!(shown(store.allowed("base").unwrap()).unwrap().len(), 3);

    // Once each.
    store.allow("base", &hosts(&["github.com"])).unwrap();
    assert_eq!(shown(store.allowed("base").unwrap()).unwrap().len(), 3);
    // What every list has isn't kept as the owner's, which deny would then
    // refuse to take off.
    store.allow("base", &hosts(&["API.anthropic.com"])).unwrap();
    let config = fs::read_to_string(dir.path().join("root/vms/base/config.json")).unwrap();
    assert!(!config.contains("api.anthropic.com"), "{config}");

    // Taking names off keeps the list, even once it's down to the one.
    let less = store.deny("base", &hosts(&["github.com"])).unwrap();
    assert_eq!(
        shown(Some(less)).unwrap(),
        ["api.anthropic.com", "registry.npmjs.org"]
    );
    store.deny("base", &hosts(&["registry.npmjs.org"])).unwrap();
    assert_eq!(
        shown(store.allowed("base").unwrap()),
        Some(vec!["api.anthropic.com".to_string()])
    );

    // api.anthropic.com can't be taken off, nor a name that isn't on it.
    let err = store
        .deny("base", &hosts(&["api.anthropic.com"]))
        .unwrap_err();
    assert!(err.to_string().contains("Claude Code"), "{err}");
    store.allow("base", &hosts(&["github.com"])).unwrap();
    let err = store
        .deny("base", &hosts(&["api.github.com", "github.com"]))
        .unwrap_err();
    assert!(err.to_string().contains("api.github.com"), "{err}");
    assert!(err.to_string().contains("github.com covers it"), "{err}");
    // All or nothing: github.com is still on.
    assert_eq!(shown(store.allowed("base").unwrap()).unwrap().len(), 2);

    // Allowing every name drops the list, and there's then nothing to take off.
    store.allow_all("base").unwrap();
    assert_eq!(store.allowed("base").unwrap(), None);
    let err = store.deny("base", &hosts(&["github.com"])).unwrap_err();
    assert!(err.to_string().contains("every name"), "{err}");
    assert!(matches!(store.allowed("nope"), Err(Error::NoVm(_))));
    assert!(matches!(
        store.allow("nope", &hosts(&["github.com"])),
        Err(Error::NoVm(_))
    ));
}

#[test]
fn a_fork_keeps_its_parent_s_list() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    store.allow("base", &hosts(&["github.com"])).unwrap();
    store.fork("base", "dev").unwrap();
    assert_eq!(
        store.allowed("dev").unwrap(),
        store.allowed("base").unwrap()
    );
    let bigger = Config {
        cpus: Some(4),
        ..Config::default()
    };
    store.fork_with("base", "big", &bigger).unwrap();
    assert!(store.allowed("big").unwrap().is_some());
    // Its own from then on.
    store.allow_all("dev").unwrap();
    assert!(store.allowed("base").unwrap().is_some());
}

#[test]
fn a_name_that_is_not_a_hostname_is_refused_with_why() {
    let long_label = format!("{}.com", "a".repeat(64));
    let long_name = format!("{}.com", vec!["a".repeat(60); 5].join("."));
    let refused = [
        ("1.2.3.4", "an address"),
        ("::1", "an address"),
        ("10.0.0.1.", "an address"),
        ("*.github.com", "wildcard"),
        ("github.*", "wildcard"),
        ("", "empty"),
        ("github..com", "empty label"),
        (".github.com", "empty label"),
        ("com", "one label"),
        ("localhost", "one label"),
        ("git hub.com", "letters, digits and hyphens"),
        ("github.com/x", "letters, digits and hyphens"),
        ("git_hub.com", "letters, digits and hyphens"),
        ("bücher.de", "letters, digits and hyphens"),
        ("-github.com", "hyphen"),
        ("github-.com", "hyphen"),
        ("1.2.3", "number"),
        (long_label.as_str(), "63"),
        (long_name.as_str(), "253"),
    ];
    for (name, why) in refused {
        let err = name.parse::<Hostname>().unwrap_err().to_string();
        assert!(err.contains(why), "{name:?}: {err}");
        assert!(err.contains(&format!("{name:?}")), "{name:?}: {err}");
    }
    for (name, as_kept) in [
        ("github.com", "github.com"),
        ("API.GitHub.com.", "api.github.com"),
        ("xn--bcher-kva.de", "xn--bcher-kva.de"),
        ("1password.com", "1password.com"),
        ("a-b.c-d.io", "a-b.c-d.io"),
    ] {
        let host: Hostname = name.parse().unwrap();
        assert_eq!(host.to_string(), as_kept);
    }
}

// An import, or a hand edit, can't slip a bad name past the store.
#[test]
fn a_config_whose_list_holds_a_bad_name_is_an_error() {
    let (dir, store) = store();
    store.import("base", &disk(dir.path(), b"rootfs")).unwrap();
    let config = dir.path().join("root/vms/base/config.json");
    fs::write(&config, "{\"allow\": [\"*.example.com\"]}\n").unwrap();
    let err = store.allowed("base").unwrap_err();
    assert!(err.to_string().contains("wildcard"), "{err}");
    fs::write(&config, "{\"allow\": []}\n").unwrap();
    assert_eq!(
        shown(store.allowed("base").unwrap()),
        Some(vec!["api.anthropic.com".to_string()])
    );
}

proptest::proptest! {
    // Any text is a hostname that reads back as itself, or refused: never
    // a panic, and never one with a byte a DNS label can't hold.
    #[test]
    fn any_text_is_a_hostname_or_refused(s in "[a-zA-Z0-9.*_ -]{0,40}") {
        if let Ok(host) = s.parse::<Hostname>() {
            let kept = host.to_string();
            proptest::prop_assert_eq!(kept.parse::<Hostname>().unwrap(), host);
            proptest::prop_assert!(kept.split('.').count() >= 2);
            proptest::prop_assert!(kept.bytes().all(|b| b.is_ascii_lowercase()
                || b.is_ascii_digit() || b == b'-' || b == b'.'));
        }
    }
}
