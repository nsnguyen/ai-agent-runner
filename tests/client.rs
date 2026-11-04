// The laptop's `runner` (client/runner) against a fake ssh on PATH. The fake
// records its argv, then runs the remote command through bash, as the host's
// sshd does for the runner user, where a fake host `runner` records what
// arrived. No network and no host.
use nix::pty::{OpenptyResult, openpty};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use proptest::prelude::*;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use tempfile::TempDir;

mod common;
use common::wait_for;

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
        // Every call goes in SSH_LOG too, one record each. A tunnel (-L)
        // notes the VM's state as it opens, then runs its command as sshd
        // does: in a session of its own, as if on another machine, with its
        // stdout read here, so that end closes when this process dies.
        script(
            &bin.join("ssh"),
            r#"for arg; do printf '%s\0' "$arg"; done >"$SSH_ARGV"
{ for arg; do printf '%s\0' "$arg"; done; printf '\036'; } >>"$SSH_LOG"
for cmd; do :; done
for arg; do
  if [ "$arg" = -L ]; then
    cat "$VM_STATE" >"$TUNNEL_SAW" 2>/dev/null
    [ -z "${TUNNEL_EXIT-}" ] || exit "$TUNNEL_EXIT"
    rm -f "$CONN" && mkfifo "$CONN" || exit 255
    setsid -w bash -c "$cmd" >"$CONN" </dev/null &
    while read -r _; do :; done <"$CONN"
    wait $!
    exit
  fi
done
exec bash -c "$cmd"
"#,
        );
        // Reads stdin only when asked, since a terminal's would never end.
        // `start` brings the VM up and `ip` prints its address, as the real
        // ones do; HOST_LOG has every call, a line each. `hold` ends at once
        // unless HOLD_ON is set: then it holds until its stdout's far end
        // closes, which a write finds, noting both in HELD. HOLD_OLD makes
        // it a host runner from before `hold`. `export` prints HOST_EXPORT.
        script(
            &bin.join("runner"),
            r#"for arg; do printf '%s\0' "$arg"; done >"$HOST_ARGV"
printf '%s\n' "$*" >>"$HOST_LOG"
[ -z "${HOST_STDIN-}" ] || cat >"$HOST_STDIN"
[ -z "${HOST_EXIT-}" ] || { echo "runner: no VM named $2" >&2; exit "$HOST_EXIT"; }
case $1 in
start) echo running >"$VM_STATE" && echo "$2  gen 1  running" ;;
ip) printf '%s\n' "${HOST_IP-172.16.0.6}" ;;
export) [ -z "${HOST_EXPORT-}" ] || cat "$HOST_EXPORT" ;;
hold)
  [ -z "${HOLD_OLD-}" ] || { echo "error: unrecognized subcommand 'hold'" >&2; exit 2; }
  [ -n "${HOLD_ON-}" ] || exit 0
  echo holding >>"$HELD"
  trap '' PIPE
  while echo; do sleep 0.05; done 2>/dev/null
  echo "let go" >>"$HELD"
  ;;
esac
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
            .env("SSH_LOG", self.path("ssh-log"))
            .env("HOST_ARGV", self.path("host-argv"))
            .env("HOST_LOG", self.path("host-log"))
            .env("VM_STATE", self.path("vm-state"))
            .env("TUNNEL_SAW", self.path("tunnel-saw"))
            .env("CONN", self.path("conn"))
            .env("HELD", self.path("held"))
            .env_remove("HOLD_ON")
            .env_remove("HOLD_OLD")
            .env_remove("HOST_STDIN")
            .env_remove("HOST_EXIT")
            .env_remove("HOST_IP")
            .env_remove("HOST_EXPORT")
            .env_remove("TUNNEL_EXIT")
            .stdin(Stdio::null());
        cmd
    }

    fn ssh_argv(&self) -> Vec<String> {
        args_in(&self.path("ssh-argv"))
    }

    fn host_argv(&self) -> Vec<String> {
        args_in(&self.path("host-argv"))
    }

    /// Every ssh run, in order, each its argv.
    fn ssh_calls(&self) -> Vec<Vec<String>> {
        let log = fs::read(self.path("ssh-log")).unwrap_or_default();
        text(&log)
            .split_terminator('\x1e')
            .map(|call| call.split_terminator('\0').map(str::to_string).collect())
            .collect()
    }

    /// Every command the host's runner got, in order.
    fn host_calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.path("host-log")).unwrap_or_default();
        log.lines().map(str::to_string).collect()
    }

    /// The VM's state on the fake host: stopped, sleeping or running.
    fn vm(&self, state: &str) -> &Laptop {
        fs::write(self.path("vm-state"), format!("{state}\n")).unwrap();
        self
    }

    /// The VM's state when the tunnel opened, if one did.
    fn tunnel_saw(&self) -> Option<String> {
        let saw = fs::read_to_string(self.path("tunnel-saw")).ok()?;
        Some(saw.trim().to_string())
    }

    /// What the host's hold has noted: holding, then let go.
    fn held(&self) -> Vec<String> {
        let held = fs::read_to_string(self.path("held")).unwrap_or_default();
        held.lines().map(str::to_string).collect()
    }
}

// Runs `cmd` as typed at a terminal: stdin and stdout on one, stderr captured.
fn at_a_terminal(cmd: &mut Command) -> Output {
    let OpenptyResult {
        master: _master,
        slave,
    } = openpty(None, None).unwrap();
    let stdout = slave.try_clone().unwrap();
    cmd.stdin(Stdio::from(slave))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap()
}

// Runs `cmd` typed at a terminal with its output piped: `runner list | grep dev`.
fn piped_from_a_terminal(cmd: &mut Command) -> Output {
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
        let out = at_a_terminal(&mut laptop.runner(shell, &["shell", "dev"]));
        ok(&out);
        // LogLevel=ERROR drops ssh's "Connection to … closed." after each one.
        assert_eq!(
            laptop.ssh_argv(),
            ["-t", "-o", "LogLevel=ERROR", HOST, "runner shell dev"],
            "{shell:?}"
        );
        assert_eq!(laptop.host_argv(), ["shell", "dev"], "{shell:?}");
    }
}

#[test]
fn output_piped_from_a_terminal_gets_no_tty() {
    // A tty would end lines in CRLF and mix stderr into what's piped.
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = piped_from_a_terminal(&mut laptop.runner(shell, &["list"]));
        ok(&out);
        assert_eq!(laptop.ssh_argv(), [HOST, "runner list"], "{shell:?}");
    }
}

// `runner export dev | cat > f`: a tty would turn \n into \r\n on the way.
#[test]
fn an_export_piped_from_a_terminal_passes_every_byte_through_without_a_tty() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let bytes: Vec<u8> = (0..4 << 20).map(|i: u32| (i ^ (i >> 8)) as u8).collect();
        let stream = laptop.path("stream");
        fs::write(&stream, &bytes).unwrap();
        let out = piped_from_a_terminal(
            laptop
                .runner(shell, &["export", "dev"])
                .env("HOST_EXPORT", &stream),
        );
        ok(&out);
        assert_eq!(laptop.ssh_argv(), [HOST, "runner export dev"], "{shell:?}");
        assert_eq!(laptop.host_argv(), ["export", "dev"], "{shell:?}");
        assert!(
            out.stdout == bytes,
            "{shell:?}: {} bytes came",
            out.stdout.len()
        );
        assert_eq!(text(&out.stderr), "", "{shell:?}");
    }
}

// The host refuses an export onto a terminal, so it has to see this one,
// even with stdin elsewhere: `runner export dev </dev/null`.
#[test]
fn an_export_onto_a_terminal_gets_a_tty_even_without_one_on_stdin() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let OpenptyResult {
            master: _master,
            slave,
        } = openpty(None, None).unwrap();
        let out = laptop
            .runner(shell, &["export", "dev"])
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        ok(&out);
        let tty = ["-tt", "-o", "LogLevel=ERROR", HOST, "runner export dev"];
        assert_eq!(laptop.ssh_argv(), tty, "{shell:?}");
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
        let mcp = ["-T", "-o", "BatchMode=yes", HOST, "runner mcp"];
        // Even at a terminal: stdout is for JSON-RPC alone.
        ok(&at_a_terminal(&mut laptop.runner(shell, &["mcp"])));
        assert_eq!(laptop.ssh_argv(), mcp, "{shell:?}");

        let out = laptop.runner(shell, &["mcp"]).output().unwrap();
        ok(&out);
        assert_eq!(laptop.ssh_argv(), mcp, "{shell:?}");
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
        // Run as typed in the checkout, with a CDPATH that makes cd print.
        let install = |args: &[&str]| {
            Command::new(shell[0])
                .args(&shell[1..])
                .arg("client/install.sh")
                .args(args)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .env("HOME", home.path())
                .env("CDPATH", ".:/nowhere")
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
        // The apps don't get the shell's PATH, so it gives the absolute path.
        let said = text(&out.stdout);
        let path = installed.to_str().unwrap();
        assert!(said.contains(&format!("runner -- {path} mcp")), "{said}");
        assert!(said.contains(&format!(r#""command": "{path}""#)), "{said}");

        // A rerun to upgrade keeps the host.
        ok(&install(&[]));
        assert_eq!(fs::read_to_string(&host).unwrap(), format!("{HOST}\n"));

        // runner would refuse these, so install doesn't save them.
        for bad in ["", "-oProxyCommand=touch pwned"] {
            let out = install(&[bad]);
            assert_eq!(out.status.code(), Some(2), "{bad:?}");
            assert_eq!(fs::read_to_string(&host).unwrap(), format!("{HOST}\n"));
        }
    }
}

// `runner proxy`: the host starts the VM and says its address, then ssh
// forwards each laptop port to it, through the host, while the host's
// `runner hold` holds the VM in use, so it doesn't idle-sleep under the proxy.
const ASK: &str = "runner start dev >/dev/null && runner ip dev";
const HOLD: &str = "runner hold dev";

// No stdin and no tty: keys typed while the VM boots stay in the terminal,
// and the address comes back without a \r.
fn ask(cmd: &str) -> Vec<String> {
    ["-n", "-T", HOST, cmd].map(str::to_string).to_vec()
}

// The hold reads nothing, so keys typed while it's up go nowhere either.
fn tunnel(forwards: &[&str]) -> Vec<String> {
    tunnel_for(HOLD, forwards)
}

fn tunnel_for(hold: &str, forwards: &[&str]) -> Vec<String> {
    let mut argv = vec!["-n", "-T", "-o", "ExitOnForwardFailure=yes"];
    for f in forwards {
        argv.extend(["-L", f]);
    }
    argv.extend([HOST, hold]);
    argv.into_iter().map(str::to_string).collect()
}

#[test]
fn proxy_forwards_one_port_to_the_same_port_on_the_vm() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .vm("running")
            .runner(shell, &["proxy", "dev", "3000"])
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(
            laptop.ssh_calls(),
            [ask(ASK), tunnel(&["localhost:3000:172.16.0.6:3000"])],
            "{shell:?}"
        );
        assert_eq!(
            laptop.host_calls(),
            ["start dev", "ip dev", "hold dev"],
            "{shell:?}"
        );
        let err = text(&out.stderr);
        assert!(err.contains("localhost:3000 is dev's port 3000"), "{err}");
    }
}

#[test]
fn proxy_forwards_several_ports_at_once() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .vm("running")
            .runner(shell, &["proxy", "dev", "3000", "5173", "8080:80"])
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(laptop.ssh_calls().len(), 2, "{shell:?}");
        assert_eq!(
            laptop.ssh_argv(),
            tunnel(&[
                "localhost:3000:172.16.0.6:3000",
                "localhost:5173:172.16.0.6:5173",
                "localhost:8080:172.16.0.6:80"
            ]),
            "{shell:?}"
        );
    }
}

#[test]
fn proxy_takes_local_colon_remote() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .vm("running")
            .runner(shell, &["proxy", "dev", "8080:3000"])
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(
            laptop.ssh_argv(),
            tunnel(&["localhost:8080:172.16.0.6:3000"]),
            "{shell:?}"
        );
        let err = text(&out.stderr);
        assert!(err.contains("localhost:8080 is dev's port 3000"), "{err}");
    }
}

#[test]
fn proxy_starts_or_wakes_a_vm_that_isnt_running_before_the_tunnel_opens() {
    for shell in SHELLS {
        for state in ["stopped", "sleeping"] {
            let laptop = Laptop::new();
            let out = laptop
                .vm(state)
                .runner(shell, &["proxy", "dev", "3000"])
                .output()
                .unwrap();
            ok(&out);
            assert_eq!(
                laptop.host_calls(),
                ["start dev", "ip dev", "hold dev"],
                "{shell:?}"
            );
            assert_eq!(
                laptop.tunnel_saw().as_deref(),
                Some("running"),
                "{shell:?} {state}"
            );
            // start's status line is the host's business, not the laptop's.
            assert!(!text(&out.stdout).contains("gen 1"), "{shell:?}");
        }
    }
}

#[test]
fn proxy_to_a_vm_that_cant_start_opens_no_tunnel_and_says_why() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .vm("stopped")
            .runner(shell, &["proxy", "dev", "3000"])
            .env("HOST_EXIT", "1")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert_eq!(laptop.ssh_calls().len(), 1, "{shell:?}");
        assert_eq!(laptop.host_calls(), ["start dev"], "{shell:?}");
        assert!(text(&out.stderr).contains("no VM named dev"), "{shell:?}");
        assert_eq!(laptop.tunnel_saw(), None, "{shell:?}");
    }
}

#[test]
fn proxy_refuses_an_address_that_isnt_one() {
    // Say a login script on the host printed a banner first.
    for shell in SHELLS {
        for said in ["", "Welcome!\n172.16.0.6", "-oProxyCommand=x"] {
            let laptop = Laptop::new();
            let out = laptop
                .vm("running")
                .runner(shell, &["proxy", "dev", "3000"])
                .env("HOST_IP", said)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(1), "{shell:?} {said:?}");
            assert_eq!(laptop.ssh_calls().len(), 1, "{shell:?} {said:?}");
            let err = text(&out.stderr);
            assert!(err.contains("dev's address"), "{err}");
        }
    }
}

#[test]
fn proxy_quotes_the_vm_name_for_the_host() {
    let laptop = Laptop::new();
    let out = laptop
        .vm("running")
        .runner(SHELLS[0], &["proxy", "it's", "3000"])
        .output()
        .unwrap();
    ok(&out);
    let calls = laptop.ssh_calls();
    assert_eq!(
        calls[0],
        ask(r#"runner start 'it'\''s' >/dev/null && runner ip 'it'\''s'"#)
    );
    assert_eq!(
        calls[1],
        tunnel_for(
            r#"runner hold 'it'\''s'"#,
            &["localhost:3000:172.16.0.6:3000"]
        )
    );
    assert_eq!(laptop.host_argv(), ["hold", "it's"]);
}

#[test]
fn proxy_with_bad_arguments_says_how_and_runs_nothing() {
    let bad: &[&[&str]] = &[
        &["proxy"],
        &["proxy", "dev"],
        &["proxy", "", "3000"],
        &["proxy", "dev", "0"],
        &["proxy", "dev", "65536"],
        &["proxy", "dev", "030"],
        &["proxy", "dev", "web"],
        &["proxy", "dev", "3000:"],
        &["proxy", "dev", ":3000"],
        &["proxy", "dev", "1:2:3"],
        &["proxy", "dev", "3000", "-L"],
        &["proxy", "dev", "3000", "x\n-oProxyCommand=y"],
    ];
    for shell in SHELLS {
        for args in bad {
            let laptop = Laptop::new();
            let out = laptop.runner(shell, args).output().unwrap();
            assert_eq!(out.status.code(), Some(2), "{shell:?} {args:?}");
            assert!(
                text(&out.stderr).contains("runner proxy <vm> <port>"),
                "{args:?}: {}",
                text(&out.stderr)
            );
            assert!(laptop.ssh_calls().is_empty(), "{shell:?} {args:?}");
        }
    }
}

#[test]
fn proxy_help_says_how_and_runs_nothing() {
    let laptop = Laptop::new();
    let out = laptop
        .runner(SHELLS[0], &["proxy", "--help"])
        .output()
        .unwrap();
    ok(&out);
    assert!(text(&out.stdout).contains("runner proxy <vm> <port>"));
    assert!(laptop.ssh_calls().is_empty());
}

#[test]
fn the_tunnels_exit_code_is_the_proxys() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .vm("running")
            .runner(shell, &["proxy", "dev", "3000"])
            .env("TUNNEL_EXIT", "255")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(255), "{shell:?}");
    }
}

// The proxy as typed at a terminal, in a process group of its own as a
// terminal's job is, once the host's hold has the VM: the hold lasts while the
// tunnel does, which `end` then ends.
fn hold_then(shell: &[&str], end: impl FnOnce(&std::process::Child)) -> Laptop {
    let laptop = Laptop::new();
    let mut proxy = Running(
        laptop
            .vm("running")
            .runner(shell, &["proxy", "dev", "3000"])
            .env("HOLD_ON", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    wait_for("the hold", || !laptop.held().is_empty());
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        proxy.0.try_wait().unwrap().is_none(),
        "{shell:?}: proxy ended"
    );
    assert_eq!(laptop.held(), ["holding"], "{shell:?}");
    end(&proxy.0);
    proxy.0.wait().unwrap();
    wait_for("the hold to let go", || laptop.held().len() == 2);
    assert_eq!(laptop.held(), ["holding", "let go"], "{shell:?}");
    laptop
}

// A proxy a failed assert left running still ends, and with its ssh the hold.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGKILL);
        }
    }
}

#[test]
fn an_open_proxy_holds_its_vm_until_ctrl_c() {
    for shell in SHELLS {
        hold_then(shell, |proxy| {
            let group = Pid::from_raw(proxy.id() as i32);
            killpg(group, Signal::SIGINT).unwrap();
        });
    }
}

#[test]
fn a_proxy_whose_ssh_dies_lets_its_vm_go() {
    for shell in SHELLS {
        hold_then(shell, |proxy| {
            kill(Pid::from_raw(proxy.id() as i32), Signal::SIGKILL).unwrap();
        });
    }
}

// A host whose runner is older than `hold` ends the proxy at once, saying so.
#[test]
fn a_host_without_hold_ends_the_proxy_and_says_why() {
    for shell in SHELLS {
        let laptop = Laptop::new();
        let out = laptop
            .vm("running")
            .runner(shell, &["proxy", "dev", "3000"])
            .env("HOLD_OLD", "1")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("unrecognized subcommand 'hold'"), "{err}");
    }
}
