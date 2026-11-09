// A host's room: what's left of its memory for VMs to boot or wake into.
// It's MemTotal, less a reserve for the host itself (1 GiB unless
// host/install.sh kept another in /etc/runner/reserve), less what each VM
// that's up holds: its memory, and FIRECRACKER_MIB for its Firecracker, from
// booting until it's stopped. A sleeping or parked VM holds none, its memory
// being on disk in its snapshot.
//
// `runner hosts` reports it, so `create` places by it, best of N counts it,
// and each boot or wake to use a VM is refused, before anything starts, when
// the VM doesn't fit in the room the others leave (systemd::start and wake,
// and a URL's wake), rather than overcommitting until the kernel's OOM killer
// takes a Firecracker. A wake only there to shut a VM down isn't checked, so
// a full host can always stop its VMs. The check holds <root>/room.lock until
// the VM counts as up, so two boots at once can't both take the last of it.
use crate::systemd::State;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// What a VM's Firecracker takes past the VM's own memory, in MiB.
pub const FIRECRACKER_MIB: u64 = 64;

/// A host's memory, and what's left of it for VMs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    /// MemTotal, in bytes.
    pub total: u64,
    /// What the host keeps for itself, in bytes.
    pub reserve: u64,
    /// What the VMs up hold, in bytes.
    pub held: u64,
    /// How many VMs hold some.
    pub up: usize,
}

impl Room {
    /// The room on a host of `total` bytes keeping `reserve`, with VMs in
    /// these states, each with its memory in MiB.
    pub fn of(total: u64, reserve: u64, vms: impl IntoIterator<Item = (State, u32)>) -> Room {
        let (mut held, mut up) = (0, 0);
        for (state, mem_mib) in vms {
            let holds = holds(state, mem_mib);
            held += holds;
            up += usize::from(holds > 0);
        }
        Room {
            total,
            reserve,
            held,
            up,
        }
    }

    /// What's left for VMs, in bytes: none on a host that's overcommitted.
    pub fn free(&self) -> u64 {
        self.total
            .saturating_sub(self.reserve)
            .saturating_sub(self.held)
    }
}

/// What a VM in `state`, with `mem_mib` of memory, holds of the host's, in
/// bytes.
pub fn holds(state: State, mem_mib: u32) -> u64 {
    match state {
        State::Booting | State::Running | State::Stopping => needs(mem_mib),
        State::Stopped | State::Sleeping => 0,
    }
}

/// What a VM with `mem_mib` of memory needs to boot or wake, in bytes.
pub fn needs(mem_mib: u32) -> u64 {
    (u64::from(mem_mib) + FIRECRACKER_MIB) << 20
}

/// A boot or wake refused for want of room.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{vm} needs {}; {host} has {} free: stop or sleep a VM, or move {vm}",
    tenths(*need, f64::ceil),
    tenths(*free, f64::floor)
)]
pub struct NoRoom {
    pub vm: String,
    /// In bytes.
    pub need: u64,
    /// The room the other VMs leave, in bytes.
    pub free: u64,
    /// This host's name.
    pub host: String,
}

/// Bytes in GiB, to the nearest tenth, like 4.1G.
pub fn gib(bytes: u64) -> String {
    tenths(bytes, f64::round)
}

// Bytes in GiB, to a tenth rounded as `round` does, so a refusal can round
// what's needed up and what's free down, and never read as if it fit.
fn tenths(bytes: u64, round: fn(f64) -> f64) -> String {
    let tenths = round(bytes as f64 * 10.0 / f64::from(1u32 << 30));
    format!("{:.1}G", tenths / 10.0)
}

/// The lock a check holds until the VM it let through counts as up.
pub fn lock_path(root: &Path) -> PathBuf {
    root.join("room.lock")
}

/// Takes the room lock, waiting for a check under way; it's let go when the
/// file is dropped.
pub fn lock(root: &Path) -> io::Result<File> {
    let path = lock_path(root);
    let at = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(at)?;
    file.lock().map_err(at)?;
    Ok(file)
}
