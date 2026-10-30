// images/base: the check over a built disk, on small ext4 files made here
// with mkfs.ext4 -d (e2fsprogs, no root), and the boot test, which needs KVM.
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const UNIT: &str = "etc/systemd/system/runner-guest.service";
const WANTS: &str = "etc/systemd/system/multi-user.target.wants/runner-guest.service";

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("images/base").join(name)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A root tree shaped like a good base image, minus everything a check
/// doesn't look at.
fn clean_tree() -> TempDir {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    let guest = root.join("usr/local/bin/runner-guest");
    fs::create_dir_all(guest.parent().unwrap()).unwrap();
    fs::write(&guest, b"guest").unwrap();
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).unwrap();
    symlink("/home/agent/.local/bin/claude", root.join("usr/local/bin/claude")).unwrap();
    fs::create_dir_all(root.join(WANTS).parent().unwrap()).unwrap();
    fs::write(root.join(UNIT), b"[Service]\n").unwrap();
    symlink("/etc/systemd/system/runner-guest.service", root.join(WANTS)).unwrap();
    fs::create_dir_all(root.join("home/agent/.local/bin")).unwrap();
    fs::create_dir_all(root.join("root")).unwrap();
    tree
}

fn disk(tree: &Path) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("base.ext4");
    let out = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(tree)
        .arg(&disk)
        .arg("16M")
        .output()
        .expect("mkfs.ext4 from e2fsprogs");
    assert!(out.status.success(), "{}", text(&out.stderr));
    (dir, disk)
}

fn check(tree: &Path) -> Output {
    let (_dir, disk) = disk(tree);
    Command::new("bash")
        .arg(script("check.sh"))
        .arg(&disk)
        .output()
        .unwrap()
}

fn fails_mentioning(out: &Output, what: &str) {
    let all = text(&out.stdout) + &text(&out.stderr);
    assert!(!out.status.success(), "check passed:\n{all}");
    assert!(all.contains(what), "no mention of {what}:\n{all}");
}

#[test]
fn check_passes_a_clean_image() {
    let tree = clean_tree();
    let out = check(tree.path());
    assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
}

#[test]
fn check_fails_when_the_guest_agent_is_not_enabled() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join(WANTS)).unwrap();
    fails_mentioning(&check(tree.path()), "runner-guest");
}

#[test]
fn check_fails_when_the_guest_agent_is_missing() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join("usr/local/bin/runner-guest")).unwrap();
    fails_mentioning(&check(tree.path()), "/usr/local/bin/runner-guest");
}

#[test]
fn check_fails_on_claude_state_in_the_agents_home() {
    let tree = clean_tree();
    let dir = tree.path().join("home/agent/.claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".credentials.json"), b"{}").unwrap();
    fails_mentioning(&check(tree.path()), "/home/agent/.claude");
}

#[test]
fn check_fails_on_claude_state_in_roots_home() {
    let tree = clean_tree();
    fs::write(tree.path().join("root/.claude.json"), b"{}").unwrap();
    fails_mentioning(&check(tree.path()), "/root/.claude.json");
}

#[test]
fn check_allows_an_empty_claude_dir() {
    let tree = clean_tree();
    fs::create_dir_all(tree.path().join("root/.claude")).unwrap();
    let out = check(tree.path());
    assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
}

/// Boots the built image in Firecracker: run on the KVM host with
/// `cargo test -- --ignored` after images/base/build.sh. Needs /dev/kvm,
/// firecracker on PATH, and RUNNER_GUEST_PORT.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, firecracker and a built images/base/out"]
fn base_boots_with_the_guest_agent_and_claude() {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("images/base/out");
    let out = Command::new("bash")
        .arg(script("boot-test.sh"))
        .arg(out_dir.join("base.ext4"))
        .arg(out_dir.join("vmlinux"))
        .output()
        .unwrap();
    let all = text(&out.stdout) + &text(&out.stderr);
    if out.status.code() == Some(77) {
        eprintln!("SKIPPED: {all}");
        return;
    }
    assert!(out.status.success(), "{all}");
}
