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
// text), perms (the group's IpPermissions, which authorize and revoke set);
// key-fails and signed-out make those calls fail as AWS does.
const FAKE_AWS: &str = r#"{ for arg; do printf '%s\037' "$arg"; done; printf '\n'; } >>"$FAKE/calls"
log aws "$@"
fail() { echo "An error occurred ($1)" >&2; exit 254; }
case "$1 $2" in
"sts get-caller-identity") [ ! -e "$FAKE/signed-out" ] || fail ExpiredToken ;;
"ec2 describe-instances") cat "$FAKE/instances" 2>/dev/null || true ;;
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
"ec2 wait"|"ec2 stop-instances"|"ec2 start-instances") ;;
*) echo "fake aws: unexpected $*" >&2; exit 99 ;;
esac
"#;

// The last ssh's argv, NUL-split, in $FAKE/ssh. A command for a host with a
// directory in $FAKE/hosts runs there: in sh, as the user's home under it,
// with $HOSTBIN first on PATH and /etc, /var/lib/runner and ~runner moved
// under it. -n gives it no stdin, as ssh's does. Any other ssh only logs.
const FAKE_SSH: &str = r#"for arg; do printf '%s\0' "$arg"; done >"$FAKE/ssh"
log ssh "$@"
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
HOST=$H WHO=${H##*/} HOME=$home PATH="$HOSTBIN:$PATH" exec sh -c "$cmd"
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
// 50 GiB disk), mounted, packages, tailnet-name (set by tailscale up, to
// tailnet-gives if there is one), base, doctor (runner doctor's lines).
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
exec "$@"
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
    (
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
    ),
];

// Host tools that only log.
const LOGGERS: &[&str] = &["cargo", "rustup", "curl", "podman", "stty"];

const STUB_INSTALL: &str = r#"log install.sh "$@"
mkdir -p "$HOST/home/runner/.ssh"
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
        for (name, body) in HOST_TOOLS {
            script(&hostbin.join(name), body);
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
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(shell[0]);
        cmd.args(&shell[1..])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("client/aws.sh"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.home())
            .env("FAKE", self.dir.path().join("fake"))
            .env("HOSTBIN", self.dir.path().join("hostbin"))
            .env("STUBS", self.dir.path().join("stubs"))
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

    fn ssh_argv(&self) -> Vec<String> {
        let bytes = fs::read(self.dir.path().join("fake/ssh")).unwrap();
        text(&bytes)
            .split_terminator('\0')
            .map(str::to_string)
            .collect()
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
        ok(&aws.run(shell, &["stop"]));
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
    for cmd in ["up", "setup", "ip", "ssh", "open", "close", "stop", "start"] {
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
        let close = aws.at("close", |r| {
            ran(r, "laptop", "aws") && r[3] == "revoke-security-group-ingress"
        });
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
        assert_eq!(aws.call("revoke-security-group-ingress"), None);
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
        assert_eq!(aws.call("revoke-security-group-ingress"), None);
    }
}

#[test]
fn an_empty_copy_writes_nothing_and_fails() {
    // The source said it had one, then gave nothing.
    let (aws, host, from) = second_host();
    fs::write(from.dir.join("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN"), "").unwrap();
    from.has("etc/credstore/GH_TOKEN", "ghp_x\n");
    // test -s on the source passes only for a file with something in it, so
    // the empty Claude token isn't copied: it's asked for, and nothing typed.
    let err = fails(&aws.run_as(&["sh"], NEW, &["up"], ""));
    assert!(err.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{err}");
    assert_eq!(host.read("etc/credstore/CLAUDE_CODE_OAUTH_TOKEN"), None);
    assert_eq!(aws.call("revoke-security-group-ingress"), None);
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
        assert_eq!(aws.call("revoke-security-group-ingress"), None);
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
    for name in ["kvm host", "kvm;rm", "-x", "kvm_host"] {
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
