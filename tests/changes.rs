// What a run changed, measured from two ext4 disks that the tests build with
// mkfs.ext4 -d and edit with debugfs: no root, no mounts, no KVM.
mod common;

use common::{debugfs, image, tree};
use runner::changes::{self, Change, Changes, Error, Kind};
use runner::store::Store;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

fn change(path: &str, change: Kind) -> Change {
    let path = path.to_string();
    let contains = None;
    Change {
        path,
        change,
        contains,
    }
}

fn whole_dir(path: &str, change: Kind, entries: u64) -> Change {
    let contains = Some(entries);
    Change {
        contains,
        ..self::change(path, change)
    }
}

fn between(before: &Path, after: &Path) -> Changes {
    changes::between(&image(before), &image(after), changes::CAP, changes::TIME).unwrap()
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

// A directory added or deleted whole is one entry with a count, so a new
// cache or build directory can't push the real edits past the cap.
#[test]
fn modes_links_and_type_changes_count_and_whole_directories_are_one_entry() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(
        &dir.path().join("before"),
        &[("mode.sh", "x"), ("swap", "a file"), ("old/f", "f")],
    );
    symlink("a", before.join("link")).unwrap();
    let after = tree(
        &dir.path().join("after"),
        &[
            ("mode.sh", "x"),
            ("swap/inner", "now a directory"),
            ("cache/a/b", "x"),
            ("cache/c", "x"),
        ],
    );
    fs::set_permissions(after.join("mode.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("b", after.join("link")).unwrap();

    let got = between(&before, &after);
    let want = vec![
        whole_dir("/cache", Kind::Added, 3),
        change("/link", Kind::Modified),
        change("/mode.sh", Kind::Modified),
        whole_dir("/old", Kind::Deleted, 1),
        whole_dir("/swap", Kind::Modified, 1),
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

    let got = changes::between(&before, &after, 2, changes::TIME).unwrap();
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
fn names_that_are_not_utf8_stay_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(&dir.path().join("before"), &[]);
    let after = tree(&dir.path().join("after"), &[("caf\\xe9", "text")]);
    fs::write(after.join(OsStr::from_bytes(b"caf\xe9")), "latin-1").unwrap();
    let got = between(&before, &after);
    // A backslash doubles and a byte that isn't UTF-8 becomes \xNN.
    let want = vec![
        change(r"/caf\\xe9", Kind::Added),
        change(r"/caf\xe9", Kind::Added),
    ];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
}

// A guest with root can write its block device directly, under the
// filesystem, so the inode, times included, says nothing changed. The bytes do.
#[test]
fn bytes_written_under_the_filesystem_still_count() {
    let dir = tempfile::tempdir().unwrap();
    let was = "echo the original script, long enough to find on the disk";
    let now = was.replace("original", "replaced");
    let before = image(&tree(&dir.path().join("t"), &[("run.sh", was)]));
    let mut raw = fs::read(&before).unwrap();
    let at: Vec<_> = raw
        .windows(was.len())
        .enumerate()
        .filter(|(_, w)| *w == was.as_bytes())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(at.len(), 1, "the script's bytes are on the disk once");
    raw[at[0]..at[0] + now.len()].copy_from_slice(now.as_bytes());
    let after = dir.path().join("after.ext4");
    fs::write(&after, raw).unwrap();

    let got = changes::between(&before, &after, changes::CAP, changes::TIME).unwrap();
    let want = vec![change("/run.sh", Kind::Modified)];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
}

#[test]
fn identical_disks_have_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    let disk = image(&tree(&dir.path().join("t"), &[("a/b/c", "deep")]));
    let copy = dir.path().join("copy.ext4");
    fs::copy(&disk, &copy).unwrap();
    let got = changes::between(&disk, &copy, changes::CAP, changes::TIME).unwrap();
    assert_eq!(got, Changes::default());
}

#[test]
fn a_disk_that_is_not_ext4_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let good = image(&tree(&dir.path().join("t"), &[]));
    let junk = dir.path().join("junk.ext4");
    fs::write(&junk, vec![0u8; 1 << 20]).unwrap();
    let err = changes::between(&good, &junk, changes::CAP, changes::TIME).unwrap_err();
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
    let err = changes::between(&before, &after, changes::CAP, changes::TIME).unwrap_err();
    assert!(
        matches!(&err, Error::Unbounded { disk, .. } if *disk == after),
        "{err}"
    );
}

// Directories linked twice into their parents, sixteen deep, reach one file
// over a hundred thousand ways: small on disk, a walk of minutes. The walk's
// time is bounded, so it ends in an error.
#[test]
fn a_disk_that_fans_out_through_links_is_an_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let big = "x".repeat(256 << 10);
    let before = image(&tree(&dir.path().join("before"), &[("d16/f", &big)]));
    let links: String = (0..16)
        .rev()
        .map(|i| {
            format!(
                "mkdir /d{i}\nln /d{} /d{i}/a\nln /d{} /d{i}/b\n",
                i + 1,
                i + 1
            )
        })
        .collect();
    debugfs(&before, &links);
    let after = dir.path().join("after.ext4");
    fs::copy(&before, &after).unwrap();
    let started = Instant::now();
    let err = changes::between(&before, &after, changes::CAP, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(err, Error::Unbounded { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

// A link's length is an inode field like any other, so a guest can claim a
// target of a terabyte. It counts as changed, without reading it.
#[test]
fn a_link_claiming_a_huge_target_is_modified_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(&dir.path().join("t"), &[]);
    symlink("a", t.join("l")).unwrap();
    let before = image(&t);
    let after = dir.path().join("after.ext4");
    fs::copy(&before, &after).unwrap();
    debugfs(&after, "sif /l size 0x10000000000");
    let got = changes::between(&before, &after, changes::CAP, changes::TIME).unwrap();
    let want = vec![change("/l", Kind::Modified)];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
}

// A sparse file is mostly holes, so it can be far bigger than its disk.
#[test]
fn a_sparse_file_bigger_than_the_disk_is_measured() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(&dir.path().join("before"), &[("edit.txt", "aaaa")]);
    let after = tree(&dir.path().join("after"), &[("edit.txt", "bbbb")]);
    for t in [&before, &after] {
        let f = fs::File::create(t.join("sparse.img")).unwrap();
        f.set_len(64 << 20).unwrap();
    }
    let got = between(&before, &after);
    let want = vec![change("/edit.txt", Kind::Modified)];
    assert_eq!(
        got,
        Changes {
            paths: want,
            more: 0
        }
    );
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
