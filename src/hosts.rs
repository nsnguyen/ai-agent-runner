// This host's row in the laptop's `runner hosts`: its VMs, how many are up,
// and its free memory. The laptop asks every host for its row at once, and
// `runner create` there places a new VM on the host with the most free memory.
use crate::store::{self, Store};
use crate::systemd::{self, State};
use rmcp::schemars;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error("reading this host's free memory: {0}")]
    Mem(std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct HostRow {
    /// The host's own name for itself.
    pub name: String,
    /// How many VMs it has, base included, as `list` lists them.
    pub vms: usize,
    /// How many of those are up: booting, running, sleeping or stopping.
    pub up: usize,
    /// Its free memory in bytes: MemAvailable from /proc/meminfo.
    pub mem_available: u64,
}

impl HostRow {
    /// The row as the host's `runner hosts` prints it, which the laptop puts
    /// the host's name in front of. The laptop reads the fifth field, the free
    /// bytes, to place a VM.
    pub fn line(&self) -> String {
        let s = if self.vms == 1 { "" } else { "s" };
        format!(
            "{} VM{s}  {} up  {} bytes free",
            self.vms, self.up, self.mem_available
        )
    }
}

/// This host's row, asking systemd how each VM stands.
pub fn row(store: &Store) -> Result<HostRow, Error> {
    let vms = store.list()?;
    let mut up = 0;
    for vm in &vms {
        if systemd::status(store, &vm.name)?.state != State::Stopped {
            up += 1;
        }
    }
    Ok(HostRow {
        name: hostname(),
        vms: vms.len(),
        up,
        mem_available: crate::host::mem_available().map_err(Error::Mem)?,
    })
}

// The kernel's name for this machine, or empty if it won't say.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}
