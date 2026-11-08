// What a run changed, measured from two ext4 disks that the tests build with
// mkfs.ext4 -d and edit with debugfs: no root, no mounts, no KVM.
mod common;

use common::{debugfs, image, image_of, tree};
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
    Change {
        path,
        change,
        contains: None,
        changed_under: None,
    }
}

fn whole_dir(path: &str, change: Kind, entries: u64) -> Change {
    let contains = Some(entries);
    Change {
        contains,
        ..self::change(path, change)
    }
}

fn summed(path: &str, change: Kind, paths: u64) -> Change {
    let changed_under = Some(paths);
    Change {
        changed_under,
        ..self::change(path, change)
    }
}

fn listed(paths: Vec<Change>, more: u64, boot_only: u64) -> Changes {
    Changes {
        paths,
        more,
        boot_only,
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
    assert_eq!(got, listed(want, 0, 0));
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
    assert_eq!(got, listed(want, 0, 0));
}

#[test]
fn the_list_is_capped_and_counts_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let before = image(&tree(&dir.path().join("before"), &[]));
    let files: Vec<_> = ["a", "b", "c", "d", "e"].map(|n| (n, n)).into();
    let after = image(&tree(&dir.path().join("after"), &files));

    let got = changes::between(&before, &after, 2, changes::TIME).unwrap();
    let want = vec![change("/a", Kind::Added), change("/b", Kind::Added)];
    assert_eq!(got, listed(want, 3, 0));
}

// Paths a guest names can be long, and a result goes to the caller whole.
#[test]
fn the_list_is_capped_by_its_bytes_too() {
    let dir = tempfile::tempdir().unwrap();
    let before = image(&tree(&dir.path().join("before"), &[]));
    let names: Vec<String> = (0..200)
        .map(|i| format!("{i:03}{}", "x".repeat(240)))
        .collect();
    let files: Vec<_> = names.iter().map(|n| (n.as_str(), "")).collect();
    let after = image(&tree(&dir.path().join("after"), &files));

    let got = changes::between(&before, &after, changes::CAP, changes::TIME).unwrap();
    let listed: usize = got.paths.iter().map(|c| c.path.len()).sum();
    assert!(listed <= changes::PATH_BYTES, "{listed} bytes listed");
    assert_eq!(got.paths.len(), changes::PATH_BYTES / 244);
    assert_eq!(got.paths.len() as u64 + got.more, 200);
}

// As a cancel does to a run whose disk is being measured.
#[test]
fn measuring_stops_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let before = image(&tree(&dir.path().join("before"), &[]));
    let after = image(&tree(&dir.path().join("after"), &[("a", "a")]));
    let got = changes::between_until(&before, &after, changes::CAP, changes::TIME, &|| true);
    let Err(e @ Error::Unbounded { .. }) = got else {
        panic!("measuring went on: {got:?}");
    };
    assert!(
        e.to_string().ends_with(".ext4: measuring was stopped"),
        "{e}"
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
    assert_eq!(got, listed(want, 0, 0));
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
    assert_eq!(got, listed(want, 0, 0));
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
    assert_eq!(got, listed(want, 0, 0));
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
    assert_eq!(got, listed(want, 0, 0));
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

    let base = store.disk("dev", run.from).unwrap();
    let got = changes::between(&base, &run.disk, changes::CAP, changes::TIME).unwrap();
    let want = vec![
        change("/home/agent/work/main.rs", Kind::Modified),
        change("/home/agent/work/new.rs", Kind::Added),
        change("/home/agent/work/old.rs", Kind::Deleted),
    ];
    assert_eq!(got, listed(want, 0, 0));
}

// What a boot of the base image writes on its own, beside the agent's one
// edit: the journal, the timers' stamps, systemd's random seed, the first
// boot's machine id and the login records.
fn a_boot_and_one_edit(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let id = "var/log/journal/73791ad19edd4802b88a49d7333b8a22";
    let before = tree(
        &dir.join("before"),
        &[
            ("etc/machine-id", ""),
            (&format!("{id}/system.journal"), "boot 1"),
            ("var/log/wtmp", "boot 1"),
            ("var/lib/systemd/random-seed", "seed 1"),
            ("home/agent/work/keep.txt", "keep"),
        ],
    );
    let after = tree(
        &dir.join("after"),
        &[
            ("etc/machine-id", "73791ad19edd4802b88a49d7333b8a22\n"),
            (&format!("{id}/system.journal"), "boot 1, boot 2"),
            (&format!("{id}/system@e14baf-01e3.journal"), "boot 1"),
            ("var/log/wtmp", "boot 1, boot 2"),
            ("var/lib/systemd/random-seed", "seed 2"),
            ("var/lib/systemd/timers/stamp-apt-daily.timer", ""),
            ("var/lib/systemd/timers/stamp-motd-news.timer", ""),
            ("home/agent/work/keep.txt", "keep"),
            ("home/agent/work/a.txt", "hi"),
        ],
    );
    (image(&before), image(&after))
}

// #107: the agent's changes don't get lost among the boot's.
#[test]
fn what_only_the_boot_writes_is_left_out_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let (before, after) = a_boot_and_one_edit(dir.path());
    let got = changes::between(&before, &after, changes::CAP, changes::TIME).unwrap();
    let want = vec![change("/home/agent/work/a.txt", Kind::Added)];
    // machine-id, the journal's two files, the seed, the timers directory
    // added whole, and wtmp.
    assert_eq!(got, listed(want, 0, 6));
}

// Left out, the boot's paths leave the whole list to the agent's.
#[test]
fn what_only_the_boot_writes_takes_no_room_in_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let (before, after) = a_boot_and_one_edit(dir.path());
    let got = changes::between(&before, &after, 1, changes::TIME).unwrap();
    let want = vec![change("/home/agent/work/a.txt", Kind::Added)];
    assert_eq!(got, listed(want, 0, 6));
}

// Only the paths themselves: not the same names elsewhere, a name that only
// starts the same, or what a user puts in a directory the boot makes.
#[test]
fn paths_beside_or_inside_what_the_boot_writes_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let was = [
        ("var/lib/systemd/x", ""),
        ("var/log/x", ""),
        ("home/agent/var/log/journal/old", ""),
        ("home/agent/etc/machine-id", "a"),
    ];
    let before = tree(&dir.path().join("before"), &was);
    let after = tree(
        &dir.path().join("after"),
        &[
            ("var/lib/systemd/x", ""),
            ("var/log/x", ""),
            ("home/agent/var/log/journal/old", ""),
            ("home/agent/var/log/journal/j", "the agent's"),
            ("home/agent/etc/machine-id", "b"),
            ("var/log/journal-old/j", "not the journal"),
            ("var/lib/systemd/linger/agent", ""),
            ("var/lib/systemd/random-seed.bak", ""),
        ],
    );
    let got = between(&before, &after);
    let want = vec![
        change("/home/agent/etc/machine-id", Kind::Modified),
        change("/home/agent/var/log/journal/j", Kind::Added),
        whole_dir("/var/lib/systemd/linger", Kind::Added, 1),
        change("/var/lib/systemd/random-seed.bak", Kind::Added),
        whole_dir("/var/log/journal-old", Kind::Added, 1),
    ];
    assert_eq!(got, listed(want, 0, 0));
}

// Every boot empties /tmp, then makes X11's socket directories in it, and
// the first boot makes systemd's linger directory: left out while empty.
#[test]
fn tmp_emptied_by_the_boot_and_the_empty_directories_it_makes_are_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(
        &dir.path().join("before"),
        &[("tmp/last-run.txt", "x"), ("tmp/cache/a", "x")],
    );
    fs::create_dir_all(before.join("var/lib/systemd")).unwrap();
    let after = tree(&dir.path().join("after"), &[("tmp/this-run.txt", "x")]);
    for empty in ["tmp/.X11-unix", "tmp/.ICE-unix", "var/lib/systemd/linger"] {
        fs::create_dir_all(after.join(empty)).unwrap();
    }
    let got = between(&before, &after);
    let want = vec![change("/tmp/this-run.txt", Kind::Added)];
    // Two empty directories, linger, and the two entries the boot deleted.
    assert_eq!(got, listed(want, 0, 5));
}

// What dockerd's and containerd's first start makes, as Docker 29 with
// containerd 2 made it, started on empty state directories: empty
// directories and a few small files, all on BOOT_ONLY.
const DOCKER_S_FILES: &[&str] = &[
    "var/lib/containerd/io.containerd.grpc.v1.introspection/uuid",
    "var/lib/containerd/io.containerd.metadata.v1.bolt/meta.db",
    "var/lib/docker/buildkit/cache.db",
    "var/lib/docker/buildkit/containerd-overlayfs/metadata_v2.db",
    "var/lib/docker/buildkit/containerd-overlayfs/workerid",
    "var/lib/docker/buildkit/history_c8d.db",
    "var/lib/docker/engine-id",
    "var/lib/docker/image/identity-cache.db",
    "var/lib/docker/network/files/local-kv.db",
    // A block device, there; a file stands in for it.
    "var/lib/docker/volumes/backingFsBlockDev",
    "var/lib/docker/volumes/metadata.db",
];
const DOCKER_S_DIRS: &[&str] = &[
    "var/lib/containerd/io.containerd.content.v1.content",
    "var/lib/containerd/io.containerd.snapshotter.v1.overlayfs/snapshots",
    "var/lib/containerd/tmpmounts",
    "var/lib/docker/buildkit/containerd-overlayfs/cachemounts",
    "var/lib/docker/buildkit/executor",
    "var/lib/docker/containers",
    "var/lib/docker/plugins/storage",
    "var/lib/docker/plugins/tmp",
    "var/lib/docker/runtimes",
    "var/lib/docker/swarm",
    "var/lib/docker/tmp",
];

// A tree as a VM's first boot leaves it: Docker's first start, with each
// file reading `boot`, beside `files`.
fn first_boot(dir: &Path, boot: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let mut all: Vec<(&str, &str)> = DOCKER_S_FILES.iter().map(|f| (*f, boot)).collect();
    all.extend(files);
    let tree = tree(dir, &all);
    for d in DOCKER_S_DIRS {
        fs::create_dir_all(tree.join(d)).unwrap();
    }
    tree
}

// #170: dockerd and containerd make their state directories at a VM's first
// boot, and every start rewrites four bolt databases there. Those are left
// out; what an agent's containers, volumes and images add is summed (#171),
// and a name that only starts the same is listed.
#[test]
fn docker_s_own_state_from_a_boot_is_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let keep = ("home/agent/work/keep.txt", "keep");
    // A fork's first boot: neither directory was there.
    let before = tree(&dir.path().join("before"), &[keep]);
    fs::create_dir_all(before.join("var/lib")).unwrap();
    let after = first_boot(&dir.path().join("after"), "1", &[keep]);
    // Each directory, added whole and all Docker's own, is one.
    assert_eq!(between(&before, &after), listed(vec![], 0, 2));

    // A later boot: the four databases rewritten, and what the agent ran.
    let later = first_boot(
        &dir.path().join("later"),
        "1",
        &[
            keep,
            ("var/lib/docker/network/files/local-kv.db", "2"),
            ("var/lib/docker/volumes/metadata.db", "2"),
            ("var/lib/docker/volumes/pgdata/_data/PG_VERSION", "17"),
            ("var/lib/docker/buildkit/cache.db", "2"),
            ("var/lib/docker/containers/c0ffee/config.v2.json", "{}"),
            (
                "var/lib/containerd/io.containerd.metadata.v1.bolt/meta.db",
                "2",
            ),
            ("var/lib/docker.bak/engine-id", "id"),
        ],
    );
    let got = between(&after, &later);
    let want = vec![
        // c0ffee and what's in it, and pgdata and the two under it.
        summed("/var/lib/docker", Kind::Modified, 5),
        whole_dir("/var/lib/docker.bak", Kind::Added, 1),
    ];
    assert_eq!(got, listed(want, 0, 4));

    // Gone, Docker's state is listed: no boot deletes it.
    let gone = tree(&dir.path().join("gone"), &[keep]);
    fs::create_dir_all(gone.join("var/lib")).unwrap();
    let got = between(&later, &gone);
    let want = vec![
        whole_dir("/var/lib/containerd", Kind::Deleted, 8),
        whole_dir("/var/lib/docker", Kind::Deleted, 29),
        whole_dir("/var/lib/docker.bak", Kind::Deleted, 1),
    ];
    assert_eq!(got, listed(want, 0, 0));
}

// containerd's first start, at a VM's first boot, also makes empty
// directories outside its state: /etc/cni/net.d and /opt/containerd's bin
// and lib (seen on base's own containerd, 2026-11-08's milestone check).
// They're the boot's; a file the agent put there lists the directory whole.
#[test]
fn containerd_s_empty_directories_from_a_first_boot_are_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let keep = ("home/agent/work/keep.txt", "keep");
    let made = |at: &Path| {
        for d in ["etc/cni/net.d", "opt/containerd/bin", "opt/containerd/lib"] {
            fs::create_dir_all(at.join(d)).unwrap();
        }
    };
    let before = tree(&dir.path().join("before"), &[keep]);
    fs::create_dir_all(before.join("etc")).unwrap();
    fs::create_dir_all(before.join("opt")).unwrap();
    let after = tree(&dir.path().join("after"), &[keep]);
    made(&after);
    assert_eq!(between(&before, &after), listed(vec![], 0, 2));

    let conf = ("etc/cni/net.d/10-bridge.conflist", "{}");
    let agent = tree(&dir.path().join("agent"), &[keep, conf]);
    made(&agent);
    let want = vec![whole_dir("/etc/cni", Kind::Added, 2)];
    assert_eq!(between(&before, &agent), listed(want, 0, 1));
}

// #171: on a VM's first boot, Docker's state is new whole, and what the
// agent's first run pulled and ran is in it beside Docker's own. That much
// is summed; Docker's own is left out.
#[test]
fn what_an_agent_runs_at_a_vm_s_first_boot_is_summed() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(&dir.path().join("before"), &[]);
    fs::create_dir_all(before.join("var/lib")).unwrap();
    let blob = "var/lib/containerd/io.containerd.content.v1.content/blobs/sha256/aa";
    let config = "var/lib/docker/containers/c0ffee/config.v2.json";
    let after = first_boot(
        &dir.path().join("after"),
        "1",
        &[(blob, "layer"), (config, "{}")],
    );
    let got = between(&before, &after);
    let want = vec![
        // The blob and the three directories it's in.
        summed("/var/lib/containerd", Kind::Added, 4),
        // The config, c0ffee and containers.
        summed("/var/lib/docker", Kind::Added, 3),
    ];
    // Docker's own: 7 of containerd's and 23 of Docker's.
    assert_eq!(got, listed(want, 0, 30));
}

// #171, Done when: a run whose disk gained 5,000 files under
// /var/lib/docker and 3 elsewhere lists one entry counting the 5,000, beside
// the 3 paths, and containerd's are one entry of their own.
#[test]
fn docker_s_and_containerd_s_changes_are_one_entry_each_with_a_count() {
    let dir = tempfile::tempdir().unwrap();
    let data = "var/lib/docker/volumes/pgdata/_data";
    let blobs = "var/lib/containerd/io.containerd.content.v1.content/blobs/sha256";
    let was = [
        ("home/agent/work/keep.txt", "keep"),
        ("var/lib/docker/engine-id", "id"),
        (&format!("{blobs}/aa") as &str, "layer 1"),
        (&format!("{data}/PG_VERSION"), "17"),
    ];
    let before = image(&tree(&dir.path().join("before"), &was));
    let pg: Vec<String> = (0..5000).map(|i| format!("{data}/{i:04}")).collect();
    let mut now: Vec<(&str, &str)> = was.to_vec();
    now[2].1 = "layer 2";
    let bb = format!("{blobs}/bb");
    now.push((&bb, "layer 3"));
    for f in ["a", "b", "c"] {
        now.push((f, f));
    }
    now.extend(pg.iter().map(|f| (f.as_str(), "")));
    // 5,000 inodes, more than a 16M disk has.
    let after = image_of(&tree(&dir.path().join("after"), &now), "64M");
    let got = changes::between(&before, &after, changes::CAP, changes::TIME).unwrap();
    let want = vec![
        change("/a", Kind::Added),
        change("/b", Kind::Added),
        change("/c", Kind::Added),
        // aa rewritten and bb added.
        summed("/var/lib/containerd", Kind::Modified, 2),
        summed("/var/lib/docker", Kind::Modified, 5000),
    ];
    assert_eq!(got, listed(want, 0, 0));
}

// Summed, the paths under Docker's state take one place in the list however
// many there are; once the list is full, they're one more in `more`, as a
// directory added whole is.
#[test]
fn a_summed_entry_takes_one_place_in_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let was = [("home/agent/keep", "k"), ("var/lib/docker/engine-id", "id")];
    let before = tree(&dir.path().join("before"), &was);
    fs::create_dir(before.join("var/lib/docker/overlay2")).unwrap();
    let before = image(&before);
    let after = image(&tree(
        &dir.path().join("after"),
        &[
            ("home/agent/keep", "k"),
            ("home/agent/a.txt", "a"),
            ("var/lib/docker/engine-id", "id"),
            ("var/lib/docker/overlay2/x/diff/etc/hosts", "h"),
            ("var/lib/docker/overlay2/y/diff/etc/hosts", "h"),
        ],
    ));
    let got = changes::between(&before, &after, 2, changes::TIME).unwrap();
    let want = vec![
        change("/home/agent/a.txt", Kind::Added),
        summed("/var/lib/docker", Kind::Modified, 8),
    ];
    assert_eq!(got, listed(want, 0, 0));
    // With room for one path, x and y, each added whole under it, are the
    // one more.
    let got = changes::between(&before, &after, 1, changes::TIME).unwrap();
    let want = vec![change("/home/agent/a.txt", Kind::Added)];
    assert_eq!(got, listed(want, 1, 0));
}

// Only under the directories themselves: their own mode is listed, and so
// is a name that only starts the same. With changes under it too, the
// directory is still one entry.
#[test]
fn only_what_is_under_docker_s_state_is_summed() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(
        &dir.path().join("before"),
        &[("var/lib/docker/engine-id", "1")],
    );
    let after = tree(
        &dir.path().join("after"),
        &[
            ("var/lib/docker/engine-id", "1"),
            ("var/lib/docker2/f", "x"),
            ("var/lib/dockerfile", "x"),
        ],
    );
    let mode = fs::Permissions::from_mode(0o700);
    fs::set_permissions(after.join("var/lib/docker"), mode).unwrap();
    let got = between(&before, &after);
    let want = vec![
        change("/var/lib/docker", Kind::Modified),
        whole_dir("/var/lib/docker2", Kind::Added, 1),
        change("/var/lib/dockerfile", Kind::Added),
    ];
    assert_eq!(got, listed(want, 0, 0));

    let files = [
        ("var/lib/docker/engine-id", "1"),
        ("var/lib/docker/new", "x"),
    ];
    let more = tree(&dir.path().join("more"), &files);
    fs::set_permissions(
        more.join("var/lib/docker"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let got = between(&before, &more);
    let want = vec![summed("/var/lib/docker", Kind::Modified, 1)];
    assert_eq!(got, listed(want, 0, 0));
}

// The list never reaches into /home, where the agent works, and nor does a
// summed entry.
#[test]
fn nothing_under_home_is_ever_left_out() {
    let summed = changes::SUMMED.iter();
    for path in changes::BOOT_ONLY.iter().map(|(p, _)| p).chain(summed) {
        assert!(path.starts_with('/') && !path.ends_with('/'), "{path}");
        assert!(*path != "/home" && !path.starts_with("/home/"), "{path}");
        assert!(*path != "/", "{path}");
    }
}

// A file on the list is left out only as a file: a directory in its place,
// and what's under it, are listed. A directory the boot makes empty is left
// out only while it's new and empty: a new mode on it is listed.
#[test]
fn only_what_the_boot_writes_there_is_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let before = tree(
        &dir.path().join("before"),
        &[("etc/mtab", "a file"), ("etc/machine-id", "")],
    );
    fs::create_dir_all(before.join("var/lib/systemd/linger")).unwrap();
    let after = tree(
        &dir.path().join("after"),
        &[("etc/mtab/hidden", "x"), ("etc/machine-id", "")],
    );
    fs::create_dir_all(after.join("var/lib/systemd/linger")).unwrap();
    let mode = fs::Permissions::from_mode(0o777);
    fs::set_permissions(after.join("var/lib/systemd/linger"), mode).unwrap();
    let got = between(&before, &after);
    let want = vec![
        whole_dir("/etc/mtab", Kind::Modified, 1),
        change("/var/lib/systemd/linger", Kind::Modified),
    ];
    assert_eq!(got, listed(want, 0, 0));
}
