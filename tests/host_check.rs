// host/check.sh against fakes, as tests/aws.rs fakes aws: cargo, sudo and
// runner on PATH, and a checkout of its own whose host/install.sh and
// images/base/build.sh are fakes too. Each fake records its call in
// $FAKE/calls and answers from files there, so a test says what the host has
// by making them. The base disks are real ext4, read with the real debugfs.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const OLD: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222222222222222222222222222";

// Every fake starts with this: its name and argv on a line, args split by US.
const LOG: &str = r#"{ printf '%s' "${0##*/}"; for arg; do printf '\037%s' "$arg"; done; printf '\n'; } >>"$FAKE/calls"
"#;

// test-output is what `cargo test` prints, test-exit its status, and
// build-fails fails every build.
const FAKE_CARGO: &str = r#"case "$1" in
build) [ ! -e "$FAKE/build-fails" ] || { echo "error: could not compile \`runner\`" >&2; exit 101; } ;;
test)
  { echo "runner=${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER-}"
    echo "disk=${RUNNER_TEST_DISK-}"
    echo "kernel=${RUNNER_TEST_KERNEL-}"
    echo "port=${RUNNER_GUEST_PORT-}"; } >"$FAKE/test-env"
  cat "$FAKE/test-output"
  exit "$(cat "$FAKE/test-exit" 2>/dev/null || echo 0)" ;;
*) echo "fake cargo: unexpected $*" >&2; exit 99 ;;
esac
"#;

// Drops -u, -E and VAR=value as the real one would, and runs the rest as is.
const FAKE_SUDO: &str = r#"while [ $# -gt 0 ]; do
  case "$1" in
  -u) shift 2 ;;
  -E) shift ;;
  *=*) export "$1"; shift ;;
  *) break ;;
  esac
done
exec "$@"
"#;

// host-fp is the fingerprint the host's base was built from: none, no base.
// doctor-extra is a line to add to doctor's. An import takes on the disk's
// record, unless import-fails or import-keeps.
const FAKE_RUNNER: &str = r#"case "$1" in
doctor)
  echo "ok    kvm          /dev/kvm opens read-write"
  if [ -e "$FAKE/host-fp" ]; then
    echo "ok    base         gen 3, 4096 bytes, built from $(cat "$FAKE/host-fp") at commit 0123456789ab"
  else
    echo "fail  base         no base VM, which create forks: runner import base - <path/to/base.ext4"
  fi
  cat "$FAKE/doctor-extra" 2>/dev/null
  if grep -q '^fail' "$FAKE/doctor-extra" 2>/dev/null || [ ! -e "$FAKE/host-fp" ]; then exit 1; fi ;;
import)
  [ ! -e "$FAKE/import-fails" ] || { echo "runner: base is up: runner stop base first" >&2; exit 1; }
  cat >"$FAKE/imported"
  [ -e "$FAKE/import-keeps" ] ||
    debugfs -R 'cat /.runner/build' "$FAKE/imported" 2>/dev/null | sed -n 's/^fingerprint=//p' >"$FAKE/host-fp" ;;
*) echo "fake runner: unexpected $*" >&2; exit 99 ;;
esac
"#;

const FAKE_INSTALL: &str = r#"echo "==> installing runner from the fake"
[ ! -e "$FAKE/install-fails" ] || { echo "install.sh: run it as root" >&2; exit 1; }
"#;

// want is the checkout's fingerprint. A build writes new.ext4 and new-kernel.
const FAKE_BUILD: &str = r#"out=$(dirname "$0")/out
if [ "${1-}" = --fingerprint ]; then cat "$FAKE/want"; exit 0; fi
echo "engine=${ENGINE-}" >"$FAKE/build-env"
mkdir -p "$out"
cp "$FAKE/new.ext4" "$out/base.ext4"
cp "$FAKE/new-kernel" "$out/vmlinux"
"#;

// What cargo test prints, cut from a real run.
const PASSING: &str = "   Compiling runner v0.1.0 (/home/ubuntu/ai-agent-runner)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 41.20s
     Running unittests src/lib.rs (target/debug/deps/runner-48f1c1bd67fc1817)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 212 filtered out; finished in 0.00s

     Running tests/base_image.rs (target/debug/deps/base_image-01220095c9b25911)

running 1 test
test base_boots_and_runner_guest_accepts_on_vsock ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 21 filtered out; finished in 9.12s

     Running tests/kvm.rs (target/debug/deps/kvm-2959d868bbe323b9)

running 2 tests
test a_vm_boots_and_answers_exec ... ok
test sleep_and_wake_keep_a_process ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 61.30s

   Doc-tests runner

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

";

const ONE_FAILS: &str = "     Running tests/kvm.rs (target/debug/deps/kvm-2959d868bbe323b9)

running 2 tests
test a_vm_boots_and_answers_exec ... FAILED
test sleep_and_wake_keep_a_process ... ok

failures:

---- a_vm_boots_and_answers_exec stdout ----

thread 'a_vm_boots_and_answers_exec' panicked at tests/kvm.rs:6:18:
no answer on vsock

failures:
    a_vm_boots_and_answers_exec

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 60.34s

error: test failed, to rerun pass `--test kvm`
     Running tests/net.rs (target/debug/deps/net-e50d42795a3cf632)

running 1 test
test a_tap_comes_and_goes ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.20s

error: 1 target failed:
    `--test kvm`
";

const CRASHES: &str = "     Running tests/kvm.rs (target/debug/deps/kvm-2959d868bbe323b9)

running 2 tests
test a_vm_boots_and_answers_exec ... ok
test sleep_and_wake_keep_a_process ... error: test failed, to rerun pass `--test kvm`

Caused by:
  process didn't exit successfully: `target/debug/deps/kvm-2959d868bbe323b9 --ignored --test-threads=1` (signal: 6, SIGABRT: process abort signal)
error: 1 target failed:
    `--test kvm`
";

const DOESNT_BUILD: &str = "   Compiling runner v0.1.0 (/home/ubuntu/ai-agent-runner)
error[E0308]: mismatched types
 --> tests/kvm.rs:4:22
  |
4 | fn broken() -> u32 { \"x\" }
  |                ---   ^^^ expected `u32`, found `&str`

For more information about this error, try `rustc --explain E0308`.
error: could not compile `runner` (test \"kvm\") due to 1 previous error
";

// A child that writes to the test's stdout puts its verdict on a line of its own.
const CHILD_WRITES: &str = "     Running tests/kvm.rs (target/debug/deps/kvm-2959d868bbe323b9)

running 2 tests
test a_vm_boots_and_answers_exec ... child-out
ok
test sleep_and_wake_keep_a_process ... more child-out
FAILED

failures:
    sleep_and_wake_keep_a_process

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 60.34s

error: test failed, to rerun pass `--test kvm`
";

const NONE_RAN: &str = "     Running tests/kvm.rs (target/debug/deps/kvm-2959d868bbe323b9)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s

";

fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\n{LOG}{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A base disk whose /.runner/build says it was built from `fp`.
fn disk(path: &Path, fp: &str) {
    let tree = tempfile::tempdir().unwrap();
    fs::create_dir(tree.path().join(".runner")).unwrap();
    let record = format!("fingerprint={fp}\ncommit=0123456789ab\n");
    fs::write(tree.path().join(".runner/build"), record).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let out = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(tree.path())
        .arg(path)
        .arg("16M")
        .output()
        .expect("mkfs.ext4 from e2fsprogs");
    assert!(out.status.success(), "{}", text(&out.stderr));
}

struct Host {
    dir: TempDir,
}

impl Host {
    /// A host whose base, disk and kernel all match a checkout made from
    /// NEW, and whose tests pass.
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let host = Host { dir };
        for sub in ["bin", "fake", "home", "root/kernel"] {
            fs::create_dir_all(host.path(sub)).unwrap();
        }
        script(&host.path("bin/cargo"), FAKE_CARGO);
        script(&host.path("bin/sudo"), FAKE_SUDO);
        script(&host.path("bin/runner"), FAKE_RUNNER);
        let repo = host.repo();
        fs::create_dir_all(repo.join("host")).unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("host/check.sh"),
            repo.join("host/check.sh"),
        )
        .unwrap();
        script(&repo.join("host/install.sh"), FAKE_INSTALL);
        script(&repo.join("images/base/build.sh"), FAKE_BUILD);

        host.has("want", NEW);
        host.has("host-fp", NEW);
        disk(&host.path("fake/new.ext4"), NEW);
        host.has("new-kernel", "kernel 2");
        disk(&host.out("base.ext4"), NEW);
        fs::write(host.out("vmlinux"), "kernel 2").unwrap();
        fs::write(host.path("root/kernel/vmlinux"), "kernel 2").unwrap();
        host.has("test-output", PASSING);
        host
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn repo(&self) -> PathBuf {
        self.path("repo")
    }

    fn out(&self, file: &str) -> PathBuf {
        self.repo().join("images/base/out").join(file)
    }

    /// Writes a file the fakes answer from, as their comments say.
    fn has(&self, what: &str, body: &str) {
        fs::write(self.path("fake").join(what), body).unwrap();
    }

    fn check(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap()
        );
        Command::new("bash")
            .arg(self.repo().join("host/check.sh"))
            .args(args)
            .current_dir(self.dir.path())
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("FAKE", self.path("fake"))
            .env("RUNNER_ROOT", self.path("root"))
            .env_remove("FORCE_BASE")
            .env_remove("ENGINE")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn run(&self) -> Output {
        self.check(&[], &[])
    }

    fn calls(&self) -> Vec<Vec<String>> {
        let calls = fs::read_to_string(self.path("fake/calls")).unwrap_or_default();
        calls
            .lines()
            .map(|line| line.split('\x1f').map(str::to_string).collect())
            .collect()
    }

    /// Where in the calls the first one starting with `argv` is, if any.
    fn called(&self, argv: &[&str]) -> Option<usize> {
        self.calls()
            .iter()
            .position(|c| c.len() >= argv.len() && c[..argv.len()] == *argv)
    }

    /// Whether base was built: a build.sh run but --fingerprint.
    fn built(&self) -> bool {
        self.calls()
            .iter()
            .any(|c| c[0] == "build.sh" && c.get(1).map(String::as_str) != Some("--fingerprint"))
    }
}

fn passes(out: &Output) -> String {
    let stdout = text(&out.stdout);
    assert!(
        out.status.success(),
        "{:?}\n{stdout}{}",
        out.status,
        text(&out.stderr)
    );
    stdout
}

fn fails(out: &Output) -> String {
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}{}", text(&out.stderr));
    stdout
}

/// The output's PASS or FAIL line for `what`: its name, then a colon, a
/// space or the end.
fn line<'a>(stdout: &'a str, verdict: &str, what: &str) -> &'a str {
    let names = |l: &str| {
        let rest = l.strip_prefix(verdict).map(str::trim_start);
        let rest = rest.and_then(|l| l.strip_prefix(what));
        rest.is_some_and(|r| r.is_empty() || r.starts_with([':', ' ']))
    };
    stdout
        .lines()
        .find(|l| names(l))
        .unwrap_or_else(|| panic!("no {verdict} line for {what}:\n{stdout}"))
}

fn count(stdout: &str, verdict: &str) -> usize {
    stdout
        .lines()
        .filter(|l| l.starts_with(&format!("{verdict} ")))
        .count()
}

#[test]
fn an_unchanged_base_builds_and_imports_nothing() {
    let host = Host::new();
    let stdout = passes(&host.run());
    assert!(!host.built(), "{:?}", host.calls());
    assert_eq!(host.called(&["runner", "import"]), None);
    assert!(
        line(&stdout, "PASS", "base").contains("unchanged"),
        "{stdout}"
    );
    assert!(
        line(&stdout, "PASS", "kernel").contains("unchanged"),
        "{stdout}"
    );
    // And the kernel isn't copied in again.
    let installs = host
        .calls()
        .into_iter()
        .filter(|c| c[0] == "sudo" && c.get(1).is_some_and(|a| a == "install"));
    assert_eq!(installs.count(), 0, "{:?}", host.calls());
}

#[test]
fn a_changed_base_is_built_then_imported_with_replace_from_stdin() {
    let host = Host::new();
    host.has("host-fp", OLD);
    disk(&host.out("base.ext4"), OLD);
    let stdout = passes(&host.run());

    let build = host
        .called(&["sudo", "ENGINE=podman"])
        .expect("no sudo build.sh");
    assert!(
        host.calls()[build]
            .last()
            .unwrap()
            .ends_with("images/base/build.sh")
    );
    assert_eq!(
        fs::read_to_string(host.path("fake/build-env")).unwrap(),
        "engine=podman\n"
    );
    let import = host
        .called(&["runner", "import", "--replace", "base", "-"])
        .expect("no import --replace from stdin");
    assert!(build < import, "{:?}", host.calls());
    // As its own user, as the runner user can't read the checkout.
    assert!(
        host.called(&["sudo", "-u", "runner", "runner", "import"])
            .is_some()
    );
    assert_eq!(
        fs::read(host.path("fake/imported")).unwrap(),
        fs::read(host.path("fake/new.ext4")).unwrap()
    );
    assert!(line(&stdout, "PASS", "base").contains("built"), "{stdout}");
    // Doctor saw the new base, and the tests ran on it.
    line(&stdout, "PASS", "doctor");
    line(&stdout, "PASS", "kvm: a_vm_boots_and_answers_exec");
}

#[test]
fn a_missing_base_is_built_then_imported() {
    let host = Host::new();
    fs::remove_file(host.path("fake/host-fp")).unwrap();
    fs::remove_file(host.out("base.ext4")).unwrap();
    passes(&host.run());
    assert!(host.built());
    assert!(
        host.called(&["runner", "import", "--replace", "base", "-"])
            .is_some()
    );
}

#[test]
fn a_missing_kernel_is_built_with_base() {
    let host = Host::new();
    fs::remove_file(host.out("vmlinux")).unwrap();
    passes(&host.run());
    assert!(host.built());
    assert_eq!(fs::read_to_string(host.out("vmlinux")).unwrap(), "kernel 2");
}

#[test]
fn a_base_only_the_host_lacks_is_imported_without_a_rebuild() {
    let host = Host::new();
    host.has("host-fp", OLD);
    let stdout = passes(&host.run());
    assert!(!host.built(), "{:?}", host.calls());
    assert!(
        host.called(&["runner", "import", "--replace", "base", "-"])
            .is_some()
    );
    assert!(
        line(&stdout, "PASS", "base").contains("imported"),
        "{stdout}"
    );
}

#[test]
fn force_base_rebuilds_an_unchanged_base() {
    let host = Host::new();
    passes(&host.check(&[], &[("FORCE_BASE", "1")]));
    assert!(host.built());
    assert!(
        host.called(&["runner", "import", "--replace", "base", "-"])
            .is_some()
    );
}

#[test]
fn a_kernel_that_differs_from_the_host_s_is_installed() {
    let host = Host::new();
    fs::write(host.path("root/kernel/vmlinux"), "kernel 1").unwrap();
    let stdout = passes(&host.run());
    assert_eq!(
        fs::read_to_string(host.path("root/kernel/vmlinux")).unwrap(),
        "kernel 2"
    );
    assert!(
        line(&stdout, "PASS", "kernel").contains("installed"),
        "{stdout}"
    );
}

#[test]
fn all_passing_exits_0_with_a_line_per_test_and_a_count_naming_the_log() {
    let host = Host::new();
    let stdout = passes(&host.run());
    for what in [
        "install",
        "guest",
        "base",
        "kernel",
        "doctor",
        "base_image: base_boots_and_runner_guest_accepts_on_vsock",
        "kvm: a_vm_boots_and_answers_exec",
        "kvm: sleep_and_wake_keep_a_process",
    ] {
        line(&stdout, "PASS", what);
    }
    assert_eq!(count(&stdout, "FAIL"), 0, "{stdout}");
    let last = stdout.lines().last().unwrap();
    assert!(last.contains("8 passed, 0 failed"), "{last}");
    let log = last
        .split("Log: ")
        .nth(1)
        .unwrap_or_else(|| panic!("{last}"));
    let log = fs::read_to_string(log.trim()).unwrap();
    // Each step's own output is there.
    assert!(log.contains("installing runner from the fake"), "{log}");
    assert!(
        log.contains("test sleep_and_wake_keep_a_process ... ok"),
        "{log}"
    );
    assert!(
        log.contains("ok    kvm          /dev/kvm opens read-write"),
        "{log}"
    );
}

#[test]
fn it_installs_then_tests_every_ignored_test_as_root_with_the_built_disk() {
    let host = Host::new();
    passes(&host.run());
    let build = host.called(&["cargo", "build", "--release"]).unwrap();
    let install = host.called(&["sudo"]).unwrap();
    assert!(
        host.calls()[install][1].ends_with("host/install.sh"),
        "{:?}",
        host.calls()
    );
    assert!(build < install);
    host.called(&[
        "cargo",
        "build",
        "--release",
        "--target",
        "x86_64-unknown-linux-musl",
        "--bin",
        "runner-guest",
    ])
    .expect("no runner-guest build");
    let doctor = host.called(&["sudo", "-u", "runner", "runner", "doctor"]);
    assert!(doctor.is_some(), "{:?}", host.calls());

    let test = host.called(&["cargo", "test"]).expect("no cargo test");
    assert_eq!(
        host.calls()[test],
        [
            "cargo",
            "test",
            "--no-fail-fast",
            "--",
            "--ignored",
            "--test-threads=1"
        ]
    );
    let out = host.repo().join("images/base/out");
    assert_eq!(
        fs::read_to_string(host.path("fake/test-env")).unwrap(),
        format!(
            "runner=sudo -E\ndisk={}\nkernel={}\nport=10022\n",
            out.join("base.ext4").display(),
            out.join("vmlinux").display()
        )
    );
}

#[test]
fn a_filter_runs_only_the_tests_that_match() {
    let host = Host::new();
    passes(&host.check(&["sleep"], &[]));
    let test = host.called(&["cargo", "test"]).unwrap();
    assert_eq!(host.calls()[test].last().unwrap(), "sleep");
}

#[test]
fn a_test_cargo_reports_failed_is_a_fail_naming_it() {
    let host = Host::new();
    host.has("test-output", ONE_FAILS);
    host.has("test-exit", "101");
    let stdout = fails(&host.run());
    line(&stdout, "FAIL", "kvm: a_vm_boots_and_answers_exec");
    line(&stdout, "PASS", "kvm: sleep_and_wake_keep_a_process");
    line(&stdout, "PASS", "net: a_tap_comes_and_goes");
    assert_eq!(count(&stdout, "FAIL"), 1, "{stdout}");
    assert!(
        stdout
            .lines()
            .last()
            .unwrap()
            .contains("7 passed, 1 failed"),
        "{stdout}"
    );
}

#[test]
fn a_test_binary_that_crashes_is_a_fail_naming_the_test() {
    let host = Host::new();
    host.has("test-output", CRASHES);
    host.has("test-exit", "101");
    let stdout = fails(&host.run());
    line(&stdout, "PASS", "kvm: a_vm_boots_and_answers_exec");
    line(&stdout, "FAIL", "kvm: sleep_and_wake_keep_a_process");
    assert_eq!(count(&stdout, "FAIL"), 1, "{stdout}");
}

#[test]
fn a_test_whose_verdict_lands_after_a_child_s_output_still_gets_its_line() {
    let host = Host::new();
    host.has("test-output", CHILD_WRITES);
    host.has("test-exit", "101");
    let stdout = fails(&host.run());
    line(&stdout, "PASS", "kvm: a_vm_boots_and_answers_exec");
    line(&stdout, "FAIL", "kvm: sleep_and_wake_keep_a_process");
    assert_eq!(count(&stdout, "FAIL"), 1, "{stdout}");
}

#[test]
fn a_test_binary_that_doesn_t_build_is_a_fail() {
    let host = Host::new();
    host.has("test-output", DOESNT_BUILD);
    host.has("test-exit", "101");
    let stdout = fails(&host.run());
    assert!(line(&stdout, "FAIL", "kvm").contains("build"), "{stdout}");
}

#[test]
fn no_test_run_is_a_fail() {
    let host = Host::new();
    host.has("test-output", NONE_RAN);
    let stdout = fails(&host.check(&["no_such_test"], &[]));
    line(&stdout, "FAIL", "tests");
}

#[test]
fn a_cargo_test_that_fails_without_naming_a_test_is_a_fail() {
    let host = Host::new();
    host.has("test-output", "error: no space left on device\n");
    host.has("test-exit", "101");
    let stdout = fails(&host.run());
    line(&stdout, "FAIL", "tests");
}

#[test]
fn a_doctor_line_that_isn_t_ok_is_a_fail_naming_it_and_the_tests_still_run() {
    let host = Host::new();
    host.has(
        "doctor-extra",
        "warn  reflink      /var/lib/runner can't reflink, so every fork and run copies the whole disk\n",
    );
    let stdout = fails(&host.run());
    let fail = line(&stdout, "FAIL", "doctor");
    assert!(fail.contains("reflink"), "{stdout}");
    line(&stdout, "PASS", "kvm: a_vm_boots_and_answers_exec");
}

#[test]
fn a_host_base_that_isn_t_the_checkout_s_after_import_is_a_fail() {
    let host = Host::new();
    host.has("host-fp", OLD);
    host.has("import-keeps", "");
    let stdout = fails(&host.run());
    let fail = line(&stdout, "FAIL", "doctor");
    assert!(
        fail.contains("base") && fail.contains(&NEW[..12]),
        "{stdout}"
    );
}

#[test]
fn a_failed_install_is_a_fail_and_nothing_after_it_runs() {
    let host = Host::new();
    host.has("install-fails", "");
    let stdout = fails(&host.run());
    assert!(
        line(&stdout, "FAIL", "install").contains("install.sh"),
        "{stdout}"
    );
    assert_eq!(host.called(&["cargo", "test"]), None);
    assert!(!host.built());
    assert!(
        stdout
            .lines()
            .last()
            .unwrap()
            .contains("0 passed, 1 failed"),
        "{stdout}"
    );
}

#[test]
fn a_failed_build_is_a_fail_and_nothing_after_it_runs() {
    let host = Host::new();
    host.has("build-fails", "");
    let stdout = fails(&host.run());
    assert!(
        line(&stdout, "FAIL", "install").contains("cargo build"),
        "{stdout}"
    );
    assert_eq!(host.called(&["sudo"]), None, "{:?}", host.calls());
}

#[test]
fn a_failed_import_is_a_fail_and_no_test_runs() {
    let host = Host::new();
    host.has("host-fp", OLD);
    host.has("import-fails", "");
    let stdout = fails(&host.run());
    assert!(line(&stdout, "FAIL", "base").contains("import"), "{stdout}");
    assert_eq!(host.called(&["cargo", "test"]), None);
}
