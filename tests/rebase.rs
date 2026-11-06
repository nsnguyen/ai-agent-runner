// `runner rebase` in the store (src/rebase.rs): a VM's disk rebuilt from a
// copy of base's, with its /home, /root, machine id and services, on disks
// the tests build with mkfs.ext4 -d. No root, no mounts, no KVM.
mod common;

use common::{debugfs, image_of, tree};
use ext4_view::{Ext4, PathBuf as InPathBuf};
use runner::graft::{Skip, Why};
use runner::rebase::{self, Error, KEPT};
use runner::store::{Sizes, Source, Store};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const WANTS: &str = "etc/systemd/system/multi-user.target.wants";

// base as images/base builds it, in miniature: its system, its runner-guest,
// its Claude Code under /home/agent, an empty hostname and machine id, and
// a runner service no VM of its own should keep.
fn base_tree(at: &Path) -> PathBuf {
    let t = tree(
        at,
        &[
            ("etc/os-release", "base 2\n"),
            ("etc/hostname", ""),
            ("etc/machine-id", ""),
            ("etc/systemd/system/runner-guest.service", "[Unit]\n"),
            ("etc/systemd/system/runner-svc-stale.service", "[Unit]\n"),
            ("usr/local/bin/runner-guest", "guest 2"),
            ("home/agent/.bashrc", "base's bashrc\n"),
            ("home/agent/.local/share/claude/versions/2.0.0", "claude 2"),
            ("root/.profile", "base's profile\n"),
        ],
    );
    fs::create_dir_all(t.join(WANTS)).unwrap();
    symlink(
        "../runner-guest.service",
        t.join(WANTS).join("runner-guest.service"),
    )
    .unwrap();
    let stale = t.join(WANTS).join("runner-svc-stale.service");
    symlink("../runner-svc-stale.service", stale).unwrap();
    let bin = t.join("home/agent/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    symlink("../share/claude/versions/2.0.0", bin.join("claude")).unwrap();
    t
}

// A VM forked from an older base, then used: its own work, dotfiles, machine
// id, hostname, two services, an apt package and an older Claude Code.
fn dev_tree(at: &Path) -> PathBuf {
    let t = tree(
        at,
        &[
            ("etc/os-release", "base 1\n"),
            ("etc/hostname", "dev\n"),
            ("etc/machine-id", "0123456789abcdef\n"),
            ("etc/systemd/system/runner-guest.service", "[Unit]\nold\n"),
            (
                "etc/systemd/system/runner-svc-web.service",
                "[Service]\nweb\n",
            ),
            (
                "etc/systemd/system/runner-svc-api.service",
                "[Service]\napi\n",
            ),
            ("etc/systemd/system/not-the-runner-s.service", "[Service]\n"),
            ("usr/local/bin/runner-guest", "guest 1"),
            ("usr/bin/htop", "an apt package"),
            ("home/agent/.bashrc", "dev's bashrc\n"),
            ("home/agent/work/main.rs", "fn main() {}\n"),
            ("home/agent/.rustup/toolchains/stable/bin/rustc", "rustc"),
            ("home/agent/.local/share/claude/versions/1.0.0", "claude 1"),
            ("root/.bash_history", "ls\n"),
        ],
    );
    fs::create_dir_all(t.join(WANTS)).unwrap();
    symlink(
        "../runner-guest.service",
        t.join(WANTS).join("runner-guest.service"),
    )
    .unwrap();
    for svc in ["web", "api"] {
        let unit = format!("runner-svc-{svc}.service");
        symlink(format!("../{unit}"), t.join(WANTS).join(&unit)).unwrap();
    }
    let bin = t.join("home/agent/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    symlink("../share/claude/versions/1.0.0", bin.join("claude")).unwrap();
    t
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

struct Setup {
    dir: TempDir,
    store: Store,
}

impl Setup {
    fn gens(&self, vm: &str) -> Vec<String> {
        let gens = self.dir.path().join("root/vms").join(vm).join("gens");
        let mut names: Vec<String> = fs::read_dir(gens)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn current(&self, vm: &str) -> PathBuf {
        let generation = self.store.get(vm).unwrap().generation;
        self.store.disk(vm, generation).unwrap()
    }
}

// base imported, and dev made from an older one's disk, the agent's files
// owned as its guest would leave them.
fn setup(dev: impl FnOnce(&Path) -> PathBuf) -> Setup {
    setup_sized(dev, "32M")
}

// `setup`, with dev's disk `size` big, as mkfs.ext4 reads a size.
fn setup_sized(dev: impl FnOnce(&Path) -> PathBuf, size: &str) -> Setup {
    setup_with(base_tree, dev, size)
}

// `setup_sized`, with base's 32M disk made from `base`.
fn setup_with(
    base: impl FnOnce(&Path) -> PathBuf,
    dev: impl FnOnce(&Path) -> PathBuf,
    size: &str,
) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root")).with_net_file(dir.path().join("net"));
    let base = image_of(&base(&dir.path().join("base")), "32M");
    store.import("base", &base).unwrap();
    let old = image_of(&dev(&dir.path().join("dev")), size);
    debugfs(
        &old,
        "sif /home/agent/work/main.rs uid 1000\nsif /home/agent/work/main.rs gid 1000\n\
         sif /home/agent/work/main.rs mode 0100600\nsif /home/agent/work uid 1000\n",
    );
    store.import("dev", &old).unwrap();
    Setup { dir, store }
}

fn fs_of(disk: &Path) -> Ext4 {
    Ext4::load_from_path(disk).unwrap()
}

fn read(fs: &Ext4, path: &str) -> String {
    String::from_utf8(fs.read(&InPathBuf::try_from(path).unwrap()).unwrap()).unwrap()
}

fn has(fs: &Ext4, path: &str) -> bool {
    fs.symlink_metadata(&InPathBuf::try_from(path).unwrap())
        .is_ok()
}

fn link(fs: &Ext4, path: &str) -> String {
    let to = fs.read_link(&InPathBuf::try_from(path).unwrap()).unwrap();
    String::from_utf8(to.as_ref().to_vec()).unwrap()
}

#[test]
fn a_rebased_vm_has_base_s_system_and_keeps_its_own_files() {
    let s = setup(dev_tree);
    let config = s.store.set_idle("dev", "90s".parse().unwrap()).unwrap();
    let creds = s.store.creds("dev").unwrap();

    let done = rebase::rebase(&s.store, "dev").unwrap();

    assert_eq!((done.vm.name.as_str(), done.vm.generation), ("dev", 1));
    assert_eq!(done.base_generation, 0);
    assert_eq!(done.kept, KEPT);
    assert_eq!(done.services, ["api", "web"]);
    assert!(done.skipped.is_empty() && done.more == 0, "{done:?}");
    assert_eq!(done.own_claude, None);
    let fs = fs_of(&s.current("dev"));
    // base's system: its /etc, its runner-guest, its hostname.
    assert_eq!(read(&fs, "/etc/os-release"), "base 2\n");
    assert_eq!(read(&fs, "/usr/local/bin/runner-guest"), "guest 2");
    assert_eq!(read(&fs, "/etc/hostname"), "");
    assert_eq!(
        read(&fs, "/etc/systemd/system/runner-guest.service"),
        "[Unit]\n"
    );
    // What dev installed outside what's kept is gone.
    assert!(!has(&fs, "/usr/bin/htop"));
    assert!(!has(&fs, "/etc/systemd/system/not-the-runner-s.service"));
    // dev's own: its home, owners and modes too, root's, its machine id.
    assert_eq!(read(&fs, "/home/agent/work/main.rs"), "fn main() {}\n");
    let main = fs
        .symlink_metadata(&InPathBuf::try_from("/home/agent/work/main.rs").unwrap())
        .unwrap();
    assert_eq!((main.uid(), main.gid(), main.mode()), (1000, 1000, 0o600));
    assert_eq!(read(&fs, "/home/agent/.bashrc"), "dev's bashrc\n");
    let rustc = "/home/agent/.rustup/toolchains/stable/bin/rustc";
    assert_eq!(read(&fs, rustc), "rustc");
    assert_eq!(read(&fs, "/root/.bash_history"), "ls\n");
    assert!(!has(&fs, "/root/.profile"));
    assert_eq!(read(&fs, "/etc/machine-id"), "0123456789abcdef\n");
    // Its services and their links; base's own runner service is gone.
    for svc in ["web", "api"] {
        let unit = format!("/etc/systemd/system/runner-svc-{svc}.service");
        assert_eq!(read(&fs, &unit), format!("[Service]\n{svc}\n"));
        let wants = format!("/{WANTS}/runner-svc-{svc}.service");
        assert_eq!(link(&fs, &wants), format!("../runner-svc-{svc}.service"));
    }
    assert!(!has(&fs, "/etc/systemd/system/runner-svc-stale.service"));
    assert!(!has(&fs, &format!("/{WANTS}/runner-svc-stale.service")));
    assert_eq!(
        link(&fs, &format!("/{WANTS}/runner-guest.service")),
        "../runner-guest.service"
    );
    // Claude Code is base's.
    let share = "/home/agent/.local/share/claude/versions";
    assert_eq!(read(&fs, &format!("{share}/2.0.0")), "claude 2");
    assert!(!has(&fs, &format!("{share}/1.0.0")));
    let claude = "/home/agent/.local/bin/claude";
    assert_eq!(link(&fs, claude), "../share/claude/versions/2.0.0");

    // Noted as the rebase it is, and on top of history: undo goes back.
    let listed = s.store.checkpoints("dev").unwrap();
    assert_eq!(listed[0].id, 1);
    assert!(listed[0].current && listed[0].auto);
    assert_eq!(listed[0].comment.as_deref(), Some("rebase onto base gen 0"));
    // Its settings stay its own.
    assert_eq!(s.store.idle("dev").unwrap(), config);
    assert_eq!(s.store.creds("dev").unwrap(), creds);
    assert_eq!(s.store.undo("dev").unwrap().generation, 0);
    let fs = fs_of(&s.current("dev"));
    assert_eq!(read(&fs, "/usr/local/bin/runner-guest"), "guest 1");
    assert_eq!(read(&fs, "/usr/bin/htop"), "an apt package");
    // Nothing staged is left.
    assert_eq!(s.gens("dev"), ["0.ext4", "0.json", "1.ext4", "1.json"]);
}

// No Claude Code, no /root and no services of its own: base's Claude Code
// still comes, and base's /root goes, as dev had none.
#[test]
fn a_vm_without_some_of_what_s_kept_gets_base_s_claude_code_alone() {
    let s = setup(|at| tree(at, &[("home/agent/work/notes", "mine\n")]));

    let done = rebase::rebase(&s.store, "dev").unwrap();

    assert!(done.services.is_empty());
    let fs = fs_of(&s.current("dev"));
    assert_eq!(read(&fs, "/home/agent/work/notes"), "mine\n");
    assert!(!has(&fs, "/home/agent/.bashrc"));
    assert!(!has(&fs, "/root"));
    assert_eq!(read(&fs, "/etc/os-release"), "base 2\n");
    assert_eq!(
        link(&fs, "/home/agent/.local/bin/claude"),
        "../share/claude/versions/2.0.0"
    );
    let share = "/home/agent/.local/share/claude/versions/2.0.0";
    assert_eq!(read(&fs, share), "claude 2");
    assert!(!has(&fs, "/etc/systemd/system/runner-svc-stale.service"));
}

#[test]
fn a_held_vm_is_busy_and_gets_no_generation() {
    let s = setup(dev_tree);
    let before = s.gens("dev");

    let locked = s.store.lock("dev").unwrap();
    let busy = rebase::rebase(&s.store, "dev").unwrap_err();
    assert!(matches!(&busy, Error::Busy(vm) if vm == "dev"), "{busy}");
    assert!(
        busy.to_string().contains("stop it first: runner stop dev"),
        "{busy}"
    );
    drop(locked);
    // A boot holds it the same way while the VM is up.
    let run = s.store.begin_run("dev").unwrap();
    let busy = rebase::rebase(&s.store, "dev").unwrap_err();
    assert!(matches!(&busy, Error::Busy(vm) if vm == "dev"), "{busy}");
    run.discard();

    assert_eq!(s.gens("dev"), before);
    assert_eq!(s.store.get("dev").unwrap().generation, 0);
}

// A base that booted has a disk its guest wrote, which debugfs mustn't
// open: refused until base is imported again.
#[test]
fn a_base_whose_current_disk_a_guest_wrote_is_refused() {
    let s = setup(dev_tree);
    let run = s.store.begin_run("base").unwrap();
    run.commit().unwrap();
    let before = s.gens("dev");

    let refused = rebase::rebase(&s.store, "dev").unwrap_err();
    let said = refused.to_string();
    let why = "base's current generation, 1, came from \"run\", not an import";
    assert!(said.contains(why), "{said}");
    assert!(said.contains("Import base again first"), "{said}");
    assert_eq!(s.gens("dev"), before);

    // `runner import base --replace`, as image.rs does it.
    let disk = s.store.disk("base", 0).unwrap();
    let staged = s
        .store
        .stage_disk("base", Source::File(&disk), "import")
        .unwrap();
    s.store.finish_restore("base", &staged).unwrap();
    let done = rebase::rebase(&s.store, "dev").unwrap();
    assert_eq!(done.base_generation, 2);
}

#[test]
fn base_and_a_vm_that_isn_t_there_are_refused() {
    let s = setup(dev_tree);
    let said = rebase::rebase(&s.store, "base").unwrap_err().to_string();
    assert_eq!(said, "base can't be rebased onto itself");
    let said = rebase::rebase(&s.store, "nope").unwrap_err().to_string();
    assert_eq!(said, "no VM named nope");
    assert_eq!(s.store.get("base").unwrap().generation, 0);
}

// The new disk passes a forced check that changes nothing.
#[track_caller]
fn assert_clean(disk: &Path) {
    let out = Command::new("e2fsck")
        .arg("-fn")
        .arg(disk)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&[&out.stdout[..], &out.stderr[..]].concat()).into_owned();
    assert!(out.status.success(), "{said}");
}

// A tree of `files` with a file of noise `mib` MiB big at `big`.
fn with_big(at: &Path, files: impl FnOnce(&Path) -> PathBuf, big: &str, mib: usize) -> PathBuf {
    let t = files(at);
    let big = t.join(big);
    fs::create_dir_all(big.parent().unwrap()).unwrap();
    fs::write(big, noise(mib << 20)).unwrap();
    t
}

// A resize's bigger disk stays dev's through a rebase: the new one is a copy
// of base's, grown to dev's size, not base's size.
#[test]
fn a_resized_vm_keeps_its_disk_size_through_a_rebase() {
    let s = setup(dev_tree);
    let grown = Sizes {
        disk: Some(64 << 20),
        ..Sizes::default()
    };
    assert_eq!(s.store.resize("dev", &grown).unwrap().bytes, 64 << 20);

    let done = rebase::rebase(&s.store, "dev").unwrap();

    assert_eq!((done.vm.generation, done.vm.bytes), (2, 64 << 20));
    let disk = s.current("dev");
    assert_eq!(runner::image::ext4_size(&disk), Some(64 << 20));
    assert_clean(&disk);
    let fs = fs_of(&disk);
    assert_eq!(read(&fs, "/etc/os-release"), "base 2\n");
    assert_eq!(read(&fs, "/home/agent/work/main.rs"), "fn main() {}\n");
    // base's own disk is as it was.
    assert_eq!(s.store.get("base").unwrap().bytes, 32 << 20);
    assert_eq!(s.gens("base"), ["0.ext4", "0.json"]);
}

// A /home bigger than base's whole disk fits dev's own, which the new disk
// takes the size of.
#[test]
fn a_home_bigger_than_base_s_disk_comes_on_dev_s_bigger_one() {
    let s = setup_sized(
        |at| with_big(at, |at| tree(at, &[]), "home/agent/big", 40),
        "64M",
    );

    let done = rebase::rebase(&s.store, "dev").unwrap();

    assert_eq!(done.vm.bytes, 64 << 20);
    let disk = s.current("dev");
    assert_clean(&disk);
    let fs = fs_of(&disk);
    let big = fs.read(&InPathBuf::try_from("/home/agent/big").unwrap());
    assert!(big.unwrap() == noise(40 << 20), "/home/agent/big differs");
    assert_eq!(read(&fs, "/etc/os-release"), "base 2\n");
}

// A home that won't fit beside base's system fails the copy: the new disk
// goes, staged files too, and history stays as it was.
#[test]
fn a_failed_copy_leaves_the_vm_as_it_was() {
    // base's system takes 20M of its 32M. dev's disk is base's size, then
    // bigger, so its copy is grown before it fills.
    for (size, home) in [("32M", 20), ("48M", 30)] {
        let s = setup_with(
            |at| with_big(at, base_tree, "usr/lib/big", 20),
            |at| with_big(at, |at| tree(at, &[]), "home/agent/big", home),
            size,
        );
        let before = s.gens("dev");

        let failed = rebase::rebase(&s.store, "dev").unwrap_err();
        assert!(
            failed.to_string().contains("no room for the copy"),
            "{size}: {failed}"
        );
        assert_eq!(s.gens("dev"), before, "{size}");
        assert_eq!(s.store.get("dev").unwrap().generation, 0);
        assert_eq!(s.store.checkpoints("dev").unwrap().len(), 1);
    }
}

// A dev smaller than base comes out base's size: a disk only grows.
#[test]
fn a_vm_smaller_than_base_gets_base_s_size() {
    let s = setup_sized(dev_tree, "16M");
    assert_eq!(s.store.get("dev").unwrap().bytes, 16 << 20);

    let done = rebase::rebase(&s.store, "dev").unwrap();

    assert_eq!(done.vm.bytes, 32 << 20);
    assert_clean(&s.current("dev"));
    let fs = fs_of(&s.current("dev"));
    assert_eq!(read(&fs, "/home/agent/work/main.rs"), "fn main() {}\n");
}

// What the copy can't take is named, and the rest comes.
#[test]
fn names_the_copy_skips_are_listed() {
    let s = setup(|at| {
        tree(
            at,
            &[
                ("home/agent/work/ok", "ok"),
                ("home/agent/work/a\"quote", "skipped"),
            ],
        )
    });

    let done = rebase::rebase(&s.store, "dev").unwrap();

    let skip = Skip {
        path: "/home/agent/work/a\"quote".to_string(),
        why: Why::Name,
    };
    assert_eq!(done.skipped, [skip]);
    let fs = fs_of(&s.current("dev"));
    assert_eq!(read(&fs, "/home/agent/work/ok"), "ok");
}

// A dotfile manager's link where base's Claude Code would go: dev keeps
// its own Claude Code, and the rebase says so, rather than failing.
#[test]
fn a_linked_local_share_keeps_the_vm_s_own_claude_code() {
    let s = setup(|at| {
        let t = tree(
            at,
            &[
                (
                    "home/agent/dotfiles/share/claude/versions/1.0.0",
                    "claude 1",
                ),
                ("home/agent/work/notes", "mine\n"),
            ],
        );
        fs::create_dir_all(t.join("home/agent/.local")).unwrap();
        symlink("../dotfiles/share", t.join("home/agent/.local/share")).unwrap();
        t
    });

    let done = rebase::rebase(&s.store, "dev").unwrap();

    assert_eq!(done.own_claude.as_deref(), Some("/home/agent/.local/share"));
    let said = rebase::said(&done);
    assert!(
        said.contains(
            "Claude Code is dev's own, not base's: /home/agent/.local/share isn't a directory.\n"
        ),
        "{said}"
    );
    let fs = fs_of(&s.current("dev"));
    assert_eq!(link(&fs, "/home/agent/.local/share"), "../dotfiles/share");
    assert_eq!(read(&fs, "/home/agent/work/notes"), "mine\n");
    assert_eq!(read(&fs, "/etc/os-release"), "base 2\n");
    // base's link to it isn't there: the bin directory was dev's, empty.
    assert!(!has(&fs, "/home/agent/.local/bin"));
}
