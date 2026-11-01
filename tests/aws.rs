// client/aws.sh against a fake aws CLI, curl and ssh on PATH. The fake aws
// records each call and answers from files in its directory, so a test says
// what AWS has by making them. No network and no AWS account.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

// Linux's sh, and bash in POSIX mode, which is what macOS's /bin/sh is.
const SHELLS: &[&[&str]] = &[&["sh"], &["bash", "--posix"]];

const LAPTOP_IP: &str = "198.51.100.4";
const HOST_IP: &str = "203.0.113.7";

// Each call's argv on a line, its args split by US. A file in $FAKE says
// what AWS has: has-key, has-sg, instances (describe-instances' text), perms
// (the group's IpPermissions); key-fails and signed-out make those calls fail
// as AWS does.
const FAKE_AWS: &str = r#"{ for arg; do printf '%s\037' "$arg"; done; printf '\n'; } >>"$FAKE/calls"
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
"ec2 wait"|"ec2 stop-instances"|"ec2 start-instances"|"ec2 authorize-security-group-ingress"|"ec2 revoke-security-group-ingress") ;;
*) echo "fake aws: unexpected $*" >&2; exit 99 ;;
esac
"#;

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
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
    assert!(!out.status.success(), "{}", text(&out.stdout));
    text(&out.stderr)
}

struct Aws {
    dir: TempDir,
}

impl Aws {
    fn new() -> Aws {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["bin", "home", "fake"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let bin = dir.path().join("bin");
        script(&bin.join("aws"), FAKE_AWS);
        script(&bin.join("curl"), &format!("echo {LAPTOP_IP}\n"));
        script(
            &bin.join("ssh"),
            r#"for arg; do printf '%s\0' "$arg"; done >"$FAKE/ssh""#,
        );
        Aws { dir }
    }

    /// Makes AWS have `what`, as the fake's comment says.
    fn has(&self, what: &str, body: &str) {
        fs::write(self.dir.path().join("fake").join(what), body).unwrap();
    }

    fn key(&self) -> PathBuf {
        self.dir.path().join("home/.ssh/kvm-host.pem")
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
            .env("HOME", self.dir.path().join("home"))
            .env("FAKE", self.dir.path().join("fake"))
            .env("HOST_IP", HOST_IP)
            .env_remove("RUNNER_AWS_NAME")
            .env_remove("RUNNER_AWS_TYPE")
            .env_remove("RUNNER_AWS_KEY")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, shell: &[&str], args: &[&str]) -> Output {
        self.cmd(shell, args).output().unwrap()
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
        assert!(out.contains("ssh ls -l /dev/kvm"), "{out}");
    }
}

#[test]
fn up_finds_a_host_already_there() {
    for shell in SHELLS {
        let aws = Aws::new();
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
            out.contains("stopped") && out.contains("aws.sh start"),
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
    for cmd in ["up", "ip", "ssh", "open", "close", "stop", "start"] {
        assert!(err.contains(cmd), "{cmd}: {err}");
    }
    assert!(aws.calls().is_empty(), "{:?}", aws.calls());
}
