// The binary end to end, as `ssh -t host runner …` runs it.
use runner::store::Store;
use std::fs;
use std::os::unix::fs::MetadataExt;
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

// A VM's allocated bytes, which `list` shows and the filesystem decides.
fn used(root: &Path, vm: &str) -> u64 {
    fs::read_dir(root.join("vms").join(vm).join("gens"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().blocks() * 512)
        .sum()
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
    let (base, dev) = (used(dir.path(), "base"), used(dir.path(), "dev"));
    assert_eq!(
        text(&out.stdout),
        format!("base  gen 0  6 bytes  {base} used\ndev  gen 0  6 bytes  {dev} used\n")
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
    let base = used(dir.path(), "base");
    assert_eq!(
        text(&out.stdout),
        format!("base  gen 0  6 bytes  {base} used\n")
    );
    let out = runner(dir.path(), &["delete", "dev"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no VM named dev"));
}

// dev at generation 3, every generation committed and still on disk.
fn with_history() -> TempDir {
    let dir = with_base();
    let store = Store::new(dir.path()).keeping(usize::MAX);
    store.fork("base", "dev").unwrap();
    for _ in 0..3 {
        store.begin_run("dev").unwrap().commit().unwrap();
    }
    dir
}

#[test]
fn gc_dry_run_says_what_it_would_free_and_frees_nothing() {
    let dir = with_history();
    let gens = dir.path().join("vms/dev/gens");
    let bytes: u64 = [0, 1]
        .iter()
        .map(|g| {
            fs::metadata(gens.join(format!("{g}.ext4")))
                .unwrap()
                .blocks()
                * 512
        })
        .sum();
    let out = runner(dir.path(), &["gc", "dev", "--dry-run", "--keep", "1"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!("dev  would free gen 0 1  {bytes} bytes\n")
    );
    assert!(gens.join("0.ext4").exists());

    let out = runner(dir.path(), &["gc", "dev", "--keep", "1"]);
    assert_eq!(
        text(&out.stdout),
        format!("dev  freed gen 0 1  {bytes} bytes\n")
    );
    assert!(!gens.join("0.ext4").exists());
    assert!(!gens.join("1.ext4").exists());
}

#[test]
fn gc_sweeps_every_vm_and_keeps_five_by_default() {
    let dir = with_history();
    let out = runner(dir.path(), &["gc"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "base  nothing to free\ndev  nothing to free\n"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .env("RUNNER_KEEP", "2")
        .arg("gc")
        .output()
        .unwrap();
    let got = text(&out.stdout);
    assert!(
        got.starts_with("base  nothing to free\ndev  freed gen 0  "),
        "{got}"
    );
}

#[test]
fn gc_reports_a_busy_vm_and_still_sweeps_the_rest() {
    let dir = with_history();
    let store = Store::new(dir.path());
    let run = store.begin_run("dev").unwrap();
    let out = runner(dir.path(), &["gc", "--keep", "0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("dev is busy"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(text(&out.stdout), "base  nothing to free\n");
    drop(run);
    let out = runner(dir.path(), &["gc", "dev", "--keep", "0"]);
    assert!(
        text(&out.stdout).starts_with("dev  freed gen 0 1 2 4  "),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn gc_with_nothing_there_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let out = runner(dir.path(), &["gc"]);
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "no VMs yet\n");
    let out = runner(dir.path(), &["gc", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no VM named nope"));
}
