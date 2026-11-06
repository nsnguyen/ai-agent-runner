// `runner resize` and `create --disk`: the store's side on real ext4 disks,
// read back with ext4-view and checked with e2fsck, and the CLI's with
// systemctl faked.
mod common;

use common::{FakeSystemd, text};
use ext4_view::Ext4;
use runner::store::{Config, Error, Sizes, Slept, Store, parse_disk, size_text};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

// As tests/store.rs: a lock one test drops can look held for a moment to
// another whose thread just forked, so store tests take turns.
static TURN: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

// What each test disk holds: text, a file of every byte, and one big
// enough to span block groups.
fn files() -> Vec<(&'static str, Vec<u8>)> {
    let every: Vec<u8> = (0..=255).cycle().take(300_000).collect();
    let big: Vec<u8> = (0..9 * MIB).map(|i| (i * 7 % 251) as u8).collect();
    vec![
        ("home/agent/notes.txt", b"kept through a resize\n".to_vec()),
        ("home/agent/every", every),
        ("srv/big", big),
    ]
}

// dev, imported from a 1 GiB ext4 disk holding files().
fn with_dev() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    for (path, bytes) in files() {
        let path = tree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let disk = common::image_of(&tree, "1G");
    let store = Store::new(dir.path().join("root")).with_net_file(dir.path().join("net"));
    store.import("dev", &disk).unwrap();
    (dir, store)
}

fn gens(store: &Store, vm: &str) -> Vec<String> {
    let dir = store.root().join("vms").join(vm).join("gens");
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn config_json(store: &Store, vm: &str) -> Option<String> {
    fs::read_to_string(store.root().join("vms").join(vm).join("config.json")).ok()
}

fn history(store: &Store, vm: &str) -> String {
    fs::read_to_string(store.root().join("vms").join(vm).join("history")).unwrap()
}

// The disk passes a forced check that changes nothing.
#[track_caller]
fn assert_clean(disk: &Path) {
    let out = Command::new("e2fsck")
        .arg("-fn")
        .arg(disk)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

#[track_caller]
fn assert_same_files(disk: &Path) {
    let fs = Ext4::load_from_path(disk).unwrap();
    for (path, bytes) in files() {
        let got = fs.read(format!("/{path}").as_str()).unwrap();
        assert!(got == bytes, "/{path} differs after the resize");
    }
}

fn disk_size(text: &str) -> Sizes {
    Sizes {
        disk: Some(parse_disk(text).unwrap()),
        ..Sizes::default()
    }
}

#[test]
fn a_grown_disk_is_a_new_sparse_generation_holding_the_same_files() {
    let _turn = turn();
    let (_dir, store) = with_dev();
    let vm = store.resize("dev", &disk_size("2G")).unwrap();
    assert_eq!((vm.generation, vm.bytes), (1, 2 * GIB));

    let disk = store.disk("dev", 1).unwrap();
    let meta = fs::metadata(&disk).unwrap();
    assert_eq!(meta.len(), 2 * GIB);
    // Still sparse: the files and ext4's own, not the new gigabyte.
    let allocated = meta.blocks() * 512;
    assert!(allocated < 128 * MIB, "{allocated} bytes allocated");
    // The filesystem fills the disk, and is as it was.
    assert_eq!(runner::image::ext4_size(&disk), Some(2 * GIB));
    assert_clean(&disk);
    assert_same_files(&disk);

    let noted = store.checkpoints("dev").unwrap();
    let now = noted.iter().find(|c| c.id == 1).unwrap();
    assert!(now.current && now.auto, "{now:?}");
    assert_eq!(now.comment.as_deref(), Some("resize to 2G"));

    // Undo takes it back.
    let back = store.undo("dev").unwrap();
    assert_eq!((back.generation, back.bytes), (0, GIB));
    assert_same_files(&store.disk("dev", 0).unwrap());
}

#[test]
fn a_disk_only_grows() {
    let _turn = turn();
    let (_dir, store) = with_dev();
    let before = (gens(&store, "dev"), history(&store, "dev"));
    for size in ["1G", "512M", "1024"] {
        let err = store.resize("dev", &disk_size(size)).unwrap_err();
        let why = err.to_string();
        assert!(matches!(err, Error::BadConfig(_)), "{size}: {why}");
        assert!(why.contains("1G already"), "{size}: {why}");
        assert_eq!((gens(&store, "dev"), history(&store, "dev")), before);
    }
}

#[test]
fn sizes_parse_as_memory_does_and_print_back() {
    assert_eq!(parse_disk("40G").unwrap(), 40 * GIB);
    assert_eq!(parse_disk("40gib").unwrap(), 40 * GIB);
    assert_eq!(parse_disk("1T").unwrap(), 1024 * GIB);
    assert_eq!(parse_disk("512").unwrap(), 512 * MIB);
    for bad in [
        "",
        "0",
        "0G",
        "-1G",
        "40GB",
        "4.5G",
        "G",
        "big",
        "99999999999G",
    ] {
        let err = parse_disk(bad).unwrap_err().to_string();
        assert!(err.contains("isn't a disk size"), "{bad:?}: {err}");
    }
    assert_eq!(size_text(40 * GIB), "40G");
    assert_eq!(size_text(2048 * GIB), "2T");
    assert_eq!(size_text(1536 * MIB), "1536M");
    assert_eq!(size_text(6), "6 bytes");
}

mod props {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // Every whole MiB parse_disk can give, up to its u32 MiB, reads back
        // from what size_text writes, as errors and `resize` print sizes.
        #[test]
        fn size_text_reads_back_as_the_same_disk(mib in 1..=u64::from(u32::MAX)) {
            let bytes = mib << 20;
            prop_assert_eq!(parse_disk(&size_text(bytes)).unwrap(), bytes);
        }

        // Anything else is written in bytes, which isn't read back as a
        // size it isn't.
        #[test]
        fn a_size_not_in_whole_mib_isnt_read_back(bytes in 1..u64::MAX) {
            prop_assume!(bytes % MIB != 0);
            let text = size_text(bytes);
            prop_assert_eq!(text.clone(), format!("{bytes} bytes"));
            prop_assert!(parse_disk(&text).is_err());
        }
    }
}

#[test]
fn a_failed_grow_leaves_the_vm_as_it_was() {
    let _turn = turn();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root"));
    // Not ext4, so e2fsck refuses it.
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, vec![7; MIB as usize]).unwrap();
    store.import("dev", &disk).unwrap();
    let before = (gens(&store, "dev"), history(&store, "dev"));
    let err = store
        .resize("dev", &disk_size("2M"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("e2fsck"), "{err}");
    assert_eq!((gens(&store, "dev"), history(&store, "dev")), before);
    assert_eq!(store.get("dev").unwrap().bytes, MIB);
}

#[test]
fn a_vm_that_is_held_or_parked_is_busy_and_keeps_its_sizes() {
    let _turn = turn();
    let (_dir, store) = with_dev();
    let all = Sizes {
        disk: Some(2 * GIB),
        cpus: Some(4),
        mem_mib: Some(4096),
    };
    let before = (gens(&store, "dev"), history(&store, "dev"));
    // As its boot holds it while it's up.
    let held = store.lock("dev").unwrap();
    let err = store.resize("dev", &all).unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err}");
    drop(held);
    // Parked at a host shutdown: no boot, but a snapshot on its disk.
    let snap = store.snapshot("dev").unwrap();
    snap.make_room().unwrap();
    let at = chrono::Utc::now();
    snap.note(&Slept {
        generation: 0,
        at,
        parked: true,
    })
    .unwrap();
    let err = store.resize("dev", &all).unwrap_err();
    assert!(matches!(err, Error::Busy(_)), "{err}");
    assert_eq!((gens(&store, "dev"), history(&store, "dev")), before);
    assert_eq!(config_json(&store, "dev"), None);
}

#[test]
fn memory_and_vcpus_go_in_its_config_and_bad_ones_change_nothing() {
    let _turn = turn();
    let (_dir, store) = with_dev();
    let vm = store.get("dev").unwrap();
    assert_eq!((vm.cpus, vm.mem_mib), (2, 2048));
    let sizes = Sizes {
        cpus: Some(4),
        mem_mib: Some(4096),
        ..Sizes::default()
    };
    let vm = store.resize("dev", &sizes).unwrap();
    assert_eq!((vm.generation, vm.cpus, vm.mem_mib), (0, 4, 4096));
    let machine = store.machine("dev").unwrap();
    assert_eq!((machine.cpus, machine.mem_mib), (4, 4096));
    let config = config_json(&store, "dev");

    let before = gens(&store, "dev");
    for bad in [
        Sizes {
            cpus: Some(33),
            disk: Some(2 * GIB),
            ..Sizes::default()
        },
        Sizes {
            mem_mib: Some(64),
            ..Sizes::default()
        },
        Sizes::default(),
    ] {
        let err = store.resize("dev", &bad).unwrap_err();
        assert!(matches!(err, Error::BadConfig(_)), "{bad:?}: {err}");
    }
    assert_eq!(gens(&store, "dev"), before);
    assert_eq!(config_json(&store, "dev"), config);
}

// The config is checked before the disk grows, so a resize that can't set
// memory doesn't leave a bigger disk behind it. A disk alone needs no config.
#[test]
fn a_config_that_won_t_read_stops_a_resize_before_the_disk_grows() {
    let _turn = turn();
    let (_dir, store) = with_dev();
    let path = store.root().join("vms/dev/config.json");
    fs::write(&path, "{\"cpus\": 2, \"from_a_newer_runner\": 1}\n").unwrap();
    let before = (gens(&store, "dev"), history(&store, "dev"));
    let both = Sizes {
        disk: Some(2 * GIB),
        mem_mib: Some(4096),
        ..Sizes::default()
    };
    store.resize("dev", &both).unwrap_err();
    assert_eq!((gens(&store, "dev"), history(&store, "dev")), before);
    let vm = store.resize("dev", &disk_size("2G")).unwrap();
    assert_eq!((vm.generation, vm.bytes), (1, 2 * GIB));
}

#[test]
fn a_fork_can_grow_its_disk_and_its_source_stays_as_it_was() {
    let _turn = turn();
    let (_dir, store) = with_dev();
    let before = (gens(&store, "dev"), history(&store, "dev"));
    let vm = store
        .fork_sized("dev", "app", &Config::default(), Some(2 * GIB))
        .unwrap();
    assert_eq!((vm.generation, vm.bytes), (0, 2 * GIB));
    let disk = store.disk("app", 0).unwrap();
    assert_eq!(runner::image::ext4_size(&disk), Some(2 * GIB));
    assert_clean(&disk);
    assert_same_files(&disk);
    let noted = store.checkpoints("app").unwrap();
    assert_eq!(
        noted[0].comment.as_deref(),
        Some("fork of dev, resized to 2G")
    );
    assert_eq!((gens(&store, "dev"), history(&store, "dev")), before);
    assert_eq!(store.get("dev").unwrap().bytes, GIB);

    // Its own size is a plain fork; a smaller one is refused, and no VM made.
    let same = store
        .fork_sized("dev", "same", &Config::default(), Some(GIB))
        .unwrap();
    assert_eq!(same.bytes, GIB);
    let err = store
        .fork_sized("dev", "small", &Config::default(), Some(GIB / 2))
        .unwrap_err();
    assert!(matches!(err, Error::BadConfig(_)), "{err}");
    assert!(matches!(store.get("small"), Err(Error::NoVm(_))));
}

// The CLI, with systemctl faked: dev is stopped unless a test says.
struct Host {
    _dir: TempDir,
    store: Store,
    sd: FakeSystemd,
}

fn host() -> Host {
    let (dir, store) = with_dev();
    let sd = FakeSystemd::new(dir.path());
    Host {
        _dir: dir,
        store,
        sd,
    }
}

impl Host {
    fn runner(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", self.store.root())
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

    fn unchanged(&self) -> (Vec<String>, String, Option<String>) {
        let s = &self.store;
        (gens(s, "dev"), history(s, "dev"), config_json(s, "dev"))
    }
}

#[test]
fn resize_sets_memory_and_vcpus_and_status_shows_them() {
    let h = host();
    let status = h.ok(&["status", "dev"]);
    assert_eq!(
        status,
        format!("dev  gen 0  {GIB} bytes  cpus 2  mem 2G  stopped\n")
    );
    let said = h.ok(&["resize", "dev", "--mem", "4G", "--cpus", "4"]);
    assert_eq!(said, format!("dev  gen 0  {GIB} bytes  cpus 4  mem 4G\n"));
    let config = config_json(&h.store, "dev").unwrap();
    assert!(
        config.contains("\"cpus\":4") && config.contains("\"mem_mib\":4096"),
        "{config}"
    );
    let status = h.ok(&["status", "dev"]);
    assert_eq!(
        status,
        format!("dev  gen 0  {GIB} bytes  cpus 4  mem 4G  stopped\n")
    );
    let listed = h.ok(&["list"]);
    let want = format!("dev  gen 0  {GIB} bytes  cpus 4  mem 4G  ");
    assert!(
        listed.starts_with(&want) && listed.ends_with(" bytes used\n"),
        "{listed}"
    );

    let before = h.unchanged();
    let why = h.fails(&["resize", "dev", "--cpus", "33"]);
    assert!(why.contains("1 to 32 vCPUs"), "{why}");
    assert_eq!(h.unchanged(), before);
}

#[test]
fn resize_grows_the_disk_and_undo_takes_it_back() {
    let h = host();
    let said = h.ok(&["resize", "dev", "--disk", "2G"]);
    assert_eq!(
        said,
        format!("dev  gen 1  {} bytes  cpus 2  mem 2G\n", 2 * GIB)
    );
    assert_same_files(&h.store.disk("dev", 1).unwrap());
    h.ok(&["undo", "dev"]);
    assert_eq!(h.store.get("dev").unwrap().bytes, GIB);
}

#[test]
fn resize_refuses_bad_sizes_and_changes_nothing() {
    let h = host();
    let before = h.unchanged();
    for (args, says) in [
        (&["resize", "dev", "--disk", "1G"][..], "1G already"),
        (&["resize", "dev", "--disk", "40GB"], "isn't a disk size"),
        (
            &["resize", "dev", "--disk", "2G", "--mem", "lots"],
            "isn't a memory size",
        ),
        (&["resize", "dev"], "--disk"),
    ] {
        let why = h.fails(args);
        assert!(why.contains(says), "{args:?}: {why}");
        assert_eq!(h.unchanged(), before, "{args:?}");
    }
}

#[test]
fn resize_refuses_a_vm_that_is_up_or_asleep() {
    let h = host();
    let before = h.unchanged();
    let resize = [
        "resize", "dev", "--disk", "2G", "--mem", "4G", "--cpus", "4",
    ];
    for state in ["active", "activating", "deactivating"] {
        h.sd.set_state(state);
        let why = h.fails(&resize);
        assert!(why.contains("runner stop dev"), "{state}: {why}");
        assert_eq!(h.unchanged(), before, "{state}");
    }
    // Asleep: its boot up, its snapshot noted.
    h.sd.set_state("active");
    let snap = h.store.snapshot("dev").unwrap();
    snap.make_room().unwrap();
    let at = chrono::Utc::now();
    let mut slept = Slept {
        generation: 0,
        at,
        parked: false,
    };
    snap.note(&slept).unwrap();
    let why = h.fails(&resize);
    assert!(why.contains("dev is sleeping"), "{why}");
    // Parked at a host shutdown: its unit stopped, its snapshot kept.
    h.sd.set_state("inactive");
    slept.parked = true;
    snap.note(&slept).unwrap();
    let why = h.fails(&resize);
    assert!(why.contains("dev is sleeping"), "{why}");
    assert_eq!(h.unchanged(), before);
}

#[test]
fn create_with_a_disk_forks_and_grows_in_one_step() {
    let h = host();
    let before = h.unchanged();
    let said = h.ok(&["create", "app", "--from", "dev", "--disk", "2G"]);
    assert_eq!(said, format!("app  gen 0  {} bytes\n", 2 * GIB));
    assert_same_files(&h.store.disk("app", 0).unwrap());
    assert_eq!(h.unchanged(), before);
    let why = h.fails(&["create", "small", "--from", "dev", "--disk", "512M"]);
    assert!(why.contains("1G"), "{why}");
    let why = h.fails(&["create", "bad", "--from", "dev", "--disk", "x"]);
    assert!(why.contains("isn't a disk size"), "{why}");
}
