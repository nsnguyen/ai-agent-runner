// `runner import`, end to end through the CLI: a new disk on top of a VM's
// history, from a file or from stdin, and what the disk says it was built
// from. systemctl is faked, so neither systemd nor KVM is needed.
mod common;

use chrono::Utc;
use common::{FakeSystemd, text};
use runner::image::Build;
use runner::store::{Slept, Store};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Host {
    dir: TempDir,
    sd: FakeSystemd,
}

impl Host {
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let sd = FakeSystemd::new(dir.path());
        Host { dir, sd }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    fn store(&self) -> Store {
        Store::new(self.root())
    }

    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path.display().to_string()
    }

    // With `stdin` fed through a pipe, as `runner import base - <disk` over
    // ssh reads it.
    fn runner(&self, args: &[&str], stdin: Option<Vec<u8>>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", self.root())
            .env("PATH", self.sd.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take().unwrap();
        let feed = std::thread::spawn(move || {
            if let Some(bytes) = stdin {
                let _ = pipe.write_all(&bytes);
            }
        });
        let out = child.wait_with_output().unwrap();
        feed.join().unwrap();
        out
    }

    fn ok(&self, args: &[&str]) -> String {
        self.ok_in(args, None)
    }

    fn ok_in(&self, args: &[&str], stdin: Option<Vec<u8>>) -> String {
        let out = self.runner(args, stdin);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.runner(args, None);
        assert!(!out.status.success(), "{args:?}: {}", text(&out.stdout));
        text(&out.stderr)
    }

    fn current(&self, vm: &str) -> PathBuf {
        let store = self.store();
        store.disk(vm, store.get(vm).unwrap().generation).unwrap()
    }

    fn gens(&self, vm: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.root().join("vms").join(vm).join("gens"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[test]
fn replace_on_a_stopped_base_makes_the_new_disk_current_and_undo_brings_the_old_back() {
    let host = Host::new();
    host.ok(&["import", "img", &host.file("old.ext4", b"old base")]);
    host.ok(&[
        "create", "base", "--from", "img", "--cpus", "4", "--mem", "4G", "--idle", "30m",
    ]);
    let store = host.store();
    let settings = || {
        let config = store.config("base").unwrap();
        (config, store.machine("base").unwrap())
    };
    let before = settings();
    assert_eq!(before.0.cpus, Some(4));

    let new = host.file("new.ext4", b"the new base");
    let out = host.ok(&["import", "--replace", "base", &new]);
    assert_eq!(out, "base  gen 1  12 bytes\n");
    assert_eq!(fs::read(host.current("base")).unwrap(), b"the new base");
    assert_eq!(settings(), before);
    // It asked systemd whether base was up first.
    let show = "systemctl show --property=ActiveState --value firecracker@base.service";
    assert!(
        host.sd.calls().contains(&show.to_string()),
        "{:?}",
        host.sd.calls()
    );

    assert_eq!(host.ok(&["undo", "base"]), "base  gen 0  8 bytes\n");
    assert_eq!(fs::read(host.current("base")).unwrap(), b"old base");
    assert_eq!(settings(), before);
}

#[test]
fn replace_is_refused_while_base_is_up() {
    let host = Host::new();
    host.ok(&["import", "base", &host.file("old.ext4", b"old base")]);
    let new = host.file("new.ext4", b"the new base");
    host.sd.set_state("active");
    let err = host.fails(&["import", "--replace", "base", &new]);
    assert!(err.contains("base is running"), "{err}");
    assert!(err.contains("runner stop base"), "{err}");

    // Asleep, its unit stays active, with its snapshot noted.
    let snapshot = host.store().snapshot("base").unwrap();
    snapshot.make_room().unwrap();
    let slept = Slept {
        generation: 0,
        at: Utc::now(),
    };
    snapshot.note(&slept).unwrap();
    let err = host.fails(&["import", "--replace", "base", &new]);
    assert!(err.contains("base is sleeping"), "{err}");

    // Nothing changed, and no copy was left.
    assert_eq!(fs::read(host.current("base")).unwrap(), b"old base");
    assert_eq!(host.gens("base"), ["0.ext4", "0.json"]);
}

#[test]
fn plain_import_of_a_name_that_exists_is_still_refused() {
    let host = Host::new();
    host.ok(&["import", "base", &host.file("old.ext4", b"old base")]);
    let err = host.fails(&["import", "base", &host.file("new.ext4", b"new")]);
    assert!(err.contains("a VM named base already exists"), "{err}");
    assert!(err.contains("--replace"), "{err}");
    assert_eq!(fs::read(host.current("base")).unwrap(), b"old base");
    // A plain import never asks systemd.
    assert!(host.sd.calls().is_empty(), "{:?}", host.sd.calls());
}

#[test]
fn replace_of_a_name_that_does_not_exist_imports_it() {
    let host = Host::new();
    let out = host.ok(&[
        "import",
        "--replace",
        "base",
        &host.file("new.ext4", b"new"),
    ]);
    assert_eq!(out, "base  gen 0  3 bytes\n");
}

#[test]
fn forks_keep_their_own_disks() {
    let host = Host::new();
    host.ok(&["import", "base", &host.file("old.ext4", b"old base")]);
    host.ok(&["create", "dev"]);
    host.ok(&[
        "import",
        "--replace",
        "base",
        &host.file("new.ext4", b"new"),
    ]);
    assert_eq!(fs::read(host.current("dev")).unwrap(), b"old base");
}

// 64 MiB, with data in a few places, one across a block and one across a
// MiB, and holes everywhere else. It ends in data, or in zeros, which only
// the copy's length keeps.
fn sparse_disk(ends_in_data: bool) -> Vec<u8> {
    let mut disk = vec![0u8; 64 << 20];
    let mut data = vec![
        (0, &b"superblock"[..]),
        (4094, b"across"),
        ((5 << 20) - 2, b"middle"),
    ];
    if ends_in_data {
        data.push(((64 << 20) - 3, b"end"));
    }
    for (at, what) in data {
        disk[at..at + what.len()].copy_from_slice(what);
    }
    disk
}

fn allocated(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

#[test]
fn an_import_from_stdin_of_a_sparse_disk_allocates_about_its_data() {
    let host = Host::new();
    let disk = sparse_disk(true);
    let out = host.ok_in(&["import", "base", "-"], Some(disk.clone()));
    assert_eq!(out, format!("base  gen 0  {} bytes\n", 64 << 20));
    let current = host.current("base");
    assert_eq!(fs::read(&current).unwrap(), disk);
    assert!(
        allocated(&current) < 1 << 20,
        "{} bytes allocated",
        allocated(&current)
    );
    assert_eq!(fs::metadata(&current).unwrap().mode() & 0o777, 0o644);

    let disk = sparse_disk(false);
    let out = host.ok_in(&["import", "--replace", "base", "-"], Some(disk.clone()));
    assert_eq!(out, format!("base  gen 1  {} bytes\n", 64 << 20));
    let current = host.current("base");
    assert_eq!(fs::read(&current).unwrap(), disk);
    assert!(
        allocated(&current) < 1 << 20,
        "{} bytes allocated",
        allocated(&current)
    );
}

// An ssh that drops, or a `-` with no `<`, ends the stream early: that's
// refused, not made base.
#[test]
fn a_stream_that_is_empty_or_cut_off_is_refused() {
    let host = Host::new();
    let out = host.runner(&["import", "base", "-"], Some(Vec::new()));
    assert!(!out.status.success(), "{}", text(&out.stdout));
    let err = text(&out.stderr);
    assert!(err.contains("the stream was empty"), "{err}");
    assert!(!host.root().join("vms/base").exists());

    host.ok(&["import", "base", &host.file("old.ext4", b"old base")]);
    let whole = fs::read(ext4(host.dir.path(), "whole.ext4", None)).unwrap();
    let cut = whole[..8 << 20].to_vec();
    let out = host.runner(&["import", "--replace", "base", "-"], Some(cut));
    let err = text(&out.stderr);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    assert!(
        err.contains(&format!("ended at {} bytes", 8 << 20)),
        "{err}"
    );
    assert!(err.contains(&format!("says {}", 16 << 20)), "{err}");
    assert_eq!(fs::read(host.current("base")).unwrap(), b"old base");
    assert_eq!(host.gens("base"), ["0.ext4", "0.json"]);
    // The whole one goes in.
    host.ok_in(&["import", "--replace", "base", "-"], Some(whole.clone()));
    assert_eq!(fs::read(host.current("base")).unwrap(), whole);
}

const PRINT: &str = "3f2a4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819";

// An ext4 disk made with mkfs.ext4 -d (e2fsprogs, no root), holding a
// /.runner/build with `record` in it, or none.
fn ext4(dir: &Path, name: &str, record: Option<&str>) -> String {
    let tree = dir.join(format!("{name}.tree"));
    fs::create_dir_all(tree.join(".runner")).unwrap();
    if let Some(record) = record {
        fs::write(tree.join(".runner/build"), record).unwrap();
    }
    let disk = dir.join(name);
    let out = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(&tree)
        .arg(&disk)
        .arg("16M")
        .output()
        .expect("mkfs.ext4 from e2fsprogs");
    assert!(out.status.success(), "{}", text(&out.stderr));
    disk.display().to_string()
}

fn record() -> String {
    format!("# What this image was built from.\nfingerprint={PRINT}\ncommit=42f0b7d1e2a3-dirty\n")
}

fn build() -> Build {
    Build {
        fingerprint: PRINT.to_string(),
        commit: Some("42f0b7d1e2a3-dirty".to_string()),
    }
}

#[test]
fn the_build_a_disk_records_goes_into_its_generation_s_note() {
    let host = Host::new();
    let built = ext4(host.dir.path(), "built.ext4", Some(&record()));
    host.ok(&["import", "base", &built]);
    let store = host.store();
    assert_eq!(store.build("base").unwrap(), Some(build()));
    let note = fs::read_to_string(host.root().join("vms/base/gens/0.json")).unwrap();
    assert!(note.contains(PRINT), "{note}");

    // A replace notes the new disk's; one without a record is unknown.
    let bare = ext4(host.dir.path(), "bare.ext4", None);
    host.ok(&["import", "--replace", "base", &bare]);
    assert_eq!(store.build("base").unwrap(), None);
    host.ok(&["undo", "base"]);
    assert_eq!(store.build("base").unwrap(), Some(build()));

    // From stdin too, read from the copy.
    let bytes = fs::read(&built).unwrap();
    host.ok_in(&["import", "--replace", "base", "-"], Some(bytes));
    assert_eq!(store.build("base").unwrap(), Some(build()));

    // A disk that isn't ext4 still comes in, its build unknown.
    host.ok(&["import", "raw", &host.file("raw.ext4", b"not ext4")]);
    assert_eq!(store.build("raw").unwrap(), None);
}

#[test]
fn a_record_that_does_not_say_is_unknown() {
    let host = Host::new();
    let store = host.store();
    for (name, record) in [
        ("short", "fingerprint=3f2a\n".to_string()),
        ("upper", format!("fingerprint={}\n", PRINT.to_uppercase())),
        ("none", "commit=42f0b7d1e2a3\n".to_string()),
    ] {
        let disk = ext4(host.dir.path(), &format!("{name}.ext4"), Some(&record));
        host.ok(&["import", name, &disk]);
        assert_eq!(store.build(name).unwrap(), None, "{name}");
    }
    // Without a commit, or with one that isn't one, the fingerprint still counts.
    for (name, record) in [
        ("nocommit", format!("fingerprint={PRINT}\n")),
        ("badcommit", format!("fingerprint={PRINT}\ncommit=a b\n")),
    ] {
        let disk = ext4(host.dir.path(), &format!("{name}.ext4"), Some(&record));
        host.ok(&["import", name, &disk]);
        let want = Build {
            fingerprint: PRINT.to_string(),
            commit: None,
        };
        assert_eq!(store.build(name).unwrap(), Some(want), "{name}");
    }
}
