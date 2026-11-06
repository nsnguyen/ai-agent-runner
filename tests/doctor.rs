// `runner doctor` against a faked host: each check's verdict from what the
// host would answer. Only the reflink probe at the bottom touches the real
// filesystem.
use runner::doctor::{self, Host, Ran, Report, Status};
use runner::host;
use runner::net::DEFAULT_SUBNET;
use runner::power;
use runner::store::Store;
use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// What `nft list table ip runner` prints for the rules install.sh writes.
const NAT: &str = "table ip runner {
\tset listed {
\t\ttype ifname
\t}

\tset reach {
\t\ttype ifname . ipv4_addr
\t\tflags timeout
\t}

\tchain postrouting {
\t\ttype nat hook postrouting priority srcnat; policy accept;
\t\tip saddr 172.16.0.0/16 ip daddr != 172.16.0.0/16 masquerade
\t}

\tchain forward {
\t\ttype filter hook forward priority filter; policy accept;
\t\tiifname \"fc*\" ip saddr != 172.16.0.0/16 drop
\t\tiifname \"fc*\" iifname != @listed ip daddr != 172.16.0.0/16 meta l4proto { tcp, udp } th dport 53 accept
\t\tiifname \"fc*\" ip daddr { 10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16 } drop
\t\tiifname @listed jump listed
\t}

\tchain listed {
\t\tct state established,related accept
\t\tiifname . ip daddr @reach accept
\t\tdrop
\t}

\tchain input {
\t\ttype filter hook input priority filter; policy accept;
\t\tiifname \"fc*\" ip saddr != 172.16.0.0/16 drop
\t\tiifname \"fc*\" ct state established,related accept
\t\tiifname \"fc*\" fib daddr . iif type local meta l4proto { tcp, udp } th dport 53 socket wildcard 0 accept
\t\tiifname \"fc*\" drop
\t}
}
";

// The ip input chain's rules as install.sh writes them, port 53 to a tap's
// own address for the VM's DNS included, and as it wrote them before, by
// source address, which let in a VM that forged its source.
const INPUT: &str = "\t\tiifname \"fc*\" ip saddr != 172.16.0.0/16 drop
\t\tiifname \"fc*\" ct state established,related accept
\t\tiifname \"fc*\" fib daddr . iif type local meta l4proto { tcp, udp } th dport 53 socket wildcard 0 accept
\t\tiifname \"fc*\" drop
";
const INPUT_BY_SOURCE: &str = "\t\tip saddr 172.16.0.0/16 ct state established,related accept
\t\tip saddr 172.16.0.0/16 drop
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

// Rules as an install.sh from before lists of names wrote them, loaded or
// in the file: no sets for boot to put a listed VM's tap and addresses in,
// and nothing that sends a listed tap's packets to be held to them.
fn without_lists(rules: &str) -> String {
    let mut kept = String::new();
    let mut skipping = false;
    for line in rules.split_inclusive('\n') {
        let words = line.trim();
        if words.starts_with("set ") || words == "chain listed {" {
            skipping = true;
        }
        if !skipping && words != "iifname @listed jump listed" {
            kept += &line.replace(" iifname != @listed", "");
        }
        if skipping && words == "}" {
            skipping = false;
        }
    }
    kept
}

// The file install.sh writes, without its ip6 table: VMs reach the host over
// IPv6, its link-local address on their tap.
fn without_ip6(rules: &str) -> String {
    let start = rules.find("table ip6 runner\n").unwrap();
    let end = start + rules[start..].find("\n}\n").unwrap() + "\n}\n".len();
    format!("{}{}", &rules[..start], &rules[end..])
}

const NAMES: [&str; 10] = [
    "kvm",
    "firecracker",
    "e2fsprogs",
    "reflink",
    "kernel",
    "base",
    "nat",
    "unit",
    "key",
    "host-idle",
];

// The faked user's home, where its SSH key is.
const HOME: &str = "/home/runner";

struct Fake {
    files: HashMap<PathBuf, Result<String, ErrorKind>>,
    runs: HashMap<String, Result<Ran, ErrorKind>>,
    reflink: Result<bool, ErrorKind>,
    home: Result<PathBuf, ErrorKind>,
}

// By full path: sshd gives the runner user a PATH without /usr/sbin.
fn version() -> String {
    format!("{} --version", host::FIRECRACKER)
}

fn key() -> String {
    format!("{HOME}/.ssh/id_ed25519")
}

// What `runner resize` grows a disk with, by full path as debugfs is.
fn e2fsck() -> String {
    format!("{} -V", host::E2FSCK)
}

fn resize2fs() -> String {
    host::RESIZE2FS.to_string()
}

// An ed25519 public key's base64: its type's name first.
const ED25519: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIK3nBk4m9Oe5Tq2xsVdN2xTqgZrW6Wl4y8m3m6d2r5Xq";

fn public_key() -> String {
    format!("{}.pub", key())
}

fn list_nat() -> String {
    format!("{} list table ip runner", host::NFT)
}

fn list_nat6() -> String {
    format!("{} list table ip6 runner", host::NFT)
}

fn timer() -> String {
    format!("systemctl is-active {}", host::IDLE_TIMER)
}

// Polkit's answer for this process, which runs the checks.
fn pkcheck() -> String {
    let pid = std::process::id();
    format!("pkcheck --action-id {} --process {pid}", power::POWER_OFF)
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
            (
                key().into(),
                Ok("-----BEGIN OPENSSH PRIVATE KEY-----\n".to_string()),
            ),
            (
                public_key().into(),
                Ok(format!("ssh-ed25519 {ED25519} runner@host\n")),
            ),
            (host::HOST_IDLE.into(), Ok("1h\n".to_string())),
            (power::busy_lock(root), Ok(String::new())),
        ];
        let runs = [
            (version(), ran(true, &pinned, "")),
            (e2fsck(), ran(true, "", "e2fsck 1.47.0 (5-Feb-2023)\n")),
            (resize2fs(), ran(false, "", "resize2fs 1.47.0 (5-Feb-2023)\nUsage: ...\n")),
            (
                "systemctl is-active runner-nat.service".to_string(),
                ran(true, "active\n", ""),
            ),
            (list_nat(), ran(true, NAT, "")),
            (list_nat6(), ran(true, NAT6, "")),
            (timer(), ran(true, "active\n", "")),
            (pkcheck(), ran(true, "", "")),
        ];
        Fake {
            files: files.into_iter().collect(),
            runs: runs.into_iter().collect(),
            reflink: Ok(true),
            home: Ok(HOME.into()),
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

    fn home(&self) -> io::Result<PathBuf> {
        self.home.clone().map_err(io::Error::from)
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

    // The range comes from the masquerade rule, wherever nft lists it: the
    // filters name it after a !=.
    let (head, filters) = NAT.split_at(NAT.find("\tchain forward").unwrap());
    let postrouting = head.strip_prefix("table ip runner {\n").unwrap();
    let filters = filters.strip_suffix("}\n").unwrap();
    let last = format!("table ip runner {{\n{filters}\n{postrouting}}}\n");
    let fake = Fake::healthy(store.root()).cmd(&list_nat(), ran(true, &last, ""));
    let nat = verdict(&doctor::check(&fake, &store), "nat");
    assert_eq!(nat.0, Status::Ok, "{}", nat.1);
    assert!(nat.1.contains("masquerading 172.16.0.0/16"), "{}", nat.1);
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

// VMs run without them; only resize and create --disk need them.
#[test]
fn e2fsprogs_warns_when_a_disk_can_t_grow() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let report = doctor::check(&fake(), &store);
    let detail = verdict(&report, "e2fsprogs").1;
    assert!(detail.contains("e2fsck 1.47.0"), "{detail}");
    let missing = fake().cmd(&e2fsck(), Err(ErrorKind::NotFound));
    assert_verdict(missing, &store, "e2fsprogs", Status::Warn, host::E2FSCK);
    let missing = fake().cmd(&resize2fs(), Err(ErrorKind::NotFound));
    let report = assert_verdict(missing, &store, "e2fsprogs", Status::Warn, host::RESIZE2FS);
    assert!(verdict(&report, "e2fsprogs").1.contains("install.sh"));
    let denied = fake().cmd(&e2fsck(), Err(ErrorKind::PermissionDenied));
    assert_verdict(denied, &store, "e2fsprogs", Status::Warn, host::E2FSCK);
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

// An ext4 disk made with mkfs.ext4 -d (e2fsprogs, no root), with `record`
// as its /.runner/build, or none.
fn ext4(dir: &Path, record: Option<&str>) -> PathBuf {
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join(".runner")).unwrap();
    if let Some(record) = record {
        fs::write(tree.join(".runner/build"), record).unwrap();
    }
    let disk = dir.join("base.ext4");
    let out = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(&tree)
        .arg(&disk)
        .arg("16M")
        .output()
        .expect("mkfs.ext4 from e2fsprogs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    disk
}

#[test]
fn base_says_what_it_was_built_from_or_that_it_is_unknown() {
    let print = "3f2a4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819";
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root"));
    let record = format!("fingerprint={print}\ncommit=42f0b7d1e2a3\n");
    store
        .import("base", &ext4(dir.path(), Some(&record)))
        .unwrap();
    let fake = Fake::healthy(store.root());
    let report = assert_verdict(fake, &store, "base", Status::Ok, print);
    let detail = verdict(&report, "base").1;
    assert!(detail.contains("at commit 42f0b7d1e2a3"), "{detail}");

    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("root"));
    store.import("base", &ext4(dir.path(), None)).unwrap();
    let fake = Fake::healthy(store.root());
    assert_verdict(fake, &store, "base", Status::Ok, "build unknown");
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
    let accepts = NAT.replace("\t\tiifname \"fc*\" drop\n", "");
    assert!(
        accepts.contains("ip saddr != 172.16.0.0/16 drop"),
        "{accepts}"
    );
    let open = fake().cmd(list, ran(true, &accepts, ""));
    assert_verdict(open, &store, "nat", Status::Warn, "off the host");
    let forward = NAT.replace("hook input", "hook forward");
    let elsewhere = fake().cmd(list, ran(true, &forward, ""));
    assert_verdict(elsewhere, &store, "nat", Status::Warn, "off the host");

    // Rules that go by source address let in a VM that forges one, loaded
    // or in the file.
    assert!(NAT.contains(INPUT));
    let by_source = NAT.replace(INPUT, INPUT_BY_SOURCE);
    let old = fake().cmd(list, ran(true, &by_source, ""));
    assert_verdict(old, &store, "nat", Status::Warn, "off the host");
    let file = host::nat_rules(&DEFAULT_SUBNET);
    assert!(file.contains(INPUT), "{file}");
    let old = unprivileged().file(host::NAT_RULES, Ok(&file.replace(INPUT, INPUT_BY_SOURCE)));
    assert_verdict(old, &store, "nat", Status::Warn, "off the host");
    // Nor is a drop on the taps that lets a forged reply in first.
    let forged = "\t\tiifname \"fc*\" ip saddr != 172.16.0.0/16 drop\n";
    let forged_in = NAT.replace(INPUT, &INPUT.replace(forged, ""));
    let forged_in = fake().cmd(list, ran(true, &forged_in, ""));
    assert_verdict(forged_in, &store, "nat", Status::Warn, "off the host");
    let late = INPUT
        .replace(forged, "")
        .replace("accept\n", &format!("accept\n{forged}"));
    assert!(late.ends_with("drop\n\t\tiifname \"fc*\" drop\n"), "{late}");
    let late = fake().cmd(list, ran(true, &NAT.replace(INPUT, &late), ""));
    assert_verdict(late, &store, "nat", Status::Warn, "off the host");

    // Along with a firewall that drops forwarded traffic, it says both.
    let docker = "systemctl is-active docker.service";
    let both = fake()
        .cmd(list, ran(true, &without_input(NAT), ""))
        .cmd(docker, ran(true, "active\n", ""));
    let report = assert_verdict(both, &store, "nat", Status::Warn, "off the host");
    assert!(verdict(&report, "nat").1.contains("docker.service"));
}

// Rules from before lists of names have no sets for boot to fill, so a VM
// with a list doesn't boot there. The rest still run, so it only warns, read
// from the loaded rules as root and from the file the unit loads as the
// runner user.
#[test]
fn nat_warns_when_the_rules_can_t_hold_a_vm_to_its_list() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let list = &list_nat();
    let old = without_lists(NAT);
    assert!(!old.contains("@listed") && !old.contains("set "), "{old}");
    assert!(
        old.contains("iifname \"fc*\" ip daddr != 172.16.0.0/16 meta"),
        "{old}"
    );
    let report = assert_verdict(
        fake().cmd(list, ran(true, &old, "")),
        &store,
        "nat",
        Status::Warn,
        "list of names",
    );
    let nat = verdict(&report, "nat").1;
    assert!(nat.contains("rerun host/install.sh"), "{nat}");
    assert!(nat.contains("VMs kept off the host"), "{nat}");

    let why = "Operation not permitted (you must be root)";
    let unprivileged = || fake().cmd(list, ran(false, "", why));
    let file = without_lists(&host::nat_rules(&DEFAULT_SUBNET));
    // Its comments still speak of them.
    let rules: Vec<&str> = file.lines().filter(|l| !l.starts_with('#')).collect();
    assert!(
        !rules
            .iter()
            .any(|l| l.contains("@listed") || l.contains("set ")),
        "{file}"
    );
    let old = unprivileged().file(host::NAT_RULES, Ok(&file));
    let report = assert_verdict(old, &store, "nat", Status::Warn, "list of names");
    assert!(verdict(&report, "nat").1.contains(host::NAT_RULES));

    // The sets, but nothing sending a listed tap's packets to be held.
    let unsent = NAT.replace("\t\tiifname @listed jump listed\n", "");
    assert_ne!(unsent, NAT);
    assert_verdict(
        fake().cmd(list, ran(true, &unsent, "")),
        &store,
        "nat",
        Status::Warn,
        "list of names",
    );
    let one_set = NAT.replace("set reach {", "set reached {");
    assert_verdict(
        fake().cmd(list, ran(true, &one_set, "")),
        &store,
        "nat",
        Status::Warn,
        "list of names",
    );

    // Both warnings at once.
    let both = without_input(&without_lists(NAT));
    let report = assert_verdict(
        fake().cmd(list, ran(true, &both, "")),
        &store,
        "nat",
        Status::Warn,
        "list of names",
    );
    assert!(verdict(&report, "nat").1.contains("off the host"));
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

// Hosts drive each other's runners with the runner user's key, which
// `runner hosts add` shares. VMs still run without one, so it only warns.
#[test]
fn key_warns_when_runner_has_no_key() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let report = assert_verdict(fake(), &store, "key", Status::Ok, &key());
    assert!(verdict(&report, "key").1.contains("runner hosts add"));

    let no_key = fake().file(key(), Err(ErrorKind::NotFound));
    let no_pub = fake().file(public_key(), Err(ErrorKind::NotFound));
    let empty = fake().file(key(), Ok(""));
    let empty_pub = fake().file(public_key(), Ok(""));
    for lacking in [no_key, no_pub, empty, empty_pub] {
        let report = assert_verdict(lacking, &store, "key", Status::Warn, "host/install.sh");
        let detail = verdict(&report, "key").1;
        assert!(detail.contains(&key()), "{detail}");
        assert!(detail.contains("runner hosts add"), "{detail}");
        assert!(report.ok);
    }

    // From review: one `runner hosts add` wouldn't share, since it takes
    // only a bare ed25519 key.
    for not_one in [
        "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ runner@host\n".to_string(),
        format!("command=\"sh\" ssh-ed25519 {ED25519} runner@host\n"),
        format!("ssh-ed25519 {ED25519};id runner@host\n"),
    ] {
        let fake = fake().file(public_key(), Ok(&not_one));
        let report = assert_verdict(fake, &store, "key", Status::Warn, "ed25519");
        assert!(verdict(&report, "key").1.contains("host/install.sh"));
    }

    // Anything else warns too, with the error.
    let unreadable = fake().file(key(), Err(ErrorKind::PermissionDenied));
    assert_verdict(unreadable, &store, "key", Status::Warn, "permission denied");
    let mut homeless = fake();
    homeless.home = Err(ErrorKind::NotFound);
    assert_verdict(homeless, &store, "key", Status::Warn, "not found");
}

// The host's idle time, and whether it can power itself off once idle:
// the timer looking, polkit letting this user, and the busy lock open to
// commands. A host that stays up still runs VMs, so it only warns.
#[test]
fn host_idle_shows_the_setting_and_whether_the_host_can_power_itself_off() {
    let (_dir, store) = with_base();
    let fake = || Fake::healthy(store.root());
    let report = assert_verdict(fake(), &store, "host-idle", Status::Ok, "1h");
    assert!(
        verdict(&report, "host-idle")
            .1
            .contains("powers itself off")
    );

    // Never needs none of it.
    let never = fake()
        .file(host::HOST_IDLE, Ok("never\n"))
        .cmd(&timer(), ran(false, "inactive\n", ""))
        .cmd(&pkcheck(), ran(false, "", "Not authorized."));
    let report = assert_verdict(never, &store, "host-idle", Status::Ok, "never");
    assert!(verdict(&report, "host-idle").1.contains("RUNNER_HOST_IDLE"));

    // install.sh writes it only when RUNNER_HOST_IDLE sets it, so
    // aws.sh's setup can tell a host that has none.
    let unset = fake()
        .file(host::HOST_IDLE, Err(ErrorKind::NotFound))
        .cmd(&timer(), ran(false, "inactive\n", ""));
    assert_verdict(unset, &store, "host-idle", Status::Ok, "never");
    let bad = fake().file(host::HOST_IDLE, Ok("soon\n"));
    let report = assert_verdict(bad, &store, "host-idle", Status::Warn, "soon");
    assert!(verdict(&report, "host-idle").1.contains("RUNNER_HOST_IDLE"));
    let stopped = fake().cmd(&timer(), ran(false, "inactive\n", ""));
    let report = assert_verdict(stopped, &store, "host-idle", Status::Warn, host::IDLE_TIMER);
    assert!(verdict(&report, "host-idle").1.contains("inactive"));
    let refused = fake().cmd(&pkcheck(), ran(false, "", "Not authorized."));
    let report = assert_verdict(refused, &store, "host-idle", Status::Warn, "polkit");
    assert!(verdict(&report, "host-idle").1.contains("host/install.sh"));
    let no_pkcheck = fake().cmd(&pkcheck(), Err(ErrorKind::NotFound));
    assert_verdict(no_pkcheck, &store, "host-idle", Status::Warn, "pkcheck");
    let lock = power::busy_lock(store.root());
    let unlocked = fake().file(&lock, Err(ErrorKind::PermissionDenied));
    let report = assert_verdict(unlocked, &store, "host-idle", Status::Warn, "busy.lock");
    assert!(verdict(&report, "host-idle").1.contains("host/install.sh"));
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
