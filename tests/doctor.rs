// `runner doctor` against a faked host: each check's verdict from what the
// host would answer. Only the reflink probe at the bottom touches the real
// filesystem.
use runner::doctor::{self, Host, Ran, Report, Status};
use runner::host;
use runner::net::DEFAULT_SUBNET;
use runner::store::Store;
use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// What `nft list table ip runner` prints for the rules install.sh writes.
const NAT: &str = "table ip runner {
\tchain postrouting {
\t\ttype nat hook postrouting priority srcnat; policy accept;
\t\tip saddr 172.16.0.0/16 ip daddr != 172.16.0.0/16 masquerade
\t}

\tchain forward {
\t\ttype filter hook forward priority filter; policy accept;
\t\tip saddr 172.16.0.0/16 ip daddr != 172.16.0.0/16 meta l4proto { tcp, udp } th dport 53 accept
\t\tip saddr 172.16.0.0/16 ip daddr { 10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16 } drop
\t}

\tchain input {
\t\ttype filter hook input priority filter; policy accept;
\t\tip saddr 172.16.0.0/16 ct state established,related accept
\t\tip saddr 172.16.0.0/16 drop
\t}
}
";

// What `nft list table ip6 runner` prints for them: VMs get no IPv6 through
// the host, and can't reach it over IPv6 either.
const NAT6: &str = "table ip6 runner {
\tchain input {
\t\ttype filter hook input priority filter; policy accept;
\t\tiifname \"fc*\" drop
\t}

\tchain forward {
\t\ttype filter hook forward priority filter; policy accept;
\t\tiifname \"fc*\" drop
\t}
}
";

// Rules as an install.sh from before the input chain wrote them, which
// let VMs reach the host's own services.
fn without_input(rules: &str) -> String {
    let start = rules.find("\tchain input {").unwrap();
    let end = start + rules[start..].find("\t}\n").unwrap() + "\t}\n".len();
    format!("{}{}", &rules[..start], &rules[end..])
}

// The file install.sh writes, without its ip6 table: VMs reach the host over
// IPv6, its link-local address on their tap.
fn without_ip6(rules: &str) -> String {
    let start = rules.find("table ip6 runner\n").unwrap();
    let end = start + rules[start..].find("\n}\n").unwrap() + "\n}\n".len();
    format!("{}{}", &rules[..start], &rules[end..])
}

const NAMES: [&str; 7] = [
    "kvm",
    "firecracker",
    "reflink",
    "kernel",
    "base",
    "nat",
    "unit",
];

struct Fake {
    files: HashMap<PathBuf, Result<String, ErrorKind>>,
    runs: HashMap<String, Result<Ran, ErrorKind>>,
    reflink: Result<bool, ErrorKind>,
}

// By full path: sshd gives the runner user a PATH without /usr/sbin.
fn version() -> String {
    format!("{} --version", host::FIRECRACKER)
}

fn list_nat() -> String {
    format!("{} list table ip runner", host::NFT)
}

fn list_nat6() -> String {
    format!("{} list table ip6 runner", host::NFT)
}

fn ran(ok: bool, stdout: &str, stderr: &str) -> Result<Ran, ErrorKind> {
    Ok(Ran {
        ok,
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    })
}

impl Fake {
    // A host where every check passes.
    fn healthy(root: &Path) -> Self {
        let pinned = format!("Firecracker {}\n\n", host::firecracker_version());
        let files = [
            ("/dev/kvm".into(), Ok(String::new())),
            (host::kernel(root), Ok("\x7fELF".to_string())),
            (host::IP_FORWARD.into(), Ok("1\n".to_string())),
            (host::UNIT_PATH.into(), Ok(host::UNIT.to_string())),
            (host::NAT_RULES.into(), Ok(host::nat_rules(&DEFAULT_SUBNET))),
        ];
        let runs = [
            (version(), ran(true, &pinned, "")),
            (
                "systemctl is-active runner-nat.service".to_string(),
                ran(true, "active\n", ""),
            ),
            (list_nat(), ran(true, NAT, "")),
            (list_nat6(), ran(true, NAT6, "")),
        ];
        Fake {
            files: files.into_iter().collect(),
            runs: runs.into_iter().collect(),
            reflink: Ok(true),
        }
    }

    fn file(mut self, path: impl Into<PathBuf>, got: Result<&str, ErrorKind>) -> Self {
        self.files.insert(path.into(), got.map(str::to_string));
        self
    }

    fn cmd(mut self, line: &str, got: Result<Ran, ErrorKind>) -> Self {
        self.runs.insert(line.to_string(), got);
        self
    }

    fn lookup(&self, path: &Path) -> io::Result<String> {
        let got = self.files.get(path).cloned();
        got.unwrap_or(Err(ErrorKind::NotFound))
            .map_err(io::Error::from)
    }
}

impl Host for Fake {
    fn open_rw(&self, path: &Path) -> io::Result<()> {
        self.lookup(path).map(drop)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        self.lookup(path)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.lookup(path).map(|s| s.len() as u64)
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran> {
        let line = [program].iter().chain(args).copied().collect::<Vec<_>>();
        let got = self.runs.get(&line.join(" ")).cloned();
        got.unwrap_or(Err(ErrorKind::NotFound))
            .map_err(io::Error::from)
    }

    fn reflinks(&self, _dir: &Path) -> io::Result<bool> {
        self.reflink.map_err(io::Error::from)
    }
}

fn with_base() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path().join("root"));
    store.import("base", &disk).unwrap();
    (dir, store)
}

// The status and detail of one check, by name.
fn verdict(report: &Report, name: &str) -> (Status, String) {
    let check = report.checks.iter().find(|c| c.name == name).unwrap();
    (check.status, check.detail.clone())
}

#[track_caller]
fn assert_verdict(fake: Fake, store: &Store, name: &str, want: Status, says: &str) -> Report {
    let report = doctor::check(&fake, store);
    let (status, detail) = verdict(&report, name);
    assert_eq!(status, want, "{name}: {detail}");
    assert!(detail.contains(says), "{name}: {detail:?} lacks {says:?}");
    // Only the check under test moved.
    for c in report.checks.iter().filter(|c| c.name != name) {
        assert_eq!(c.status, Status::Ok, "{}: {}", c.name, c.detail);
    }
    assert_eq!(report.ok, want != Status::Fail);
    report
}

#[test]
fn a_healthy_host_passes_every_check_in_order() {
    let (_dir, store) = with_base();
    let report = doctor::check(&Fake::healthy(store.root()), &store);
    let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, NAMES);
    for c in &report.checks {
        assert_eq!(c.status, Status::Ok, "{}: {}", c.name, c.detail);
    }
    assert!(report.ok);
    assert!(verdict(&report, "base").1.contains("gen 0"));
    let nat = verdict(&report, "nat").1;
    assert!(nat.contains("172.16.0.0/16"), "{nat}");
    assert!(nat.contains("VMs kept off the host"), "{nat}");
    let pinned = host::firecracker_version();
    assert!(verdict(&report, "firecracker").1.contains(pinned));
}

#[test]
fn kvm_fails_when_missing_or_not_writable() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let missing = fake().file("/dev/kvm", Err(ErrorKind::NotFound));
    assert_verdict(missing, &store, "kvm", Status::Fail, "no KVM");
    let denied = fake().file("/dev/kvm", Err(ErrorKind::PermissionDenied));
    assert_verdict(denied, &store, "kvm", Status::Fail, "kvm group");
}

#[test]
fn firecracker_fails_when_missing_or_not_the_pinned_version() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let line = &version();
    let missing = fake().cmd(line, Err(ErrorKind::NotFound));
    assert_verdict(missing, &store, "firecracker", Status::Fail, "install.sh");
    let old = fake().cmd(line, ran(true, "Firecracker v1.0.0\n\n", ""));
    let report = assert_verdict(old, &store, "firecracker", Status::Fail, "v1.0.0");
    let pinned = host::firecracker_version();
    assert!(verdict(&report, "firecracker").1.contains(pinned));
    let broken = fake().cmd(line, ran(false, "", "Illegal instruction"));
    assert_verdict(broken, &store, "firecracker", Status::Fail, "Illegal");
}

#[test]
fn no_reflink_is_a_warning_and_an_unusable_root_a_failure() {
    let (_dir, store) = with_base();
    let mut ext4 = Fake::healthy(store.root());
    ext4.reflink = Ok(false);
    assert_verdict(ext4, &store, "reflink", Status::Warn, "XFS or btrfs");
    let mut gone = Fake::healthy(store.root());
    gone.reflink = Err(ErrorKind::NotFound);
    let root = store.root().display().to_string();
    assert_verdict(gone, &store, "reflink", Status::Fail, &root);
}

#[test]
fn kernel_fails_when_missing_or_empty() {
    let (_dir, store) = with_base();
    let kernel = host::kernel(store.root());
    let fake = || Fake::healthy(store.root());
    let path = kernel.display().to_string();
    let missing = fake().file(&kernel, Err(ErrorKind::NotFound));
    assert_verdict(missing, &store, "kernel", Status::Fail, &path);
    let empty = fake().file(&kernel, Ok(""));
    assert_verdict(empty, &store, "kernel", Status::Fail, "empty");
}

#[test]
fn base_fails_until_one_is_imported() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let fake = Fake::healthy(store.root());
    assert_verdict(fake, &store, "base", Status::Fail, "runner import base");
}

#[test]
fn nat_needs_forwarding_the_service_and_the_rules() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let off = fake().file(host::IP_FORWARD, Ok("0\n"));
    assert_verdict(off, &store, "nat", Status::Fail, "forwarding is off");

    let unit = "systemctl is-active runner-nat.service";
    let stopped = fake().cmd(unit, ran(false, "inactive\n", ""));
    assert_verdict(stopped, &store, "nat", Status::Fail, "inactive");

    // A firewall reload flushed the ruleset under a still-active unit.
    let list = &list_nat();
    let flushed = fake().cmd(list, ran(false, "", "Error: No such file or directory"));
    assert_verdict(flushed, &store, "nat", Status::Fail, "systemctl restart");
    let no_rule = fake().cmd(list, ran(true, "table ip runner {\n}\n", ""));
    assert_verdict(no_rule, &store, "nat", Status::Fail, "systemctl restart");

    // As the runner user, nft can't list rules, so the unit has to do.
    let why = "Operation not permitted (you must be root)";
    let unprivileged = fake().cmd(list, ran(false, "", why));
    let report = assert_verdict(unprivileged, &store, "nat", Status::Ok, "root");
    assert!(verdict(&report, "nat").1.contains("VMs kept off the host"));
}

// An install.sh from before the input chain left VMs able to reach the
// host's sshd: VMs still run, so it's a warning, read from the loaded rules
// as root and from the file the unit loads as the runner user.
#[test]
fn nat_warns_when_nothing_keeps_vms_off_the_host() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let list = &list_nat();
    let old = fake().cmd(list, ran(true, &without_input(NAT), ""));
    let report = assert_verdict(old, &store, "nat", Status::Warn, "off the host");
    assert!(verdict(&report, "nat").1.contains("rerun host/install.sh"));

    let why = "Operation not permitted (you must be root)";
    let unprivileged = || fake().cmd(list, ran(false, "", why));
    let file = without_input(&host::nat_rules(&DEFAULT_SUBNET));
    let old = unprivileged().file(host::NAT_RULES, Ok(&file));
    let report = assert_verdict(old, &store, "nat", Status::Warn, "off the host");
    assert!(verdict(&report, "nat").1.contains(host::NAT_RULES));
    // No file to read: the active unit is all there is to go on.
    let unread = unprivileged().file(host::NAT_RULES, Err(ErrorKind::PermissionDenied));
    assert_verdict(unread, &store, "nat", Status::Ok, "root");

    // Without the ip6 table, a VM reaches the host over IPv6.
    let list6 = &list_nat6();
    let gone = "Error: No such file or directory";
    let no_ip6 = fake().cmd(list6, ran(false, "", gone));
    assert_verdict(no_ip6, &store, "nat", Status::Warn, "off the host");
    let file = without_ip6(&host::nat_rules(&DEFAULT_SUBNET));
    assert!(!file.contains("table ip6"), "{file}");
    let old = unprivileged().file(host::NAT_RULES, Ok(&file));
    assert_verdict(old, &store, "nat", Status::Warn, "off the host");
    let open6 = NAT6.replace("hook input", "hook output");
    let open6 = fake().cmd(list6, ran(true, &open6, ""));
    assert_verdict(open6, &store, "nat", Status::Warn, "off the host");

    // A drop in another chain, or an input chain that only accepts, isn't it.
    let accepts = NAT.replace("\t\tip saddr 172.16.0.0/16 drop\n", "");
    let open = fake().cmd(list, ran(true, &accepts, ""));
    assert_verdict(open, &store, "nat", Status::Warn, "off the host");
    let forward = NAT.replace("hook input", "hook forward");
    let elsewhere = fake().cmd(list, ran(true, &forward, ""));
    assert_verdict(elsewhere, &store, "nat", Status::Warn, "off the host");

    // Along with a firewall that drops forwarded traffic, it says both.
    let docker = "systemctl is-active docker.service";
    let both = fake()
        .cmd(list, ran(true, &without_input(NAT), ""))
        .cmd(docker, ran(true, "active\n", ""));
    let report = assert_verdict(both, &store, "nat", Status::Warn, "off the host");
    assert!(verdict(&report, "nat").1.contains("docker.service"));
}

// Both set forwarding to drop by default, so the VMs lose the internet while
// the rules look fine.
#[test]
fn nat_warns_when_docker_or_firewalld_runs() {
    let (_dir, store) = with_base();
    for unit in ["docker.service", "firewalld.service"] {
        let line = format!("systemctl is-active {unit}");
        let fake = Fake::healthy(store.root()).cmd(&line, ran(true, "active\n", ""));
        let report = assert_verdict(fake, &store, "nat", Status::Warn, unit);
        assert!(verdict(&report, "nat").1.contains("172.16.0.0/16"));
    }
}

#[test]
fn unit_fails_when_missing_or_stale() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let missing = fake().file(host::UNIT_PATH, Err(ErrorKind::NotFound));
    assert_verdict(missing, &store, "unit", Status::Fail, "isn't installed");
    let stale = fake().file(host::UNIT_PATH, Ok("[Service]\nExecStart=/bin/true\n"));
    assert_verdict(stale, &store, "unit", Status::Fail, "rerun host/install.sh");
}

#[test]
fn the_pin_is_a_release_version() {
    let v = host::firecracker_version();
    let parts: Vec<&str> = v.strip_prefix('v').unwrap().split('.').collect();
    assert_eq!(parts.len(), 3, "{v}");
    assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()), "{v}");
}

// For real, on whatever filesystem holds this session's temp directory: ext4
// can't reflink and btrfs always can. XFS depends on how it was made, so there
// the probe only has to answer.
#[test]
fn reflink_probe_matches_this_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let got = doctor::reflinks(dir.path()).unwrap();
    let out = Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(dir.path())
        .output()
        .unwrap();
    let fs_type = String::from_utf8_lossy(&out.stdout).trim().to_string();
    eprintln!("{fs_type}: reflink {got}");
    match fs_type.as_str() {
        "ext2/ext3" | "tmpfs" => assert!(!got, "{fs_type}"),
        "btrfs" => assert!(got, "{fs_type}"),
        _ => {}
    }
    let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert!(left.is_empty(), "the probe left {left:?}");
}

#[test]
fn reflink_probe_fails_on_a_missing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let err = doctor::reflinks(&dir.path().join("nope")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}
