// A VM's network. Its slot, picked at create (store.rs), gives it a /30 of the
// host's range: the network, the host's end of the tap (the guest's gateway),
// the guest, and broadcast. The tap is named for the /30. NAT and forwarding
// are the host's, set up once by host/install.sh; the runner only adds and
// removes taps.
use crate::host;
use crate::store::{Error, Result};
use rmcp::schemars;
use serde::Serialize;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

/// The subnet VM addresses come from until host/install.sh writes another.
pub const DEFAULT_SUBNET: Subnet = Subnet {
    base: Ipv4Addr::new(172, 16, 0, 0),
    prefix: 16,
};

/// Where a host keeps its resolvers, in the order a guest should take them.
/// A host on systemd-resolved points /etc/resolv.conf at a stub on 127.0.0.53
/// that a guest can't reach, so resolved's upstream list comes next.
pub const HOST_RESOLV_CONFS: [&str; 2] = ["/etc/resolv.conf", "/run/systemd/resolve/resolv.conf"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    base: Ipv4Addr,
    prefix: u8,
}

impl Subnet {
    /// How many /30s it holds, so how many VMs.
    pub fn slots(&self) -> u32 {
        1 << (30 - self.prefix)
    }

    /// The `slot`th /30, or None past the end.
    pub fn net(&self, slot: u32) -> Option<Net> {
        if slot >= self.slots() {
            return None;
        }
        // Fits: slots() * 4 is the subnet's size, and base is aligned to it.
        let network = u32::from(self.base) + slot * 4;
        let address = Ipv4Addr::from(network + 2);
        let [a, b, c, d] = address.octets();
        Some(Net {
            slot,
            tap: format!("fc{network:08x}"),
            gateway: Ipv4Addr::from(network + 1),
            address,
            // Locally administered, and readable as the address it goes with.
            mac: format!("06:00:{a:02x}:{b:02x}:{c:02x}:{d:02x}"),
        })
    }
}

impl FromStr for Subnet {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let bad = || format!("{s:?} isn't a subnet like 172.16.0.0/16");
        let (base, prefix) = s.split_once('/').ok_or_else(bad)?;
        let base: Ipv4Addr = base.parse().map_err(|_| bad())?;
        // u8's parse takes a leading +; a prefix is plain digits.
        if !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let prefix: u8 = prefix.parse().map_err(|_| bad())?;
        if prefix > 30 {
            return Err(format!("{s} is smaller than the /30 one VM needs"));
        }
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        let aligned = Ipv4Addr::from(u32::from(base) & mask);
        if aligned != base {
            return Err(format!("{s} has host bits set; use {aligned}/{prefix}"));
        }
        Ok(Subnet { base, prefix })
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.prefix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Net {
    /// Unique among live VMs; picks everything below.
    pub slot: u32,
    /// The tap device on the host: fc and its /30 in hex.
    pub tap: String,
    /// The host's end of the tap, and the guest's gateway.
    pub gateway: Ipv4Addr,
    /// The guest's address.
    pub address: Ipv4Addr,
    /// The guest's MAC.
    pub mac: String,
}

impl Net {
    /// The kernel's `ip=` boot argument: the guest's address, gateway, netmask
    /// and hostname on eth0, no autoconfiguration, and up to two DNS servers,
    /// which the kernel shows the guest in /proc/net/pnp.
    pub fn boot_arg(&self, hostname: &str, dns: &[Ipv4Addr]) -> String {
        let mut arg = format!(
            "ip={}::{}:255.255.255.252:{hostname}:eth0:off",
            self.address, self.gateway
        );
        for server in dns.iter().take(2) {
            arg += &format!(":{server}");
        }
        arg
    }
}

/// The first two IPv4 nameservers a guest can reach, from the first of these
/// resolv.conf files that has any. Loopback ones are the host's own stub.
pub fn dns(resolv_confs: &[&Path]) -> Vec<Ipv4Addr> {
    for path in resolv_confs {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let found: Vec<Ipv4Addr> = text
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some("nameserver")).then(|| words.next())?
            })
            .filter_map(|addr| addr.parse::<Ipv4Addr>().ok())
            .filter(|addr| !addr.is_loopback() && !addr.is_unspecified())
            .take(2)
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// A VM's tap on the host, with the gateway address on it, removed when
/// dropped. `runner boot` holds one while Firecracker runs. Making one needs
/// CAP_NET_ADMIN, which the VM's unit has and the CLI user doesn't;
/// Firecracker can open it without, so boot should drop it before then.
#[derive(Debug)]
pub struct Tap {
    name: String,
    ip: PathBuf,
}

impl Tap {
    pub fn add(net: &Net) -> Result<Tap> {
        Tap::add_with(Path::new(host::IP), net)
    }

    /// `add`, with iproute2's `ip` at the given path.
    pub fn add_with(ip: &Path, net: &Net) -> Result<Tap> {
        // A tap by this name that nothing holds open was left by a boot that
        // was killed. `tuntap del` removes only that: a tap still in use, by
        // a VM in another root with the same range, makes the add below fail
        // instead of losing it its network.
        let _ = run(ip, &["tuntap", "del", "dev", &net.tap, "mode", "tap"]);
        run(ip, &["tuntap", "add", "dev", &net.tap, "mode", "tap"])?;
        // From here a failure drops the tap, which removes it.
        let tap = Tap {
            name: net.tap.clone(),
            ip: ip.to_path_buf(),
        };
        let host_end = format!("{}/30", net.gateway);
        run(ip, &["addr", "add", &host_end, "dev", &net.tap])?;
        run(ip, &["link", "set", "dev", &net.tap, "up"])?;
        Ok(tap)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        if let Err(e) = run(&self.ip, &["link", "del", "dev", &self.name]) {
            // Not eprintln: under the unit, stderr is the journal stream
            // Firecracker made non-blocking, and a failed write would panic.
            let _ = writeln!(io::stderr(), "runner: {e}");
        }
    }
}

// Runs `ip` quietly; a failure says what was run and what `ip` said.
fn run(ip: &Path, args: &[&str]) -> Result<()> {
    let failed = |why: String| Error::Command {
        cmd: format!("{} {}", ip.display(), args.join(" ")),
        why,
    };
    let out = Command::new(ip)
        .args(args)
        .output()
        .map_err(|e| failed(e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(failed(if said.is_empty() {
            out.status.to_string()
        } else {
            said
        }))
    }
}
