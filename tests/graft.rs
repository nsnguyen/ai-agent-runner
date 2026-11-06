// Copying trees from one VM's disk into another's (src/graft.rs), on disks
// the tests build with mkfs.ext4 -d and read back with ext4-view: no root,
// no mounts, no KVM.
mod common;

use common::{debugfs, image, image_of, tree};
use ext4_view::{Ext4, PathBuf as InPathBuf};
use proptest::prelude::*;
use runner::changes::{self, Limits};
use runner::graft::{self, Copied, Error, Skip, Why};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::Duration;

// One path on a disk as ext4-view reads it: what the copy keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: &'static str,
    mode: u16,
    uid: u32,
    gid: u32,
    atime: (i64, u32),
    mtime: (i64, u32),
    // A file's contents or a link's target.
    data: Vec<u8>,
}

// Each path at and under `at` on `disk`, by its path below `at`.
fn listing(disk: &Path, at: &str) -> BTreeMap<Vec<u8>, Entry> {
    let fs = Ext4::load_from_path(disk).unwrap();
    let at = InPathBuf::try_from(at).unwrap();
    let mut out = BTreeMap::new();
    if fs.symlink_metadata(&at).is_ok() {
        walk(&fs, &at, b"", &mut out);
    }
    out
}

fn walk(fs: &Ext4, path: &InPathBuf, below: &[u8], out: &mut BTreeMap<Vec<u8>, Entry>) {
    let m = fs.symlink_metadata(path).unwrap();
    let t = m.file_type();
    let (kind, data) = if t.is_dir() {
        ("dir", Vec::new())
    } else if t.is_symlink() {
        ("link", fs.read_link(path).unwrap().as_ref().to_vec())
    } else if t.is_regular_file() {
        ("file", fs.read(path).unwrap())
    } else {
        ("special", Vec::new())
    };
    let time = |t: ext4_view::Timestamp| (t.seconds(), t.nanoseconds());
    let entry = Entry {
        kind,
        mode: m.mode(),
        uid: m.uid(),
        gid: m.gid(),
        atime: time(m.accessed()),
        mtime: time(m.modified()),
        data,
    };
    out.insert(below.to_vec(), entry);
    if t.is_dir() {
        for e in fs.read_dir(path).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().as_ref().to_vec();
            if name != b"." && name != b".." {
                let below = [below, b"/", &name].concat();
                walk(fs, &path.join(&name), &below, out);
            }
        }
    }
}

// Everything on `disk` but what's at or under `paths`, and the lost+found
// mkfs makes.
fn rest(disk: &Path, paths: &[&str]) -> BTreeMap<Vec<u8>, Entry> {
    let mut all = listing(disk, "/");
    all.retain(|path, _| {
        let path = if path.is_empty() {
            b"/".to_vec()
        } else {
            path.clone()
        };
        !paths.iter().any(|p| {
            let p = p.as_bytes();
            path == p || path.strip_prefix(p).is_some_and(|r| r.starts_with(b"/"))
        })
    });
    all
}

fn copy(from: &Path, to: &Path, paths: &[&str]) -> Copied {
    graft::copy(from, to, paths, changes::LIMITS).unwrap()
}

fn skip(path: &str, why: Why) -> Skip {
    let path = path.to_string();
    Skip { path, why }
}

// What's next to `disk` that a copy into it left: its staged files.
fn staged(disk: &Path) -> Vec<String> {
    let dir = fs::read_dir(disk.parent().unwrap()).unwrap();
    let names = dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned());
    let staging = graft::staging(disk);
    let staging = staging.file_name().unwrap().to_string_lossy();
    names.filter(|n| *n == staging).collect()
}

// Bytes no run of zeros hides, so the disk holds every block.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

// A VM's disk: what the user made under /home and /root, and its system.
fn dev(dir: &Path) -> std::path::PathBuf {
    let t = tree(
        &dir.join("dev"),
        &[
            ("home/agent/work/main.rs", "fn main() {}\n"),
            ("home/agent/work/src/lib.rs", "pub fn x() {}\n"),
            ("home/agent/.bashrc", "export EDITOR=vi\n"),
            ("home/agent/empty", ""),
            ("home/agent/secret", "token\n"),
            ("root/.profile", "umask 022\n"),
            ("etc/machine-id", "0123456789abcdef0123456789abcdef\n"),
            ("etc/hostname", ""),
            ("usr/bin/old-tool", "#!/bin/sh\n"),
        ],
    );
    fs::create_dir(t.join("home/agent/nothing")).unwrap();
    symlink("work/main.rs", t.join("home/agent/main")).unwrap();
    symlink("/etc/hostname", t.join("home/agent/abs")).unwrap();
    symlink("no/such/file", t.join("home/agent/dangling")).unwrap();
    let mode = |p: &str, m| fs::set_permissions(t.join(p), fs::Permissions::from_mode(m)).unwrap();
    mode("home/agent/secret", 0o600);
    mode("home/agent/work/src", 0o750);
    mode("usr/bin/old-tool", 0o755);
    let disk = image_of(&t, "64M");
    debugfs(
        &disk,
        "sif /home/agent/secret uid 1000\nsif /home/agent/secret gid 1000\n\
         sif /home/agent/work uid 1000\nsif /home/agent/work gid 100000\n\
         sif /home/agent/main uid 1000\n\
         sif /home/agent/work/main.rs mtime @1500000000\n\
         sif /home/agent/work/main.rs mtime_extra 4000\n",
    );
    disk
}

// A fresh copy of base: its own /home/agent, Claude Code's install in it,
// and a system the copy mustn't touch.
fn base(dir: &Path) -> std::path::PathBuf {
    let t = tree(
        &dir.join("base"),
        &[
            ("home/agent/.bashrc", "# base's\n"),
            ("home/agent/.local/share/claude/versions/2.1", "claude"),
            ("home/agent/work/main.rs", "base's own main\n"),
            ("root/.bashrc", "# root\n"),
            ("etc/machine-id", ""),
            ("etc/hostname", ""),
            ("etc/os-release", "ID=debian\n"),
            ("usr/local/bin/runner-guest", "guest v2"),
        ],
    );
    image_of(&t, "64M")
}

const KEPT: &[&str] = &["/home", "/root", "/etc/machine-id"];

#[test]
fn files_dirs_links_and_owners_come_across_whole() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dev(dir.path()), base(dir.path()));
    let before = rest(&b, KEPT);

    let got = copy(&a, &b, KEPT);

    assert_eq!(got, Copied::default());
    for path in KEPT {
        assert_eq!(listing(&b, path), listing(&a, path), "{path}");
    }
    // What base had there is gone: its Claude Code install, and its files
    // the user's replaced.
    let home = listing(&b, "/home");
    assert!(!home.contains_key(b"/agent/.local".as_slice()));
    let secret = &home[b"/agent/secret".as_slice()];
    assert_eq!((secret.uid, secret.gid, secret.mode), (1000, 1000, 0o600));
    let work = &home[b"/agent/work".as_slice()];
    assert_eq!((work.uid, work.gid), (1000, 100000));
    let main = &home[b"/agent/work/main.rs".as_slice()];
    assert_eq!(main.mtime, (1_500_000_000, 1000));
    assert_eq!(home[b"/agent/main".as_slice()].data, b"work/main.rs");
    assert_eq!(home[b"/agent/empty".as_slice()].kind, "file");
    // The rest of base is base's, down to its times.
    assert_eq!(rest(&b, KEPT), before);
    assert!(staged(&b).is_empty(), "{:?}", staged(&b));
}

#[test]
fn a_64_mib_file_comes_across_in_batches_with_others_around_it() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(
        &dir.path().join("a"),
        &[
            ("home/agent/a.txt", "before"),
            ("home/agent/z.txt", "after"),
        ],
    );
    let big = noise(64 << 20);
    fs::write(t.join("home/agent/big.bin"), &big).unwrap();
    let a = image_of(&t, "160M");
    let b = image_of(
        &tree(&dir.path().join("b"), &[("etc/os-release", "x")]),
        "160M",
    );

    assert_eq!(copy(&a, &b, &["/home"]), Copied::default());

    let home = listing(&b, "/home");
    assert_eq!(home, listing(&a, "/home"));
    assert!(home[b"/agent/big.bin".as_slice()].data == big);
    assert!(staged(&b).is_empty());
}

#[test]
fn what_the_target_had_there_is_gone_and_the_rest_untouched() {
    let dir = tempfile::tempdir().unwrap();
    // dev has no /root, and its x is a file where base has a directory.
    let a = image(&tree(
        &dir.path().join("a"),
        &[("home/agent/x", "a file now"), ("etc/machine-id", "id\n")],
    ));
    let b = image(&tree(
        &dir.path().join("b"),
        &[
            ("home/agent/x/deep/er/file", "base's"),
            ("home/agent/old.txt", "base's"),
            ("root/.bashrc", "base's"),
            ("etc/machine-id", ""),
            ("etc/os-release", "ID=debian\n"),
        ],
    ));
    let before = rest(&b, KEPT);

    copy(&a, &b, KEPT);

    assert_eq!(listing(&b, "/home"), listing(&a, "/home"));
    assert!(listing(&b, "/root").is_empty());
    assert_eq!(
        listing(&b, "/etc/machine-id"),
        listing(&a, "/etc/machine-id")
    );
    assert_eq!(rest(&b, KEPT), before);
}

#[test]
fn a_parent_the_target_lacks_is_made_as_the_sources() {
    let dir = tempfile::tempdir().unwrap();
    let wants = "etc/systemd/system/multi-user.target.wants";
    let t = tree(
        &dir.path().join("a"),
        &[("etc/systemd/system/runner-svc-web.service", "[Service]\n")],
    );
    fs::create_dir(t.join(wants)).unwrap();
    fs::set_permissions(t.join(wants), fs::Permissions::from_mode(0o750)).unwrap();
    let link = format!("{wants}/runner-svc-web.service");
    symlink("../runner-svc-web.service", t.join(&link)).unwrap();
    let a = image(&t);
    let b = image(&tree(
        &dir.path().join("b"),
        &[("etc/systemd/system/getty.target", "x")],
    ));
    let before = rest(&b, &["/etc/systemd/system/multi-user.target.wants"]);
    let link = format!("/{link}");

    copy(
        &a,
        &b,
        &["/etc/systemd/system/runner-svc-web.service", &link],
    );

    let made = listing(&b, &format!("/{wants}"));
    assert_eq!(made, listing(&a, &format!("/{wants}")));
    assert_eq!(made[&Vec::new()].mode, 0o750);
    let unit = "/etc/systemd/system/runner-svc-web.service";
    assert_eq!(listing(&b, unit), listing(&a, unit));
    let mut want = before;
    want.extend(listing(&a, unit).into_iter().map(|(k, v)| {
        let path = [b"/etc/systemd/system/runner-svc-web.service".as_slice(), &k].concat();
        (path, v)
    }));
    assert_eq!(
        rest(&b, &["/etc/systemd/system/multi-user.target.wants"]),
        want
    );
}

#[test]
fn names_debugfs_cant_take_are_skipped_named_and_change_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    // Where a shell that debugfs ran would leave a file: its directory,
    // and so this one, since a name has no slash.
    let pwned = Path::new("graft-pwned");
    let t = tree(
        &dir.path().join("a"),
        &[
            ("home/agent/ok.txt", "fine"),
            ("home/agent/new\nline", "x"),
            ("home/agent/q\"uote", "x"),
            ("home/agent/cr\rx", "x"),
            ("home/agent/bad\"dir/inside.txt", "x"),
            ("home/agent/sp ace and 'single'\\back", "fine too"),
        ],
    );
    // Were a newline to reach debugfs, each would end in a command of its
    // own, the shell's `#` or a newline cutting off the quote after it.
    let shell = "x\n!touch graft-pwned #";
    fs::write(t.join("home/agent").join(shell), "x").unwrap();
    fs::write(t.join("home/agent/y\nmkdir evil\n"), "x").unwrap();
    symlink("to\"where", t.join("home/agent/link")).unwrap();
    symlink("to\nmkdir /evil2", t.join("home/agent/link2")).unwrap();
    let a = image(&t);
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));
    let before = rest(&b, &["/home"]);

    let got = graft::copy(&a, &b, &["/home"], changes::LIMITS);

    assert!(!pwned.exists());
    let got = got.unwrap();
    let want = [
        "/home/agent/bad\"dir",
        "/home/agent/cr\rx",
        "/home/agent/link",
        "/home/agent/link2",
        "/home/agent/new\nline",
        "/home/agent/q\"uote",
        &format!("/home/agent/{shell}"),
        "/home/agent/y\nmkdir evil\n",
    ];
    let want: Vec<_> = want.iter().map(|p| skip(p, Why::Name)).collect();
    assert_eq!(
        got,
        Copied {
            skipped: want,
            more: 0
        }
    );
    let home = listing(&b, "/home");
    let names: Vec<_> = home.keys().map(|k| String::from_utf8_lossy(k)).collect();
    assert_eq!(
        names,
        [
            "",
            "/agent",
            "/agent/ok.txt",
            "/agent/sp ace and 'single'\\back"
        ]
    );
    let mut theirs = listing(&a, "/home");
    theirs.retain(|k, _| home.contains_key(k));
    assert_eq!(home, theirs);
    assert_eq!(rest(&b, &["/home"]), before);
}

#[test]
fn past_the_first_200_skipped_are_only_counted() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(&dir.path().join("a"), &[("home/ok", "fine")]);
    for i in 0..205 {
        fs::write(t.join(format!("home/bad\n{i:03}")), "x").unwrap();
    }
    let a = image_of(&t, "32M");
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));

    let got = copy(&a, &b, &["/home"]);

    let want = (0..200).map(|i| skip(&format!("/home/bad\n{i:03}"), Why::Name));
    let want = Copied {
        skipped: want.collect(),
        more: 5,
    };
    assert_eq!(got, want);
    assert_eq!(listing(&b, "/home").len(), 2);
}

#[test]
fn a_path_too_long_for_a_debugfs_line_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(&dir.path().join("a"), &[("home/ok", "fine")]);
    // A link target near the most a link holds, on a short path.
    symlink("ttt/".repeat(1022), t.join("home/link")).unwrap();
    let a = image(&t);
    // Directories 250 bytes a name, 16 deep: /home/a…, 4021 bytes of path,
    // then a directory past 4096 bytes and two files, one whose write fits
    // and one whose doesn't. Made with debugfs, since the host's own calls
    // take no path past 4096.
    let name = "a".repeat(250);
    let mut path = "/home".to_string();
    let mut made = String::new();
    for _ in 0..17 {
        path = format!("{path}/{name}");
        made.push_str(&format!("mkdir {path}\n"));
    }
    let deep = path[..path.len() - 251].to_string();
    let data = dir.path().join("data");
    fs::write(&data, "deep").unwrap();
    let long = format!("{deep}/{}", "g".repeat(60));
    made.push_str(&format!("write {} {deep}/f\n", data.display()));
    made.push_str(&format!("write {} {long}\n", data.display()));
    debugfs(&a, &made);
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));

    let got = copy(&a, &b, &["/home"]);

    let skipped = vec![
        skip(&path, Why::Long),
        skip(&long, Why::Long),
        skip("/home/link", Why::Long),
    ];
    assert_eq!(got, Copied { skipped, more: 0 });
    let home = listing(&b, "/home");
    let f = format!("{}/f", &deep["/home".len()..]);
    assert_eq!(home[f.as_bytes()].data, b"deep");
    assert_eq!(home[b"/ok".as_slice()].data, b"fine");
    assert_eq!(home.len(), 2 + 16 + 1);
}

#[test]
fn an_inode_ext4_view_refuses_is_skipped_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(
        &dir.path().join("a"),
        &[
            ("home/agent/ok", "fine"),
            ("home/agent/future", "x"),
            ("home/agent/past", "x"),
            ("home/agent/later/inside", "x"),
        ],
    );
    // A target whose one name is longer than a name can be.
    symlink("n".repeat(300), t.join("home/agent/link")).unwrap();
    let a = image(&t);
    // Times anyone can give a file: ext4-view 1.0 reads neither.
    debugfs(
        &a,
        "sif /home/agent/future mtime @2208988800\n\
         sif /home/agent/past mtime @-100\n\
         sif /home/agent/later atime @2208988800\n",
    );
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));

    let got = copy(&a, &b, &["/home"]);

    let skipped = ["future", "later", "link", "past"];
    let skipped = (skipped.iter())
        .map(|n| skip(&format!("/home/agent/{n}"), Why::Unreadable))
        .collect();
    assert_eq!(got, Copied { skipped, more: 0 });
    let home = listing(&b, "/home");
    let names: Vec<_> = home.keys().map(|k| String::from_utf8_lossy(k)).collect();
    assert_eq!(names, ["", "/agent", "/agent/ok"]);
}

#[test]
fn a_fifo_is_skipped_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(&dir.path().join("a"), &[("home/agent/ok.txt", "fine")]);
    let fifo = t.join("home/agent/pipe");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o644)).unwrap();
    let a = image(&t);
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));

    let got = copy(&a, &b, &["/home"]);

    let skipped = vec![skip("/home/agent/pipe", Why::Special)];
    assert_eq!(got, Copied { skipped, more: 0 });
    assert!(!listing(&b, "/home").contains_key(b"/agent/pipe".as_slice()));
    assert!(listing(&b, "/home").contains_key(b"/agent/ok.txt".as_slice()));
}

#[test]
fn a_source_past_a_limit_fails_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<_> = (0..10).map(|i| (format!("home/a/b/c/{i}"), "x")).collect();
    let files: Vec<_> = files.iter().map(|(p, t)| (p.as_str(), *t)).collect();
    let a = image(&tree(&dir.path().join("a"), &files));
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));
    let fails = |limits: Limits| match graft::copy(&a, &b, &["/home"], limits) {
        Err(Error::Disk(changes::Error::Unbounded { disk, why })) => {
            assert_eq!(disk, a);
            assert!(staged(&b).is_empty());
            why
        }
        other => panic!("{other:?}"),
    };

    let entries = Limits {
        entries: 5,
        ..changes::LIMITS
    };
    assert_eq!(fails(entries), "more than 5 entries");
    let depth = Limits {
        depth: 2,
        ..changes::LIMITS
    };
    assert!(fails(depth).starts_with("more than 2 directories deep"));
    let time = Limits {
        time: Duration::ZERO,
        ..changes::LIMITS
    };
    assert_eq!(fails(time), "copying took over 0s");
}

#[test]
fn a_copy_too_big_for_the_target_fails() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(&dir.path().join("a"), &[("home/agent/a", "x")]);
    fs::write(t.join("home/agent/big.bin"), noise(24 << 20)).unwrap();
    let a = image_of(&t, "64M");
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));

    let got = graft::copy(&a, &b, &["/home"], changes::LIMITS);

    assert!(
        matches!(&got, Err(Error::Full { disk, .. }) if *disk == b),
        "{got:?}"
    );
    assert!(staged(&b).is_empty());
}

#[test]
fn paths_must_be_plain_absolute_and_apart() {
    let dir = tempfile::tempdir().unwrap();
    let a = image(&tree(&dir.path().join("a"), &[("home/agent/a", "x")]));
    let b = image(&tree(&dir.path().join("b"), &[("etc/os-release", "x")]));
    let before = listing(&b, "/");
    let bad: &[&[&str]] = &[
        &["home"],
        &["/"],
        &[""],
        &["/home/"],
        &["/home//agent"],
        &["/home/../etc"],
        &["/home/./agent"],
        &["/ho\"me"],
        &["/ho\nme"],
        &["/home", "/home/agent"],
        &["/home", "/home"],
    ];
    for paths in bad {
        let got = graft::copy(&a, &b, paths, changes::LIMITS);
        assert!(matches!(got, Err(Error::Path { .. })), "{paths:?}: {got:?}");
    }
    let got = graft::copy(&a, &a, &["/home"], changes::LIMITS);
    assert!(matches!(got, Err(Error::Same { .. })), "{got:?}");
    assert_eq!(listing(&b, "/"), before);
}

#[test]
fn a_target_parent_that_isnt_a_directory_fails() {
    let dir = tempfile::tempdir().unwrap();
    let a = image(&tree(&dir.path().join("a"), &[("etc/machine-id", "id")]));
    let b = image(&tree(&dir.path().join("b"), &[("etc", "a file")]));
    let before = listing(&b, "/");

    let got = graft::copy(&a, &b, &["/etc/machine-id"], changes::LIMITS);

    assert!(
        matches!(&got, Err(Error::Parent { path, .. }) if path == "/etc"),
        "{got:?}"
    );
    assert_eq!(listing(&b, "/"), before);
}

// A name the generated trees use: no '/', and none of the bytes debugfs
// can't take, but spaces, quotes it can and bytes that aren't UTF-8.
fn name() -> impl Strategy<Value = Vec<u8>> {
    let byte = prop_oneof![
        prop::sample::select(b"abcXYZ019 ._-'\\$*!#".to_vec()),
        Just(0xff_u8),
        Just(0xc3_u8),
    ];
    prop::collection::vec(byte, 1..12).prop_filter("not . or ..", |n| n != b"." && n != b"..")
}

#[derive(Debug, Clone)]
enum Node {
    File(Vec<u8>, u32),
    Link(Vec<u8>),
    Dir(BTreeMap<Vec<u8>, Node>, u32),
}

fn node() -> impl Strategy<Value = Node> {
    let mode = prop::sample::select(vec![0o600, 0o644, 0o755, 0o4711, 0o1777]);
    let leaf = prop_oneof![
        (prop::collection::vec(any::<u8>(), 0..5000), mode.clone())
            .prop_map(|(d, m)| Node::File(d, m)),
        name().prop_map(Node::Link),
    ];
    leaf.prop_recursive(3, 24, 6, move |inner| {
        (
            prop::collection::btree_map(name(), inner, 0..6),
            mode.clone(),
        )
            .prop_map(|(kids, m)| Node::Dir(kids, m))
    })
}

fn build(at: &Path, node: &Node) {
    match node {
        Node::File(data, mode) => {
            fs::write(at, data).unwrap();
            fs::set_permissions(at, fs::Permissions::from_mode(*mode)).unwrap();
        }
        Node::Link(to) => symlink(std::ffi::OsStr::from_bytes(to), at).unwrap(),
        Node::Dir(kids, mode) => {
            fs::create_dir(at).unwrap();
            for (name, kid) in kids {
                build(&at.join(std::ffi::OsStr::from_bytes(name)), kid);
            }
            fs::set_permissions(at, fs::Permissions::from_mode(*mode | 0o700)).unwrap();
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn any_tree_comes_across_unchanged(data in node(), theirs in node()) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        build(&a.join("data"), &data);
        build(&b.join("data"), &theirs);
        fs::write(b.join("keep"), "base's").unwrap();
        let (a, b) = (image(&a), image(&b));
        let before = rest(&b, &["/data"]);

        let got = copy(&a, &b, &["/data"]);

        prop_assert_eq!(got, Copied::default());
        prop_assert_eq!(listing(&b, "/data"), listing(&a, "/data"));
        prop_assert_eq!(rest(&b, &["/data"]), before);
    }
}
