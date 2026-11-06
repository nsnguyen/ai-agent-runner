// client/aws.sh against a fake aws CLI, curl, rsync and ssh on PATH. The fake
// aws records each call and answers from files in its directory, so a test
// says what AWS has by making them. The fake ssh runs a command on a fake
// host, a directory under $FAKE/hosts named as ssh dials it, with fake host
// tools first on PATH and the host's paths under that directory. No network
// and no AWS account.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

// Linux's sh, and bash in POSIX mode, which is what macOS's /bin/sh is.
const SHELLS: &[&[&str]] = &[&["sh"], &["bash", "--posix"]];

const LAPTOP_IP: &str = "198.51.100.4";
const HOST_IP: &str = "203.0.113.7";
const PUBKEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFAKEFAKEFAKE nguyen@laptop";
const KEY_LINE: &str = "from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\",restrict,pty,port-forwarding ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFAKEFAKEFAKE nguyen@laptop";
const TOKEN: &str = "sk-ant-oat01-SECRET-TOKEN";

// Every fake, laptop's and host's, logs its argv to $FAKE/log: who ran it
// (laptop, or the host's directory), then each arg, split by US, a record
// ending in RS, since args can span lines.
const LOG: &str = r#"log() { { printf '%s\037' "${WHO:-laptop}" "$@"; printf '\036'; } >>"$FAKE/log"; }
"#;

// Each aws call's argv on a line, its args split by US, in $FAKE/calls. A file
// in $FAKE says what AWS has: has-key, has-sg, instances (describe-instances'
// text; instances.<name> answers for the instance named <name> instead, and
// key-of.<name> is the key pair it was launched with, when not <name>),
// perms (the group's IpPermissions, which authorize and revoke set);
// key-fails and signed-out make those calls fail as AWS does. stop-instances
// and start-instances set each instance they name stopped or running, start
// at the address in ip.<id>, else HOST_IP. shutdown.<id> is what an instance
// does at a power-off, else stop; modify-instance-attribute sets it stop.
const FAKE_AWS: &str = r#"{ for arg; do printf '%s\037' "$arg"; done; printf '\n'; } >>"$FAKE/calls"
log aws "$@"
fail() { echo "An error occurred ($1)" >&2; exit 254; }
case "$1 $2" in
"sts get-caller-identity") [ ! -e "$FAKE/signed-out" ] || fail ExpiredToken ;;
"ec2 describe-instances")
  n=$(printf '%s\n' "$@" | sed -n 's/^Name=tag:Name,Values=//p')
  k=$(printf '%s\n' "$@" | sed -n 's/^Name=key-name,Values=//p')
  if [ -n "$k" ] && [ -e "$FAKE/key-of.$n" ] && [ "$(cat "$FAKE/key-of.$n")" != "$k" ]; then :
  elif [ -n "$n" ] && [ -e "$FAKE/instances.$n" ]; then cat "$FAKE/instances.$n"
  else cat "$FAKE/instances" 2>/dev/null || true; fi ;;
"ec2 describe-key-pairs") [ -e "$FAKE/has-key" ] || fail InvalidKeyPair.NotFound ;;
"ec2 create-key-pair")
  [ ! -e "$FAKE/key-fails" ] || fail UnauthorizedOperation
  echo "-----BEGIN RSA PRIVATE KEY-----" ;;
"ec2 describe-security-groups")
  [ -e "$FAKE/has-sg" ] || fail InvalidGroup.NotFound
  cat "$FAKE/perms" 2>/dev/null || echo '[]' ;;
"ec2 create-security-group") touch "$FAKE/has-sg"; echo '{"GroupId": "sg-1"}' ;;
"ec2 run-instances") printf 'i-0new\trunning\t%s\n' "$HOST_IP" >"$FAKE/instances"; echo i-0new ;;
"ec2 authorize-security-group-ingress")
  while [ $# -gt 0 ]; do [ "$1" != --cidr ] || cidr=$2; shift; done
  printf '[{"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": "%s"}]}]\n' "$cidr" >"$FAKE/perms" ;;
"ec2 revoke-security-group-ingress") rm -f "$FAKE/perms" ;;
"ec2 stop-instances"|"ec2 start-instances")
  to="stopped	None"
  [ "$2" = stop-instances ] || to="running	$HOST_IP"
  ids=
  shift 2
  while [ $# -gt 0 ]; do
    case $1 in
    --instance-ids) shift; while [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; do ids="$ids $1"; shift; done ;;
    *) shift ;;
    esac
  done
  for f in "$FAKE"/instances "$FAKE"/instances.*; do
    [ -e "$f" ] || continue
    for id in $ids; do
      t=$to
      [ "$t" = "stopped	None" ] || [ ! -e "$FAKE/ip.$id" ] || t="running	$(cat "$FAKE/ip.$id")"
      sed "s/^$id	.*/$id	$t/" "$f" >"$f.new" && mv "$f.new" "$f"
    done
  done ;;
"ec2 describe-instance-attribute")
  while [ $# -gt 0 ]; do [ "$1" != --instance-id ] || id=$2; shift; done
  cat "$FAKE/shutdown.$id" 2>/dev/null || echo stop ;;
"ec2 modify-instance-attribute")
  while [ $# -gt 0 ]; do [ "$1" != --instance-id ] || id=$2; shift; done
  echo stop >"$FAKE/shutdown.$id" ;;
"ec2 wait") ;;
*) echo "fake aws: unexpected $*" >&2; exit 99 ;;
esac
"#;

// The last ssh's argv, NUL-split, in $FAKE/ssh. Each says on stderr that
// it added the host's key, as ssh's accept-new does the first time. A
// command for a host with a directory in $FAKE/hosts runs there: in sh, as
// the user's home under it, with $HOSTBIN, the host's usr/bin, and then only
// the system's, on PATH (not this machine's cargo), and /etc, /var/lib/runner
// and ~runner moved under it. -n gives it no stdin, as ssh's does. Any other
// ssh only logs.
const FAKE_SSH: &str = r#"for arg; do printf '%s\0' "$arg"; done >"$FAKE/ssh"
log ssh "$@"
echo "Warning: Permanently added 'somewhere' (ED25519) to the list of known hosts." >&2
none=
while [ $# -gt 0 ]; do
  case $1 in
  -i|-o|-p|-l|-F|-E|-J) shift 2 ;;
  -n) none=1; shift ;;
  -*) shift ;;
  *) break ;;
  esac
done
dest=$1
shift
[ $# -gt 0 ] && [ -d "$FAKE/hosts/${dest#*@}" ] || exit 0
H=$(cd -P "$FAKE/hosts/${dest#*@}" && pwd)
home=$H/home/${dest%@*}
mkdir -p "$home"
cmd=$(printf '%s' "$*" | sed -e "s#/etc/#$H/etc/#g" -e "s#/var/lib/runner#$H/var/lib/runner#g" -e "s#~runner/#$H/home/runner/#g")
cd "$home" || exit 255
[ -z "$none" ] || exec </dev/null
HOST=$H WHO=${H##*/} HOME=$home PATH="$HOSTBIN:$H/usr/bin:/usr/bin:/bin" exec sh -c "$cmd"
"#;

// rsync to a fake host makes its checkout there: stubs of host/install.sh and
// images/base/build.sh.
const FAKE_RSYNC: &str = r#"log rsync "$@"
for dest; do :; done
host=${dest%%:*}
[ -d "$FAKE/hosts/${host#*@}" ] || exit 0
co=$FAKE/hosts/${host#*@}/home/ubuntu/ai-agent-runner
mkdir -p "$co/host" "$co/images/base"
cp "$STUBS/install.sh" "$co/host/install.sh"
cp "$STUBS/build.sh" "$co/images/base/build.sh"
"#;

// The host's tools, each acting on files in $HOST: disk-fs (what's on the
// 50 GiB disk), mounted, packages, base, doctor (runner doctor's lines), and
// reads-nothing, which makes sudo cat print nothing. curl's installers put
// Rust in ~/.cargo and Tailscale in the host's usr/bin, from $PKGS.
const HOST_TOOLS: &[(&str, &str)] = &[
    (
        "sudo",
        r#"log sudo "$@"
while [ $# -gt 0 ]; do
  case $1 in
  -u) shift 2 ;;
  -*) shift ;;
  *=*) export "$1"; shift ;;
  *) break ;;
  esac
done
[ "$1" != cat ] || [ ! -e "$HOST/reads-nothing" ] || exit 0
exec "$@"
"#,
    ),
    (
        "curl",
        r#"log curl "$@"
case "$*" in
*sh.rustup.rs*) printf '. "%s/rustup-init"\n' "$PKGS" ;;
*tailscale.com/install.sh*) printf '. "%s/tailscale-install"\n' "$PKGS" ;;
esac
"#,
    ),
    (
        "lsblk",
        r#"log lsblk "$@"
printf '/dev/nvme0n1 42949672960 disk\n/dev/nvme1n1 53687091200 disk\n'
"#,
    ),
    (
        "blkid",
        r#"log blkid "$@"
fs=$(cat "$HOST/disk-fs" 2>/dev/null) || exit 2
case "$*" in
*UUID*) echo 0b5e0000-1111-4222-8333-444455556666 ;;
*) printf 'DEVNAME=/dev/nvme1n1\nUUID=0b5e0000-1111-4222-8333-444455556666\nTYPE=%s\n' "$fs" ;;
esac
"#,
    ),
    (
        "mkfs.xfs",
        r#"log mkfs.xfs "$@"
echo xfs >"$HOST/disk-fs"
"#,
    ),
    (
        "mount",
        r#"log mount "$@"
[ "$(cat "$HOST/disk-fs" 2>/dev/null)" = xfs ] || exit 32
touch "$HOST/mounted"
"#,
    ),
    (
        "findmnt",
        r#"log findmnt "$@"
[ -e "$HOST/mounted" ] || exit 1
cat "$HOST/disk-fs"
"#,
    ),
    (
        "dpkg",
        r#"log dpkg "$@"
grep -qx "$2" "$HOST/packages" 2>/dev/null
"#,
    ),
    (
        "apt-get",
        r#"log apt-get "$@"
case " $* " in *" install "*)
  for p; do case $p in -*|install) ;; *) echo "$p" >>"$HOST/packages" ;; esac; done ;;
esac
"#,
    ),
];

// What curl's installers install, and the installers.
const PKGS: &[(&str, &str)] = &[
    ("cargo", "log cargo \"$@\"\n"),
    ("rustup", "log rustup \"$@\"\n"),
    (
        "rustup-init",
        r#"mkdir -p "$HOME/.cargo/bin"
cp "$PKGS/cargo" "$PKGS/rustup" "$HOME/.cargo/bin/"
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >"$HOME/.cargo/env"
"#,
    ),
    (
        "tailscale-install",
        r#"mkdir -p "$HOST/usr/bin"
cp "$PKGS/tailscale" "$HOST/usr/bin/"
"#,
    ),
    // tailnet-name is set by tailscale up, to tailnet-gives if there is one.
    (
        "tailscale",
        r#"log tailscale "$@"
case $1 in
status)
  if [ -e "$HOST/tailnet-name" ]; then
    printf '{\n  "BackendState": "Running",\n  "Self": {\n    "HostName": "%s",\n    "DNSName": "%s.tail0fake.ts.net."\n  },\n  "Peer": {\n    "nodekey:1": {\n      "DNSName": "kvm-host.tail0fake.ts.net."\n    }\n  }\n}\n' "$(cat "$HOST/tailnet-name")" "$(cat "$HOST/tailnet-name")"
  else
    printf '{\n  "BackendState": "NeedsLogin",\n  "Self": {\n    "DNSName": ""\n  }\n}\n'
  fi ;;
up)
  echo "To authenticate, visit: https://login.tailscale.com/a/fake"
  cat "$HOST/tailnet-gives" >"$HOST/tailnet-name" 2>/dev/null || echo "$3" >"$HOST/tailnet-name" ;;
esac
"#,
    ),
];

const MORE_HOST_TOOLS: &[(&str, &str)] = &[(
    "runner",
    r#"log runner "$@"
case $1 in
status) [ -e "$HOST/base" ] ;;
import) cat >/dev/null; touch "$HOST/base" ;;
doctor)
  if [ -e "$HOST/doctor" ]; then cat "$HOST/doctor"; else
    printf 'ok    kvm          /dev/kvm opens read-write\nok    base         built from abc\n'
  fi ;;
esac
"#,
)];

// Host tools that only log.
const LOGGERS: &[&str] = &["podman", "stty"];

// It notes each run's RUNNER_HOST_IDLE in install-idle, and keeps it as
// install.sh does.
const STUB_INSTALL: &str = r#"log install.sh "$@"
mkdir -p "$HOST/home/runner/.ssh" "$HOST/etc/runner"
echo "${RUNNER_HOST_IDLE-unset}" >>"$HOST/install-idle"
[ -z "${RUNNER_HOST_IDLE-}" ] || echo "$RUNNER_HOST_IDLE" >"$HOST/etc/runner/host-idle"
"#;

const STUB_BUILD: &str = r#"log build.sh "$@"
out=$(dirname "$0")/out
mkdir -p "$out"
echo disk >"$out/base.ext4"
echo kernel >"$out/vmlinux"
"#;

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{LOG}{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{:?}: {}{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    );
    text(&out.stdout)
}

fn fails(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    text(&out.stderr)
}

struct Aws {
    dir: TempDir,
}

impl Aws {
    fn new() -> Aws {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["bin", "hostbin", "stubs", "home/.ssh", "fake/hosts"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let bin = dir.path().join("bin");
        script(&bin.join("aws"), FAKE_AWS);
        script(&bin.join("curl"), &format!("echo {LAPTOP_IP}\n"));
        script(&bin.join("ssh"), FAKE_SSH);
        script(&bin.join("rsync"), FAKE_RSYNC);
        let hostbin = dir.path().join("hostbin");
        for (name, body) in HOST_TOOLS.iter().chain(MORE_HOST_TOOLS) {
            script(&hostbin.join(name), body);
        }
        fs::create_dir(dir.path().join("pkgs")).unwrap();
        for (name, body) in PKGS {
            script(&dir.path().join("pkgs").join(name), body);
        }
        for name in LOGGERS {
            script(&hostbin.join(name), &format!("log {name} \"$@\"\n"));
        }
        script(&dir.path().join("stubs/install.sh"), STUB_INSTALL);
        script(&dir.path().join("stubs/build.sh"), STUB_BUILD);
        fs::write(
            dir.path().join("home/.ssh/id_ed25519.pub"),
            format!("{PUBKEY}\n"),
        )
        .unwrap();
        Aws { dir }
    }

    /// Makes AWS have `what`, as the fake's comment says.
    fn has(&self, what: &str, body: &str) {
        fs::write(self.dir.path().join("fake").join(what), body).unwrap();
    }

    /// The laptop's ~/.config/runner/hosts, as `runner hosts add` writes it.
    fn hosts(&self, lines: &str) {
        let dir = self.home().join(".config/runner");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hosts"), lines).unwrap();
    }

    fn key(&self) -> PathBuf {
        self.dir.path().join("home/.ssh/kvm-host.pem")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// A fresh host, reached at HOST_IP and, on the tailnet, as `name`.
    fn host(&self, name: &str) -> Host {
        let hosts = self.dir.path().join("fake/hosts");
        let dir = hosts.join(HOST_IP);
        fs::create_dir_all(dir.join("etc")).unwrap();
        std::os::unix::fs::symlink(&dir, hosts.join(name)).unwrap();
        Host { dir }
    }

    /// Another host, `name` on the tailnet, with its key on this laptop.
    fn source(&self, name: &str) -> Host {
        let dir = self.dir.path().join("fake/hosts").join(name);
        fs::create_dir_all(dir.join("etc/credstore")).unwrap();
        fs::write(self.home().join(format!(".ssh/{name}.pem")), "theirs\n").unwrap();
        Host { dir }
    }

    /// The script under `shell`, with the fakes first on PATH.
    fn cmd(&self, shell: &[&str], args: &[&str]) -> Command {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("client/aws.sh");
        self.cmd_of(shell, &script, args)
    }

    /// A copy of the script at `script`, as `cmd` runs the checkout's.
    fn cmd_of(&self, shell: &[&str], script: &Path, args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg(script)
            .args(args)
            .env("PATH", path)
            .env("HOME", self.home())
            .env("FAKE", self.dir.path().join("fake"))
            .env("HOSTBIN", self.dir.path().join("hostbin"))
            .env("STUBS", self.dir.path().join("stubs"))
            .env("PKGS", self.dir.path().join("pkgs"))
            .env("HOST_IP", HOST_IP)
            .env_remove("RUNNER_AWS_NAME")
            .env_remove("RUNNER_AWS_TYPE")
            .env_remove("RUNNER_AWS_KEY")
            .env_remove("RUNNER_AWS_TOKEN_FROM")
            .env_remove("WHO")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, shell: &[&str], args: &[&str]) -> Output {
        self.cmd(shell, args).output().unwrap()
    }

    /// The script as `name`, with `stdin` typed in.
    fn run_as(&self, shell: &[&str], name: &str, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .cmd(shell, args)
            .env("RUNNER_AWS_NAME", name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn calls(&self) -> Vec<Vec<String>> {
        let calls = fs::read_to_string(self.dir.path().join("fake/calls")).unwrap_or_default();
        calls
            .lines()
            .map(|line| line.split_terminator('\x1f').map(str::to_string).collect())
            .collect()
    }

    /// The args of the first `aws ec2 <op>` call, or None.
    fn call(&self, op: &str) -> Option<Vec<String>> {
        self.calls()
            .into_iter()
            .find(|c| c.len() > 1 && c[0] == "ec2" && c[1] == op)
    }

    /// What each `aws ec2 wait` waited for, in order.
    fn waits(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.len() > 2 && c[0] == "ec2" && c[1] == "wait")
            .map(|c| c[2].clone())
            .collect()
    }

    /// The ids each `aws ec2 wait <what>` waited on, a list per call.
    fn waited(&self, what: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.len() > 2 && c[0] == "ec2" && c[1] == "wait" && c[2] == what)
            .map(|c| {
                values(&c, "--instance-ids")
                    .iter()
                    .map(|v| v.to_string())
                    .collect()
            })
            .collect()
    }

    fn ssh_argv(&self) -> Vec<String> {
        let bytes = fs::read(self.dir.path().join("fake/ssh")).unwrap();
        text(&bytes)
            .split_terminator('\0')
            .map(str::to_string)
            .collect()
    }

    /// Whether SSH from the internet is open: the group has a rule.
    fn open(&self) -> bool {
        self.dir.path().join("fake/perms").exists()
    }

    /// Every fake's run, in order: who ran it, then its argv.
    fn log(&self) -> Vec<Vec<String>> {
        let log = fs::read_to_string(self.dir.path().join("fake/log")).unwrap_or_default();
        log.split_terminator('\x1e')
            .map(|r| r.split_terminator('\x1f').map(str::to_string).collect())
            .collect()
    }

    /// Where in the log the first run that `is` picks is.
    fn at(&self, what: &str, is: impl Fn(&[String]) -> bool) -> usize {
        self.log()
            .iter()
            .position(|r| is(r))
            .unwrap_or_else(|| panic!("no {what} in {:#?}", self.log()))
    }

    /// How many runs `is` picks.
    fn count(&self, is: impl Fn(&[String]) -> bool) -> usize {
        self.log().iter().filter(|r| is(r)).count()
    }
}

struct Host {
    dir: PathBuf,
}

impl Host {
    fn has(&self, path: &str, body: &str) {
        let path = self.dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A file there, with the host's paths as the host sees them.
    fn read(&self, path: &str) -> Option<String> {
        let body = fs::read_to_string(self.dir.join(path)).ok()?;
        let at = self.dir.canonicalize().unwrap();
        Some(body.replace(at.to_str().unwrap(), ""))
    }

    /// A Claude token there already, so the setup asks for none.
    fn has_token(&self) {
        self.has("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN", "tok\n");
    }
}

/// A run of `prog` by the host, or the laptop, `who`.
fn ran(r: &[String], who: &str, prog: &str) -> bool {
    r.len() > 1 && r[0] == who && r[1] == prog
}

/// A run whose args hold `part`.
fn says(r: &[String], part: &str) -> bool {
    r.iter().skip(1).any(|a| a.contains(part))
}

/// Every value after `flag`, up to the next flag: --instance-ids takes several.
fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    let at = args.iter().position(|a| a == flag).expect("no flag") + 1;
    args[at..]
        .iter()
        .take_while(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect()
}

/// The value after `flag` in `args`.
fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let at = args
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("no {flag} in {args:?}"));
    &args[at + 1]
}

#[test]
fn up_launches_a_host_with_kvm_and_a_disk_for_vms() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.host("kvm-host").has_token();
        let out = ok(&aws.run(shell, &["up"]));

        let run = aws.call("run-instances").expect("no run-instances");
        // Nested virtualization is what gives a non-metal instance /dev/kvm.
        assert_eq!(after(&run, "--cpu-options"), "NestedVirtualization=enabled");
        assert_eq!(after(&run, "--instance-type"), "m7i-flex.large");
        let image = after(&run, "--image-id");
        assert!(
            image.starts_with("resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/")
                && image.contains("/amd64/"),
            "{image}"
        );
        assert_eq!(after(&run, "--key-name"), "kvm-host");
        assert_eq!(after(&run, "--security-groups"), "kvm-host");
        let disks: serde_json::Value =
            serde_json::from_str(after(&run, "--block-device-mappings")).unwrap();
        let sizes: Vec<u64> = disks
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["Ebs"]["VolumeSize"].as_u64().unwrap())
            .collect();
        assert_eq!(sizes, [40, 50], "{disks}");
        // Ubuntu's root. Under another name the 40 GiB would be a third disk,
        // beside the image's own 8 GiB root.
        assert_eq!(disks[0]["DeviceName"], "/dev/sda1", "{disks}");
        // One hop: the VMs behind the host's NAT never reach the metadata
        // service, even without install.sh's drop of 169.254.0.0/16.
        assert_eq!(
            after(&run, "--metadata-options"),
            "HttpTokens=required,HttpPutResponseHopLimit=1"
        );
        assert!(
            after(&run, "--tag-specifications").contains("{Key=Name,Value=kvm-host}"),
            "{run:?}"
        );
        // It powers itself off when idle: stopped, not terminated, which
        // would lose it and its VMs.
        assert_eq!(
            after(&run, "--instance-initiated-shutdown-behavior"),
            "stop"
        );

        // The key pair's private key, kept for this user only.
        assert_eq!(
            fs::read_to_string(aws.key()).unwrap(),
            "-----BEGIN RSA PRIVATE KEY-----\n"
        );
        let mode = fs::metadata(aws.key()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");

        // SSH from this laptop's address only.
        let rule = aws
            .call("authorize-security-group-ingress")
            .expect("no rule");
        assert_eq!(after(&rule, "--group-name"), "kvm-host");
        assert_eq!(after(&rule, "--port"), "22");
        assert_eq!(after(&rule, "--cidr"), format!("{LAPTOP_IP}/32"));
        // Once: the setup doesn't open it again straight after.
        assert_eq!(
            aws.count(|r| ran(r, "laptop", "aws") && r[3] == "authorize-security-group-ingress"),
            1
        );

        // AWS lists a terminated instance for an hour after: it isn't the host.
        let find = aws.call("describe-instances").unwrap();
        let at = find.iter().position(|a| a == "--filters").unwrap();
        assert_eq!(
            find[at + 2],
            "Name=instance-state-name,Values=pending,running,stopping,stopped"
        );

        // Its status checks pass once it has booted, so the first ssh gets in.
        assert_eq!(aws.call("wait").expect("no wait")[2], "instance-status-ok");
        assert!(out.contains(HOST_IP), "{shell:?}: {out}");
        assert!(out.contains("runner hosts add runner@kvm-host"), "{out}");
    }
}

#[test]
fn up_finds_a_host_already_there() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.host("kvm-host").has_token();
        aws.has("has-sg", "");
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        let out = ok(&aws.run(shell, &["up"]));
        assert!(out.contains("i-0old") && out.contains(HOST_IP), "{out}");
        assert_eq!(aws.call("run-instances"), None);
        assert_eq!(aws.call("create-key-pair"), None);
    }
}

#[test]
fn up_on_a_stopped_host_says_to_start_it() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", "i-0old\tstopped\tNone\n");
        let out = ok(&aws.run(shell, &["up"]));
        assert!(
            out.contains("stopped") && out.contains("aws.sh start") && out.contains("aws.sh setup"),
            "{out}"
        );
        assert!(!out.contains("None"), "{out}");
        assert_eq!(aws.call("run-instances"), None);
    }
}

#[test]
fn start_on_a_stopping_host_waits_for_it_to_stop() {
    // Or AWS refuses the start: IncorrectInstanceState.
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\tstopping\t{HOST_IP}\n"));
        ok(&aws.run(shell, &["start"]));
        assert_eq!(aws.waits(), ["instance-stopped", "instance-status-ok"]);
        let ops: Vec<String> = aws.calls().into_iter().map(|c| c[1].clone()).collect();
        let at = |op: &str| ops.iter().position(|o| o == op).unwrap();
        assert!(at("wait") < at("start-instances"), "{ops:?}");
    }
}

#[test]
fn up_reuses_a_key_pair_and_a_group_aws_has() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.host("kvm-host").has_token();
        aws.has("has-key", "");
        aws.has("has-sg", "");
        fs::create_dir_all(aws.key().parent().unwrap()).unwrap();
        fs::write(aws.key(), "mine\n").unwrap();
        ok(&aws.run(shell, &["up"]));
        assert_eq!(aws.call("create-key-pair"), None);
        assert_eq!(aws.call("create-security-group"), None);
        assert_eq!(fs::read_to_string(aws.key()).unwrap(), "mine\n");
        assert!(aws.call("run-instances").is_some());
    }
}

#[test]
fn up_stops_when_aws_has_the_key_pair_but_not_this_laptop() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("has-key", "");
        let err = fails(&aws.run(shell, &["up"]));
        assert!(err.contains(&aws.key().display().to_string()), "{err}");
        assert_eq!(aws.call("run-instances"), None);
    }
}

#[test]
fn up_never_overwrites_a_key_file() {
    for shell in SHELLS {
        let aws = Aws::new();
        fs::create_dir_all(aws.key().parent().unwrap()).unwrap();
        fs::write(aws.key(), "someone else's\n").unwrap();
        let err = fails(&aws.run(shell, &["up"]));
        assert!(err.contains("RUNNER_AWS_KEY"), "{err}");
        assert_eq!(aws.call("create-key-pair"), None);
        assert_eq!(fs::read_to_string(aws.key()).unwrap(), "someone else's\n");
    }
}

#[test]
fn a_key_pair_aws_refuses_leaves_no_key_file() {
    // Or the next `up` would find a file and refuse to make the pair.
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("key-fails", "");
        fails(&aws.run(shell, &["up"]));
        let left: Vec<_> = fs::read_dir(aws.key().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "id_ed25519.pub")
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(aws.call("run-instances"), None);
    }
}

#[test]
fn open_lets_only_this_laptop_in() {
    // An address the laptop had before, a café's say, goes.
    let perms = r#"[{"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": "192.0.2.9/32"}]}]"#;
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("has-sg", "");
        aws.has("perms", &format!("{perms}\n"));
        ok(&aws.run(shell, &["open"]));
        let revoke = aws
            .call("revoke-security-group-ingress")
            .expect("no revoke");
        assert_eq!(after(&revoke, "--ip-permissions"), perms);
        let rule = aws
            .call("authorize-security-group-ingress")
            .expect("no rule");
        assert_eq!(after(&rule, "--cidr"), format!("{LAPTOP_IP}/32"));
        // Revoked first, or the new one would go too.
        let ops: Vec<String> = aws.calls().into_iter().map(|c| c[1].clone()).collect();
        let at = |op: &str| ops.iter().position(|o| o == op).unwrap();
        assert!(
            at("revoke-security-group-ingress") < at("authorize-security-group-ingress"),
            "{ops:?}"
        );
    }
}

#[test]
fn signed_out_says_how_to_sign_in() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("signed-out", "");
        let err = fails(&aws.run(shell, &["up"]));
        assert!(err.contains("aws login"), "{err}");
        assert_eq!(aws.calls().len(), 1, "{:?}", aws.calls());
    }
}

#[test]
fn ssh_logs_in_as_ubuntu_with_the_key() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        ok(&aws.run(shell, &["ssh", "ls", "-l", "/dev/kvm"]));
        let argv = aws.ssh_argv();
        assert_eq!(after(&argv, "-i"), aws.key().display().to_string());
        // Only that key: an agent offering six others first would use up
        // sshd's tries.
        assert!(
            argv.windows(2).any(|w| w == ["-o", "IdentitiesOnly=yes"]),
            "{argv:?}"
        );
        assert_eq!(
            argv[argv.len() - 4..],
            [
                format!("ubuntu@{HOST_IP}"),
                "ls".into(),
                "-l".into(),
                "/dev/kvm".into()
            ],
            "{argv:?}"
        );
    }
}

#[test]
fn ssh_goes_over_the_tailnet_once_ssh_is_closed() {
    // `close` let nothing in from the internet: the host is kvm-host there.
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        aws.has("has-sg", "");
        ok(&aws.run(shell, &["ssh", "uptime"]));
        let argv = aws.ssh_argv();
        assert_eq!(
            argv[argv.len() - 2..],
            ["ubuntu@kvm-host", "uptime"],
            "{argv:?}"
        );
        assert_eq!(after(&argv, "-i"), aws.key().display().to_string());
    }
}

#[test]
fn ssh_goes_to_the_public_address_while_ssh_is_open() {
    let perms = r#"[{"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": "198.51.100.4/32"}]}]"#;
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        aws.has("has-sg", "");
        aws.has("perms", &format!("{perms}\n"));
        ok(&aws.run(shell, &["ssh", "uptime"]));
        let argv = aws.ssh_argv();
        assert_eq!(
            argv[argv.len() - 2..],
            [format!("ubuntu@{HOST_IP}"), "uptime".into()],
            "{argv:?}"
        );
    }
}

#[test]
fn ip_prints_the_address() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        assert_eq!(ok(&aws.run(shell, &["ip"])), format!("{HOST_IP}\n"));
    }
}

#[test]
fn a_stopped_host_has_no_address() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", "i-0old\tstopped\tNone\n");
        let err = fails(&aws.run(shell, &["ip"]));
        assert!(err.contains("stopped") && err.contains("start"), "{err}");
    }
}

#[test]
fn no_host_says_how_to_make_one() {
    for shell in SHELLS {
        let aws = Aws::new();
        let err = fails(&aws.run(shell, &["ssh"]));
        assert!(err.contains("up"), "{err}");
    }
}

#[test]
fn two_hosts_with_one_name_is_an_error() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has(
            "instances",
            &format!("i-0a\trunning\t{HOST_IP}\ni-0b\tstopped\tNone\n"),
        );
        let err = fails(&aws.run(shell, &["stop"]));
        assert!(err.contains("i-0a") && err.contains("i-0b"), "{err}");
        assert_eq!(aws.call("stop-instances"), None);
    }
}

#[test]
fn stop_and_start() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        let out = ok(&aws.run(shell, &["stop"]));
        assert!(out.contains("Stopping kvm-host (i-0old)...\n"), "{out}");
        let stop = aws.call("stop-instances").expect("no stop-instances");
        assert_eq!(after(&stop, "--instance-ids"), "i-0old");
        // Stopped, not stopping, so a start straight after doesn't get
        // IncorrectInstanceState.
        assert_eq!(aws.waits(), ["instance-stopped"]);

        let out = ok(&aws.run(shell, &["start"]));
        let start = aws.call("start-instances").expect("no start-instances");
        assert_eq!(after(&start, "--instance-ids"), "i-0old");
        assert_eq!(aws.waits(), ["instance-stopped", "instance-status-ok"]);
        assert!(out.contains(HOST_IP), "{out}");
    }
}

#[test]
fn stop_and_start_act_on_every_listed_host() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("runner@kvm-host\nrunner@kvm-host-2\n");
        aws.has("instances.kvm-host", &format!("i-0a\trunning\t{HOST_IP}\n"));
        aws.has("instances.kvm-host-2", "i-0b\trunning\t192.0.2.8\n");
        aws.has("ip.i-0b", "192.0.2.8");
        let run = aws.run(shell, &["stop"]);
        let out = ok(&run);
        assert_eq!(text(&run.stderr), "", "nothing on stderr");
        assert!(
            out.contains("Stopping kvm-host (i-0a), kvm-host-2 (i-0b)...\n"),
            "{out}"
        );
        let stop = aws.call("stop-instances").expect("no stop-instances");
        assert_eq!(values(&stop, "--instance-ids"), ["i-0a", "i-0b"]);
        // One wait for both, so they stop side by side.
        assert_eq!(aws.waited("instance-stopped"), [["i-0a", "i-0b"]]);
        assert!(
            out.contains("kvm-host is stopped") && out.contains("kvm-host-2 is stopped"),
            "{out}"
        );

        let out = ok(&aws.run(shell, &["start"]));
        let start = aws.call("start-instances").expect("no start-instances");
        assert_eq!(values(&start, "--instance-ids"), ["i-0a", "i-0b"]);
        // One wait each: AWS's status wait sees only running instances, so a
        // wait for both would pass once one had booted.
        assert_eq!(aws.waited("instance-status-ok"), [["i-0a"], ["i-0b"]]);
        assert!(
            out.contains(&format!("kvm-host is up at {HOST_IP}."))
                && out.contains("kvm-host-2 is up at 192.0.2.8."),
            "{out}"
        );
    }
}

#[test]
fn runner_aws_name_stops_and_starts_only_that_host() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("runner@kvm-host\nrunner@kvm-host-2\n");
        aws.has("instances.kvm-host", &format!("i-0a\trunning\t{HOST_IP}\n"));
        aws.has(
            "instances.kvm-host-2",
            &format!("i-0b\trunning\t{HOST_IP}\n"),
        );
        ok(&aws.run_as(shell, "kvm-host-2", &["stop"], ""));
        let stop = aws.call("stop-instances").expect("no stop-instances");
        assert_eq!(values(&stop, "--instance-ids"), ["i-0b"]);
        ok(&aws.run_as(shell, "kvm-host-2", &["start"], ""));
        let start = aws.call("start-instances").expect("no start-instances");
        assert_eq!(values(&start, "--instance-ids"), ["i-0b"]);
    }
}

#[test]
fn a_listed_host_aws_sh_didnt_make_is_left_alone() {
    // No instance has its name, or its name is no instance's (a dotted one).
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("# mine\nrunner@kvm-host\n\nrunner@homelab\nrunner@box.example.com\nkvm-host\n");
        aws.has("instances.kvm-host", &format!("i-0a\trunning\t{HOST_IP}\n"));
        let out = aws.run(shell, &["stop"]);
        let err = text(&out.stderr);
        assert!(out.status.success(), "{err}");
        let stop = aws.call("stop-instances").expect("no stop-instances");
        // kvm-host listed twice is stopped once.
        assert_eq!(values(&stop, "--instance-ids"), ["i-0a"]);
        assert!(
            err.contains("homelab") && err.contains("box.example.com"),
            "{err}"
        );
    }
}

#[test]
fn a_listed_host_named_like_an_instance_aws_sh_didnt_make_is_left_alone() {
    // up launches with the key pair of the host's name; another instance
    // that only shares the name isn't one of these.
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("runner@kvm-host\nrunner@web\n");
        aws.has("instances.kvm-host", &format!("i-0a\trunning\t{HOST_IP}\n"));
        aws.has("instances.web", &format!("i-0w\trunning\t{HOST_IP}\n"));
        aws.has("key-of.web", "deploy");
        let out = aws.run(shell, &["stop"]);
        let err = text(&out.stderr);
        assert!(out.status.success(), "{err}");
        let stop = aws.call("stop-instances").expect("no stop-instances");
        assert_eq!(values(&stop, "--instance-ids"), ["i-0a"]);
        assert!(err.contains("web"), "{err}");
    }
}

#[test]
fn no_listed_host_with_an_instance_is_an_error() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("runner@homelab\n");
        let err = fails(&aws.run(shell, &["stop"]));
        assert!(err.contains("homelab"), "{err}");
        assert_eq!(aws.call("stop-instances"), None);
    }
}

#[test]
fn start_waits_only_for_the_stopping_hosts() {
    // AWS refuses to start a stopping instance, so those finish stopping
    // first; a stopped one is started with them.
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("runner@kvm-host\nrunner@kvm-host-2\n");
        aws.has(
            "instances.kvm-host",
            &format!("i-0a\tstopping\t{HOST_IP}\n"),
        );
        aws.has("instances.kvm-host-2", "i-0b\tstopped\tNone\n");
        let out = ok(&aws.run(shell, &["start"]));
        let calls = aws.calls();
        let wait = calls
            .iter()
            .find(|c| c.len() > 2 && c[1] == "wait" && c[2] == "instance-stopped")
            .expect("no wait for stopping");
        assert_eq!(values(wait, "--instance-ids"), ["i-0a"]);
        let start = aws.call("start-instances").expect("no start-instances");
        assert_eq!(values(&start, "--instance-ids"), ["i-0a", "i-0b"]);
        assert_eq!(
            aws.waits(),
            [
                "instance-stopped",
                "instance-status-ok",
                "instance-status-ok"
            ]
        );
        assert!(out.contains("kvm-host-2 is up at"), "{out}");
    }
}

/// The ids each `aws ec2 modify-instance-attribute` set to stop at a power-off.
fn set_to_stop(aws: &Aws) -> Vec<String> {
    aws.calls()
        .into_iter()
        .filter(|c| c.len() > 1 && c[1] == "modify-instance-attribute")
        .map(|c| {
            assert_eq!(
                after(&c, "--instance-initiated-shutdown-behavior"),
                "Value=stop"
            );
            after(&c, "--instance-id").to_string()
        })
        .collect()
}

#[test]
fn start_makes_a_host_that_would_terminate_stop_instead() {
    // A host launched before up launched them to stop would be terminated
    // when it powers itself off idle, and its VMs with it.
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", "i-0old\tstopped\tNone\n");
        aws.has("shutdown.i-0old", "terminate\n");
        let out = ok(&aws.run(shell, &["start"]));
        let asked = aws
            .call("describe-instance-attribute")
            .expect("no describe-instance-attribute");
        assert_eq!(after(&asked, "--instance-id"), "i-0old");
        assert_eq!(
            after(&asked, "--attribute"),
            "instanceInitiatedShutdownBehavior"
        );
        assert_eq!(set_to_stop(&aws), ["i-0old"]);
        let ops: Vec<String> = aws.calls().into_iter().map(|c| c[1].clone()).collect();
        let at = |op: &str| ops.iter().position(|o| o == op).unwrap();
        assert!(
            at("modify-instance-attribute") < at("start-instances"),
            "{ops:?}"
        );
        assert!(
            out.contains("kvm-host stops at a power-off now, rather than being terminated."),
            "{out}"
        );
    }
}

#[test]
fn start_leaves_a_host_that_stops_as_it_is() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", "i-0old\tstopped\tNone\n");
        let out = ok(&aws.run(shell, &["start"]));
        assert!(aws.call("describe-instance-attribute").is_some());
        assert_eq!(set_to_stop(&aws), Vec::<String>::new());
        assert!(!out.contains("power-off"), "{out}");
    }
}

#[test]
fn start_makes_only_the_listed_hosts_that_would_terminate_stop() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.hosts("runner@kvm-host\nrunner@kvm-host-2\n");
        aws.has("instances.kvm-host", "i-0a\tstopped\tNone\n");
        aws.has("instances.kvm-host-2", "i-0b\tstopped\tNone\n");
        aws.has("shutdown.i-0b", "terminate\n");
        let out = ok(&aws.run(shell, &["start"]));
        assert_eq!(set_to_stop(&aws), ["i-0b"]);
        assert!(out.contains("kvm-host-2 stops at a power-off"), "{out}");
        assert!(!out.contains("kvm-host stops"), "{out}");
    }
}

#[test]
fn state_prints_what_aws_has_the_host_as() {
    for shell in SHELLS {
        for state in ["pending", "running", "stopping", "stopped"] {
            let aws = Aws::new();
            // Always the one host, whatever's listed.
            aws.hosts("runner@kvm-host-2\nrunner@kvm-host-3\n");
            aws.has("instances", &format!("i-0old\t{state}\t{HOST_IP}\n"));
            assert_eq!(ok(&aws.run(shell, &["state"])), format!("{state}\n"));
            // Only an instance up made: its name and its key pair.
            let find = aws.call("describe-instances").unwrap();
            assert_eq!(
                values(&find, "--filters")[..2],
                [
                    "Name=tag:Name,Values=kvm-host",
                    "Name=key-name,Values=kvm-host"
                ]
            );
            assert_eq!(aws.calls().len(), 2, "{:?}", aws.calls());
        }
    }
}

#[test]
fn state_of_an_instance_up_didnt_make_is_none() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has(
            "instances.kvm-host-2",
            &format!("i-0b\trunning\t{HOST_IP}\n"),
        );
        aws.has("key-of.kvm-host-2", "deploy");
        let out = aws.run_as(shell, "kvm-host-2", &["state"], "");
        assert_eq!(ok(&out), "none\n");

        // Nor is there any instance of that name.
        let aws = Aws::new();
        assert_eq!(ok(&aws.run(shell, &["state"])), "none\n");
    }
}

#[test]
fn state_of_a_name_up_doesnt_give_is_none() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        let out = aws.run_as(shell, "box.example.com", &["state"], "");
        assert_eq!(ok(&out), "none\n");
        assert_eq!(text(&out.stderr), "");
        assert_eq!(aws.call("describe-instances"), None);
    }
}

#[test]
fn state_with_no_aws_cli_is_none() {
    // A laptop with no AWS stays quiet.
    for shell in SHELLS {
        let aws = Aws::new();
        // The fakes, but no aws, and none of this machine's, where a real
        // one may be: the shell by its full path, and PATH only the fakes.
        let bin = aws.dir.path().join("no-aws");
        fs::create_dir(&bin).unwrap();
        script(&bin.join("curl"), &format!("echo {LAPTOP_IP}\n"));
        script(&bin.join("ssh"), FAKE_SSH);
        script(&bin.join("rsync"), FAKE_RSYNC);
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|d| d.join(shell[0]))
            .find(|p| p.is_file())
            .unwrap();
        let mut full = vec![found.to_str().unwrap()];
        full.extend(&shell[1..]);
        let out = aws
            .cmd(&full, &["state"])
            .env("PATH", &bin)
            .output()
            .unwrap();
        assert_eq!(ok(&out), "none\n");
        assert_eq!(text(&out.stderr), "");
    }
}

#[test]
fn state_signed_out_says_how_to_sign_in() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("signed-out", "");
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        let out = aws.run(shell, &["state"]);
        let err = fails(&out);
        assert!(err.contains("aws login"), "{err}");
        assert_eq!(text(&out.stdout), "");
    }
}

#[test]
fn an_installed_copy_runs_no_up_or_setup() {
    // setup copies the checkout around aws.sh to the host: around a copy in
    // ~/.local/bin, that's ~/.local.
    for shell in SHELLS {
        let aws = Aws::new();
        let bin = aws.home().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let copy = bin.join("aws.sh");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("client/aws.sh"),
            &copy,
        )
        .unwrap();
        aws.host("kvm-host").has_token();
        aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
        for cmd in ["up", "setup"] {
            let err = fails(&aws.cmd_of(shell, &copy, &[cmd]).output().unwrap());
            assert!(
                err.contains("checkout") && err.contains("client/aws.sh up"),
                "{cmd}: {err}"
            );
            assert!(aws.calls().is_empty(), "{cmd}: {:?}", aws.calls());
            assert_eq!(aws.count(|r| ran(r, "laptop", "rsync")), 0);
        }
        // The rest work from it.
        let out = aws.cmd_of(shell, &copy, &["state"]).output().unwrap();
        assert_eq!(ok(&out), "running\n");
        let out = aws.cmd_of(shell, &copy, &["ip"]).output().unwrap();
        assert_eq!(ok(&out), format!("{HOST_IP}\n"));
    }
}

#[test]
fn close_takes_every_inbound_rule_off() {
    let perms = r#"[{"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": "198.51.100.4/32"}, {"CidrIp": "192.0.2.9/32"}]}]"#;
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("has-sg", "");
        aws.has("perms", &format!("{perms}\n"));
        ok(&aws.run(shell, &["close"]));
        let revoke = aws
            .call("revoke-security-group-ingress")
            .expect("no revoke");
        assert_eq!(after(&revoke, "--group-name"), "kvm-host");
        assert_eq!(after(&revoke, "--ip-permissions"), perms);
    }
}

#[test]
fn close_with_nothing_open_changes_nothing() {
    for shell in SHELLS {
        let aws = Aws::new();
        aws.has("has-sg", "");
        ok(&aws.run(shell, &["close"]));
        assert_eq!(aws.call("revoke-security-group-ingress"), None);
    }
}

#[test]
fn a_second_host_takes_a_name_of_its_own() {
    let aws = Aws::new();
    aws.host("kvm-2").has_token();
    let out = aws
        .cmd(&["sh"], &["up"])
        .env("RUNNER_AWS_NAME", "kvm-2")
        .output()
        .unwrap();
    ok(&out);
    let run = aws.call("run-instances").unwrap();
    assert_eq!(after(&run, "--key-name"), "kvm-2");
    assert_eq!(after(&run, "--security-groups"), "kvm-2");
    let filter = after(&aws.call("describe-instances").unwrap(), "--filters").to_string();
    assert_eq!(filter, "Name=tag:Name,Values=kvm-2");
    assert!(aws.dir.path().join("home/.ssh/kvm-2.pem").exists());
}

#[test]
fn no_command_prints_how_to_use_it() {
    let aws = Aws::new();
    let err = fails(&aws.run(&["sh"], &[]));
    for cmd in [
        "up", "setup", "ip", "ssh", "open", "close", "stop", "start", "state",
    ] {
        assert!(err.contains(cmd), "{cmd}: {err}");
    }
    assert!(aws.calls().is_empty(), "{:?}", aws.calls());
}

// The setup, as `up` runs it after the launch and `setup` runs it alone.

const NEW: &str = "kvm-host-2";

/// A launch of kvm-host-2, with kvm-host on the tailnet holding the tokens.
fn second_host() -> (Aws, Host, Host) {
    let aws = Aws::new();
    let host = aws.host(NEW);
    let from = aws.source("kvm-host");
    from.has(
        "etc/credstore/CLAUDE_CODE_OAUTH_TOKEN",
        &format!("{TOKEN}\n"),
    );
    (aws, host, from)
}

/// The host's write of credential `c`, or the key line, for counting.
fn writes(r: &[String], who: &str, what: &str) -> bool {
    ran(r, who, "sudo") && says(r, what) && (says(r, "cat >") || says(r, "head -n 1 >"))
}

#[test]
fn up_sets_the_host_up_in_order() {
    for shell in SHELLS {
        let (aws, host, _) = second_host();
        let out = ok(&aws.run_as(shell, NEW, &["up"], ""));
        let h = HOST_IP;

        let mkfs = aws.at("mkfs", |r| ran(r, h, "mkfs.xfs"));
        assert_eq!(aws.log()[mkfs][2..], ["-q", "/dev/nvme1n1"]);
        let rsync = aws.at("rsync", |r| ran(r, "laptop", "rsync"));
        let install = aws.at("install.sh", |r| ran(r, h, "install.sh"));
        let build = aws.at("base build", |r| ran(r, h, "build.sh"));
        let import = aws.at("import", |r| ran(r, h, "runner") && r[2] == "import");
        let tailscale = aws.at("tailscale up", |r| ran(r, h, "tailscale") && r[2] == "up");
        let key = aws.at("key line", |r| {
            ran(r, h, "sudo") && says(r, "tee") && says(r, "authorized_keys")
        });
        let token = aws.at("token", |r| writes(r, h, "CLAUDE_CODE_OAUTH_TOKEN"));
        let doctor = aws.at("doctor", |r| ran(r, h, "runner") && r[2] == "doctor");
        // The last revoke: setup's open revokes the laptop's old address too.
        let close = aws
            .log()
            .iter()
            .rposition(|r| ran(r, "laptop", "aws") && r[3] == "revoke-security-group-ingress")
            .unwrap();
        assert!(!aws.open());
        let order = [
            mkfs, rsync, install, build, import, tailscale, key, token, doctor, close,
        ];
        assert!(order.is_sorted(), "{shell:?}: {order:?}");

        // Over SSH as ubuntu, at its address while SSH is open.
        let first = aws.at("ssh", |r| ran(r, "laptop", "ssh"));
        assert!(
            aws.log()[first].contains(&format!("ubuntu@{HOST_IP}")),
            "{:?}",
            aws.log()[first]
        );
        assert!(
            says(
                &aws.log()[rsync],
                &format!("ubuntu@{HOST_IP}:ai-agent-runner/")
            ),
            "{:?}",
            aws.log()[rsync]
        );
        // The doctor, as runner over the tailnet, as the laptop will reach it.
        assert!(
            aws.log()[..doctor]
                .iter()
                .rev()
                .find(|r| ran(r, "laptop", "ssh"))
                .unwrap()
                .contains(&format!("runner@{NEW}")),
            "{:?}",
            aws.log()
        );
        assert_eq!(aws.log()[tailscale][2..], ["up", "--hostname", NEW]);

        let fstab = host.read("etc/fstab").unwrap();
        assert_eq!(
            fstab,
            "UUID=0b5e0000-1111-4222-8333-444455556666 /var/lib/runner xfs defaults,nofail 0 2\n"
        );
        assert_eq!(
            host.read("home/runner/.ssh/authorized_keys").unwrap(),
            format!("{KEY_LINE}\n")
        );
        assert_eq!(
            host.read("var/lib/runner/kernel/vmlinux").unwrap(),
            "kernel\n"
        );
        for pkg in ["build-essential", "podman", "e2fsprogs", "rsync"] {
            assert!(host.read("packages").unwrap().contains(pkg), "{pkg}");
        }
        // Rust before the build, Tailscale before it's brought up.
        let rust = aws.at("rustup", |r| ran(r, h, "curl") && says(r, "sh.rustup.rs"));
        let cargo = aws.at("cargo build", |r| ran(r, h, "cargo") && r[2] == "build");
        assert!(rust < rsync && cargo < install, "{rust} {cargo} {install}");
        let ts = aws.at("tailscale's install", |r| {
            ran(r, h, "curl") && says(r, "tailscale.com/install.sh")
        });
        assert!(ts < tailscale, "{ts} {tailscale}");
        assert!(
            out.contains(&format!("runner hosts add runner@{NEW}")),
            "{out}"
        );
    }
}

#[test]
fn a_rerun_redoes_nothing() {
    for shell in SHELLS {
        let (aws, host, _) = second_host();
        ok(&aws.run_as(shell, NEW, &["up"], ""));
        let out = ok(&aws.run_as(shell, NEW, &["up"], ""));
        let h = HOST_IP;
        assert_eq!(aws.count(|r| ran(r, h, "mkfs.xfs")), 1);
        assert_eq!(aws.count(|r| ran(r, h, "build.sh")), 1);
        assert_eq!(aws.count(|r| ran(r, h, "runner") && r[2] == "import"), 1);
        assert_eq!(aws.count(|r| ran(r, h, "tailscale") && r[2] == "up"), 1);
        assert_eq!(aws.count(|r| ran(r, h, "apt-get") && says(r, "install")), 1);
        assert_eq!(aws.count(|r| ran(r, h, "curl")), 2);
        assert_eq!(aws.count(|r| writes(r, h, "CLAUDE_CODE_OAUTH_TOKEN")), 1);
        assert_eq!(
            aws.count(|r| ran(r, "laptop", "aws") && r[3] == "run-instances"),
            1
        );
        assert_eq!(
            host.read("home/runner/.ssh/authorized_keys").unwrap(),
            format!("{KEY_LINE}\n")
        );
        assert_eq!(host.read("etc/fstab").unwrap().lines().count(), 1);
        // SSH is closed now, so the rerun goes over the tailnet.
        let last = aws
            .log()
            .into_iter()
            .rfind(|r| ran(r, "laptop", "ssh"))
            .unwrap();
        assert!(last.contains(&format!("runner@{NEW}")), "{last:?}");
        let rsync = aws
            .log()
            .into_iter()
            .rfind(|r| ran(r, "laptop", "rsync"))
            .unwrap();
        assert!(says(&rsync, &format!("ubuntu@{NEW}:")), "{rsync:?}");
        assert!(
            out.contains(&format!("runner hosts add runner@{NEW}")),
            "{out}"
        );
    }
}

// A host aws.sh made powers itself off after an hour idle (src/power.rs),
// unless it has an idle time already, which reruns keep.
#[test]
fn the_setup_sets_an_hour_idle_on_a_host_with_no_setting() {
    for shell in SHELLS {
        let (aws, host, _) = second_host();
        ok(&aws.run_as(shell, NEW, &["up"], ""));
        assert_eq!(host.read("etc/runner/host-idle").unwrap(), "1h\n");
        ok(&aws.run_as(shell, NEW, &["setup"], ""));
        assert_eq!(host.read("install-idle").unwrap(), "1h\nunset\n");

        let (aws, host, _) = second_host();
        host.has("etc/runner/host-idle", "2h\n");
        ok(&aws.run_as(shell, NEW, &["up"], ""));
        assert_eq!(host.read("install-idle").unwrap(), "unset\n");
        assert_eq!(host.read("etc/runner/host-idle").unwrap(), "2h\n");
    }
}

#[test]
fn a_disk_with_ext4_on_it_is_never_formatted() {
    for shell in SHELLS {
        let (aws, host, _) = second_host();
        host.has("disk-fs", "ext4\n");
        let err = fails(&aws.run_as(shell, NEW, &["up"], ""));
        assert!(
            err.contains("ext4") && err.contains("/dev/nvme1n1"),
            "{err}"
        );
        assert_eq!(aws.count(|r| ran(r, HOST_IP, "mkfs.xfs")), 0);
        assert_eq!(aws.count(|r| ran(r, "laptop", "rsync")), 0);
        assert_eq!(host.read("etc/fstab"), None);
        assert!(err.contains("aws.sh setup"), "{err}");
    }
}

#[test]
fn a_disk_already_xfs_is_mounted_not_formatted() {
    // A setup that stopped between mkfs and mount.
    let (aws, host, _) = second_host();
    host.has("disk-fs", "xfs\n");
    ok(&aws.run_as(&["sh"], NEW, &["up"], ""));
    assert_eq!(aws.count(|r| ran(r, HOST_IP, "mkfs.xfs")), 0);
    assert!(host.dir.join("mounted").exists());
}

#[test]
fn close_waits_for_an_all_ok_doctor() {
    for shell in SHELLS {
        let (aws, host, _) = second_host();
        host.has(
            "doctor",
            "ok    kvm          /dev/kvm opens read-write\nwarn  reflink      /var/lib/runner can't reflink\n",
        );
        let err = fails(&aws.run_as(shell, NEW, &["up"], ""));
        assert!(
            err.contains("reflink") && err.contains("can't reflink"),
            "{err}"
        );
        assert!(err.contains("open"), "{err}");
        assert!(aws.open(), "SSH was closed");
        assert!(aws.dir.path().join("fake/perms").exists());
    }
}

#[test]
fn the_tokens_come_from_the_source_host_through_one_pipe() {
    for shell in SHELLS {
        let (aws, host, from) = second_host();
        from.has("etc/credstore/GH_TOKEN", "ghp_SECRET_GH\n");
        ok(&aws.run_as(shell, NEW, &["up"], ""));

        let cred = |c: &str| format!("etc/credstore/{c}");
        assert_eq!(
            host.read(&cred("CLAUDE_CODE_OAUTH_TOKEN")).unwrap(),
            format!("{TOKEN}\n")
        );
        assert_eq!(host.read(&cred("GH_TOKEN")).unwrap(), "ghp_SECRET_GH\n");
        // The source has no API key, so none is written or asked for.
        assert_eq!(host.read(&cred("ANTHROPIC_API_KEY")), None);
        for c in ["CLAUDE_CODE_OAUTH_TOKEN", "GH_TOKEN"] {
            let mode = fs::metadata(host.dir.join(cred(c)))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{c}: {mode:o}");
            assert!(!host.dir.join(format!("{}.new", cred(c))).exists());
        }

        // In no argv a fake saw, and in no file on the laptop.
        let log = fs::read_to_string(aws.dir.path().join("fake/log")).unwrap();
        for secret in [TOKEN, "ghp_SECRET_GH"] {
            assert!(!log.contains(secret), "{secret} in an argv");
            for file in walk(&aws.home()) {
                let body = fs::read(&file).unwrap();
                assert!(
                    !text(&body).contains(secret),
                    "{secret} in {}",
                    file.display()
                );
            }
        }
        // Each host with its own key. The two ends of the pipe run at once,
        // so they're found by what they run.
        let pem = |n: &str| {
            aws.home()
                .join(format!(".ssh/{n}.pem"))
                .display()
                .to_string()
        };
        let read = aws.at("the source's read", |r| {
            ran(r, "laptop", "ssh") && says(r, "sudo cat /etc/credstore/CLAUDE_CODE_OAUTH_TOKEN")
        });
        let read = &aws.log()[read];
        assert_eq!(after(read, "-i"), pem("kvm-host"));
        assert!(read.contains(&"ubuntu@kvm-host".to_string()), "{read:?}");
        let write = aws.at("the write", |r| {
            ran(r, "laptop", "ssh") && says(r, "c=CLAUDE_CODE_OAUTH_TOKEN;")
        });
        let write = &aws.log()[write];
        assert_eq!(after(write, "-i"), pem("kvm-host-2"));
        assert!(write.contains(&format!("ubuntu@{HOST_IP}")), "{write:?}");
        // No stty: nothing was typed.
        assert_eq!(aws.count(|r| ran(r, HOST_IP, "stty")), 0);
    }
}

#[test]
fn with_no_source_the_token_is_typed_in_with_echo_off() {
    for shell in SHELLS {
        let aws = Aws::new();
        let host = aws.host(NEW);
        // No ~/.ssh/kvm-host.pem: no source.
        let out = aws.run_as(shell, NEW, &["up"], &format!("{TOKEN}\n"));
        let said = ok(&out);
        assert_eq!(
            host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN").unwrap(),
            format!("{TOKEN}\n")
        );
        assert!(!said.contains(TOKEN) && !text(&out.stderr).contains(TOKEN));
        let off = aws.at("stty -echo", |r| ran(r, HOST_IP, "stty") && r[2] == "-echo");
        let read = aws.at("the read", |r| {
            writes(r, HOST_IP, "CLAUDE_CODE_OAUTH_TOKEN")
        });
        let on = aws.at("stty echo", |r| ran(r, HOST_IP, "stty") && r[2] == "echo");
        assert!(off < read && read < on, "{off} {read} {on}");
        // On a terminal there, which stty needs.
        let ssh = aws.log()[..off]
            .iter()
            .rev()
            .find(|r| ran(r, "laptop", "ssh"))
            .unwrap()
            .clone();
        assert!(ssh.contains(&"-t".to_string()), "{ssh:?}");
        let log = fs::read_to_string(aws.dir.path().join("fake/log")).unwrap();
        assert!(!log.contains(TOKEN));
    }
}

#[test]
fn an_empty_token_writes_nothing_and_fails() {
    for typed in ["", "\n", "  \n"] {
        let aws = Aws::new();
        let host = aws.host(NEW);
        let err = fails(&aws.run_as(&["sh"], NEW, &["up"], typed));
        assert!(err.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{err}");
        assert_eq!(host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN"), None);
        assert_eq!(host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN.new"), None);
        assert!(aws.open(), "SSH was closed");
    }
}

#[test]
fn an_empty_copy_writes_nothing_and_fails() {
    // The source says it has the token, then its read gives nothing.
    for shell in SHELLS {
        let (aws, host, from) = second_host();
        from.has("reads-nothing", "");
        let err = fails(&aws.run_as(shell, NEW, &["up"], ""));
        assert!(err.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{err}");
        assert!(err.contains("from kvm-host"), "{err}");
        assert_eq!(
            aws.count(|r| ran(r, "kvm-host", "sudo") && r[2] == "cat"),
            1
        );
        assert_eq!(host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN"), None);
        assert_eq!(host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN.new"), None);
        assert_eq!(aws.count(|r| ran(r, HOST_IP, "stty")), 0);
        assert!(aws.open(), "SSH was closed");
    }
}

#[test]
fn an_empty_source_file_is_asked_for_instead() {
    // test -s on the source passes only for a file with something in it.
    let (aws, host, from) = second_host();
    fs::write(from.dir.join("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN"), "").unwrap();
    ok(&aws.run_as(&["sh"], NEW, &["up"], "typed\n"));
    assert_eq!(
        host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN").unwrap(),
        "typed\n"
    );
}

#[test]
fn a_tailnet_name_not_its_own_stops_before_close() {
    for shell in SHELLS {
        let (aws, host, _) = second_host();
        host.has("tailnet-gives", "kvm-host-2-1\n");
        let err = fails(&aws.run_as(shell, NEW, &["up"], ""));
        assert!(
            err.contains("kvm-host-2-1") && err.contains("emove"),
            "{err}"
        );
        assert!(aws.open(), "SSH was closed");
        assert_eq!(
            aws.count(|r| ran(r, HOST_IP, "runner") && r[2] == "doctor"),
            0
        );
    }
}

#[test]
fn setup_alone_sets_up_a_running_host() {
    let (aws, host, _) = second_host();
    aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
    aws.has("has-sg", "");
    aws.has(
        "perms",
        r#"[{"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": "198.51.100.4/32"}]}]"#,
    );
    ok(&aws.run_as(&["sh"], NEW, &["setup"], ""));
    assert_eq!(aws.call("run-instances"), None);
    assert_eq!(aws.call("create-key-pair"), None);
    assert!(host.dir.join("mounted").exists());
    assert!(aws.call("revoke-security-group-ingress").is_some());
}

#[test]
fn setup_lets_this_laptops_new_address_in_first() {
    // Up ran at a café; SSH isn't closed yet.
    let (aws, _, _) = second_host();
    aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
    aws.has("has-sg", "");
    let cafe = r#"[{"IpProtocol": "tcp", "FromPort": 22, "ToPort": 22, "IpRanges": [{"CidrIp": "192.0.2.9/32"}]}]"#;
    aws.has("perms", cafe);
    ok(&aws.run_as(&["sh"], NEW, &["setup"], ""));
    let rule = aws
        .call("authorize-security-group-ingress")
        .expect("no rule");
    assert_eq!(after(&rule, "--cidr"), format!("{LAPTOP_IP}/32"));
    let open = aws.at("the open", |r| {
        ran(r, "laptop", "aws") && r[3] == "authorize-security-group-ingress"
    });
    let first = aws.at("the first ssh", |r| ran(r, "laptop", "ssh"));
    assert!(open < first, "{open} {first}");
}

#[test]
fn setup_on_a_closed_host_opens_nothing() {
    // Over the tailnet, as a rerun after close goes.
    let (aws, host, _) = second_host();
    host.has_token();
    aws.has("instances", &format!("i-0old\trunning\t{HOST_IP}\n"));
    aws.has("has-sg", "");
    ok(&aws.run_as(&["sh"], NEW, &["setup"], ""));
    assert_eq!(aws.call("authorize-security-group-ingress"), None);
    let first = aws.at("the first ssh", |r| ran(r, "laptop", "ssh"));
    assert!(
        aws.log()[first].contains(&format!("ubuntu@{NEW}")),
        "{:?}",
        aws.log()[first]
    );
}

#[test]
fn setup_on_a_stopped_host_says_to_start_it() {
    let aws = Aws::new();
    aws.has("instances", "i-0old\tstopped\tNone\n");
    let err = fails(&aws.run(&["sh"], &["setup"]));
    assert!(
        err.contains("stopped") && err.contains("aws.sh start"),
        "{err}"
    );
}

#[test]
fn up_needs_the_laptops_key_before_it_launches() {
    let aws = Aws::new();
    fs::remove_file(aws.home().join(".ssh/id_ed25519.pub")).unwrap();
    let err = fails(&aws.run(&["sh"], &["up"]));
    assert!(
        err.contains("id_ed25519.pub") && err.contains("ssh-keygen"),
        "{err}"
    );
    assert_eq!(aws.call("run-instances"), None);
}

#[test]
fn a_name_the_tailnet_cant_take_is_refused() {
    for name in ["kvm host", "kvm;rm", "-x", "kvm_host", "Kvm-2"] {
        let aws = Aws::new();
        let out = aws
            .cmd(&["sh"], &["up"])
            .env("RUNNER_AWS_NAME", name)
            .output()
            .unwrap();
        let err = fails(&out);
        assert!(err.contains("RUNNER_AWS_NAME"), "{name}: {err}");
        assert!(aws.calls().is_empty(), "{:?}", aws.calls());
    }
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}
