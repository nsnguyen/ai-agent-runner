// What host/install.sh puts on the machine, for the code that relies on it.
use std::path::{Path, PathBuf};

/// The systemd unit each VM runs under, as host/install.sh installs it.
pub const UNIT: &str = include_str!("../host/firecracker@.service");
pub const UNIT_PATH: &str = "/etc/systemd/system/firecracker@.service";

/// The unit that loads the VMs' NAT rules into the nftables table `ip runner`.
pub const NAT_UNIT: &str = "runner-nat.service";
pub const NAT_TABLE: [&str; 2] = ["ip", "runner"];
pub const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

// By full path, because sshd gives the runner user a PATH without /usr/sbin.
pub const FIRECRACKER: &str = "/usr/local/bin/firecracker";
pub const NFT: &str = "/usr/sbin/nft";

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
