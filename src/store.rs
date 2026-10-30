// The filesystem is the database: each VM is a directory.
//
//   <root>/vms/<name>/gens/<n>.ext4  one disk per generation; a run writes a new one
//   <root>/vms/<name>/history        committed generations, one per line; the last is current
//   <root>/vms/<name>/lock           flock'd by the process running the VM, one at a time
//   <root>/vms/<name>/creds          the credentials it gets at boot, by name; never a value
//   <root>/vms/<name>/slot           its network slot, unique among live VMs (net.rs)
//   <root>/vms/.new-<name>-<pid>/    a VM being created, renamed into place when done
//   <root>/vms/.del-<name>-<pid>/    a VM being deleted, renamed out of the way first
//   <root>/slots.lock                flock'd while a slot is picked
//   <root>/subnet                    host config: the subnet slots carve up; 172.16.0.0/16 if absent
use crate::creds::{Cred, DEFAULT};
use crate::net::{DEFAULT_SUBNET, Net, Subnet};
use rmcp::schemars;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The VM `create` forks unless told otherwise.
pub const BASE: &str = "base";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0:?} isn't a VM name: use up to 32 of a-z, 0-9 and -, not starting with -")]
    BadName(String),
    #[error("no VM named {0}")]
    NoVm(String),
    #[error("a VM named {0} already exists")]
    Exists(String),
    #[error("{0} is busy: a run holds it")]
    Busy(String),
    #[error("{0} has no earlier generation to go back to")]
    NothingToUndo(String),
    #[error("no free address left in {0}: delete a VM, or widen <root>/subnet")]
    NoSlot(Subnet),
    #[error("{cmd}: {why}")]
    Command { cmd: String, why: String },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

fn at(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
    let path = path.to_path_buf();
    move |source| Error::Io { path, source }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Vm {
    pub name: String,
    /// The disk generation the next boot starts from.
    pub generation: u64,
    /// Size of that disk.
    pub bytes: u64,
}

// A name becomes a directory, so it has to be one plain path component.
pub fn check_name(name: &str) -> Result<()> {
    let plain = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    let ok = (1..=32).contains(&name.len()) && !name.starts_with('-') && name.bytes().all(plain);
    if ok {
        Ok(())
    } else {
        Err(Error::BadName(name.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn vms(&self) -> PathBuf {
        self.root.join("vms")
    }

    // A VM exists once it has a history, which creating one writes last.
    fn existing(&self, name: &str) -> Result<PathBuf> {
        check_name(name)?;
        let dir = self.vms().join(name);
        if dir.join("history").is_file() {
            Ok(dir)
        } else {
            Err(Error::NoVm(name.to_string()))
        }
    }

    /// Brings an ext4 disk in as a new VM, usually `base`.
    pub fn import(&self, name: &str, disk: &Path) -> Result<Vm> {
        self.make(name, disk)
    }

    /// A new VM whose disk is a copy-on-write fork of `from`'s current disk.
    pub fn fork(&self, from: &str, name: &str) -> Result<Vm> {
        let src = self.existing(from)?;
        self.make(name, &disk_of(&src, current(&src)?))
    }

    // Built aside under a dot-name that list skips, then renamed into place, so
    // a create killed partway never takes the name.
    fn make(&self, name: &str, disk: &Path) -> Result<Vm> {
        check_name(name)?;
        let vms = self.vms();
        fs::create_dir_all(&vms).map_err(at(&vms))?;
        sweep_staging(&vms);
        let dir = vms.join(name);
        if dir.exists() {
            return Err(Error::Exists(name.to_string()));
        }
        let staged = vms.join(format!(".new-{name}-{}", std::process::id()));
        let made = fs::create_dir(&staged)
            .and_then(|()| fs::create_dir(staged.join("gens")))
            .map_err(at(&staged))
            .and_then(|()| self.assign_slot(&staged).map(drop))
            .and_then(|()| clone(disk, &disk_of(&staged, 0)))
            .and_then(|()| write_history(&staged, &[0]))
            // rename won't replace a directory with anything in it, so a
            // create that raced this one keeps its VM. Under the slots lock,
            // or a slot scan could miss this VM between its two names.
            .and_then(|()| {
                let _slots = self.lock_slots()?;
                fs::rename(&staged, &dir).map_err(|e| match e.kind() {
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists => {
                        Error::Exists(name.to_string())
                    }
                    _ => at(&dir)(e),
                })
            })
            .and_then(|()| sync_dir(&vms));
        if made.is_err() {
            let _ = fs::remove_dir_all(&staged);
        }
        made?;
        read_vm(name, &dir)
    }

    /// The VM's tap and addresses, from its slot.
    pub fn net(&self, name: &str) -> Result<Net> {
        let dir = self.existing(name)?;
        let slot = match read_slot(&dir)? {
            Some(slot) => slot,
            None => self.assign_slot(&dir)?,
        };
        let subnet = self.subnet()?;
        subnet.net(slot).ok_or_else(|| {
            let why = format!("slot {slot} is past the end of {subnet}");
            invalid(&dir.join("slot"), why)
        })
    }

    /// The subnet VM addresses come from. Changing it moves every VM's
    /// address, but not its slot.
    pub fn subnet(&self) -> Result<Subnet> {
        let path = self.root.join("subnet");
        match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DEFAULT_SUBNET),
            Err(e) => Err(at(&path)(e)),
            Ok(text) => text.trim().parse().map_err(|why| invalid(&path, why)),
        }
    }

    // The lowest slot no other VM holds, counting creates in flight, picked
    // under a lock so two creates can't take the same one. A VM keeps the slot
    // it has; one made before slots existed gets one here.
    fn assign_slot(&self, dir: &Path) -> Result<u32> {
        let _slots = self.lock_slots()?;
        if let Some(slot) = read_slot(dir)? {
            return Ok(slot);
        }
        let subnet = self.subnet()?;
        let taken = self.taken_slots()?;
        let slot = (0..subnet.slots())
            .find(|s| !taken.contains(s))
            .ok_or(Error::NoSlot(subnet))?;
        write_aside(dir, "slot", &format!("{slot}\n"))?;
        Ok(slot)
    }

    // Held while slots are picked and while a new VM moves into place.
    fn lock_slots(&self) -> Result<Lock> {
        let path = self.root.join("slots.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(at(&path))?;
        file.lock().map_err(at(&path))?;
        Ok(Lock(file))
    }

    // A VM being deleted has let go of its slot.
    fn taken_slots(&self) -> Result<HashSet<u32>> {
        let vms = self.vms();
        let mut taken = HashSet::new();
        for entry in fs::read_dir(&vms).map_err(at(&vms))? {
            let entry = entry.map_err(at(&vms))?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|n| n.starts_with(".del-")) {
                continue;
            }
            // An unreadable slot fails its own VM's reads, not every create.
            if let Ok(Some(slot)) = read_slot(&entry.path()) {
                taken.insert(slot);
            }
        }
        Ok(taken)
    }

    pub fn get(&self, name: &str) -> Result<Vm> {
        read_vm(name, &self.existing(name)?)
    }

    pub fn list(&self) -> Result<Vec<Vm>> {
        let vms = self.vms();
        let entries = match fs::read_dir(&vms) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            r => r.map_err(at(&vms))?,
        };
        let mut out = Vec::new();
        for entry in entries {
            let name = entry.map_err(at(&vms))?.file_name();
            let Some(name) = name.to_str() else { continue };
            match self.get(name) {
                Ok(vm) => out.push(vm),
                // Not a VM's directory: a staging one, or something else.
                Err(Error::NoVm(_) | Error::BadName(_)) => {}
                // One broken VM shouldn't hide the rest.
                Err(e) => eprintln!("runner: skipping {name}: {e}"),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Clones the current disk into a new generation for one run to boot
    /// from. Only `Run::commit` makes it current; dropping the run keeps the
    /// old one current and leaves the new disk for a look.
    pub fn begin_run(&self, name: &str) -> Result<Run> {
        let dir = self.existing(name)?;
        let lock = take_lock(&dir, name)?;
        let generation = next_generation(&dir)?;
        let disk = disk_of(&dir, generation);
        clone(&disk_of(&dir, current(&dir)?), &disk)?;
        let name = name.to_string();
        Ok(Run {
            generation,
            disk,
            name,
            dir,
            _lock: lock,
        })
    }

    /// Makes the previous committed generation current again.
    pub fn undo(&self, name: &str) -> Result<Vm> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        let mut gens = history(&dir)?;
        if gens.len() < 2 {
            return Err(Error::NothingToUndo(name.to_string()));
        }
        gens.pop();
        write_history(&dir, &gens)?;
        read_vm(name, &dir)
    }

    /// The credentials the VM gets at boot: what its allowlist names, or
    /// `creds::DEFAULT` if it has none. Names only; no value is kept here.
    pub fn creds(&self, name: &str) -> Result<Vec<Cred>> {
        let path = self.existing(name)?.join("creds");
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DEFAULT.to_vec()),
            r => r.map_err(at(&path))?,
        };
        let bad = |e| at(&path)(io::Error::new(io::ErrorKind::InvalidData, e));
        let mut creds = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.parse().map_err(bad))
            .collect::<Result<Vec<Cred>>>()?;
        creds.sort();
        creds.dedup();
        Ok(creds)
    }

    /// Sets the VM's allowlist to exactly `creds`. It takes effect at the
    /// next boot, so a running VM doesn't block it.
    pub fn set_creds(&self, name: &str, creds: &[Cred]) -> Result<Vec<Cred>> {
        let dir = self.existing(name)?;
        let mut creds = creds.to_vec();
        creds.sort();
        creds.dedup();
        let text: String = creds.iter().map(|c| format!("{c}\n")).collect();
        write_aside(&dir, "creds", &text).map_err(|e| match e {
            // Deleted between the check and the write.
            Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                Error::NoVm(name.to_string())
            }
            e => e,
        })?;
        Ok(creds)
    }

    /// Removes a VM and all its disk generations. Forks of it are unaffected:
    /// each has a disk of its own.
    pub fn delete(&self, name: &str) -> Result<Vm> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        let vm = read_vm(name, &dir)?;
        // Renamed first, so the VM is gone at once and the name is free; a
        // delete killed partway leaves a dot-directory that the next create
        // from a new process sweeps away. The lock goes with the rename, still held until we return.
        let vms = self.vms();
        // The nanos keep a leftover from colliding with the next delete of the
        // same name in a long-lived `runner mcp`, whose pid never goes away.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let doomed = vms.join(format!(".del-{name}-{nanos}-{}", std::process::id()));
        fs::rename(&dir, &doomed).map_err(at(&dir))?;
        sync_dir(&vms)?;
        // The VM is gone either way, so a failed removal is a warning, not a
        // failure a caller would retry into "no such VM".
        if let Err(e) = fs::remove_dir_all(&doomed) {
            eprintln!("runner: {name} is deleted, but {}: {e}", doomed.display());
        }
        Ok(vm)
    }
}

#[derive(Debug)]
pub struct Run {
    pub generation: u64,
    pub disk: PathBuf,
    name: String,
    dir: PathBuf,
    _lock: Lock,
}

impl Run {
    pub fn commit(self) -> Result<Vm> {
        let mut gens = history(&self.dir)?;
        gens.push(self.generation);
        write_history(&self.dir, &gens)?;
        read_vm(&self.name, &self.dir)
    }
}

// flock, not a pid file: the kernel drops it when the holder dies, so a
// crashed run never wedges its VM.
fn take_lock(dir: &Path, name: &str) -> Result<Lock> {
    let path = dir.join("lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| match e.kind() {
            // The VM was deleted between the caller's check and here.
            io::ErrorKind::NotFound => Error::NoVm(name.to_string()),
            _ => at(&path)(e),
        })?;
    match file.try_lock() {
        Ok(()) => Ok(Lock(file)),
        Err(TryLockError::WouldBlock) => Err(Error::Busy(name.to_string())),
        Err(TryLockError::Error(e)) => Err(at(&path)(e)),
    }
}

fn invalid(path: &Path, why: impl Into<String>) -> Error {
    at(path)(io::Error::new(io::ErrorKind::InvalidData, why.into()))
}

fn read_slot(dir: &Path) -> Result<Option<u32>> {
    let path = dir.join("slot");
    match fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(&path)(e)),
        Ok(text) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|e: std::num::ParseIntError| invalid(&path, e.to_string())),
    }
}

// Unlocked on drop, not only closed: a child spawned on another thread holds a
// copy of the descriptor until it execs, and the lock lasts while any copy is
// open.
#[derive(Debug)]
struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn read_vm(name: &str, dir: &Path) -> Result<Vm> {
    let generation = current(dir)?;
    let disk = disk_of(dir, generation);
    let bytes = fs::metadata(&disk).map_err(at(&disk))?.len();
    let name = name.to_string();
    Ok(Vm {
        name,
        generation,
        bytes,
    })
}

fn disk_of(dir: &Path, generation: u64) -> PathBuf {
    dir.join("gens").join(format!("{generation}.ext4"))
}

fn history(dir: &Path) -> Result<Vec<u64>> {
    let path = dir.join("history");
    let text = fs::read_to_string(&path).map_err(at(&path))?;
    let bad = |e| at(&path)(io::Error::new(io::ErrorKind::InvalidData, e));
    text.lines()
        .map(|l| l.trim().parse().map_err(bad))
        .collect()
}

fn current(dir: &Path) -> Result<u64> {
    let gens = history(dir)?;
    let empty = || at(&dir.join("history"))(io::ErrorKind::InvalidData.into());
    gens.last().copied().ok_or_else(empty)
}

fn write_history(dir: &Path, gens: &[u64]) -> Result<()> {
    let text: String = gens.iter().map(|g| format!("{g}\n")).collect();
    write_aside(dir, "history", &text)
}

// Written aside, synced and renamed, so even a host crash leaves either the old
// file or the new. The temp name is per process, since `set_creds` runs
// without the VM's lock and two of them must not share one.
fn write_aside(dir: &Path, name: &str, text: &str) -> Result<()> {
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    let mut f = File::create(&tmp).map_err(at(&tmp))?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(at(&tmp))?;
    let path = dir.join(name);
    fs::rename(&tmp, &path).map_err(at(&path))?;
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(at(dir))
}

// A staging directory whose creator is gone is what a killed create or delete left.
fn sweep_staging(vms: &Path) {
    let Ok(entries) = fs::read_dir(vms) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|n| {
                n.strip_prefix(".new-")
                    .or(n.strip_prefix(".del-"))?
                    .rsplit_once('-')
            })
            .and_then(|(_, pid)| pid.parse::<u32>().ok());
        if pid.is_some_and(|pid| !Path::new(&format!("/proc/{pid}")).exists()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

// Past every disk on hand, failed runs' included, so none is overwritten.
fn next_generation(dir: &Path) -> Result<u64> {
    let gens = dir.join("gens");
    let mut last = 0;
    for entry in fs::read_dir(&gens).map_err(at(&gens))? {
        let name = entry.map_err(at(&gens))?.file_name();
        let n = name
            .to_str()
            .and_then(|s| s.strip_suffix(".ext4")?.parse().ok());
        last = last.max(n.unwrap_or(0));
    }
    Ok(last + 1)
}

// GNU cp does the copying: a reflink on XFS or btrfs, which takes constant
// time and no space until one side writes, else a copy that keeps the disk's
// holes, so a sparse image stays small. Every generation is writable whatever
// the source's mode, and synced before anything points at it.
fn clone(from: &Path, to: &Path) -> Result<()> {
    let out = Command::new("cp")
        .args(["--reflink=auto", "--sparse=always", "--"])
        .args([from, to])
        .output()
        .map_err(at(to))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(at(to)(io::Error::other(why)));
    }
    fs::set_permissions(to, fs::Permissions::from_mode(0o644)).map_err(at(to))?;
    File::open(to).and_then(|f| f.sync_all()).map_err(at(to))
}
