// What a run changed, measured from two ext4 disks that the tests build with
// mkfs.ext4 -d and edit with debugfs: no root, no mounts, no KVM.
use runner::changes::{self, Change, Changes, Kind};
use runner::store::Store;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

// A directory tree from (path, contents) pairs, parents made as needed.
fn tree(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
    fs::create_dir(dir).unwrap();
    for (path, text) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir.to_path_buf()
}

fn ok(cmd: &mut Command) {
    let out = cmd
        .output()
        .expect("e2fsprogs (mkfs.ext4, debugfs) is installed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{cmd:?}: {err}");
}

// An ext4 disk holding a copy of `tree`, the way images/base builds one.
fn image(tree: &Path) -> PathBuf {
    let disk = tree.with_extension("ext4");
    ok(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .args([tree, &disk])
        .arg("16M"));
    disk
}

// Edits a disk in place, as a run's guest would.
fn debugfs(disk: &Path, request: &str) {
    ok(Command::new("debugfs")
        .args(["-w", "-R", request])
        .arg(disk));
}

fn change(path: &str, change: Kind) -> Change {
    let path = path.to_string();
    Change { path, change }
}

fn between(before: &Path, after: &Path) -> Changes {
    changes::between(&image(before), &image(after), changes::CAP).unwrap()
}

#[test]
fn a_file_added_one_modified_and_one_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(
        &dir.path().join("before"),
        &[
            ("same.txt", "same"),
            ("edit.txt", "aaaa"),
            ("gone.txt", "bye"),
        ],
    );
    let after = tree(
        &dir.path().join("after"),
        &[
            ("same.txt", "same"),
            ("edit.txt", "bbbb"),
            ("new.txt", "hi"),
        ],
    );
    // Touched but not changed: the contents decide, not the times.
    let then = SystemTime::now() - Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(after.join("same.txt"))
        .unwrap()
        .set_modified(then)
        .unwrap();

    let got = between(&before, &after);
    let want = vec![
        change("/edit.txt", Kind::Modified),
        change("/gone.txt", Kind::Deleted),
        change("/new.txt", Kind::Added),
    ];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
}

#[test]
fn modes_links_and_type_changes_count_and_whole_directories_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(
        &dir.path().join("before"),
        &[("mode.sh", "x"), ("swap", "a file"), ("old/f", "f")],
    );
    symlink("a", before.join("link")).unwrap();
    let after = tree(
        &dir.path().join("after"),
        &[("mode.sh", "x"), ("swap/inner", "now a directory")],
    );
    fs::set_permissions(after.join("mode.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("b", after.join("link")).unwrap();

    let got = between(&before, &after);
    let want = vec![
        change("/link", Kind::Modified),
        change("/mode.sh", Kind::Modified),
        change("/old", Kind::Deleted),
        change("/old/f", Kind::Deleted),
        change("/swap", Kind::Modified),
        change("/swap/inner", Kind::Added),
    ];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
}

#[test]
fn the_list_is_capped_and_counts_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let before = image(&tree(&dir.path().join("before"), &[]));
    let files: Vec<_> = ["a", "b", "c", "d", "e"].map(|n| (n, n)).into();
    let after = image(&tree(&dir.path().join("after"), &files));

    let got = changes::between(&before, &after, 2).unwrap();
    let want = vec![change("/a", Kind::Added), change("/b", Kind::Added)];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 3
        }
    );
}

#[test]
fn identical_disks_have_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    let disk = image(&tree(&dir.path().join("t"), &[("a/b/c", "deep")]));
    let copy = dir.path().join("copy.ext4");
    fs::copy(&disk, &copy).unwrap();
    let got = changes::between(&disk, &copy, changes::CAP).unwrap();
    assert_eq!(got, Changes::default());
}

#[test]
fn a_disk_that_is_not_ext4_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let good = image(&tree(&dir.path().join("t"), &[]));
    let junk = dir.path().join("junk.ext4");
    fs::write(&junk, vec![0u8; 1 << 20]).unwrap();
    let err = changes::between(&good, &junk, changes::CAP).unwrap_err();
    assert!(err.to_string().contains("junk.ext4"), "{err}");
}

// A disk that loops back on itself, as a guest with root could write one,
// ends in an error rather than a walk that never finishes.
#[test]
fn a_directory_that_contains_itself_is_an_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let before = image(&tree(&dir.path().join("before"), &[("d/f", "f")]));
    let after = dir.path().join("after.ext4");
    fs::copy(&before, &after).unwrap();
    debugfs(&after, "ln /d /d/again");
    let err = changes::between(&before, &after, changes::CAP).unwrap_err();
    assert!(err.to_string().contains("after.ext4"), "{err}");
}

// The way a real run looks: its disk is a clone of the last generation, and
// the guest changed a few files on it.
#[test]
fn a_run_reports_what_its_guest_changed_on_its_disk() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("etc/hostname", "dev"),
        ("home/agent/work/main.rs", "fn main() {}"),
        ("home/agent/work/old.rs", "// unused"),
    ];
    let base = image(&tree(&dir.path().join("t"), &files));
    let store = Store::new(dir.path().join("root"));
    store.import("dev", &base).unwrap();
    let run = store.begin_run("dev").unwrap();
    assert_eq!(run.from, 0);

    let new = dir.path().join("new.rs");
    fs::write(&new, "fn main() { println!(\"hi\") }").unwrap();
    let write = |to: &str| format!("write {} /home/agent/work/{to}", new.display());
    debugfs(&run.disk, "rm /home/agent/work/old.rs");
    debugfs(&run.disk, "rm /home/agent/work/main.rs");
    debugfs(&run.disk, &write("main.rs"));
    debugfs(&run.disk, &write("new.rs"));

    let got = run.changes(changes::CAP).unwrap();
    let want = vec![
        change("/home/agent/work/main.rs", Kind::Modified),
        change("/home/agent/work/new.rs", Kind::Added),
        change("/home/agent/work/old.rs", Kind::Deleted),
    ];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
}
