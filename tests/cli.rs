// The binary end to end, as `ssh -t host runner …` runs it.
use runner::store::Store;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn runner(root: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_runner");
    // RUNNER_KEEP from the shell running the tests would change gc's answers.
    Command::new(bin)
        .env("RUNNER_ROOT", root)
        .env_remove("RUNNER_KEEP")
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
        format!("base  gen 0  6 bytes  {base} bytes used\ndev  gen 0  6 bytes  {dev} bytes used\n")
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
        format!("base  gen 0  6 bytes  {base} bytes used\n")
    );
    let out = runner(dir.path(), &["delete", "dev"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no VM named dev"));
}

// dev at generation 6, every generation committed and still on disk.
fn with_history() -> TempDir {
    let dir = with_base();
    let store = Store::new(dir.path()).keeping(usize::MAX);
    store.fork("base", "dev").unwrap();
    for _ in 0..6 {
        store.begin_run("dev").unwrap().commit().unwrap();
    }
    dir
}

#[test]
fn gc_dry_run_says_what_it_would_free_and_frees_nothing() {
    let dir = with_history();
    let gens = dir.path().join("vms/dev/gens");
    let bytes: u64 = (0..=4)
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
        format!("dev  would free gen 0 1 2 3 4  up to {bytes} bytes\n")
    );
    assert!(gens.join("0.ext4").exists());

    let out = runner(dir.path(), &["gc", "dev", "--keep", "1"]);
    assert_eq!(
        text(&out.stdout),
        format!("dev  freed gen 0 1 2 3 4  up to {bytes} bytes\n")
    );
    assert!(!gens.join("0.ext4").exists());
    assert!(!gens.join("4.ext4").exists());
    assert!(gens.join("5.ext4").exists());
}

#[test]
fn gc_sweeps_every_vm_and_keeps_five_by_default() {
    let dir = with_history();
    let out = runner(dir.path(), &["gc"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let got = text(&out.stdout);
    assert!(
        got.starts_with("base  nothing to free\ndev  freed gen 0  up to "),
        "{got}"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .env("RUNNER_KEEP", "2")
        .arg("gc")
        .output()
        .unwrap();
    let got = text(&out.stdout);
    assert!(
        got.starts_with("base  nothing to free\ndev  freed gen 1 2 3  up to "),
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
        text(&out.stdout).starts_with("dev  freed gen 0 1 2 3 4 5 7  up to "),
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

// Boot notes which credentials the guest took; status shows the names while
// the VM is up, and never a value.
#[test]
fn status_shows_the_credentials_the_guest_took_while_it_runs() {
    let dir = with_base();
    let sd = FakeSystemd::new(dir.path());
    runner(dir.path(), &["create", "dev"]);
    let run = |args: &[&str]| {
        let out = runner_on(dir.path(), &sd, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    let handed = dir.path().join("vms/dev/handed");
    sd.set_state("active");
    assert_eq!(run(&["status", "dev"]), "dev  gen 0  6 bytes  running\n");
    std::fs::write(&handed, "claude\ngithub\n").unwrap();
    assert_eq!(
        run(&["status", "dev"]),
        "dev  gen 0  6 bytes  running  creds claude github\n"
    );
    std::fs::write(&handed, "").unwrap();
    assert_eq!(
        run(&["status", "dev"]),
        "dev  gen 0  6 bytes  running  creds none\n"
    );
    // What a killed boot left is no VM's credentials.
    sd.set_state("inactive");
    assert_eq!(run(&["status", "dev"]), "dev  gen 0  6 bytes  stopped\n");
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
fn the_help_points_to_proxy_which_only_the_laptop_runs() {
    // The laptop's `runner --help` is this help, over SSH.
    let dir = tempfile::tempdir().unwrap();
    let help = text(&runner(dir.path(), &["--help"]).stdout);
    assert!(help.contains("runner proxy <vm> <port>"), "{help}");
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

#[test]
fn idle_shows_and_sets_how_long_a_vm_goes_unused_before_it_sleeps() {
    let dir = with_base();
    let ok = |args: &[&str]| {
        let out = runner(dir.path(), args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    ok(&["create", "dev"]);
    assert_eq!(ok(&["idle", "dev"]), "dev  idle 10m\n");
    assert_eq!(ok(&["idle", "dev", "30"]), "dev  idle 30m\n");
    assert_eq!(ok(&["idle", "dev"]), "dev  idle 30m\n");
    assert_eq!(ok(&["idle", "dev", "never"]), "dev  idle never\n");
    // A fork keeps it, unless told otherwise.
    ok(&["create", "dev-2", "--from", "dev"]);
    assert_eq!(ok(&["idle", "dev-2"]), "dev-2  idle never\n");
    ok(&["create", "quick", "--idle", "90s"]);
    assert_eq!(ok(&["idle", "quick"]), "quick  idle 90s\n");

    let out = runner(dir.path(), &["idle", "dev", "0"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("isn't an idle time"),
        "{}",
        text(&out.stderr)
    );
    let out = runner(dir.path(), &["idle", "nope", "5"]);
    assert!(!out.status.success());
    assert_eq!(ok(&["idle", "dev"]), "dev  idle never\n");
}

#[test]
fn url_prints_turns_off_and_on_and_sets_the_vm_side_port() {
    let dir = with_base();
    let run = |args: &[&str]| runner(dir.path(), args);
    let ok = |args: &[&str]| {
        let out = run(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    ok(&["create", "dev"]);
    let store = Store::new(dir.path());
    // dev took slot 1, after base.
    let port = runner::url::FIRST_PORT + 1;
    // On by default: its URL on a host with Tailscale up, else why not.
    let shown = |out: Output| {
        if out.status.success() {
            let url = text(&out.stdout);
            assert!(url.starts_with("http://100."), "{url}");
            assert!(url.ends_with(&format!(":{port}\n")), "{url}");
        } else {
            let err = text(&out.stderr);
            assert!(err.contains("no tailnet address"), "{err}");
        }
    };
    shown(run(&["url", "dev"]));

    let off = "dev's URL is off: `runner url dev on` turns it on\n";
    assert_eq!(ok(&["url", "dev", "off"]), off);
    assert_eq!(ok(&["url", "dev"]), off);
    // The port can change while it's off, and stays off.
    assert_eq!(ok(&["url", "dev", "--port", "3000"]), off);
    assert_eq!(store.url("dev").unwrap().guest_port(), 3000);
    assert!(store.url("dev").unwrap().off);
    shown(run(&["url", "dev", "on"]));
    assert!(!store.url("dev").unwrap().off);
    assert_eq!(store.url("dev").unwrap().guest_port(), 3000);

    let zero = run(&["url", "dev", "--port", "0"]);
    assert!(!zero.status.success());
    assert!(
        text(&zero.stderr).contains("not 0"),
        "{}",
        text(&zero.stderr)
    );
    let bad = run(&["url", "dev", "sideways"]);
    assert!(!bad.status.success());
    assert!(!run(&["url", "nope"]).status.success());
    assert_eq!(store.url("dev").unwrap().guest_port(), 3000);
}
