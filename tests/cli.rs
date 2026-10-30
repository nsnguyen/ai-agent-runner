// The binary end to end, as `ssh -t host runner …` runs it.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn runner(root: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_runner");
    Command::new(bin)
        .env("RUNNER_ROOT", root)
        .args(args)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn with_base() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let out = runner(dir.path(), &["import", "base", disk.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "base  gen 0  6 bytes\n");
    dir
}

#[test]
fn create_forks_base_and_list_shows_both() {
    let dir = with_base();
    let out = runner(dir.path(), &["create", "dev"]);
    assert_eq!(text(&out.stdout), "dev  gen 0  6 bytes\n");
    let out = runner(dir.path(), &["list"]);
    assert_eq!(
        text(&out.stdout),
        "base  gen 0  6 bytes\ndev  gen 0  6 bytes\n"
    );
}

#[test]
fn create_can_fork_any_vm() {
    let dir = with_base();
    runner(dir.path(), &["create", "dev"]);
    let out = runner(dir.path(), &["create", "dev-2", "--from", "dev"]);
    assert_eq!(text(&out.stdout), "dev-2  gen 0  6 bytes\n");
}

#[test]
fn list_says_when_there_is_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = runner(dir.path(), &["list"]);
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "no VMs yet\n");
}

#[test]
fn errors_go_to_stderr_with_a_failing_exit() {
    let dir = with_base();
    for (args, want) in [
        (&["undo", "base"][..], "base has no earlier generation"),
        (&["create", "dev", "--from", "nope"], "no VM named nope"),
        (&["create", "Bad"], "\"Bad\" isn't a VM name"),
    ] {
        let out = runner(dir.path(), args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            text(&out.stderr).contains(want),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
}

#[test]
fn delete_removes_a_vm() {
    let dir = with_base();
    runner(dir.path(), &["create", "dev"]);
    let out = runner(dir.path(), &["delete", "dev"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "deleted dev  gen 0  6 bytes\n");
    let out = runner(dir.path(), &["list"]);
    assert_eq!(text(&out.stdout), "base  gen 0  6 bytes\n");
    let out = runner(dir.path(), &["delete", "dev"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no VM named dev"));
}

// Each line with its timestamp, which no test can know, as T.
fn untimed(out: &[u8]) -> String {
    let timed = |col: &str| chrono::DateTime::parse_from_rfc3339(col).is_ok();
    text(out)
        .lines()
        .map(|l| {
            let cols: Vec<&str> = l.split("  ").collect();
            assert_eq!(cols.iter().filter(|c| timed(c)).count(), 1, "{l}");
            let cols: Vec<&str> = cols
                .into_iter()
                .map(|c| if timed(c) { "T" } else { c })
                .collect();
            cols.join("  ") + "\n"
        })
        .collect()
}

#[test]
fn checkpoint_list_and_restore() {
    let dir = with_base();
    runner(dir.path(), &["create", "dev"]);
    let out = runner(dir.path(), &["checkpoint", "dev", "-m", "clean"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        untimed(&out.stdout),
        "  1  T  checkpoint  6 bytes  \"clean\"\n"
    );
    let out = runner(dir.path(), &["checkpoint", "dev"]);
    assert_eq!(untimed(&out.stdout), "  2  T  checkpoint  6 bytes\n");

    let out = runner(dir.path(), &["checkpoints", "dev"]);
    assert_eq!(
        untimed(&out.stdout),
        "  2  T  checkpoint  6 bytes\n  1  T  checkpoint  6 bytes  \"clean\"\n* 0  T  auto  6 bytes  \"fork of base\"\n"
    );

    let out = runner(dir.path(), &["restore", "dev", "1"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "dev  gen 3  6 bytes\n");
    let out = runner(dir.path(), &["restore", "dev", "9"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(text(&out.stderr).contains("dev has no checkpoint 9"));
}
