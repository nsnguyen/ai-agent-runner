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
    match host.run("firecracker", &["--version"]) {
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
    match host.run("nft", &["list", "table", family, table]) {
        Ok(r) if r.ok && r.stdout.contains("masquerade") => {
            let mut words = r.stdout.split_whitespace();
            let range = words.find(|w| *w == "saddr").and(words.next());
            ok(format!(
                "forwarding on, masquerading {}",
                range.unwrap_or("?")
            ))
        }
        Ok(r) if r.ok => fail(format!(
            "table {family} {table} has no masquerade rule: {restart}"
        )),
        Ok(r) if r.stderr.contains("Operation not permitted") => ok(format!(
            "forwarding on, {unit} active (reading the rules needs root)"
        )),
        Ok(_) => fail(format!(
            "no nftables table {family} {table}, though {unit} is active (a firewall reload?): {restart}"
        )),
        Err(e) => fail(format!("nft: {e}")),
    }
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
