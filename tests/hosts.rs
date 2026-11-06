// The laptop's `runner` (client/runner) with several hosts in
// ~/.config/runner/hosts, against a fake ssh on PATH that serves three fake
// hosts. Each host is a directory: `vms` lists its VMs, `free` its free
// memory, `calls` what its runner got, and `down` makes ssh time out on it,
// as `stopped` does until the fake aws.sh beside the laptop's runner, a copy,
// starts it. No network, no host and no AWS.
mod common;

use runner::peers;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

// Linux's sh, and bash in POSIX mode, which is what macOS's /bin/sh is.
const SHELLS: &[&[&str]] = &[&["sh"], &["bash", "--posix"]];

const HOSTS: [&str; 3] = ["runner@kvm-host", "runner@kvm-host-2", "runner@kvm-host-3"];

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

// Every ssh run is logged, its arguments ended by \x1f, one record each,
// written at once, since the hosts are asked at once. The host
// is the argument before the command, which runs in the host's directory
// as its home, where its ~/.ssh is. A host that's down times out, as
// ConnectTimeout makes ssh do, with ssh's own words and exit code. Without
// -n, ssh reads stdin as it comes, whether or not the far end wants it, so
// the fake reads it all before the command starts.
const SSH: &str = r#"us=$(printf '\037')
rec=
for arg; do rec=$rec$arg$us; done
printf '%s\036' "$rec" >>"$SSH_LOG"
for arg; do host=$cmd; cmd=$arg; done
dir=$FLEET/${host##*@}
if [ ! -d "$dir" ]; then
  echo "ssh: Could not resolve hostname ${host##*@}: Name or service not known" >&2
  exit 255
fi
if [ -e "$dir/down" ] || [ -e "$dir/stopped" ]; then
  echo "ssh: connect to host ${host##*@} port 22: Connection timed out" >&2
  exit 255
fi
export HOST_DIR="$dir" HOME="$dir"
[ ! -e "$dir/slow" ] || sleep 1
[ "$1" != -n ] || exec bash -c "$cmd" </dev/null
read_in=$(mktemp)
cat >"$read_in"
bash -c "$cmd" <"$read_in"
rc=$?
rm -f "$read_in"
exit "$rc"
"#;

// A host's runner, as far as the laptop sees it. It names the VMs it has.
const RUNNER: &str = r#"printf '%s\n' "$*" >>"$HOST_DIR/calls"
touch "$HOST_DIR/vms"
has() { grep -qx -- "$1" "$HOST_DIR/vms"; }
no() { echo "runner: no VM named $1" >&2; exit 1; }
# The first argument after the command that isn't an option.
for a; do
  [ "$a" = "$1" ] && continue
  case $a in -*) continue ;; esac
  name=$a
  break
done
case $1 in
list)
  [ -s "$HOST_DIR/vms" ] || { echo "no VMs yet"; exit 0; }
  while read -r v; do echo "$v  gen 0  6 bytes  4096 bytes used"; done <"$HOST_DIR/vms"
  ;;
hosts)
  n=$(wc -l <"$HOST_DIR/vms" | tr -d ' ')
  echo "$n VMs  0 up  $(cat "$HOST_DIR/free") bytes free"
  ;;
ip) has "$2" || no "$2"; echo 172.16.0.6 ;;
start) has "$2" || no "$2"; echo "$2  gen 1  running" ;;
hold) has "$2" || no "$2" ;;
create | import)
  # import --replace takes the place of a VM of that name, if there is one.
  case " $* " in *" --replace "*) replace=1 ;; esac
  if has "$name" && [ -z "${replace-}" ]; then echo "runner: a VM named $name already exists" >&2; exit 1; fi
  [ "$1" = create ] || cat >"$HOST_DIR/stdin"
  has "$name" || echo "$name" >>"$HOST_DIR/vms"
  echo "$name  gen 0  6 bytes"
  ;;
delete)
  has "$2" || no "$2"
  grep -vx -- "$2" "$HOST_DIR/vms" >"$HOST_DIR/vms.new"
  mv "$HOST_DIR/vms.new" "$HOST_DIR/vms"
  echo "deleted $2  gen 0  6 bytes"
  ;;
move)
  # The target is last, as ssh takes it; its name is its directory. A
  # stopped one doesn't answer, so nothing moves.
  has "$name" || no "$name"
  for to; do :; done
  if [ -e "$FLEET/${to##*@}/stopped" ]; then echo "runner: ${to##*@} didn't answer" >&2; exit 1; fi
  grep -vx -- "$name" "$HOST_DIR/vms" >"$HOST_DIR/vms.new"
  mv "$HOST_DIR/vms.new" "$HOST_DIR/vms"
  echo "$name" >>"$FLEET/${to##*@}/vms"
  echo "moved $name to ${to##*@}"
  ;;
exec)
  # Some argument names a VM here, a run id's VM included.
  for a; do has "$a" && found=1; done
  [ -n "${found-}" ] || no "$name"
  cat >"$HOST_DIR/stdin"
  ;;
doctor) echo "ok    kvm  /dev/kvm" ;;
gc) while read -r v; do echo "$v  nothing to free"; done <"$HOST_DIR/vms" ;;
esac
"#;

// The laptop's aws.sh: each call in $FLEET/aws-log, RUNNER_AWS_NAME first.
// `state` says stopped for a host with `stopped`, running for the rest;
// `start` starts it.
const AWS_SH: &str = r#"printf '%s %s\n' "${RUNNER_AWS_NAME-}" "$*" >>"$FLEET/aws-log"
case $1 in
state) if [ -e "$FLEET/$RUNNER_AWS_NAME/stopped" ]; then echo stopped; else echo running; fi ;;
start) rm -f "$FLEET/$RUNNER_AWS_NAME/stopped"; echo "$RUNNER_AWS_NAME is up at 203.0.113.7." ;;
esac
"#;

// The laptop's key line, as README.md's Add the laptop's key writes it.
const LAPTOP: &str = r#"from="100.64.0.0/10,fd7a:115c:a1e0::/48",restrict,pty,port-forwarding ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIlaptopAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA nguyen@laptop"#;

// A host's own key, an ed25519 one in shape, told apart by its name.
fn key(name: &str) -> String {
    format!("AAAAC3NzaC1lZDI1NTE5AAAAI{:A<43}", name.replace('-', ""))
}

// The line another host's key gets on a host, as `runner hosts add` writes it.
fn peer_line(name: &str, key: &str) -> String {
    format!("{} ssh-ed25519 {key} {name}", peers::OPTIONS)
}

// A host's authorized_keys: the laptop's line, then each of these hosts'.
fn trusting(peers: &[&str]) -> String {
    let mut want = format!("{LAPTOP}\n");
    for p in peers {
        want += &format!("{}\n", peer_line(p, &key(p)));
    }
    want
}

struct Fleet {
    dir: tempfile::TempDir,
}

impl Fleet {
    /// Three hosts, listed in that order: kvm-host has base and dev,
    /// kvm-host-2 base and web, kvm-host-3 nothing yet.
    fn new() -> Fleet {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        script(&bin.join("ssh"), SSH);
        script(&bin.join("runner"), RUNNER);
        let laptop = dir.path().join("laptop");
        fs::create_dir(&laptop).unwrap();
        fs::copy(client("runner"), laptop.join("runner")).unwrap();
        script(&laptop.join("aws.sh"), AWS_SH);
        let fleet = Fleet { dir };
        fleet.host("kvm-host", &["base", "dev"], 2 << 30);
        fleet.host("kvm-host-2", &["base", "web"], 6 << 30);
        fleet.host("kvm-host-3", &[], 4 << 30);
        let conf = fleet.home().join(".config/runner");
        fs::create_dir_all(&conf).unwrap();
        fs::write(conf.join("hosts"), HOSTS.join("\n") + "\n").unwrap();
        fleet
    }

    fn host(&self, name: &str, vms: &[&str], free: u64) {
        let dir = self.dir.path().join("fleet").join(name);
        fs::create_dir_all(&dir).unwrap();
        let vms: String = vms.iter().map(|v| format!("{v}\n")).collect();
        fs::write(dir.join("vms"), vms).unwrap();
        fs::write(dir.join("free"), free.to_string()).unwrap();
        // As host/install.sh leaves it, with the laptop's key added.
        let ssh = dir.join(".ssh");
        if !ssh.exists() {
            fs::create_dir(&ssh).unwrap();
            fs::write(ssh.join("authorized_keys"), format!("{LAPTOP}\n")).unwrap();
            self.set_key(
                name,
                Some(&format!("ssh-ed25519 {} runner@{name}", key(name))),
            );
        }
    }

    /// What `name`'s ~/.ssh/id_ed25519.pub holds, or that it has none.
    fn set_key(&self, name: &str, pub_line: Option<&str>) {
        let path = self
            .dir
            .path()
            .join("fleet")
            .join(name)
            .join(".ssh/id_ed25519.pub");
        match pub_line {
            Some(line) => fs::write(path, format!("{line}\n")).unwrap(),
            None => fs::remove_file(path).unwrap(),
        }
    }

    fn authorized_keys(&self, name: &str) -> String {
        let path = self
            .dir
            .path()
            .join("fleet")
            .join(name)
            .join(".ssh/authorized_keys");
        fs::read_to_string(path).unwrap_or_default()
    }

    fn set_authorized_keys(&self, name: &str, keys: &str) {
        let path = self
            .dir
            .path()
            .join("fleet")
            .join(name)
            .join(".ssh/authorized_keys");
        fs::write(path, keys).unwrap();
    }

    fn down(&self, name: &str) {
        fs::write(self.dir.path().join("fleet").join(name).join("down"), "").unwrap();
    }

    /// The AWS CLI is here, so the runner asks a host it hasn't reached
    /// whether it answers before a command goes there, and aws.sh about one
    /// that doesn't. The fake aws.sh never calls it.
    fn aws_cli(&self) {
        let aws = self.dir.path().join("bin/aws");
        script(&aws, "echo 'aws: not in these tests' >&2\nexit 1\n");
    }

    /// Stopped on AWS, until aws.sh starts it.
    fn stop(&self, name: &str) {
        fs::write(self.dir.path().join("fleet").join(name).join("stopped"), "").unwrap();
        self.aws_cli();
    }

    /// Every aws.sh call: its RUNNER_AWS_NAME, then its arguments.
    fn aws_calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.dir.path().join("fleet/aws-log")).unwrap_or_default();
        log.lines().map(str::to_string).collect()
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn hosts_file(&self) -> PathBuf {
        self.home().join(".config/runner/hosts")
    }

    fn cache(&self) -> String {
        fs::read_to_string(self.home().join(".cache/runner/vms")).unwrap_or_default()
    }

    /// What `name`'s runner got, a call a line.
    fn calls(&self, name: &str) -> Vec<String> {
        let path = self.dir.path().join("fleet").join(name).join("calls");
        let calls = fs::read_to_string(path).unwrap_or_default();
        calls.lines().map(str::to_string).collect()
    }

    /// What every host's runner got, less the asks for lists and rows.
    fn commands(&self, name: &str) -> Vec<String> {
        let asks = ["list", "hosts", "ip dev", "ip web", "ip base"];
        let calls = self.calls(name);
        calls
            .into_iter()
            .filter(|c| !asks.contains(&c.as_str()))
            .collect()
    }

    fn vms(&self, name: &str) -> Vec<String> {
        let path = self.dir.path().join("fleet").join(name).join("vms");
        let vms = fs::read_to_string(path).unwrap_or_default();
        vms.lines().map(str::to_string).collect()
    }

    fn forget_calls(&self) {
        for name in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            let _ = fs::remove_file(self.dir.path().join("fleet").join(name).join("calls"));
        }
        let _ = fs::remove_file(self.dir.path().join("ssh-log"));
    }

    /// Every ssh run, in order, each its argv.
    fn ssh_calls(&self) -> Vec<Vec<String>> {
        let log = fs::read(self.dir.path().join("ssh-log")).unwrap_or_default();
        text(&log)
            .split_terminator('\x1e')
            .map(|call| call.split_terminator('\x1f').map(str::to_string).collect())
            .collect()
    }

    fn runner(&self, shell: &[&str], args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            common::path_without_aws()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg(self.dir.path().join("laptop/runner"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.home())
            .env("FLEET", self.dir.path().join("fleet"))
            .env_remove("RUNNER_AWS_NAME")
            .env("SSH_LOG", self.dir.path().join("ssh-log"))
            .env_remove("RUNNER_HOST")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, shell: &[&str], args: &[&str]) -> Output {
        self.runner(shell, args).output().unwrap()
    }
}

#[test]
fn list_merges_every_host_s_vms_with_a_host_column() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["list"]);
        ok(&out);
        assert_eq!(
            text(&out.stdout),
            "kvm-host  base  gen 0  6 bytes  4096 bytes used\n\
             kvm-host  dev  gen 0  6 bytes  4096 bytes used\n\
             kvm-host-2  base  gen 0  6 bytes  4096 bytes used\n\
             kvm-host-2  web  gen 0  6 bytes  4096 bytes used\n",
            "{shell:?}"
        );
        assert_eq!(text(&out.stderr), "", "{shell:?}");
        // Each host once, all at once, with no stdin and no prompt, and a
        // limit on how long each gets to answer.
        let calls = fleet.ssh_calls();
        assert_eq!(calls.len(), 3, "{shell:?}");
        for host in HOSTS {
            let call = calls
                .iter()
                .find(|c| c[c.len() - 2] == host)
                .unwrap_or_else(|| panic!("{shell:?}: no ask of {host}: {calls:?}"));
            assert_eq!(call.last().unwrap(), "runner list", "{shell:?}");
            assert!(call.contains(&"-n".to_string()), "{call:?}");
            assert!(call.contains(&"BatchMode=yes".to_string()), "{call:?}");
            assert!(
                call.iter().any(|a| a.starts_with("ConnectTimeout=")),
                "{call:?}"
            );
        }
        // Where each VM is, for the next command naming one.
        assert_eq!(
            fleet.cache(),
            "base runner@kvm-host\ndev runner@kvm-host\nbase runner@kvm-host-2\nweb runner@kvm-host-2\n"
        );
    }
}

#[test]
fn with_no_vms_anywhere_list_says_so_once() {
    let fleet = Fleet::new();
    fleet.host("kvm-host", &[], 1);
    fleet.host("kvm-host-2", &[], 1);
    let out = fleet.run(SHELLS[0], &["list"]);
    ok(&out);
    assert_eq!(text(&out.stdout), "no VMs yet\n");
}

#[test]
fn a_host_that_times_out_is_named_and_the_rest_listed() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fleet.down("kvm-host-2");
        let out = fleet.run(shell, &["list"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert_eq!(
            text(&out.stdout),
            "kvm-host  base  gen 0  6 bytes  4096 bytes used\n\
             kvm-host  dev  gen 0  6 bytes  4096 bytes used\n",
            "{shell:?}"
        );
        let err = text(&out.stderr);
        assert!(err.contains("runner: kvm-host-2 didn't answer"), "{err}");
        assert!(!err.contains("kvm-host-3"), "{err}");

        let out = fleet.run(shell, &["hosts"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert_eq!(
            text(&out.stdout),
            "kvm-host  2 VMs  0 up  2147483648 bytes free\n\
             kvm-host-3  0 VMs  0 up  4294967296 bytes free\n",
            "{shell:?}"
        );
        assert!(text(&out.stderr).contains("runner: kvm-host-2 didn't answer"));
    }
}

#[test]
fn hosts_lists_each_host_s_row() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["hosts"]);
        ok(&out);
        assert_eq!(
            text(&out.stdout),
            "kvm-host  2 VMs  0 up  2147483648 bytes free\n\
             kvm-host-2  2 VMs  0 up  6442450944 bytes free\n\
             kvm-host-3  0 VMs  0 up  4294967296 bytes free\n",
            "{shell:?}"
        );
        for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            assert_eq!(fleet.calls(host), ["hosts"], "{shell:?}");
        }
    }
}

// The VM is the first argument after the command, past options and their
// values, and the command goes to its host alone.
#[test]
fn a_command_naming_a_vm_reaches_only_its_host() {
    let cases: &[(&[&str], &str)] = &[
        (&["exec", "web", "--", "ls", "-la"], "exec web -- ls -la"),
        (
            &["exec", "-e", "K=V", "--dir", "/tmp", "web", "--", "true"],
            "exec -e K=V --dir /tmp web -- true",
        ),
        (&["shell", "web"], "shell web"),
        (
            &["shell", "--user", "root", "-s", "work", "web"],
            "shell --user root -s work web",
        ),
        (&["output", "web.1", "--follow"], "output web.1 --follow"),
        (&["result", "web.12"], "result web.12"),
        (&["cancel", "web.3"], "cancel web.3"),
        (
            &["service", "logs", "-n", "50", "web", "srv"],
            "service logs -n 50 web srv",
        ),
        (&["logs", "-fn", "5", "web"], "logs -fn 5 web"),
        (&["checkpoint", "-m", "dev", "web"], "checkpoint -m dev web"),
        (&["claude", "--run", "web.1"], "claude --run web.1"),
        (&["--keep", "3", "gc", "web"], "--keep 3 gc web"),
    ];
    for shell in SHELLS {
        for (args, got) in cases {
            let fleet = Fleet::new();
            ok(&fleet.run(shell, &["list"]));
            fleet.forget_calls();
            let out = fleet.run(shell, args);
            ok(&out);
            assert_eq!(fleet.commands("kvm-host-2"), [*got], "{shell:?} {args:?}");
            assert!(fleet.calls("kvm-host").is_empty(), "{shell:?} {args:?}");
            assert!(fleet.calls("kvm-host-3").is_empty(), "{shell:?} {args:?}");
        }
    }
}

#[test]
fn with_nothing_cached_every_host_is_asked_first() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["exec", "web", "--", "true"]);
        ok(&out);
        assert_eq!(fleet.calls("kvm-host"), ["list"], "{shell:?}");
        assert_eq!(
            fleet.calls("kvm-host-2"),
            ["list", "exec web -- true"],
            "{shell:?}"
        );
        assert!(fleet.cache().contains("web runner@kvm-host-2\n"));

        // Then the cache has it: the host says it has web, and runs it.
        fleet.forget_calls();
        ok(&fleet.run(shell, &["exec", "web", "--", "true"]));
        assert!(fleet.calls("kvm-host").is_empty(), "{shell:?}");
        assert_eq!(
            fleet.calls("kvm-host-2"),
            ["ip web", "exec web -- true"],
            "{shell:?}"
        );
    }
}

#[test]
fn proxy_reaches_only_its_vm_s_host() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.forget_calls();
        let out = fleet.run(shell, &["proxy", "web", "3000"]);
        ok(&out);
        assert_eq!(
            fleet.commands("kvm-host-2"),
            ["start web", "hold web"],
            "{shell:?}"
        );
        assert!(fleet.calls("kvm-host").is_empty(), "{shell:?}");
        let tunnel = fleet.ssh_calls().pop().unwrap();
        assert!(
            tunnel.contains(&"localhost:3000:172.16.0.6:3000".to_string()),
            "{tunnel:?}"
        );
        assert_eq!(tunnel[tunnel.len() - 2], "runner@kvm-host-2", "{shell:?}");
    }
}

#[test]
fn a_command_naming_base_goes_to_the_first_host_unless_host_says() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["exec", "base", "--", "true"]);
        ok(&out);
        assert_eq!(fleet.calls("kvm-host"), ["exec base -- true"], "{shell:?}");
        assert!(fleet.calls("kvm-host-2").is_empty(), "{shell:?}");

        fleet.forget_calls();
        let out = fleet.run(
            shell,
            &["exec", "base", "--host", "kvm-host-2", "--", "true"],
        );
        ok(&out);
        assert!(fleet.calls("kvm-host").is_empty(), "{shell:?}");
        // --host is the laptop's: the host never sees it.
        assert_eq!(
            fleet.calls("kvm-host-2"),
            ["exec base -- true"],
            "{shell:?}"
        );
    }
}

#[test]
fn a_stale_cache_entry_is_fixed_after_one_miss() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        // web moves to kvm-host-3 behind the cache's back.
        fleet.host("kvm-host-2", &["base"], 6 << 30);
        fleet.host("kvm-host-3", &["web"], 4 << 30);
        fleet.forget_calls();

        let out = fleet.run(shell, &["exec", "web", "--", "true"]);
        ok(&out);
        assert_eq!(fleet.calls("kvm-host-2"), ["ip web", "list"], "{shell:?}");
        assert_eq!(
            fleet.calls("kvm-host-3"),
            ["list", "exec web -- true"],
            "{shell:?}"
        );
        assert!(fleet.cache().contains("web runner@kvm-host-3\n"));

        fleet.forget_calls();
        ok(&fleet.run(shell, &["exec", "web", "--", "true"]));
        assert!(fleet.calls("kvm-host-2").is_empty(), "{shell:?}");
        assert_eq!(
            fleet.calls("kvm-host-3"),
            ["ip web", "exec web -- true"],
            "{shell:?}"
        );
    }
}

#[test]
fn a_vm_no_host_has_goes_to_the_first_host_which_says_so() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["exec", "nope", "--", "true"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("no VM named nope"), "{shell:?}");
        assert_eq!(
            fleet.commands("kvm-host"),
            ["exec nope -- true"],
            "{shell:?}"
        );
        assert!(fleet.commands("kvm-host-2").is_empty(), "{shell:?}");
    }
}

// Stdin reaches the command whole, even when the cache sent it wrong first:
// the ask whether a host has the VM reads none of it.
#[test]
fn piped_stdin_survives_a_stale_cache() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.host("kvm-host-2", &["base"], 6 << 30);
        fleet.host("kvm-host-3", &["base", "web"], 4 << 30);
        let mut child = fleet
            .runner(shell, &["exec", "web", "--", "cat"])
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
            fleet.commands("kvm-host-3"),
            ["exec web -- cat"],
            "{shell:?}"
        );
        let got = fs::read_to_string(fleet.dir.path().join("fleet/kvm-host-3/stdin"));
        assert_eq!(got.unwrap(), "piped in\n", "{shell:?}");
    }
}

#[test]
fn create_goes_to_the_host_with_the_most_free_memory() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["create", "app", "--mem", "4G"]);
        ok(&out);
        assert_eq!(text(&out.stdout), "app  gen 0  6 bytes\n", "{shell:?}");
        assert_eq!(
            fleet.commands("kvm-host-2"),
            ["create app --mem 4G"],
            "{shell:?}"
        );
        assert!(fleet.commands("kvm-host").is_empty(), "{shell:?}");
        assert!(
            fleet.cache().contains("app runner@kvm-host-2\n"),
            "{shell:?}"
        );
    }
}

#[test]
fn host_overrides_where_create_goes() {
    for shell in SHELLS {
        for flag in [&["--host", "kvm-host-3"][..], &["--host=runner@kvm-host-3"]] {
            let fleet = Fleet::new();
            let mut args = vec!["create", "app"];
            args.extend(flag);
            ok(&fleet.run(shell, &args));
            assert_eq!(fleet.commands("kvm-host-3"), ["create app"], "{shell:?}");
            assert!(fleet.commands("kvm-host-2").is_empty(), "{shell:?}");
        }
    }
}

#[test]
fn a_fork_of_a_vm_other_than_base_goes_to_that_vm_s_host() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["create", "dev2", "--from", "dev"]));
        assert_eq!(
            fleet.commands("kvm-host"),
            ["create dev2 --from dev"],
            "{shell:?}"
        );
        assert!(fleet.commands("kvm-host-2").is_empty(), "{shell:?}");
        // From base, it's placed by memory.
        ok(&fleet.run(shell, &["create", "dev3", "--from=base"]));
        assert_eq!(
            fleet.commands("kvm-host-2"),
            ["create dev3 --from=base"],
            "{shell:?}"
        );
    }
}

#[test]
fn create_refuses_a_name_another_host_has() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["create", "dev", "--host", "kvm-host-3"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("kvm-host has a VM named dev"), "{err}");
        for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            assert!(fleet.commands(host).is_empty(), "{shell:?} {host}");
        }
    }
}

// A host that doesn't answer still has the VMs it had when last listed.
#[test]
fn create_counts_the_vms_a_host_that_doesn_t_answer_had() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.down("kvm-host-2");
        let out = fleet.run(shell, &["create", "web"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("kvm-host-2 didn't answer"), "{err}");
        assert!(err.contains("kvm-host-2 has a VM named web"), "{err}");

        // A new name goes to the host with the most free memory that answered.
        let out = fleet.run(shell, &["create", "app"]);
        ok(&out);
        assert_eq!(fleet.commands("kvm-host-3"), ["create app"], "{shell:?}");
        assert!(text(&out.stderr).contains("kvm-host-2 didn't answer"));
        // Its VMs stay in the cache until it answers again.
        assert!(
            fleet.cache().contains("web runner@kvm-host-2\n"),
            "{shell:?}"
        );
    }
}

#[test]
fn import_places_as_create_does_and_passes_stdin_through() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let mut child = fleet
            .runner(shell, &["import", "app", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"an export\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        ok(&out);
        assert_eq!(fleet.commands("kvm-host-2"), ["import app -"], "{shell:?}");
        assert_eq!(
            fs::read_to_string(fleet.dir.path().join("fleet/kvm-host-2/stdin")).unwrap(),
            "an export\n"
        );
        let out = fleet.run(shell, &["import", "web", "-"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("kvm-host-2 has a VM named web"));

        // base is on every host, so it's no clash: it goes to the first, or --host's.
        ok(&fleet.run(
            shell,
            &["import", "base", "--host", "kvm-host-3", "/tmp/disk"],
        ));
        assert_eq!(
            fleet.commands("kvm-host-3"),
            ["import base /tmp/disk"],
            "{shell:?}"
        );
    }
}

#[test]
fn delete_takes_the_vm_out_of_the_cache() {
    let fleet = Fleet::new();
    ok(&fleet.run(SHELLS[0], &["list"]));
    ok(&fleet.run(SHELLS[0], &["delete", "web"]));
    assert_eq!(fleet.vms("kvm-host-2"), ["base"]);
    assert!(!fleet.cache().contains("web "), "{}", fleet.cache());
}

// #141: move goes to the VM's host, whose runner reaches the target by ssh,
// so it gets the target as listed; the cache follows the VM.
#[test]
fn move_goes_to_the_vm_s_host_with_the_target_as_listed() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.forget_calls();
        let out = fleet.run(shell, &["move", "web", "kvm-host-3"]);
        ok(&out);
        assert_eq!(text(&out.stdout), "moved web to kvm-host-3\n", "{shell:?}");
        assert_eq!(
            fleet.commands("kvm-host-2"),
            ["move web runner@kvm-host-3"],
            "{shell:?}"
        );
        assert!(fleet.commands("kvm-host").is_empty(), "{shell:?}");
        assert!(fleet.commands("kvm-host-3").is_empty(), "{shell:?}");
        let cache = fleet.cache();
        assert!(cache.contains("web runner@kvm-host-3\n"), "{cache}");
        assert!(!cache.contains("web runner@kvm-host-2"), "{cache}");

        // Then web's commands go straight to its new host: no host is surveyed.
        fleet.forget_calls();
        ok(&fleet.run(shell, &["exec", "web", "--", "true"]));
        assert_eq!(
            fleet.calls("kvm-host-3"),
            ["ip web", "exec web -- true"],
            "{shell:?}"
        );
        assert!(fleet.calls("kvm-host").is_empty(), "{shell:?}");
        assert!(fleet.calls("kvm-host-2").is_empty(), "{shell:?}");
    }
}

// The target is the argument where it stands, so an option's value that
// reads as a host stays as typed, and --host coming out moves nothing.
#[test]
fn move_s_target_is_found_where_it_stands() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["move", "dev", "runner@kvm-host-3"],
            "move dev runner@kvm-host-3",
        ),
        (
            &["--root", "kvm-host-3", "move", "dev", "kvm-host-3"],
            "--root kvm-host-3 move dev runner@kvm-host-3",
        ),
        (
            &["move", "--host", "kvm-host", "dev", "kvm-host-3"],
            "move dev runner@kvm-host-3",
        ),
        (
            &["move", "--", "dev", "kvm-host-3"],
            "move -- dev runner@kvm-host-3",
        ),
        (
            &["move", "dev", "--", "kvm-host-3"],
            "move dev -- runner@kvm-host-3",
        ),
    ];
    for shell in SHELLS {
        for (args, got) in cases {
            let fleet = Fleet::new();
            ok(&fleet.run(shell, &["list"]));
            fleet.forget_calls();
            ok(&fleet.run(shell, args));
            assert_eq!(fleet.commands("kvm-host"), [*got], "{shell:?} {args:?}");
            assert!(
                fleet.commands("kvm-host-2").is_empty(),
                "{shell:?} {args:?}"
            );
            assert!(
                fleet.commands("kvm-host-3").is_empty(),
                "{shell:?} {args:?}"
            );
            let cache = fleet.cache();
            assert!(
                cache.contains("dev runner@kvm-host-3\n"),
                "{args:?} {cache}"
            );
        }
    }
}

#[test]
fn move_refuses_a_target_that_isn_t_listed_or_is_the_vm_s_own_host() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.forget_calls();
        let out = fleet.run(shell, &["move", "web", "elsewhere"]);
        assert_eq!(out.status.code(), Some(2), "{shell:?}");
        let err = text(&out.stderr);
        assert!(
            err.contains(
                "runner: elsewhere isn't one of the hosts: kvm-host kvm-host-2 kvm-host-3"
            ),
            "{err}"
        );
        for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            assert!(fleet.commands(host).is_empty(), "{shell:?} {host}");
        }

        let out = fleet.run(shell, &["move", "web", "kvm-host-2"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(
            err.contains("runner: web is on kvm-host-2 already"),
            "{err}"
        );
        for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            let calls = fleet.calls(host);
            assert!(
                calls.iter().all(|c| !c.starts_with("move")),
                "{shell:?} {host}: {calls:?}"
            );
        }
        assert_eq!(fleet.vms("kvm-host-2"), ["base", "web"], "{shell:?}");

        // A VM no host has goes to the first host, which says so; one with
        // no target, there as typed, for its runner to say what's missing.
        fleet.forget_calls();
        let out = fleet.run(shell, &["move", "nope", "kvm-host-3"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("no VM named nope"), "{shell:?}");
        assert_eq!(
            fleet.commands("kvm-host"),
            ["move nope kvm-host-3"],
            "{shell:?}"
        );
        fleet.forget_calls();
        let _ = fleet.run(shell, &["move", "web"]);
        assert_eq!(fleet.calls("kvm-host"), ["move web"], "{shell:?}");
        assert!(fleet.calls("kvm-host-2").is_empty(), "{shell:?}");
        assert!(fleet.calls("kvm-host-3").is_empty(), "{shell:?}");
    }
}

#[test]
fn doctor_and_gc_without_a_vm_go_to_every_host() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["doctor"]);
        ok(&out);
        assert_eq!(
            text(&out.stdout),
            "kvm-host  ok    kvm  /dev/kvm\n\
             kvm-host-2  ok    kvm  /dev/kvm\n\
             kvm-host-3  ok    kvm  /dev/kvm\n",
            "{shell:?}"
        );
        let out = fleet.run(shell, &["gc", "--dry-run"]);
        ok(&out);
        assert_eq!(
            text(&out.stdout),
            "kvm-host  base  nothing to free\n\
             kvm-host  dev  nothing to free\n\
             kvm-host-2  base  nothing to free\n\
             kvm-host-2  web  nothing to free\n",
            "{shell:?}"
        );
        for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            assert_eq!(fleet.calls(host), ["doctor", "gc --dry-run"], "{shell:?}");
        }
        fleet.down("kvm-host-3");
        let out = fleet.run(shell, &["doctor"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("kvm-host-3 didn't answer"));
    }
}

#[test]
fn commands_naming_no_vm_go_to_the_first_host_unless_host_says() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["unit"]));
        assert_eq!(fleet.calls("kvm-host"), ["unit"], "{shell:?}");
        ok(&fleet.run(shell, &["list", "--host", "kvm-host-2"]));
        assert_eq!(fleet.calls("kvm-host-2"), ["list"], "{shell:?}");
    }
}

// MCP is one SSH session, to the first host, or the one --host names, whose
// runner reaches the rest of the list as its peers (README.md, MCP clients).
#[test]
fn mcp_passes_the_other_hosts_as_peers() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["mcp"]));
        let peers =
            "runner mcp --name runner@kvm-host --peer runner@kvm-host-2 --peer runner@kvm-host-3";
        assert_eq!(
            fleet.ssh_calls(),
            [["-T", "-o", "BatchMode=yes", HOSTS[0], peers]],
            "{shell:?}"
        );
        assert_eq!(fleet.calls("kvm-host"), [&peers[7..]], "{shell:?}");

        fleet.forget_calls();
        ok(&fleet.run(shell, &["mcp", "--user", "bob", "--host", "kvm-host-2"]));
        let peers = "runner mcp --user bob --name runner@kvm-host-2 --peer runner@kvm-host --peer runner@kvm-host-3";
        assert_eq!(
            fleet.ssh_calls(),
            [["-T", "-o", "BatchMode=yes", HOSTS[1], peers]],
            "{shell:?}"
        );

        // Each host as the hosts file lists it, without the line's comment.
        fleet.forget_calls();
        fs::write(
            fleet.hosts_file(),
            "runner@kvm-host\nrunner@kvm-host-2 # the second\n",
        )
        .unwrap();
        ok(&fleet.run(shell, &["mcp"]));
        let peers = "runner mcp --name runner@kvm-host --peer runner@kvm-host-2";
        assert_eq!(
            fleet.ssh_calls(),
            [["-T", "-o", "BatchMode=yes", HOSTS[0], peers]],
            "{shell:?}"
        );

        // One host, or RUNNER_HOST, has no peers.
        fleet.forget_calls();
        fs::write(fleet.hosts_file(), "runner@kvm-host-2\n").unwrap();
        ok(&fleet.run(shell, &["mcp"]));
        let out = fleet
            .runner(shell, &["mcp"])
            .env("RUNNER_HOST", "runner@kvm-host-3")
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(
            fleet.ssh_calls(),
            [
                [
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "runner@kvm-host-2",
                    "runner mcp"
                ],
                [
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "runner@kvm-host-3",
                    "runner mcp"
                ]
            ],
            "{shell:?}"
        );
    }
}

#[test]
fn a_host_that_isn_t_listed_is_refused() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        for args in [&["list", "--host", "elsewhere"][..], &["list", "--host"]] {
            let out = fleet.run(shell, args);
            assert_eq!(out.status.code(), Some(2), "{shell:?} {args:?}");
            assert!(text(&out.stderr).contains("--host"), "{shell:?}");
            assert!(fleet.ssh_calls().is_empty(), "{shell:?} {args:?}");
        }
    }
}

#[test]
fn a_hosts_line_that_looks_like_an_ssh_option_is_refused() {
    let fleet = Fleet::new();
    fs::write(
        fleet.hosts_file(),
        "runner@kvm-host\n-oProxyCommand=touch pwned\n",
    )
    .unwrap();
    let out = fleet.run(SHELLS[0], &["list"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("-oProxyCommand"));
    assert!(fleet.ssh_calls().is_empty());
}

#[test]
fn runner_host_still_means_that_host_alone() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet
            .runner(shell, &["list"])
            .env("RUNNER_HOST", "runner@kvm-host-2")
            .output()
            .unwrap();
        ok(&out);
        assert_eq!(
            fleet.ssh_calls(),
            [["runner@kvm-host-2", "runner list"]],
            "{shell:?}"
        );
        // One host's list as the host prints it: no host column.
        assert_eq!(
            text(&out.stdout),
            "base  gen 0  6 bytes  4096 bytes used\nweb  gen 0  6 bytes  4096 bytes used\n"
        );
    }
}

#[test]
fn hosts_add_checks_the_host_answers_then_adds_it() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fs::remove_file(fleet.hosts_file()).unwrap();
        // The old host file is a list of one, which add starts from.
        let conf = fleet.home().join(".config/runner");
        fs::write(conf.join("host"), "runner@kvm-host\n").unwrap();

        let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host-2"]);
        ok(&out);
        assert!(text(&out.stdout).contains("kvm-host-2"), "{shell:?}");
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host\nrunner@kvm-host-2\n",
            "{shell:?}"
        );
        assert_eq!(fleet.calls("kvm-host-2"), ["hosts"], "{shell:?}");

        // Once is enough.
        let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host-2"]);
        ok(&out);
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host\nrunner@kvm-host-2\n",
            "{shell:?}"
        );

        // One that doesn't answer isn't added.
        fleet.down("kvm-host-3");
        let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host-3"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("kvm-host-3"), "{shell:?}");
        let out = fleet.run(shell, &["hosts", "add", "runner@nowhere"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host\nrunner@kvm-host-2\n",
            "{shell:?}"
        );

        // Nor one ssh would take for an option.
        for bad in ["", "-oProxyCommand=touch pwned"] {
            let out = fleet.run(shell, &["hosts", "add", bad]);
            assert_eq!(out.status.code(), Some(2), "{shell:?} {bad:?}");
        }
    }
}

#[test]
fn hosts_add_works_with_no_host_yet() {
    let fleet = Fleet::new();
    fs::remove_file(fleet.hosts_file()).unwrap();
    ok(&fleet.run(SHELLS[0], &["hosts", "add", "runner@kvm-host-3"]));
    assert_eq!(
        fs::read_to_string(fleet.hosts_file()).unwrap(),
        "runner@kvm-host-3\n"
    );
}

#[test]
fn hosts_rm_takes_a_host_out_by_its_name() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["hosts", "rm", "kvm-host-2"]);
        ok(&out);
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host\nrunner@kvm-host-3\n",
            "{shell:?}"
        );
        for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
            assert!(fleet.calls(host).is_empty(), "{shell:?} {host}");
        }
        let out = fleet.run(shell, &["hosts", "rm", "runner@kvm-host"]);
        ok(&out);
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host-3\n",
            "{shell:?}"
        );
        let out = fleet.run(shell, &["hosts", "rm", "kvm-host-2"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("kvm-host-2"), "{shell:?}");
    }
}

#[test]
fn hosts_with_bad_arguments_says_how() {
    let fleet = Fleet::new();
    for args in [
        &["hosts", "add"][..],
        &["hosts", "rm"],
        &["hosts", "move"],
        &["hosts", "add", "a", "b"],
    ] {
        let out = fleet.run(SHELLS[0], args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("runner hosts add"), "{args:?}");
    }
    let out = fleet.run(SHELLS[0], &["hosts", "--help"]);
    ok(&out);
    assert!(text(&out.stdout).contains("runner hosts rm"));
    assert!(fleet.ssh_calls().is_empty());
}

#[test]
fn install_with_a_hosts_file_sets_its_first() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let install = |args: &[&str]| {
            Command::new(shell[0])
                .args(&shell[1..])
                .arg("client/install.sh")
                .args(args)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .env("HOME", fleet.home())
                .output()
                .unwrap()
        };
        ok(&install(&["runner@kvm-host-3"]));
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host-3\nrunner@kvm-host\nrunner@kvm-host-2\n",
            "{shell:?}"
        );
        ok(&install(&["runner@kvm-host-4"]));
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host-4\nrunner@kvm-host-3\nrunner@kvm-host\nrunner@kvm-host-2\n",
            "{shell:?}"
        );
        // A rerun without one keeps the list.
        let out = install(&[]);
        ok(&out);
        assert!(text(&out.stdout).contains("runner@kvm-host-4"), "{shell:?}");
        assert_eq!(
            fs::read_to_string(fleet.hosts_file())
                .unwrap()
                .lines()
                .count(),
            4
        );
    }
}

// The client finds the VM past the options that take a value, so each of the
// host's has to be in its list: one missing would make its value look like
// the VM. Every option clap has that takes a value, in every command
// `runner help` lists, is checked against the client's `valued` line.
#[test]
fn the_client_knows_every_option_that_takes_a_value() {
    let client = fs::read_to_string(client("runner")).unwrap();
    let line = client
        .lines()
        .find_map(|l| l.strip_prefix("valued=\""))
        .expect("client/runner has a valued=\"...\" line");
    let known: Vec<&str> = line.trim_end_matches('"').split_whitespace().collect();
    let clap = clap_options_that_take_a_value();
    // Today's, so the check has something to check.
    for opt in ["--from", "--user", "-n", "--lines", "--creds", "--root"] {
        assert!(
            clap.contains(&opt.to_string()),
            "{opt} not found in clap's: {clap:?}"
        );
    }
    let missing: Vec<&String> = clap
        .iter()
        .filter(|o| !known.contains(&o.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "client/runner's valued line lacks {missing:?}"
    );
    // Seeded for tasks that merge in any order: #147's and #160's.
    for opt in ["--n", "--run", "--host"] {
        assert!(known.contains(&opt), "{opt}");
    }
}

#[test]
fn options_in_help_reads_the_names_that_take_a_value() {
    let options = options_in_help(
        "Options:\n  -d, --dir <DIR>  Where\n      --tty  A flag\n  -n, --lines <LINES>  Last\n",
    );
    assert_eq!(options, ["-d", "--dir", "-n", "--lines"]);
}

fn help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .args(args)
        .arg("--help")
        .env_remove("RUNNER_ROOT")
        .output()
        .unwrap();
    ok(&out);
    text(&out.stdout)
}

// The commands a help page's Commands: section lists, less help itself.
fn commands_in_help(page: &str) -> Vec<String> {
    page.lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        // A line of its own for each command; one wrapped would go deeper.
        .filter(|l| !l.starts_with("   "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .map(str::to_string)
        .collect()
}

// Each option line's names, when it shows a <VALUE>.
fn options_in_help(page: &str) -> Vec<String> {
    let mut got = Vec::new();
    for line in page.lines().map(str::trim_start) {
        if !line.starts_with('-') {
            continue;
        }
        let spec = line.split("  ").next().unwrap();
        if !spec.contains('<') {
            continue;
        }
        for word in spec.split([',', ' ']) {
            let name = word.split('=').next().unwrap();
            if name.starts_with('-') {
                got.push(name.to_string());
            }
        }
    }
    got
}

fn clap_options_that_take_a_value() -> Vec<String> {
    let top = help(&[]);
    let mut all = options_in_help(&top);
    for cmd in commands_in_help(&top) {
        let page = help(&[&cmd]);
        all.extend(options_in_help(&page));
        for sub in commands_in_help(&page) {
            all.extend(options_in_help(&help(&[&cmd, &sub])));
        }
    }
    all.sort();
    all.dedup();
    all
}

#[test]
fn claude_on_a_new_vm_makes_it_where_create_would() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["claude", "fresh"]));
        assert_eq!(fleet.commands("kvm-host-2"), ["claude fresh"], "{shell:?}");
        assert!(fleet.commands("kvm-host").is_empty(), "{shell:?}");
        // One that exists goes to its host, and --from a VM other than base
        // to that VM's.
        ok(&fleet.run(shell, &["claude", "dev", "--", "--continue"]));
        assert_eq!(
            fleet.commands("kvm-host"),
            ["claude dev -- --continue"],
            "{shell:?}"
        );
        ok(&fleet.run(shell, &["claude", "fresh2", "--from", "dev"]));
        assert_eq!(
            fleet.commands("kvm-host"),
            ["claude dev -- --continue", "claude fresh2 --from dev"],
            "{shell:?}"
        );
    }
}

#[test]
fn a_hosts_file_of_one_host_runs_as_one_host_always_has() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fs::write(fleet.hosts_file(), "# mine\nrunner@kvm-host-2\n").unwrap();
        ok(&fleet.run(shell, &["list"]));
        ok(&fleet.run(shell, &["exec", "web", "--", "true"]));
        assert_eq!(
            fleet.ssh_calls(),
            [
                vec!["runner@kvm-host-2", "runner list"],
                vec!["runner@kvm-host-2", "runner exec web -- true"]
            ],
            "{shell:?}"
        );
    }
}

// From review: a program's own --host, in exec's or service add's argv, or
// a prompt or session that is `--host`, is the host's to parse, not the
// laptop's.
#[test]
fn a_program_s_own_host_option_reaches_the_host() {
    let cases: &[&[&str]] = &[
        &["exec", "web", "uvicorn", "--host", "0.0.0.0"],
        &["exec", "web", "--dir", "/w", "vite", "--host=0.0.0.0"],
        &[
            "service", "add", "web", "srv", "python3", "app.py", "--host", "kvm-host",
        ],
        &["run", "web", "--host"],
        &["kill", "web", "--host"],
    ];
    for shell in SHELLS {
        for args in cases {
            // One host, as on main: exactly as typed.
            let fleet = Fleet::new();
            let out = fleet
                .runner(shell, args)
                .env("RUNNER_HOST", "runner@kvm-host-2")
                .output()
                .unwrap();
            ok(&out);
            let mut want = "runner".to_string();
            for a in *args {
                want = format!("{want} {a}");
            }
            assert_eq!(
                fleet.ssh_calls(),
                [["runner@kvm-host-2", want.as_str()]],
                "{shell:?}"
            );
            // Several: to web's host, still as typed.
            let fleet = Fleet::new();
            ok(&fleet.run(shell, &["list"]));
            fleet.forget_calls();
            ok(&fleet.run(shell, args));
            assert_eq!(
                fleet.commands("kvm-host-2"),
                [args.join(" ")],
                "{shell:?} {args:?}"
            );
        }
    }
}

#[test]
fn a_vm_after_a_double_dash_is_still_found() {
    let fleet = Fleet::new();
    ok(&fleet.run(SHELLS[0], &["exec", "--", "web", "true"]));
    assert_eq!(fleet.commands("kvm-host-2"), ["exec -- web true"]);
}

#[test]
fn claude_with_host_refuses_a_name_another_host_has() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(shell, &["claude", "dev", "--host", "kvm-host-3"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(text(&out.stderr).contains("kvm-host has a VM named dev"));
        assert!(fleet.commands("kvm-host-3").is_empty(), "{shell:?}");
        // On its own host, --host is fine.
        ok(&fleet.run(shell, &["claude", "dev", "--host", "kvm-host"]));
        assert_eq!(fleet.commands("kvm-host"), ["claude dev"], "{shell:?}");
    }
}

#[test]
fn import_replace_of_a_name_no_host_has_is_placed() {
    let fleet = Fleet::new();
    ok(&fleet.run(SHELLS[0], &["import", "--replace", "fresh", "/tmp/disk"]));
    assert_eq!(
        fleet.commands("kvm-host-2"),
        ["import --replace fresh /tmp/disk"]
    );
}

// --replace imports on top of the VM where it is, so --host can't make a
// second of one another host has.
#[test]
fn import_replace_with_host_refuses_a_name_another_host_has() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let out = fleet.run(
            shell,
            &[
                "import",
                "--replace",
                "dev",
                "/tmp/disk",
                "--host",
                "kvm-host-2",
            ],
        );
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(
            err.contains("kvm-host has a VM named dev"),
            "{shell:?} {err}"
        );
        assert!(fleet.commands("kvm-host-2").is_empty(), "{shell:?}");
        assert_eq!(fleet.vms("kvm-host-2"), ["base", "web"], "{shell:?}");

        // On its own host, --host is fine, and base is on every host.
        ok(&fleet.run(
            shell,
            &[
                "import",
                "--replace",
                "dev",
                "/tmp/disk",
                "--host",
                "kvm-host",
            ],
        ));
        assert_eq!(
            fleet.commands("kvm-host"),
            ["import --replace dev /tmp/disk"],
            "{shell:?}"
        );
        ok(&fleet.run(
            shell,
            &[
                "import",
                "--replace",
                "base",
                "/tmp/disk",
                "--host",
                "kvm-host-3",
            ],
        ));
        assert_eq!(
            fleet.commands("kvm-host-3"),
            ["import --replace base /tmp/disk"],
            "{shell:?}"
        );
    }
}

#[test]
fn hosts_add_refuses_another_login_to_a_listed_host() {
    let fleet = Fleet::new();
    let out = fleet.run(SHELLS[0], &["hosts", "add", "ubuntu@kvm-host-2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("runner@kvm-host-2"));
    assert_eq!(
        fs::read_to_string(fleet.hosts_file()).unwrap(),
        HOSTS.join("\n") + "\n"
    );
}

#[test]
fn hosts_add_and_rm_keep_the_file_s_comments() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let with = "# MCP's\nrunner@kvm-host\n\n# spare\nrunner@kvm-host-2\n";
        fs::write(fleet.hosts_file(), with).unwrap();
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host-3"]));
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            format!("{with}runner@kvm-host-3\n"),
            "{shell:?}"
        );
        ok(&fleet.run(shell, &["hosts", "rm", "kvm-host-2"]));
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "# MCP's\nrunner@kvm-host\n\n# spare\nrunner@kvm-host-3\n",
            "{shell:?}"
        );
    }
}

// The file is edited by hand too, so its last line may have no newline.
#[test]
fn hosts_add_keeps_a_last_line_with_no_newline() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fs::write(fleet.hosts_file(), "runner@kvm-host\nrunner@kvm-host-2").unwrap();
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host-3"]));
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            HOSTS.join("\n") + "\n",
            "{shell:?}"
        );
    }
}

#[test]
fn install_keeps_the_hosts_file_s_comments() {
    let fleet = Fleet::new();
    fs::write(
        fleet.hosts_file(),
        "# MCP's first\nrunner@kvm-host\nrunner@kvm-host-2\n",
    )
    .unwrap();
    let out = Command::new("sh")
        .arg("client/install.sh")
        .arg("runner@kvm-host-2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", fleet.home())
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(
        fs::read_to_string(fleet.hosts_file()).unwrap(),
        "runner@kvm-host-2\n# MCP's first\nrunner@kvm-host\n"
    );
}

#[test]
fn with_every_host_removed_it_says_to_add_one() {
    let fleet = Fleet::new();
    for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
        ok(&fleet.run(SHELLS[0], &["hosts", "rm", host]));
    }
    let out = fleet.run(SHELLS[0], &["list"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains("runner hosts add"), "{err}");
    assert!(err.contains("~/.config/runner/hosts"), "{err}");
}

// Every host is asked at once, so the slowest sets the pace, not their sum,
// and each ask has keepalives, so a host that stops answering mid-way ends.
#[test]
fn every_host_is_asked_at_once() {
    let fleet = Fleet::new();
    for host in ["kvm-host", "kvm-host-2", "kvm-host-3"] {
        fs::write(fleet.dir.path().join("fleet").join(host).join("slow"), "").unwrap();
    }
    let started = std::time::Instant::now();
    ok(&fleet.run(SHELLS[0], &["list"]));
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_millis(2500), "{took:?}");
    for call in fleet.ssh_calls() {
        assert!(
            call.iter().any(|a| a.starts_with("ServerAliveInterval=")),
            "{call:?}"
        );
    }
}

// #139: every host's key on every other, once, so each host drives the
// others' runners through `runner peer` and nothing more.
#[test]
fn hosts_add_puts_each_host_s_key_on_every_other_once() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fs::write(fleet.hosts_file(), "runner@kvm-host\nrunner@kvm-host-2\n").unwrap();
        // A line edited by hand, with no newline, stays as it is.
        fleet.set_authorized_keys("kvm-host", LAPTOP);
        let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host-3"]);
        ok(&out);
        assert_eq!(
            fleet.authorized_keys("kvm-host"),
            trusting(&["kvm-host-2", "kvm-host-3"]),
            "{shell:?}"
        );
        assert_eq!(
            fleet.authorized_keys("kvm-host-2"),
            trusting(&["kvm-host", "kvm-host-3"]),
            "{shell:?}"
        );
        assert_eq!(
            fleet.authorized_keys("kvm-host-3"),
            trusting(&["kvm-host", "kvm-host-2"]),
            "{shell:?}"
        );
        // As runner left it: only runner reads or writes it.
        let path = fleet.dir.path().join("fleet/kvm-host/.ssh/authorized_keys");
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{shell:?}: {mode:o}");
        // No runner command ran for it.
        for host in ["kvm-host", "kvm-host-2"] {
            assert!(fleet.calls(host).is_empty(), "{shell:?} {host}");
        }

        // Again changes nothing.
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host-3"]));
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host"]));
        assert_eq!(
            fleet.authorized_keys("kvm-host-2"),
            trusting(&["kvm-host", "kvm-host-3"]),
            "{shell:?}"
        );

        // A host with a new key, from a reinstall, is trusted by that key alone.
        let new = key("kvmhost2new");
        fleet.set_key(
            "kvm-host-2",
            Some(&format!("ssh-ed25519 {new} runner@kvm-host-2")),
        );
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host-2"]));
        for host in ["kvm-host", "kvm-host-3"] {
            let keys = fleet.authorized_keys(host);
            assert!(
                keys.contains(&peer_line("kvm-host-2", &new)),
                "{shell:?} {host}: {keys}"
            );
            assert!(
                !keys.contains(&key("kvm-host-2")),
                "{shell:?} {host}: {keys}"
            );
            assert_eq!(keys.lines().count(), 3, "{shell:?} {host}: {keys}");
        }
    }
}

#[test]
fn hosts_add_names_a_host_it_can_t_get_a_key_from_and_shares_the_rest() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host"]));
        // kvm-host-2 lost its key, and kvm-host-3 is down.
        fleet.set_key("kvm-host-2", None);
        fleet.down("kvm-host-3");
        let before = fleet.authorized_keys("kvm-host-3");
        let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("kvm-host-2 has no SSH key"), "{err}");
        assert!(err.contains("host/install.sh"), "{err}");
        assert!(err.contains("kvm-host-3 didn't answer"), "{err}");
        // What couldn't be asked again stays as it was, in any order.
        let sorted = |keys: String| {
            let mut lines: Vec<String> = keys.lines().map(String::from).collect();
            lines.sort();
            lines
        };
        assert_eq!(
            sorted(fleet.authorized_keys("kvm-host")),
            sorted(trusting(&["kvm-host-2", "kvm-host-3"])),
            "{shell:?}"
        );
        assert_eq!(
            sorted(fleet.authorized_keys("kvm-host-2")),
            sorted(trusting(&["kvm-host", "kvm-host-3"])),
            "{shell:?}"
        );
        assert_eq!(fleet.authorized_keys("kvm-host-3"), before, "{shell:?}");
    }
}

// A host's key comes from that host, which can't be trusted to send one: only
// a bare ed25519 key goes in, behind the runner peer options the laptop adds.
#[test]
fn hosts_add_shares_nothing_but_a_plain_ed25519_key() {
    let k = key("kvm-host-2");
    let bad = [
        String::new(),
        format!("command=\"sh\" ssh-ed25519 {k} x"),
        format!("ssh-rsa {k} x"),
        format!("ssh-ed25519 {k};touch${{IFS}}pwned x"),
        format!("ssh-ed25519 {k}\",pty x"),
        "ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQABAAABAQ x".to_string(),
        format!("ssh-ed25519 {k}'$(touch pwned)' x"),
    ];
    for shell in SHELLS {
        for pub_line in &bad {
            let fleet = Fleet::new();
            fleet.set_key("kvm-host-2", Some(pub_line));
            let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host"]);
            assert_eq!(out.status.code(), Some(1), "{shell:?} {pub_line:?}");
            assert!(
                text(&out.stderr).contains("kvm-host-2 has no SSH key"),
                "{pub_line:?}"
            );
            for host in ["kvm-host", "kvm-host-3"] {
                let keys = fleet.authorized_keys(host);
                assert!(
                    !keys.contains("kvm-host-2"),
                    "{shell:?} {pub_line:?}: {keys}"
                );
            }
            assert_eq!(fleet.authorized_keys("kvm-host"), trusting(&["kvm-host-3"]));
            assert!(!fleet.dir.path().join("fleet/kvm-host/pwned").exists());
        }
    }
}

#[test]
fn hosts_rm_takes_the_host_s_key_lines_out() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host"]));
        let out = fleet.run(shell, &["hosts", "rm", "kvm-host-2"]);
        ok(&out);
        assert_eq!(
            fleet.authorized_keys("kvm-host"),
            trusting(&["kvm-host-3"]),
            "{shell:?}"
        );
        assert_eq!(
            fleet.authorized_keys("kvm-host-3"),
            trusting(&["kvm-host"]),
            "{shell:?}"
        );
        // And the host that left drives no host, nor does any host drive it.
        assert_eq!(
            fleet.authorized_keys("kvm-host-2"),
            trusting(&[]),
            "{shell:?}"
        );

        // One that's gone for good: the rest still drop its line, and it's named.
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host-2"]));
        fleet.down("kvm-host-2");
        let out = fleet.run(shell, &["hosts", "rm", "kvm-host-2"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert!(
            text(&out.stderr).contains("kvm-host-2 didn't answer"),
            "{shell:?}"
        );
        assert_eq!(
            fs::read_to_string(fleet.hosts_file()).unwrap(),
            "runner@kvm-host\nrunner@kvm-host-3\n",
            "{shell:?}"
        );
        assert_eq!(
            fleet.authorized_keys("kvm-host"),
            trusting(&["kvm-host-3"]),
            "{shell:?}"
        );
        assert_eq!(
            fleet.authorized_keys("kvm-host-3"),
            trusting(&["kvm-host"]),
            "{shell:?}"
        );
    }
}

// A host that was down for a `hosts rm` still has the line of the host that
// left. The next `hosts add` takes it out: a host trusts only listed hosts.
#[test]
fn a_line_left_by_a_host_that_was_down_goes_at_the_next_add() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host"]));
        fleet.down("kvm-host-3");
        let out = fleet.run(shell, &["hosts", "rm", "kvm-host-2"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("kvm-host-3 didn't answer"), "{err}");
        assert!(err.contains("runner hosts add"), "{err}");
        assert_eq!(
            fleet.authorized_keys("kvm-host"),
            trusting(&["kvm-host-3"]),
            "{shell:?}"
        );

        fs::remove_file(fleet.dir.path().join("fleet/kvm-host-3/down")).unwrap();
        ok(&fleet.run(shell, &["hosts", "add", "runner@kvm-host-3"]));
        assert_eq!(
            fleet.authorized_keys("kvm-host-3"),
            trusting(&["kvm-host"]),
            "{shell:?}"
        );
    }
}

#[test]
fn hosts_add_and_rm_refuse_a_listed_host_that_looks_like_an_ssh_option() {
    let fleet = Fleet::new();
    fs::write(
        fleet.hosts_file(),
        "runner@kvm-host\n-oProxyCommand=touch\n",
    )
    .unwrap();
    let out = fleet.run(SHELLS[0], &["hosts", "add", "runner@kvm-host-2"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("-oProxyCommand"));
    assert!(fleet.ssh_calls().is_empty(), "{:?}", fleet.ssh_calls());
    // Taking it out works, and never hands it to ssh.
    ok(&fleet.run(SHELLS[0], &["hosts", "rm", "-oProxyCommand=touch"]));
    assert_eq!(
        fs::read_to_string(fleet.hosts_file()).unwrap(),
        "runner@kvm-host\n"
    );
    assert!(
        fleet
            .ssh_calls()
            .iter()
            .all(|c| !c.iter().any(|a| a.starts_with("-oProxy"))),
        "{:?}",
        fleet.ssh_calls()
    );
}

// From review: two hosts that report one key, say one cloned from the
// other's disk, can't be told apart by it, so neither would trust the other.
#[test]
fn two_hosts_with_one_key_are_named() {
    let fleet = Fleet::new();
    let k = key("kvm-host-2");
    fleet.set_key(
        "kvm-host-3",
        Some(&format!("ssh-ed25519 {k} runner@kvm-host-3")),
    );
    let out = fleet.run(SHELLS[0], &["hosts", "add", "runner@kvm-host"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(
        err.contains("kvm-host-2 and kvm-host-3 have the same key"),
        "{err}"
    );
    assert!(!text(&out.stdout).contains("shared keys"));
}

// From review: a host that says its key is the laptop's gets no peer line for
// it, which would come before a laptop line added after it and lock the
// laptop into runner peer.
#[test]
fn a_host_can_t_pass_off_another_line_s_key_as_its_own() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        let laptop = LAPTOP.split_whitespace().nth(2).unwrap();
        fleet.set_key(
            "kvm-host-2",
            Some(&format!("ssh-ed25519 {laptop} runner@kvm-host-2")),
        );
        let out = fleet.run(shell, &["hosts", "add", "runner@kvm-host"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        let err = text(&out.stderr);
        assert!(err.contains("kvm-host-2's key"), "{err}");
        assert_eq!(
            fleet.authorized_keys("kvm-host"),
            trusting(&["kvm-host-3"]),
            "{shell:?}"
        );
        assert_eq!(
            fleet.authorized_keys("kvm-host-3"),
            trusting(&["kvm-host"]),
            "{shell:?}"
        );
    }
}

// From review: a listed line that isn't a host doesn't stop rm from taking
// the lines out of the hosts that are.
#[test]
fn hosts_rm_takes_lines_out_past_a_listed_line_that_isn_t_a_host() {
    let fleet = Fleet::new();
    ok(&fleet.run(SHELLS[0], &["hosts", "add", "runner@kvm-host"]));
    fs::write(
        fleet.hosts_file(),
        "runner@kvm-host\nrunner@kvm-host-2\n-oProxyCommand=x\n",
    )
    .unwrap();
    fleet.forget_calls();
    let out = fleet.run(SHELLS[0], &["hosts", "rm", "kvm-host-2"]);
    ok(&out);
    assert_eq!(fleet.authorized_keys("kvm-host"), trusting(&[]));
    assert!(
        fleet
            .ssh_calls()
            .iter()
            .all(|c| !c.iter().any(|a| a.starts_with("-oProxy"))),
        "{:?}",
        fleet.ssh_calls()
    );
}

// From review: sshd takes a peer line with blanks before it, or a tab before
// its key, so those are peer lines for rm to take out too.
#[test]
fn a_peer_line_reformatted_by_hand_still_goes() {
    let fleet = Fleet::new();
    ok(&fleet.run(SHELLS[0], &["hosts", "add", "runner@kvm-host"]));
    let k = key("kvm-host-2");
    let opts = peers::OPTIONS;
    fleet.set_authorized_keys(
        "kvm-host",
        &format!(
            "{LAPTOP}\n  {opts} ssh-ed25519 {k} kvm-host-2\n{opts}\tssh-ed25519\t{k}  kvm-host-2\n"
        ),
    );
    ok(&fleet.run(SHELLS[0], &["hosts", "rm", "kvm-host-2"]));
    assert_eq!(fleet.authorized_keys("kvm-host"), trusting(&[]));
}

// #145: a stopped host aws.sh made is started by a command that goes to it,
// and by no other.

#[test]
fn list_starts_no_host_and_names_the_stopped_ones() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fleet.stop("kvm-host-2");
        fleet.down("kvm-host-3");
        let out = fleet.run(shell, &["list"]);
        assert_eq!(out.status.code(), Some(1), "{shell:?}");
        assert_eq!(
            text(&out.stdout),
            "kvm-host  base  gen 0  6 bytes  4096 bytes used\n\
             kvm-host  dev  gen 0  6 bytes  4096 bytes used\n",
            "{shell:?}"
        );
        let err = text(&out.stderr);
        assert!(err.contains("runner: kvm-host-2 is stopped\n"), "{err}");
        // Down, but AWS has it running: the network, say.
        assert!(err.contains("runner: kvm-host-3 didn't answer\n"), "{err}");
        assert_eq!(
            fleet.aws_calls(),
            ["kvm-host-2 state", "kvm-host-3 state"],
            "{shell:?}"
        );
        assert!(fleet.calls("kvm-host-2").is_empty(), "{shell:?}");
    }
}

#[test]
fn a_command_for_a_vm_on_a_stopped_host_starts_only_that_host() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.forget_calls();
        fleet.stop("kvm-host-2");
        fleet.stop("kvm-host-3");
        let out = fleet.run(shell, &["exec", "web", "--", "true"]);
        ok(&out);
        let err = text(&out.stderr);
        assert!(
            err.contains("runner: kvm-host-2 is stopped, so starting it"),
            "{err}"
        );
        assert_eq!(
            fleet.aws_calls(),
            ["kvm-host-2 state", "kvm-host-3 state", "kvm-host-2 start"],
            "{shell:?}"
        );
        assert_eq!(
            fleet.commands("kvm-host-2"),
            ["exec web -- true"],
            "{shell:?}"
        );
        assert!(fleet.calls("kvm-host-3").is_empty(), "{shell:?}");
        // Found stopped by every host's list, so the command waits for the
        // start, and is sent once.
        let sent = fleet
            .ssh_calls()
            .into_iter()
            .filter(|c| c.last().is_some_and(|l| l == "runner exec web -- true"))
            .count();
        assert_eq!(sent, 1, "{shell:?}");
    }
}

#[test]
fn host_naming_a_stopped_host_starts_it() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        fleet.stop("kvm-host-2");
        let out = fleet.run(shell, &["list", "--host", "kvm-host-2"]);
        ok(&out);
        assert_eq!(
            fleet.aws_calls(),
            ["kvm-host-2 state", "kvm-host-2 start"],
            "{shell:?}"
        );
        assert_eq!(fleet.calls("kvm-host-2"), ["list"], "{shell:?}");
        assert!(fleet.calls("kvm-host").is_empty(), "{shell:?}");
    }
}

// The VM's host streams it to the target over ssh, so a stopped target is
// started before the move goes to the VM's host.
#[test]
fn move_to_a_stopped_target_starts_it() {
    for shell in SHELLS {
        let fleet = Fleet::new();
        ok(&fleet.run(shell, &["list"]));
        fleet.forget_calls();
        fleet.stop("kvm-host-3");
        let out = fleet.run(shell, &["move", "web", "kvm-host-3"]);
        ok(&out);
        assert_eq!(
            fleet.aws_calls(),
            ["kvm-host-3 state", "kvm-host-3 start"],
            "{shell:?}"
        );
        assert_eq!(fleet.vms("kvm-host-3"), ["web"], "{shell:?}");
        assert_eq!(
            fleet.commands("kvm-host-2"),
            ["move web runner@kvm-host-3"],
            "{shell:?}"
        );
    }
}

// A running target answers, so AWS isn't asked.
#[test]
fn move_to_a_running_target_asks_aws_nothing() {
    let fleet = Fleet::new();
    fleet.aws_cli();
    ok(&fleet.run(SHELLS[0], &["list"]));
    ok(&fleet.run(SHELLS[0], &["move", "web", "kvm-host-3"]));
    assert!(fleet.aws_calls().is_empty());
}
