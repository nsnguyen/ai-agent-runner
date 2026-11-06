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

// By full path, because sshd gives the runner user a PATH without /usr/sbin.
pub const FIRECRACKER: &str = "/usr/local/bin/firecracker";
pub const NFT: &str = "/usr/sbin/nft";
pub const IP: &str = "/usr/sbin/ip";

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

/// This host's free memory in bytes: MemAvailable from /proc/meminfo, what the
/// kernel reckons it can hand out without swapping. `runner hosts` reports it,
/// and the laptop's `runner create` places a VM on the host with the most.
pub fn mem_available() -> io::Result<u64> {
    let path =
        std::env::var_os("RUNNER_MEMINFO").map_or_else(|| PathBuf::from(MEMINFO), PathBuf::from);
    mem_available_in(&path)
}

/// `mem_available`, from the meminfo file at `path`.
pub fn mem_available_in(path: &Path) -> io::Result<u64> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    parse_mem_available(&text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no MemAvailable in {}", path.display()),
        )
    })
}

/// MemAvailable in bytes from meminfo's text, whose figures are in KiB.
pub fn parse_mem_available(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?;
    let kib = line.trim().strip_suffix("kB").unwrap_or(line).trim();
    kib.parse::<u64>().ok()?.checked_mul(1024)
}
