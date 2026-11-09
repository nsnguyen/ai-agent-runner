// What host/install.sh puts on the machine, for the code that relies on it.
use crate::net::Subnet;
use std::io;
use std::path::{Path, PathBuf};

/// The systemd unit each VM runs under, as host/install.sh installs it.
pub const UNIT: &str = include_str!("../host/firecracker@.service");
pub const UNIT_PATH: &str = "/etc/systemd/system/firecracker@.service";
/// The unit that parks each sleeping VM at a host shutdown (src/sleep.rs), as
/// host/install.sh installs it.
pub const PARK_UNIT: &str = include_str!("../host/runner-park.service");

/// The unit that loads the VMs' NAT rules into the nftables table `ip runner`,
/// and the rules that keep their IPv6 off the host into `ip6 runner`.
pub const NAT_UNIT: &str = "runner-nat.service";
pub const NAT_TABLE: [&str; 2] = ["ip", "runner"];
pub const NAT6_TABLE: [&str; 2] = ["ip6", "runner"];
pub const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
/// Where host/install.sh writes those rules, for the unit to load.
pub const NAT_RULES: &str = "/etc/runner/nat.nft";
const NAT_TEMPLATE: &str = include_str!("../host/nat.nft");

/// host/nat.nft for the VMs' range `net`: what host/install.sh writes to
/// NAT_RULES.
pub fn nat_rules(net: &Subnet) -> String {
    NAT_TEMPLATE.replace("@NET@", &net.to_string())
}

/// The VMs' address range, as host/install.sh writes it for its NAT rules.
/// The runner carves each VM's /30 from the same file, so the two agree.
pub const NET: &str = "/etc/runner/net";

/// The host's idle time, in `runner idle`'s syntax, as host/install.sh keeps
/// it from RUNNER_HOST_IDLE: once nothing has used the host for that long,
/// it powers itself off (src/power.rs). Missing, as before install.sh wrote
/// it, is never.
pub const HOST_IDLE: &str = "/etc/runner/host-idle";
/// The look at whether the host is idle, and the timer that starts it every
/// minute, as host/install.sh installs them.
pub const IDLE_SERVICE: &str = "runner-host-idle.service";
pub const IDLE_TIMER: &str = "runner-host-idle.timer";
pub const IDLE_SERVICE_UNIT: &str = include_str!("../host/runner-host-idle.service");
pub const IDLE_TIMER_UNIT: &str = include_str!("../host/runner-host-idle.timer");
/// How long since the host booted, in seconds, first on its line.
pub const UPTIME: &str = "/proc/uptime";

// By full path, because sshd gives the runner user a PATH without /usr/sbin.
pub const FIRECRACKER: &str = "/usr/local/bin/firecracker";
pub const NFT: &str = "/usr/sbin/nft";
pub const IP: &str = "/usr/sbin/ip";
pub const DEBUGFS: &str = "/usr/sbin/debugfs";
pub const E2FSCK: &str = "/usr/sbin/e2fsck";
pub const RESIZE2FS: &str = "/usr/sbin/resize2fs";

const PIN: &str = include_str!("../host/firecracker.pin");

/// The Firecracker release this runner is built against, like `v1.17.0`.
/// host/firecracker.pin holds it, so install.sh and the binary agree.
pub fn firecracker_version() -> &'static str {
    PIN.lines()
        .find_map(|l| l.strip_prefix("FIRECRACKER_VERSION="))
        .expect("host/firecracker.pin sets FIRECRACKER_VERSION")
        .trim()
}

/// The guest kernel every VM boots unless it names its own.
pub fn kernel(root: &Path) -> PathBuf {
    root.join("kernel").join("vmlinux")
}

/// Where the host's memory figures are, unless RUNNER_MEMINFO names a fake.
pub const MEMINFO: &str = "/proc/meminfo";

/// What the host keeps of its memory for itself, in `runner create --mem`'s
/// syntax, as host/install.sh keeps it from RUNNER_HOST_RESERVE: VMs boot
/// only into the rest (src/room.rs). Missing, as before install.sh wrote it,
/// is DEFAULT_RESERVE. RUNNER_RESERVE names another file, for tests.
pub const RESERVE: &str = "/etc/runner/reserve";
pub const DEFAULT_RESERVE: &str = "1G";

fn from_env(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

/// This host's memory in bytes: MemTotal from /proc/meminfo, which room
/// counts down from (src/room.rs).
pub fn mem_total() -> io::Result<u64> {
    mem_total_in(&from_env("RUNNER_MEMINFO", MEMINFO))
}

/// `mem_total`, from the meminfo file at `path`.
pub fn mem_total_in(path: &Path) -> io::Result<u64> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    parse_mem_total(&text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no MemTotal in {}", path.display()),
        )
    })
}

/// MemTotal in bytes from meminfo's text, whose figures are in KiB.
pub fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kib = line.trim().strip_suffix("kB").unwrap_or(line).trim();
    kib.parse::<u64>().ok()?.checked_mul(1024)
}

/// The host's reserve in bytes, from RESERVE.
pub fn reserve() -> io::Result<u64> {
    reserve_in(&from_env("RUNNER_RESERVE", RESERVE))
}

/// `reserve`, from the file at `path`.
pub fn reserve_in(path: &Path) -> io::Result<u64> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => DEFAULT_RESERVE.to_string(),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let mib = crate::store::parse_mem(text.trim()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: {e}. sudo RUNNER_HOST_RESERVE=1G host/install.sh sets it again",
                path.display()
            ),
        )
    })?;
    Ok(u64::from(mib) << 20)
}
