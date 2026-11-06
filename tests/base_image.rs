// images/base: the check over a built disk, on small ext4 files made here
// with mkfs.ext4 -d (e2fsprogs, no root), and the boot test, which needs KVM.
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const UNIT: &str = "etc/systemd/system/runner-guest.service";
const WANTS: &str = "etc/systemd/system/multi-user.target.wants/runner-guest.service";
const LLM_TXT: &str = ".runner/llm.txt";
const SETTINGS: &str = "etc/claude-code/managed-settings.json";
const BUILD: &str = ".runner/build";
const RECORD: &str = "# What this image was built from: images/base/build.sh wrote it.
fingerprint=3f2a4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819
commit=42f0b7d1e2a3
";

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("images/base")
        .join(name)
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
    symlink(
        "/home/agent/.local/bin/claude",
        root.join("usr/local/bin/claude"),
    )
    .unwrap();
    fs::create_dir_all(root.join(WANTS).parent().unwrap()).unwrap();
    fs::write(root.join(UNIT), b"[Service]\n").unwrap();
    symlink("/etc/systemd/system/runner-guest.service", root.join(WANTS)).unwrap();
    fs::create_dir_all(root.join("home/agent/.local/bin")).unwrap();
    fs::create_dir_all(root.join("root")).unwrap();
    fs::create_dir_all(root.join(".runner")).unwrap();
    fs::write(root.join(LLM_TXT), b"# You're in a runner VM\n").unwrap();
    fs::write(root.join(BUILD), RECORD).unwrap();
    // The image's own, as the Dockerfile installs it.
    let settings = root.join(SETTINGS);
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::copy(script("managed-settings.json"), &settings).unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).unwrap();
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

fn passes(out: &Output) {
    let all = text(&out.stdout) + &text(&out.stderr);
    assert!(out.status.success(), "check failed:\n{all}");
}

fn fails_mentioning(out: &Output, what: &str) {
    let all = text(&out.stdout) + &text(&out.stderr);
    assert!(!out.status.success(), "check passed:\n{all}");
    assert!(all.contains(what), "no mention of {what}:\n{all}");
}

#[test]
fn check_passes_a_clean_image() {
    let tree = clean_tree();
    passes(&check(tree.path()));
}

#[test]
fn check_fails_when_the_guest_agent_is_not_enabled() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join(WANTS)).unwrap();
    fails_mentioning(
        &check(tree.path()),
        "FAIL  runner-guest.service is not enabled",
    );
}

#[test]
fn check_fails_when_the_guest_agent_is_missing() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join("usr/local/bin/runner-guest")).unwrap();
    fails_mentioning(&check(tree.path()), "FAIL  /usr/local/bin/runner-guest");
}

#[test]
fn check_fails_on_claude_state_in_the_agents_home() {
    let tree = clean_tree();
    let dir = tree.path().join("home/agent/.claude");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".credentials.json"), b"{}").unwrap();
    fails_mentioning(
        &check(tree.path()),
        "FAIL  /home/agent/.claude is not empty",
    );
}

#[test]
fn check_fails_on_claude_state_in_roots_home() {
    let tree = clean_tree();
    fs::write(tree.path().join("root/.claude.json"), b"{}").unwrap();
    fails_mentioning(&check(tree.path()), "FAIL  /root/.claude.json exists");
}

#[test]
fn check_allows_an_empty_claude_dir() {
    let tree = clean_tree();
    fs::create_dir_all(tree.path().join("root/.claude")).unwrap();
    passes(&check(tree.path()));
}

#[test]
fn check_fails_without_the_agent_s_llm_txt() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join(LLM_TXT)).unwrap();
    fails_mentioning(&check(tree.path()), "FAIL  /.runner/llm.txt is missing");
}

#[test]
fn check_fails_without_the_build_record() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join(BUILD)).unwrap();
    fails_mentioning(&check(tree.path()), "FAIL  /.runner/build is missing");
}

#[test]
fn check_fails_when_the_build_record_has_no_fingerprint() {
    let tree = clean_tree();
    fs::write(tree.path().join(BUILD), "commit=42f0b7d1e2a3\n").unwrap();
    fails_mentioning(
        &check(tree.path()),
        "FAIL  /.runner/build has no fingerprint",
    );
}

#[test]
fn check_fails_without_claude_code_s_managed_settings() {
    let tree = clean_tree();
    fs::remove_file(tree.path().join(SETTINGS)).unwrap();
    fails_mentioning(
        &check(tree.path()),
        "FAIL  /etc/claude-code/managed-settings.json is missing",
    );
}

#[test]
fn check_fails_when_the_settings_leave_the_updater_on() {
    let tree = clean_tree();
    let on = br#"{"env": {"DISABLE_AUTOUPDATER": "0"}}"#;
    fs::write(tree.path().join(SETTINGS), on).unwrap();
    fails_mentioning(&check(tree.path()), "FAIL  Claude Code's updater is on");
}

// Claude Code reads the variable only from the settings' env.
#[test]
fn check_fails_when_the_setting_is_outside_env() {
    let tree = clean_tree();
    let outside = br#"{"DISABLE_AUTOUPDATER": "1", "env": {}}"#;
    fs::write(tree.path().join(SETTINGS), outside).unwrap();
    fails_mentioning(&check(tree.path()), "FAIL  Claude Code's updater is on");
}

#[test]
fn check_fails_when_the_agent_can_t_reach_the_settings() {
    let tree = clean_tree();
    let dir = tree.path().join(SETTINGS);
    let dir = dir.parent().unwrap();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    fails_mentioning(
        &check(tree.path()),
        "FAIL  /etc/claude-code/managed-settings.json isn't readable",
    );
}

#[test]
fn check_fails_when_the_agent_can_t_read_the_settings() {
    let tree = clean_tree();
    let settings = tree.path().join(SETTINGS);
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    fails_mentioning(
        &check(tree.path()),
        "FAIL  /etc/claude-code/managed-settings.json isn't readable",
    );
}

// Every claude in a VM reads its managed settings, however it starts, and
// these turn its updater off: a new version comes only from rebuilding base.
#[test]
fn the_image_turns_claude_code_s_updater_off() {
    let json = fs::read_to_string(script("managed-settings.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(settings["env"]["DISABLE_AUTOUPDATER"], "1", "{json}");
    let dockerfile = fs::read_to_string(script("Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("COPY managed-settings.json /etc/claude-code/managed-settings.json"),
        "the Dockerfile doesn't install managed-settings.json"
    );
    let build = fs::read_to_string(script("build.sh")).unwrap();
    assert!(
        build.contains("\"$here/managed-settings.json\""),
        "build.sh doesn't copy managed-settings.json"
    );
}

// A VM with a list of names gets REFUSED for the rest, which an agent could
// take for a broken network and work around: llm.txt says it's the owner's
// list, and to ask.
#[test]
fn llm_txt_says_a_refused_name_is_off_the_owner_s_list() {
    let llm = fs::read_to_string(script("llm.txt")).unwrap();
    for said in [
        "## The network",
        "REFUSED",
        "off the owner's list for this VM",
        "ask the owner",
        "don't work around it",
        "runner net",
        runner::dns::ALWAYS[0],
    ] {
        assert!(llm.contains(said), "llm.txt doesn't say {said:?}");
    }
}

// What the agent reads to know where it is: the commands it names are the
// ones runner-guest has, and the numbers are the runner's own.
#[test]
fn llm_txt_says_how_to_checkpoint_and_restore_as_the_runner_does() {
    let llm = fs::read_to_string(script("llm.txt")).unwrap();
    for said in [
        "runner-guest checkpoint -m",
        "runner-guest checkpoints",
        "runner-guest restore <id>",
        runner::claude::DIR,
        &format!("{} an hour", runner::checkpoint::AGENT_PER_HOUR),
        &format!("newest {}", runner::store::KEEP_CHECKPOINTS),
        "CLAUDE_CODE_OAUTH_TOKEN",
        "GH_TOKEN",
        &format!("{}<name>", runner::services::PREFIX),
    ] {
        assert!(llm.contains(said), "llm.txt doesn't say {said:?}");
    }
    for call in ["checkpoint", "checkpoints", "restore"] {
        let help = Command::new(env!("CARGO_BIN_EXE_runner-guest"))
            .args([call, "--help"])
            .output()
            .unwrap();
        assert!(help.status.success(), "runner-guest has no {call}");
    }
    // Dockerfile puts it where llm.txt itself says it is.
    let dockerfile = fs::read_to_string(script("Dockerfile")).unwrap();
    assert!(dockerfile.contains("COPY llm.txt /.runner/llm.txt"));
    assert!(llm.contains("/.runner/llm.txt"));
    assert!(
        llm.contains("/.runner/build"),
        "llm.txt doesn't say what /.runner/build is"
    );
    let build = fs::read_to_string(script("build.sh")).unwrap();
    assert!(
        build.contains("\"$here/llm.txt\""),
        "build.sh doesn't copy llm.txt"
    );
}

// A copy of images/base, out/ left out, in a temp directory of its own,
// with a stand-in runner-guest beside it, so a test can change either.
fn base_copy() -> (TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("images/base");
    fs::create_dir_all(&base).unwrap();
    for entry in fs::read_dir(script("")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), base.join(entry.file_name())).unwrap();
        }
    }
    let guest = dir.path().join("runner-guest");
    fs::write(&guest, b"guest v1").unwrap();
    // Readable by anyone, for the run as nobody below.
    for path in [dir.path(), &dir.path().join("images"), &base] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, base, guest)
}

// `build.sh --fingerprint`, run as nobody when the tests run as root, so it
// shows it needs no root.
fn fingerprint(base: &Path, guest: &Path) -> String {
    fingerprint_with(base, guest, &[])
}

fn fingerprint_with(base: &Path, guest: &Path, env: &[(&str, &Path)]) -> String {
    let build = base.join("build.sh");
    let mut cmd = if nix::unistd::geteuid().is_root() {
        let mut cmd = Command::new("setpriv");
        cmd.args(["--reuid=65534", "--regid=65534", "--clear-groups", "bash"]);
        cmd
    } else {
        Command::new("bash")
    };
    let out = cmd
        .arg(&build)
        .arg("--fingerprint")
        .env("RUNNER_GUEST", guest)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    // The copy has no source beside it to be older than.
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    let print = text(&out.stdout);
    let print = print.strip_suffix('\n').unwrap_or(&print).to_string();
    let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    assert!(print.len() == 64 && print.chars().all(hex), "{print:?}");
    print
}

#[test]
fn the_fingerprint_changes_with_what_the_image_is_made_from_and_nothing_else() {
    let (dir, base, guest) = base_copy();
    let first = fingerprint(&base, &guest);
    assert_eq!(fingerprint(&base, &guest), first);

    // What a build leaves, and what lies outside images/base, don't count.
    fs::create_dir_all(base.join("out")).unwrap();
    fs::write(base.join("out/base.ext4"), b"a built disk").unwrap();
    fs::write(dir.path().join("README.md"), b"elsewhere").unwrap();
    let llm = base.join("llm.txt");
    let then = fs::metadata(&llm).unwrap().modified().unwrap();
    let file = fs::File::options().write(true).open(&llm).unwrap();
    file.set_modified(then + std::time::Duration::from_secs(60))
        .unwrap();
    assert_eq!(fingerprint(&base, &guest), first);

    // A file under images/base does, back to the same print once it's undone.
    let was = fs::read(&llm).unwrap();
    fs::write(&llm, b"# You're somewhere else\n").unwrap();
    let edited = fingerprint(&base, &guest);
    assert_ne!(edited, first);
    fs::write(&llm, &was).unwrap();
    assert_eq!(fingerprint(&base, &guest), first);
    // So does its mode, a new file, and the kernel pin in build.sh.
    fs::set_permissions(&llm, fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(fingerprint(&base, &guest), first);
    fs::set_permissions(&llm, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(fingerprint(&base, &guest), first);
    fs::write(base.join("extra.conf"), b"x").unwrap();
    assert_ne!(fingerprint(&base, &guest), first);
    fs::remove_file(base.join("extra.conf")).unwrap();
    symlink("llm.txt", base.join("link.txt")).unwrap();
    assert_ne!(fingerprint(&base, &guest), first);
    fs::remove_file(base.join("link.txt")).unwrap();
    // A build's output elsewhere under images/base doesn't count either.
    let out = base.join("built");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("base.ext4"), b"a built disk").unwrap();
    assert_eq!(fingerprint_with(&base, &guest, &[("OUT", &out)]), first);
    fs::remove_dir_all(&out).unwrap();
    let build = base.join("build.sh");
    let sh = fs::read_to_string(&build).unwrap();
    let pin = sh
        .lines()
        .find(|l| l.starts_with("kernel_sha256="))
        .unwrap();
    fs::write(&build, sh.replace(pin, "kernel_sha256=00")).unwrap();
    assert_ne!(fingerprint(&base, &guest), first);
    fs::write(&build, &sh).unwrap();
    assert_eq!(fingerprint(&base, &guest), first);

    // And so does the guest binary.
    fs::write(&guest, b"guest v2").unwrap();
    assert_ne!(fingerprint(&base, &guest), first);
}

// A pull changes the guest's source but not the binary build.sh hashes.
#[test]
fn the_fingerprint_warns_of_a_guest_older_than_its_source() {
    let (dir, base, guest) = base_copy();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let rs = src.join("guest.rs");
    fs::write(&rs, b"fn main() {}").unwrap();
    let then = fs::metadata(&guest).unwrap().modified().unwrap();
    let file = fs::File::options().write(true).open(&rs).unwrap();
    file.set_modified(then + std::time::Duration::from_secs(60))
        .unwrap();
    let out = Command::new("bash")
        .arg(base.join("build.sh"))
        .arg("--fingerprint")
        .env("RUNNER_GUEST", &guest)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("older than the source"),
        "{}",
        text(&out.stderr)
    );
    // The print is the same: only the warning says.
    fs::remove_dir_all(&src).unwrap();
    assert_eq!(text(&out.stdout).trim(), fingerprint(&base, &guest));
}

#[test]
fn the_fingerprint_needs_the_guest_binary() {
    let (dir, base, _guest) = base_copy();
    let out = Command::new("bash")
        .arg(base.join("build.sh"))
        .arg("--fingerprint")
        .env("RUNNER_GUEST", dir.path().join("missing"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no runner-guest"),
        "{}",
        text(&out.stderr)
    );
    assert!(out.stdout.is_empty());
}

// build.sh writes the record into the image, with the print --fingerprint
// gives, before it makes the disk that check.sh, which fails an image
// without one, looks at.
#[test]
fn build_sh_writes_the_record_it_fingerprints() {
    let build = fs::read_to_string(script("build.sh")).unwrap();
    let write = build
        .find("/.runner/build\"")
        .expect("build.sh doesn't write /.runner/build");
    let mkfs = build.find("mkfs.ext4 -q").unwrap();
    assert!(
        write < mkfs,
        "build.sh makes the disk before writing the record"
    );
    assert!(
        build.contains("fingerprint=$print"),
        "build.sh doesn't record its fingerprint"
    );
}

/// Boots the built image in Firecracker: run on the KVM host with
/// `cargo test -- --ignored` after images/base/build.sh. Needs /dev/kvm,
/// firecracker on PATH, and RUNNER_GUEST_PORT.
#[test]
#[ignore = "boots a VM: needs /dev/kvm, firecracker and a built images/base/out"]
fn base_boots_and_runner_guest_accepts_on_vsock() {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("images/base/out");
    let out = Command::new("bash")
        .arg(script("boot-test.sh"))
        .arg(out_dir.join("base.ext4"))
        .arg(out_dir.join("vmlinux"))
        .output()
        .unwrap();
    let all = text(&out.stdout) + &text(&out.stderr);
    // Asked for by --ignored, so a machine that can't boot is a failure.
    assert_ne!(out.status.code(), Some(77), "{all}");
    assert!(out.status.success(), "{all}");
}
