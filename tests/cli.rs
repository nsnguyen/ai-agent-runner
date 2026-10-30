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

mod common;
use common::FakeSystemd;

fn runner_on(root: &Path, systemd: &FakeSystemd, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .env("PATH", systemd.path())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn start_status_and_stop_drive_the_vms_unit() {
    let dir = with_base();
    let sd = FakeSystemd::new(dir.path());
    runner(dir.path(), &["create", "dev"]);
    let run = |args: &[&str]| {
        let out = runner_on(dir.path(), &sd, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    assert_eq!(run(&["status", "dev"]), "dev  gen 0  6 bytes  stopped\n");
    assert_eq!(run(&["start", "dev"]), "dev  gen 0  6 bytes  running\n");
    assert_eq!(run(&["status", "dev"]), "dev  gen 0  6 bytes  running\n");
    assert_eq!(run(&["stop", "dev"]), "dev  gen 0  6 bytes  stopped\n");
    let show = "systemctl show --property=ActiveState --value firecracker@dev.service";
    assert_eq!(
        sd.calls(),
        [
            show,
            "systemctl start firecracker@dev.service",
            show,
            show,
            "systemctl stop firecracker@dev.service",
            show,
        ]
    );
}

#[test]
fn status_says_booting_and_stopping_in_between() {
    let dir = with_base();
    let sd = FakeSystemd::new(dir.path());
    for (active, want) in [
        ("activating", "booting"),
        ("deactivating", "stopping"),
        ("failed", "stopped"),
    ] {
        sd.set_state(active);
        let out = runner_on(dir.path(), &sd, &["status", "base"]);
        assert_eq!(text(&out.stdout), format!("base  gen 0  6 bytes  {want}\n"));
    }
}

#[test]
fn a_missing_vm_never_reaches_systemd() {
    let dir = with_base();
    let sd = FakeSystemd::new(dir.path());
    for cmd in ["start", "stop", "status", "logs"] {
        let out = runner_on(dir.path(), &sd, &[cmd, "nope"]);
        assert_eq!(out.status.code(), Some(1), "{cmd}");
        assert!(text(&out.stderr).contains("no VM named nope"), "{cmd}");
    }
    assert_eq!(sd.calls(), Vec::<String>::new());
}

#[test]
fn a_failed_start_says_where_to_look() {
    let dir = with_base();
    let sd = FakeSystemd::new(dir.path());
    sd.fail_with("Job for firecracker@base.service failed because the control process exited with error code.\n");
    let out = runner_on(dir.path(), &sd, &["start", "base"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = text(&out.stderr);
    assert!(err.contains("the control process exited"), "{err}");
    assert!(err.contains("runner logs base"), "{err}");
}

#[test]
fn logs_reads_the_vms_journal() {
    let dir = with_base();
    let sd = FakeSystemd::new(dir.path());
    let out = runner_on(dir.path(), &sd, &["logs", "base"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "fake console: Linux version 6.1\n");
    runner_on(dir.path(), &sd, &["logs", "base", "-f", "-n", "20"]);
    assert_eq!(
        sd.calls(),
        [
            "journalctl --unit=firecracker@base.service --no-pager",
            "journalctl --unit=firecracker@base.service --no-pager --lines=20 --follow",
        ]
    );
}

#[test]
fn create_stores_sizes_and_a_fork_keeps_them() {
    let dir = with_base();
    let vms = dir.path().join("vms");
    let out = runner(dir.path(), &["create", "big", "--cpus", "4", "--mem", "4G"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let config = fs::read_to_string(vms.join("big/config.json")).unwrap();
    assert_eq!(config, "{\"cpus\":4,\"mem_mib\":4096}\n");
    runner(
        dir.path(),
        &["create", "big-2", "--from", "big", "--mem", "8192"],
    );
    let config = fs::read_to_string(vms.join("big-2/config.json")).unwrap();
    assert_eq!(config, "{\"cpus\":4,\"mem_mib\":8192}\n");
    // Defaults aren't written down, so a plain VM follows the host's.
    runner(dir.path(), &["create", "dev"]);
    assert!(!vms.join("dev/config.json").exists());

    for (args, want) in [
        (&["create", "x", "--cpus", "33"][..], "1 to 32 vCPUs"),
        (&["create", "x", "--mem", "lots"], "isn't a memory size"),
        (
            &["create", "x", "--kernel", "/no/such/vmlinux"],
            "/no/such/vmlinux",
        ),
    ] {
        let out = runner(dir.path(), args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out.stderr).contains(want),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    assert!(!vms.join("x").exists());
}

#[test]
fn create_takes_a_kernel_relative_to_where_i_am() {
    let dir = with_base();
    fs::write(dir.path().join("vmlinux"), b"kernel").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .current_dir(dir.path())
        .args(["create", "dev", "--kernel", "vmlinux"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let config = fs::read_to_string(dir.path().join("vms/dev/config.json")).unwrap();
    let kernel = fs::canonicalize(dir.path().join("vmlinux")).unwrap();
    assert_eq!(config, format!("{{\"kernel\":\"{}\"}}\n", kernel.display()));
}

#[test]
fn unit_prints_the_template_for_this_binary_and_root() {
    let dir = tempfile::tempdir().unwrap();
    let out = runner(dir.path(), &["unit", "--user", "runner"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let unit = text(&out.stdout);
    let bin = fs::canonicalize(env!("CARGO_BIN_EXE_runner")).unwrap();
    let bin = bin.display();
    assert!(
        unit.contains(&format!("\nExecStart={bin} boot %i\n")),
        "{unit}"
    );
    assert!(
        unit.contains(&format!("\nExecStop={bin} halt %i\n")),
        "{unit}"
    );
    let root = dir.path().display();
    assert!(
        unit.contains(&format!("\nEnvironment=RUNNER_ROOT={root}\n")),
        "{unit}"
    );
    assert!(unit.contains("\nUser=runner\n"), "{unit}");
}

#[test]
fn boot_and_halt_stay_out_of_the_help() {
    let dir = tempfile::tempdir().unwrap();
    let help = text(&runner(dir.path(), &["--help"]).stdout);
    for cmd in ["start", "stop", "status", "logs", "unit"] {
        assert!(help.contains(&format!("  {cmd} ")), "{cmd}: {help}");
    }
    assert!(!help.contains("  boot "), "{help}");
    assert!(!help.contains("  halt "), "{help}");
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
