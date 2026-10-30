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

#[test]
fn creds_shows_and_sets_what_a_vm_gets() {
    let dir = with_base();
    runner(dir.path(), &["create", "dev"]);
    let out = runner(dir.path(), &["creds", "dev"]);
    assert_eq!(text(&out.stdout), "dev  creds claude\n");
    let out = runner(dir.path(), &["creds", "dev", "github", "claude"]);
    assert_eq!(text(&out.stdout), "dev  creds claude github\n");
    let out = runner(dir.path(), &["creds", "dev", "--none"]);
    assert_eq!(text(&out.stdout), "dev  creds none\n");
    let out = runner(dir.path(), &["creds", "dev"]);
    assert_eq!(text(&out.stdout), "dev  creds none\n");
}

#[test]
fn creds_refuses_an_unknown_name() {
    let dir = with_base();
    let out = runner(dir.path(), &["creds", "base", "aws"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("aws"), "{}", text(&out.stderr));
    let out = runner(dir.path(), &["creds", "base", "claude", "--none"]);
    assert!(!out.status.success());
}

#[test]
fn doctor_prints_a_line_per_check_and_fails_on_any_failure() {
    let dir = with_base();
    let out = runner(dir.path(), &["doctor"]);
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let names: Vec<&str> = lines
        .iter()
        .map(|l| l.split_whitespace().nth(1).unwrap_or(""))
        .collect();
    let all = [
        "kvm",
        "firecracker",
        "reflink",
        "kernel",
        "base",
        "nat",
        "unit",
    ];
    assert_eq!(names, all, "{stdout}");
    for l in &lines {
        let status = l.split_whitespace().next().unwrap();
        assert!(["ok", "warn", "fail"].contains(&status), "{l}");
    }
    assert!(lines[4].starts_with("ok "), "{stdout}");
    let failed = lines.iter().any(|l| l.starts_with("fail"));
    assert_eq!(out.status.success(), !failed, "{stdout}");
}

#[test]
fn ip_prints_a_vm_s_address() {
    let dir = with_base();
    runner(dir.path(), &["create", "dev"]);
    // The range is the host's (/etc/runner/net), so check the shape: base has
    // the first /30's guest address and dev the next.
    let addr = |name| {
        let out = runner(dir.path(), &["ip", name]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let shown = text(&out.stdout);
        u32::from(shown.trim().parse::<std::net::Ipv4Addr>().unwrap())
    };
    let (base, dev) = (addr("base"), addr("dev"));
    assert_eq!(base % 4, 2);
    assert_eq!(dev, base + 4);
    let out = runner(dir.path(), &["ip", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no VM named nope"));
}
