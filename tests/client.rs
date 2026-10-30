// The laptop's `runner` (client/runner) against a fake ssh on PATH. The fake
// records its argv, then runs the remote command through bash, as the host's
// sshd does for the runner user, where a fake host `runner` records what
// arrived. No network and no host.
use nix::pty::{OpenptyResult, openpty};
use proptest::prelude::*;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

// Linux's sh, and bash in POSIX mode, which is what macOS's /bin/sh is.
const SHELLS: &[&[&str]] = &[&["sh"], &["bash", "--posix"]];

const HOST: &str = "runner@kvm-host";

fn client(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("client")
        .join(name)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "{:?}: {}",
        out.status,
        text(&out.stderr)
    );
}

// NUL-terminated, as the fakes write them.
fn args_in(path: &Path) -> Vec<String> {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text(&bytes)
        .split_terminator('\0')
        .map(str::to_string)
        .collect()
}

struct Laptop {
    dir: TempDir,
}

impl Laptop {
    fn new() -> Laptop {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(dir.path().join("home")).unwrap();
        script(
            &bin.join("ssh"),
            r#"for arg; do printf '%s\0' "$arg"; done >"$SSH_ARGV"
for cmd; do :; done
exec bash -c "$cmd"
"#,
        );
        // Reads stdin only when asked, since a terminal's would never end.
        script(
            &bin.join("runner"),
            r#"for arg; do printf '%s\0' "$arg"; done >"$HOST_ARGV"
[ -z "${HOST_STDIN-}" ] || cat >"$HOST_STDIN"
exit "${HOST_EXIT:-0}"
"#,
        );
        Laptop { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The wrapper under `shell`, with the fakes first on PATH and RUNNER_HOST set.
    fn runner(&self, shell: &[&str], args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg(client("runner"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.home())
            .env("RUNNER_HOST", HOST)
            .env("SSH_ARGV", self.path("ssh-argv"))
            .env("HOST_ARGV", self.path("host-argv"))
            .env_remove("HOST_STDIN")
            .env_remove("HOST_EXIT")
            .stdin(Stdio::null());
        cmd
    }

    fn ssh_argv(&self) -> Vec<String> {
        args_in(&self.path("ssh-argv"))
    }

    fn host_argv(&self) -> Vec<String> {
        args_in(&self.path("host-argv"))
    }
}

// Runs `cmd` with a terminal on stdin.
fn on_a_terminal(cmd: &mut Command) -> Output {
    let OpenptyResult {
        master: _master,
        slave,
    } = openpty(None, None).unwrap();
    cmd.stdin(Stdio::from(slave)).output().unwrap()
}

#[test]
fn a_command_typed_at_a_terminal_gets_a_tty() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = on_a_terminal(&mut laptop.runner(shell, &["shell", "dev"]));
        ok(&out);
        assert_eq!(
            laptop.ssh_argv(),
            ["-t", HOST, "runner shell dev"],
            "{shell:?}"
        );
        assert_eq!(laptop.host_argv(), ["shell", "dev"], "{shell:?}");
    }
}

#[test]
fn a_piped_command_gets_no_tty_and_its_stdin_passes_through() {
    let args = ["exec", "dev", "--", "printf", "%s\\n", "it's a \"test\""];
    for shell in SHELLS {
        let laptop = Laptop::new();
        let mut child = laptop
            .runner(shell, &args)
            .env("HOST_STDIN", laptop.path("host-stdin"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"piped in\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        ok(&out);
        assert_eq!(
            laptop.ssh_argv(),
            [
                HOST,
                r#"runner exec dev -- printf '%s\n' 'it'\''s a "test"'"#
            ],
            "{shell:?}"
        );
        assert_eq!(laptop.host_argv(), args, "{shell:?}");
        assert_eq!(
            fs::read_to_string(laptop.path("host-stdin")).unwrap(),
            "piped in\n"
        );
    }
}

#[test]
fn mcp_never_gets_a_tty_or_a_prompt_and_prints_nothing_itself() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        // A terminal on stdin still gets -T: stdout is for JSON-RPC alone.
        let out = on_a_terminal(&mut laptop.runner(shell, &["mcp"]));
        ok(&out);
        assert_eq!(
            laptop.ssh_argv(),
            ["-T", "-o", "BatchMode=yes", HOST, "runner mcp"],
            "{shell:?}"
        );
        assert_eq!(laptop.host_argv(), ["mcp"], "{shell:?}");
        assert_eq!(text(&out.stdout), "");
    }
}

#[test]
fn empty_and_odd_arguments_are_quoted_and_safe_ones_left_bare() {
    let laptop = Laptop::new();
    let args = [
        "",
        "a b",
        "$HOME",
        "~",
        "*",
        "x\ny",
        "--env",
        "K=V",
        "user@host:/a/b,c+d%e",
        "'",
    ];
    ok(&laptop.runner(SHELLS[0], &args).output().unwrap());
    assert_eq!(
        laptop.ssh_argv()[1],
        "runner '' 'a b' '$HOME' '~' '*' 'x\ny' --env K=V user@host:/a/b,c+d%e ''\\'''"
    );
    assert_eq!(laptop.host_argv(), args);
}

#[test]
fn the_host_comes_from_the_config_file_unless_runner_host_is_set() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let conf = laptop.home().join(".config/runner");
        fs::create_dir_all(&conf).unwrap();
        fs::write(conf.join("host"), "  kvm-host  \nsecond line\n").unwrap();

        let out = laptop
            .runner(shell, &["list"])
            .env_remove("RUNNER_HOST")
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(laptop.ssh_argv(), ["kvm-host", "runner list"], "{shell:?}");

        let out = laptop
            .runner(shell, &["list"])
            .env("RUNNER_HOST", "other")
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(laptop.ssh_argv(), ["other", "runner list"], "{shell:?}");
    }
}

#[test]
fn without_a_host_it_says_where_to_set_one_and_runs_nothing() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .runner(shell, &["list"])
            .env_remove("RUNNER_HOST")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("RUNNER_HOST"), "{err}");
        assert!(err.contains("~/.config/runner/host"), "{err}");
        assert!(!laptop.path("ssh-argv").exists());
    }
}

#[test]
fn a_host_that_looks_like_an_ssh_option_is_refused() {
    let laptop = Laptop::new();
    let out = laptop
        .runner(SHELLS[0], &["list"])
        .env("RUNNER_HOST", "-oProxyCommand=touch pwned")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("RUNNER_HOST"));
    assert!(!laptop.path("ssh-argv").exists());
}

#[test]
fn the_exit_code_on_the_host_is_the_exit_code_on_the_laptop() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .runner(shell, &["exec", "dev", "--", "false"])
            .env("HOST_EXIT", "7")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7), "{shell:?}");
    }
}

// Shell syntax often, any text otherwise. No NUL: an argument can't hold one.
fn arg() -> impl Strategy<Value = String> {
    prop_oneof![
        r#"[ '"\\$`!*?~#;&|<>(){}\[\]=%@,:./+\n\t-]{0,6}"#,
        "[^\\x00]{0,12}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_arguments_arrive_on_the_host_as_typed(args in prop::collection::vec(arg(), 0..5)) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        for shell in SHELLS {
            let laptop = Laptop::new();
            let out = laptop.runner(shell, &args).output().unwrap();
            prop_assert!(out.status.success(), "{shell:?}: {}", text(&out.stderr));
            prop_assert_eq!(laptop.host_argv(), args.clone());
        }
    }
}

#[test]
fn install_puts_runner_in_local_bin_and_saves_the_host() {
    for shell in SHELLS {
        let home = tempfile::tempdir().unwrap();
        let install = |args: &[&str]| {
            Command::new(shell[0])
                .args(&shell[1..])
                .arg(client("install.sh"))
                .args(args)
                .env("HOME", home.path())
                .output()
                .unwrap()
        };

        let out = install(&[HOST]);
        ok(&out);
        let installed = home.path().join(".local/bin/runner");
        assert_eq!(
            fs::read(&installed).unwrap(),
            fs::read(client("runner")).unwrap()
        );
        let mode = fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let host = home.path().join(".config/runner/host");
        assert_eq!(fs::read_to_string(&host).unwrap(), format!("{HOST}\n"));
        // The desktop apps' configs need the absolute path, so it says it.
        let said = text(&out.stdout);
        assert!(said.contains(installed.to_str().unwrap()), "{said}");

        // A rerun to upgrade keeps the host.
        ok(&install(&[]));
        assert_eq!(fs::read_to_string(&host).unwrap(), format!("{HOST}\n"));
    }
}
