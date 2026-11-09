// `runner doctor`: what this host still lacks to run VMs, one check per line.
// Checks read the host only through `Host`, so tests can fake it; `System` is
// the real machine.
use crate::fence;
use crate::host;
use crate::idle::Idle;
use crate::power;
use crate::room::{Room, gib};
use crate::store::{BASE, Error, Store};
use crate::systemd;
use rmcp::schemars;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
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
    /// This user's home directory.
    fn home(&self) -> io::Result<PathBuf>;
    /// The host's room (src/room.rs), or why it can't be read.
    fn room(&self, store: &Store) -> Result<Room, String>;
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

    // From the passwd database, not $HOME, which sudo may leave as the
    // caller's.
    fn home(&self) -> io::Result<PathBuf> {
        let uid = nix::unistd::geteuid();
        match nix::unistd::User::from_uid(uid) {
            Ok(Some(user)) => Ok(user.dir),
            Ok(None) => Err(io::Error::new(
                ErrorKind::NotFound,
                format!("uid {uid} isn't in the passwd database"),
            )),
            Err(e) => Err(e.into()),
        }
    }

    fn room(&self, store: &Store) -> Result<Room, String> {
        systemd::room(store).map_err(|e| e.to_string())
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
        named("e2fsprogs", e2fsprogs(host)),
        named("reflink", reflink(host, store.root())),
        named("kernel", kernel(host, store.root())),
        named("base", base(store)),
        named("nat", nat(host)),
        named("unit", unit(host)),
        named("key", key(host)),
        named("host-idle", host_idle(host, store.root())),
        named("room", room(host, store)),
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

fn warn(detail: impl Into<String>) -> Verdict {
    (Status::Warn, detail.into())
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

// What `runner resize` and `create --disk` grow a disk with. VMs run
// without them, so a missing one only warns. resize2fs has no version flag:
// with no disk it prints its version and usage, and exits 1.
fn e2fsprogs(host: &impl Host) -> Verdict {
    let tools = [(host::E2FSCK, &["-V"][..]), (host::RESIZE2FS, &[][..])];
    let mut version = String::new();
    for (path, args) in tools {
        match host.run(path, args) {
            Ok(r) if version.is_empty() => {
                version = r.stderr.lines().next().unwrap_or("").trim().to_string();
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return warn(format!(
                    "{path} is missing, so runner resize can't grow a disk: rerun {INSTALL}, which installs e2fsprogs"
                ));
            }
            Err(e) => return warn(format!("{path}: {e}, so runner resize can't grow a disk")),
        }
    }
    let version = if version.is_empty() {
        "e2fsck".to_string()
    } else {
        version
    };
    ok(format!("{version} and resize2fs, for runner resize"))
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

// With what it says it was built from, for a check to hold against
// `images/base/build.sh --fingerprint`.
fn base(store: &Store) -> Verdict {
    let vm = match store.get(BASE) {
        Ok(vm) => vm,
        Err(Error::NoVm(_)) => {
            let how = "runner import base - <path/to/base.ext4";
            return fail(format!("no base VM, which create forks: {how}"));
        }
        Err(e) => return fail(e.to_string()),
    };
    let built = match store.build(BASE) {
        Ok(Some(b)) => {
            let commit = b.commit.map(|c| format!("commit {c}"));
            let commit = commit.unwrap_or_else(|| "an unknown commit".to_string());
            format!("built from {} at {commit}", b.fingerprint)
        }
        Ok(None) => "build unknown".to_string(),
        Err(e) => return fail(e.to_string()),
    };
    ok(format!(
        "gen {}, {} bytes, {built}",
        vm.generation, vm.bytes
    ))
}

// Three things have to hold: forwarding on, the unit active, and its rules
// still loaded, since a firewall reload can flush them under an active unit.
// Two more only warn, since VMs still run without them: the rules keep VMs
// off the host's own services, and can hold a VM to its list of names,
// without which a VM with a list doesn't boot.
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
    let (range, rules) = match host.run(host::NFT, &["list", "table", family, table]) {
        Ok(r) if r.ok && r.stdout.contains("masquerade") => {
            let masquerade = r.stdout.lines().find(|l| l.contains("masquerade"));
            let mut words = masquerade.unwrap_or_default().split_whitespace();
            let range = words.find(|w| *w == "saddr").and(words.next());
            // An install.sh from before the ip6 table left none to list.
            let [family6, table6] = host::NAT6_TABLE;
            let ip6 = host.run(host::NFT, &["list", "table", family6, table6]);
            let ip6 = ip6.ok().filter(|r| r.ok).map(|r| r.stdout);
            let rules = format!("{}{}", r.stdout, ip6.unwrap_or_default());
            (range.map(str::to_string), Some(rules))
        }
        Ok(r) if r.ok => {
            return fail(format!(
                "table {family} {table} has no masquerade rule: {restart}"
            ));
        }
        // As the runner user, nft can't list rules, so the active unit, and
        // the file it loaded, have to do.
        Ok(r) if r.stderr.contains("Operation not permitted") => {
            (None, host.read(Path::new(host::NAT_RULES)).ok())
        }
        Ok(_) => {
            return fail(format!(
                "no nftables table {family} {table}, though {unit} is active (a firewall reload?): {restart}"
            ));
        }
        Err(e) => return fail(format!("nft: {e}")),
    };
    let guarded = rules.as_deref().map(guards_host);
    let holds = rules.as_deref().map(holds_lists);
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
    let rules = if range.is_some() {
        "the loaded rules".to_string()
    } else {
        format!("the rules in {}", host::NAT_RULES)
    };
    if guarded == Some(false) {
        warnings.push(format!(
            "{rules} don't keep the VMs off the host's own services: rerun {INSTALL}"
        ));
    }
    if holds == Some(false) {
        warnings.push(format!(
            "{rules} can't hold a VM to its list of names, so one with a list won't boot: \
             rerun {INSTALL}"
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
/// the range, before it accepts anything. Rules that went by source address,
/// as install.sh wrote them before, let in a VM that forges one.
fn guards_host(rules: &str) -> bool {
    const FROM_A_TAP: &str = r#"iifname "fc*" drop"#;
    let (mut v4, mut v6) = (false, false);
    let mut family = "";
    let (mut input, mut forged, mut accepted) = (false, false, false);
    for line in rules.lines().map(str::trim) {
        if let Some(table) = line.strip_prefix("table ") {
            family = table.split_whitespace().next().unwrap_or("");
            (input, forged, accepted) = (false, false, false);
        } else if line.starts_with("chain ") {
            (input, forged, accepted) = (false, false, false);
        } else if line.starts_with("type filter hook input ") {
            input = true;
        } else if input && family == "ip6" {
            v6 |= line == FROM_A_TAP;
        } else if input && family == "ip" {
            let outside = line.strip_prefix(r#"iifname "fc*" ip saddr != "#);
            if outside.is_some_and(|range| range.ends_with(" drop")) {
                // Too late after an accept: a forged reply is in by then.
                forged |= !accepted;
            } else if line.ends_with(" accept") {
                accepted = true;
            } else if line == FROM_A_TAP {
                v4 |= forged;
            }
        }
    }
    v4 && v6
}

/// Whether nftables rules, as `nft list` prints them or as host/nat.nft has
/// them, can hold a VM to its list of names: the sets boot puts a listed
/// VM's tap and addresses in (src/fence.rs), and a forward rule that sends a
/// listed tap's packets to the chain that holds them to those addresses.
fn holds_lists(rules: &str) -> bool {
    let lines: Vec<&str> = rules.lines().map(str::trim).collect();
    let has = |line: String| lines.contains(&line.as_str());
    let (listed, reach) = (fence::LISTED, fence::REACH);
    has(format!("set {listed} {{"))
        && has(format!("set {reach} {{"))
        && has(format!("iifname @{listed} jump {listed}"))
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

// Hosts drive each other's `runner peer` over SSH with this user's key, which
// `runner hosts add` shares. VMs still run without one, so it only warns.
fn key(host: &impl Host) -> Verdict {
    let home = match host.home() {
        Ok(home) => home,
        Err(e) => return warn(format!("no home directory to find an SSH key in: {e}")),
    };
    let key = home.join(".ssh/id_ed25519");
    let fix = format!(
        "so this host can't drive the other hosts' runners: rerun {INSTALL}, then `runner hosts add` on the laptop"
    );
    let public = home.join(".ssh/id_ed25519.pub");
    for path in [&key, &public] {
        let at = path.display();
        match host.file_len(path) {
            Ok(0) => return warn(format!("{at} is empty, {fix}")),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return warn(format!("no SSH key at {at}, {fix}"));
            }
            Err(e) => return warn(format!("{at}: {e}")),
        }
    }
    match host.read(&public) {
        Ok(line) if bare_ed25519(&line) => {}
        Ok(_) => {
            let at = public.display();
            return warn(format!(
                "{at} isn't a plain ed25519 key, which is all `runner hosts add` shares, {fix}"
            ));
        }
        Err(e) => return warn(format!("{}: {e}", public.display())),
    }
    ok(format!(
        "{}, which `runner hosts add` shares with the other hosts",
        key.display()
    ))
}

// The host's idle time (power.rs), and whether it can power itself off once
// idle: its timer looking, polkit letting this user power it off, and the
// busy lock open to the commands that keep it up. A host that stays up
// still runs VMs, so none of it fails.
fn host_idle(host: &impl Host, root: &Path) -> Verdict {
    let path = host::HOST_IDLE;
    // None there is never, as install.sh leaves it unless told otherwise.
    let idle = match host.read(Path::new(path)) {
        Ok(text) => match text.trim().parse::<Idle>() {
            Ok(idle) => idle,
            Err(e) => {
                return warn(format!(
                    "{path}: {e}. RUNNER_HOST_IDLE=1h {INSTALL} sets it, or never"
                ));
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Idle::Never,
        Err(e) => return warn(format!("{path}: {e}")),
    };
    if idle == Idle::Never {
        return ok(format!(
            "never: the host stays up until it's stopped (RUNNER_HOST_IDLE=1h {INSTALL} sets an idle time)"
        ));
    }
    let timer = host::IDLE_TIMER;
    match host.run("systemctl", &["is-active", timer]) {
        Ok(r) if r.ok => {}
        Ok(r) => {
            let state = r.stdout.trim();
            return warn(format!(
                "{idle}, but {timer} is {state}, so nothing looks whether the host is idle: rerun {INSTALL}"
            ));
        }
        Err(e) => return warn(format!("systemctl: {e}")),
    }
    let pid = std::process::id().to_string();
    let args = ["--action-id", power::POWER_OFF, "--process", &pid];
    match host.run("pkcheck", &args) {
        Ok(r) if r.ok => {}
        Ok(_) => {
            return warn(format!(
                "{idle}, but polkit doesn't let this user power the host off, so it stays up: rerun {INSTALL}, and run doctor as runner"
            ));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return warn(format!(
                "{idle}, but there's no pkcheck to ask polkit whether this user can power the host off: rerun {INSTALL}"
            ));
        }
        Err(e) => return warn(format!("pkcheck: {e}")),
    }
    let lock = power::busy_lock(root);
    if let Err(e) = host.open_rw(&lock) {
        return warn(format!(
            "{idle}, but {}: {e}, so a runner command at work can't keep the host up: rerun {INSTALL}",
            lock.display()
        ));
    }
    ok(format!(
        "{idle}: the host powers itself off once every VM has been stopped or asleep, with no one but runner logged in, for {idle}"
    ))
}

// The host's reserve, and the room the VMs up leave, which each boot and wake
// has to fit in. A host with none still runs the VMs it has, so it only warns.
fn room(host: &impl Host, store: &Store) -> Verdict {
    let room = match host.room(store) {
        Ok(room) => room,
        Err(why) => return warn(why),
    };
    let s = if room.up == 1 { "" } else { "s" };
    let sums = format!(
        "{}, less a {} reserve for the host and {} for {} VM{s} up \
         (RUNNER_HOST_RESERVE=2G {INSTALL} sets the reserve)",
        gib(room.total),
        gib(room.reserve),
        gib(room.held),
        room.up
    );
    match room.free() {
        0 => warn(format!(
            "no room: each start or wake is refused until a VM stops or sleeps. {sums}"
        )),
        free => ok(format!("{} free for VMs: {sums}", gib(free))),
    }
}

// What `runner hosts add` takes from a host's id_ed25519.pub: its first line
// an ed25519 key's type and base64, before any comment.
fn bare_ed25519(public: &str) -> bool {
    let mut words = public.lines().next().unwrap_or("").split_whitespace();
    let (Some("ssh-ed25519"), Some(key)) = (words.next(), words.next()) else {
        return false;
    };
    key.starts_with("AAAAC3NzaC1lZDI1NTE5")
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c))
}
