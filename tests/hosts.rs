// The laptop's `runner` (client/runner) with several hosts in
// ~/.config/runner/hosts, against a fake ssh on PATH that serves three fake
// hosts. Each host is a directory: `vms` lists its VMs, `free` its free
// memory, `calls` what its runner got, and `down` makes ssh time out on it.
// No network and no host.
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
// is the argument before the command. A host that's down times out, as
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
if [ -e "$dir/down" ]; then
  echo "ssh: connect to host ${host##*@} port 22: Connection timed out" >&2
  exit 255
fi
export HOST_DIR="$dir"
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
  if has "$name"; then echo "runner: a VM named $name already exists" >&2; exit 1; fi
  [ "$1" = create ] || cat >"$HOST_DIR/stdin"
  echo "$name" >>"$HOST_DIR/vms"
  echo "$name  gen 0  6 bytes"
  ;;
delete)
  has "$2" || no "$2"
  grep -vx -- "$2" "$HOST_DIR/vms" >"$HOST_DIR/vms.new"
  mv "$HOST_DIR/vms.new" "$HOST_DIR/vms"
  echo "deleted $2  gen 0  6 bytes"
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
    }

    fn down(&self, name: &str) {
        fs::write(self.dir.path().join("fleet").join(name).join("down"), "").unwrap();
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
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg(client("runner"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.home())
            .env("FLEET", self.dir.path().join("fleet"))
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
        ok(&fleet.run(shell, &["mcp"]));
        ok(&fleet.run(shell, &["unit"]));
        assert_eq!(fleet.calls("kvm-host"), ["mcp", "unit"], "{shell:?}");
        ok(&fleet.run(shell, &["list", "--host", "kvm-host-2"]));
        assert_eq!(fleet.calls("kvm-host-2"), ["list"], "{shell:?}");
        let mcp = &fleet.ssh_calls()[0];
        assert_eq!(
            mcp,
            &["-T", "-o", "BatchMode=yes", HOSTS[0], "runner mcp"],
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
        assert!(fleet.ssh_calls().is_empty(), "{shell:?}");
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
