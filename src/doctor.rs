// `runner doctor`: what this host still lacks to run VMs, one check per line.
// Checks read the host only through `Host`, so tests can fake it; `System` is
// the real machine.
use crate::host;
use crate::store::{BASE, Error, Store};
use rmcp::schemars;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// Works, but not the way it should: VMs still run.
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Report {
    /// False if any check failed. Warnings don't fail.
    pub ok: bool,
    pub checks: Vec<Check>,
}

impl Report {
    /// One line per check, as `runner doctor` prints it.
    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.checks.iter().map(|c| {
            let status = format!("{:?}", c.status).to_lowercase();
            format!("{status:<4}  {:<11}  {}", c.name, c.detail)
        })
    }
}

/// A program that ran, whatever its exit status.
#[derive(Debug, Clone)]
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// What the checks need to know about the machine.
pub trait Host {
    /// Opens a file read-write, as Firecracker opens /dev/kvm.
    fn open_rw(&self, path: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<String>;
    /// The size of a file this user can read.
    fn file_len(&self, path: &Path) -> io::Result<u64>;
    /// Err only if the program couldn't start: NotFound if it isn't installed.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran>;
    /// Whether a disk under `dir` can be forked by reflink.
    fn reflinks(&self, dir: &Path) -> io::Result<bool>;
}

/// The machine this runs on.
pub struct System;

impl Host for System {
    fn open_rw(&self, path: &Path) -> io::Result<()> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(drop)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        let meta = File::open(path)?.metadata()?;
        if meta.is_dir() {
            return Err(io::Error::new(ErrorKind::IsADirectory, "a directory"));
        }
        Ok(meta.len())
    }

    // In the C locale, so the checks can read error text.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran> {
        let out = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .output()?;
        Ok(Ran {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn reflinks(&self, dir: &Path) -> io::Result<bool> {
        reflinks(dir)
    }
}

/// Whether files in `dir` can be cloned by reflink, tried with the same GNU
/// cp the store forks disks with. Errors only if `dir` can't be written.
pub fn reflinks(dir: &Path) -> io::Result<bool> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let probe = dir.join(format!(".reflink-{nanos}-{}", std::process::id()));
    fs::create_dir(&probe)?;
    let cloned = fs::write(probe.join("a"), [0; 4096]).and_then(|()| {
        let out = Command::new("cp")
            .args(["--reflink=always", "--"])
            .args([probe.join("a"), probe.join("b")])
            .output()?;
        // Writing worked, so a failed cp is the clone being refused.
        Ok(out.status.success())
    });
    let _ = fs::remove_dir_all(&probe);
    cloned
}

/// Runs every check, in the order a new host would fix them.
pub fn check(host: &impl Host, store: &Store) -> Report {
    let checks = vec![
        named("kvm", kvm(host)),
        named("firecracker", firecracker(host)),
        named("reflink", reflink(host, store.root())),
        named("kernel", kernel(host, store.root())),
        named("base", base(store)),
        named("nat", nat(host)),
        named("unit", unit(host)),
    ];
    let ok = checks.iter().all(|c| c.status != Status::Fail);
    Report { ok, checks }
}

type Verdict = (Status, String);

fn named(name: &str, (status, detail): Verdict) -> Check {
    let name = name.to_string();
    Check {
        name,
        status,
        detail,
    }
}

fn ok(detail: impl Into<String>) -> Verdict {
    (Status::Ok, detail.into())
}

fn fail(detail: impl Into<String>) -> Verdict {
    (Status::Fail, detail.into())
}

const INSTALL: &str = "host/install.sh";

fn kvm(host: &impl Host) -> Verdict {
    match host.open_rw(Path::new("/dev/kvm")) {
        Ok(()) => ok("/dev/kvm opens read-write"),
        Err(e) if e.kind() == ErrorKind::NotFound => fail(
            "/dev/kvm is missing: no KVM here. Turn on virtualization in firmware, or use a host with nested virtualization",
        ),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => fail(format!(
            "/dev/kvm isn't writable for this user: it needs the kvm group ({INSTALL} adds runner to it; log in again after)"
        )),
        Err(e) => fail(format!("/dev/kvm: {e}")),
    }
}

fn firecracker(host: &impl Host) -> Verdict {
    let pinned = host::firecracker_version();
    match host.run(host::FIRECRACKER, &["--version"]) {
        Ok(r) if r.ok && r.stdout.split_whitespace().any(|w| w == pinned) => {
            ok(format!("Firecracker {pinned}"))
        }
        Ok(r) if r.ok => {
            let found = r.stdout.lines().next().unwrap_or("").trim();
            fail(format!(
                "found {found:?}, runner pins {pinned}: rerun {INSTALL}"
            ))
        }
        Ok(r) => fail(format!("firecracker --version failed: {}", r.stderr.trim())),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fail(format!("firecracker isn't installed: run {INSTALL}"))
        }
        Err(e) => fail(format!("firecracker: {e}")),
    }
}

fn reflink(host: &impl Host, root: &Path) -> Verdict {
    let at = root.display();
    match host.reflinks(root) {
        Ok(true) => ok(format!("{at} reflinks, so forks are instant")),
        Ok(false) => (
            Status::Warn,
            format!(
                "{at} can't reflink, so every fork and run copies the whole disk. Put it on XFS or btrfs"
            ),
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => fail(format!(
            "{at} doesn't exist: run {INSTALL}, or set RUNNER_ROOT"
        )),
        Err(e) => fail(format!("{at} isn't usable: {e}")),
    }
}

fn kernel(host: &impl Host, root: &Path) -> Verdict {
    let path = host::kernel(root);
    let at = path.display();
    match host.file_len(&path) {
        Ok(0) => fail(format!("{at} is empty")),
        Ok(n) => ok(format!("{at}, {n} bytes")),
        Err(e) if e.kind() == ErrorKind::NotFound => fail(format!(
            "no guest kernel at {at}: put Firecracker's guest kernel there"
        )),
        Err(e) => fail(format!("{at}: {e}")),
    }
}

fn base(store: &Store) -> Verdict {
    match store.get(BASE) {
        Ok(vm) => ok(format!("gen {}, {} bytes", vm.generation, vm.bytes)),
        Err(Error::NoVm(_)) => fail("no base VM, which create forks: runner import base <disk>"),
        Err(e) => fail(e.to_string()),
    }
}

// Three things have to hold: forwarding on, the unit active, and its rules
// still loaded, since a firewall reload can flush them under an active unit.
// A fourth only warns, since VMs still run without it: the rules keep VMs off
// the host's own services.
fn nat(host: &impl Host) -> Verdict {
    let unit = host::NAT_UNIT;
    match host.read(Path::new(host::IP_FORWARD)) {
        Ok(on) if on.trim() == "1" => {}
        Ok(_) => return fail(format!("IPv4 forwarding is off: rerun {INSTALL}")),
        Err(e) => return fail(format!("{}: {e}", host::IP_FORWARD)),
    }
    match host.run("systemctl", &["is-active", unit]) {
        Ok(r) if r.ok => {}
        Ok(r) => {
            let state = r.stdout.trim();
            return fail(format!("{unit} is {state}: rerun {INSTALL}"));
        }
        Err(e) => return fail(format!("systemctl: {e}")),
    }
    let [family, table] = host::NAT_TABLE;
    let restart = format!("systemctl restart {unit}");
    let (range, guarded) = match host.run(host::NFT, &["list", "table", family, table]) {
        Ok(r) if r.ok && r.stdout.contains("masquerade") => {
            let masquerade = r.stdout.lines().find(|l| l.contains("masquerade"));
            let mut words = masquerade.unwrap_or_default().split_whitespace();
            let range = words.find(|w| *w == "saddr").and(words.next());
            // An install.sh from before the ip6 table left none to list.
            let [family6, table6] = host::NAT6_TABLE;
            let ip6 = host.run(host::NFT, &["list", "table", family6, table6]);
            let ip6 = ip6.ok().filter(|r| r.ok).map(|r| r.stdout);
            let rules = format!("{}{}", r.stdout, ip6.unwrap_or_default());
            (range.map(str::to_string), Some(guards_host(&rules)))
        }
        Ok(r) if r.ok => {
            return fail(format!(
                "table {family} {table} has no masquerade rule: {restart}"
            ));
        }
        // As the runner user, nft can't list rules, so the active unit, and
        // the file it loaded, have to do.
        Ok(r) if r.stderr.contains("Operation not permitted") => {
            let file = host.read(Path::new(host::NAT_RULES));
            (None, file.ok().map(|rules| guards_host(&rules)))
        }
        Ok(_) => {
            return fail(format!(
                "no nftables table {family} {table}, though {unit} is active (a firewall reload?): {restart}"
            ));
        }
        Err(e) => return fail(format!("nft: {e}")),
    };
    let kept_off = if guarded == Some(true) {
        ", VMs kept off the host"
    } else {
        ""
    };
    let found = match &range {
        Some(range) => format!("forwarding on, masquerading {range}{kept_off}"),
        None => {
            format!("forwarding on, {unit} active{kept_off} (run as root to read the loaded rules)")
        }
    };
    let mut warnings = Vec::new();
    if guarded == Some(false) {
        let rules = if range.is_some() {
            "the loaded rules".to_string()
        } else {
            format!("the rules in {}", host::NAT_RULES)
        };
        warnings.push(format!(
            "{rules} don't keep the VMs off the host's own services: rerun {INSTALL}"
        ));
    }
    // Both drop forwarded traffic by default, so the VMs would lose the
    // internet under rules that look fine.
    let blockers: Vec<&str> = ["docker.service", "firewalld.service"]
        .into_iter()
        .filter(|u| host.run("systemctl", &["is-active", u]).is_ok_and(|r| r.ok))
        .collect();
    if !blockers.is_empty() {
        let range = range.as_deref().unwrap_or("the VMs' range");
        let blockers = blockers.join(" and ");
        warnings.push(format!(
            "{blockers} is active and may drop forwarded traffic: allow {range} there"
        ));
    }
    if warnings.is_empty() {
        return ok(found);
    }
    (
        Status::Warn,
        format!("{found}, but {}", warnings.join("; and ")),
    )
}

/// Whether nftables rules, as `nft list` prints them or as host/nat.nft has
/// them, keep the VMs off the host: input chains that drop all that comes in
/// on a tap, the ip one only after it drops what a tap brings in from outside
/// the range. Rules that went by source address, as install.sh wrote them
/// before, let in a VM that forges one.
fn guards_host(rules: &str) -> bool {
    const FROM_A_TAP: &str = r#"iifname "fc*" drop"#;
    let (mut v4, mut v6) = (false, false);
    let mut family = "";
    let (mut input, mut forged) = (false, false);
    for line in rules.lines().map(str::trim) {
        if let Some(table) = line.strip_prefix("table ") {
            family = table.split_whitespace().next().unwrap_or("");
            (input, forged) = (false, false);
        } else if line.starts_with("chain ") {
            (input, forged) = (false, false);
        } else if line.starts_with("type filter hook input ") {
            input = true;
        } else if input && family == "ip6" {
            v6 |= line == FROM_A_TAP;
        } else if input && family == "ip" {
            let outside = line.strip_prefix(r#"iifname "fc*" ip saddr != "#);
            if outside.is_some_and(|range| range.ends_with(" drop")) {
                forged = true;
            } else if line == FROM_A_TAP {
                v4 |= forged;
            }
        }
    }
    v4 && v6
}

fn unit(host: &impl Host) -> Verdict {
    let path = host::UNIT_PATH;
    match host.read(Path::new(path)) {
        Ok(text) if text == host::UNIT => ok(format!("{path} is this runner's")),
        Ok(_) => fail(format!(
            "{path} differs from this runner's: rerun {INSTALL}"
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fail(format!("{path} isn't installed: run {INSTALL}"))
        }
        Err(e) => fail(format!("{path}: {e}")),
    }
}
