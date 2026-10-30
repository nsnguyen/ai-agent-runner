// The VM store on a temp directory: no KVM needed.
use runner::store::{Config, Error, Machine, Store, Vm, check_name, parse_mem};
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
