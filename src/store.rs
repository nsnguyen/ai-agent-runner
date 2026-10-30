// The filesystem is the database: each VM is a directory.
//
//   <root>/vms/<name>/gens/<n>.ext4  one disk per generation; a run writes a new one
//   <root>/vms/<name>/gens/<n>.json  its note: when it was made, a comment, and whether it's a checkpoint
//   <root>/vms/<name>/history        committed generations, one per line; the last is current
//   <root>/vms/<name>/lock           flock'd by the process running the VM, one at a time
//   <root>/vms/<name>/config.json    sizes and kernel, only where they differ from the host's
//   <root>/vms/<name>/api.sock       Firecracker's API, while the VM runs
//   <root>/vms/<name>/vsock.sock     the guest's vsock, while the VM runs
//   <root>/vms/<name>/vsock.sock_10023  where its guest takes its credentials, while the VM runs
//   <root>/vms/<name>/creds          the credentials it gets at boot, by name; never a value
//   <root>/vms/<name>/handed         the ones its guest took this boot, by name; never a value
//   <root>/vms/<name>/slot           its network slot, unique among live VMs (net.rs)
//   <root>/vms/.new-<name>-<pid>/    a VM being created, renamed into place when done
//   <root>/vms/.del-<name>-<pid>/    a VM being deleted, renamed out of the way first
//   <root>/kernel/vmlinux            the kernel a VM boots unless its config names one
//   <root>/slots.lock                flock'd while a slot is picked
//
// The range slots carve up is the host's, in /etc/runner/net (host.rs).
use crate::changes::{self, Changes};
use crate::creds::{self, Cred, DEFAULT};
use crate::net::{DEFAULT_SUBNET, Net, Subnet};
use chrono::{DateTime, SubsecRound, Utc};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The VM `create` forks unless told otherwise.
pub const BASE: &str = "base";

/// How many generations gc keeps besides the current one and the checkpoints.
pub const KEEP: usize = 5;
/// What a VM gets when its config doesn't say.
pub const DEFAULT_CPUS: u32 = 2;
pub const DEFAULT_MEM_MIB: u32 = 2048;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0:?} isn't a VM name: use up to 32 of a-z, 0-9 and -, not starting with -")]
    BadName(String),
    #[error("no VM named {0}")]
    NoVm(String),
    #[error("a VM named {0} already exists")]
    Exists(String),
    #[error("{0} is busy: it's running, or another runner command holds it")]
    Busy(String),
    #[error("{0} has no earlier generation to go back to")]
    NothingToUndo(String),
    #[error("{0} has no checkpoint {1}")]
    NoCheckpoint(String, u64),
    #[error("{0}")]
    BadConfig(String),
    #[error(
        "no free address left in {0}: delete a VM, or rerun host/install.sh with a wider RUNNER_NET"
    )]
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
    /// Space all its generations take on the host: allocated blocks, not
    /// apparent size. A block reflinked into several disks counts in each.
    pub used: u64,
}

/// What gc deleted from one VM, or would on a dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freed {
    pub name: String,
    /// Oldest first.
    pub generations: Vec<u64>,
    /// Their allocated blocks.
    pub bytes: u64,
}

/// A disk generation `restore` can bring back: one `checkpoint` froze, or one
/// in the VM's history, which runs, imports and restores make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Checkpoint {
    /// Its generation; `restore` takes it.
    pub id: u64,
    /// When it was made.
    pub time: DateTime<Utc>,
    /// What it was made with, or for an automatic one, what made it.
    pub comment: Option<String>,
    /// Made by a run, an import, a fork or a restore, not by `checkpoint`.
    pub auto: bool,
    /// Size of its disk.
    pub bytes: u64,
    /// The disk the next boot starts from.
    pub current: bool,
}

/// A VM's sizes and kernel as stored: each one left out is the host's default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<PathBuf>,
}

impl Config {
    /// `self`, with whatever `over` sets taking its place.
    fn with(self, over: &Config) -> Config {
        Config {
            cpus: over.cpus.or(self.cpus),
            mem_mib: over.mem_mib.or(self.mem_mib),
            kernel: over.kernel.clone().or(self.kernel),
        }
    }

    // Firecracker's limits, checked here so a bad size fails at create, not boot.
    fn check(&self) -> Result<()> {
        let bad = |why: String| Err(Error::BadConfig(why));
        if let Some(n) = self.cpus
            && !(1..=32).contains(&n)
        {
            return bad(format!("a VM takes 1 to 32 vCPUs, not {n}"));
        }
        if let Some(mib) = self.mem_mib
            && mib < 128
        {
            return bad(format!("a VM needs at least 128 MiB of memory, not {mib}"));
        }
        if let Some(kernel) = &self.kernel {
            if !kernel.is_absolute() {
                return bad(format!(
                    "the kernel path {} isn't absolute",
                    kernel.display()
                ));
            }
            if !kernel.is_file() {
                return bad(format!("no kernel file at {}", kernel.display()));
            }
        }
        Ok(())
    }
}

/// What a VM boots with: its config over the host's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub cpus: u32,
    pub mem_mib: u32,
    pub kernel: PathBuf,
}

/// A memory size in MiB: a plain number is MiB, or it ends in M, G or T
/// (MiB, GiB and TiB too, in any case).
pub fn parse_mem(text: &str) -> Result<u32> {
    let bad = || {
        Error::BadConfig(format!(
            "{text:?} isn't a memory size: use MiB, or a whole number with M, G or T, like 4G"
        ))
    };
    let lower = text.to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let shift = match &lower[digits.len()..] {
        "" | "m" | "mib" => 0,
        "g" | "gib" => 10,
        "t" | "tib" => 20,
        _ => return Err(bad()),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u32 = digits.parse().map_err(|_| bad())?;
    let mib = n.checked_mul(1 << shift).ok_or_else(bad)?;
    if mib == 0 { Err(bad()) } else { Ok(mib) }
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
    net_file: PathBuf,
    keep: usize,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let net_file = PathBuf::from(crate::host::NET);
        Store {
            root,
            net_file,
            keep: KEEP,
        }
    }

    /// Reads the address range from `path` in place of /etc/runner/net.
    pub fn with_net_file(self, path: impl Into<PathBuf>) -> Self {
        let net_file = path.into();
        Store { net_file, ..self }
    }

    /// Sets how many other generations gc keeps, at every commit and in `gc`.
    pub fn keeping(self, keep: usize) -> Self {
        Store { keep, ..self }
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
        self.make(name, disk, &Config::default(), "import")
    }

    /// A new VM whose disk is a copy-on-write fork of `from`'s current disk,
    /// and whose sizes and kernel are `from`'s.
    pub fn fork(&self, from: &str, name: &str) -> Result<Vm> {
        self.fork_with(from, name, &Config::default())
    }

    /// A fork, with whatever `over` sets in place of `from`'s sizes and kernel.
    pub fn fork_with(&self, from: &str, name: &str, over: &Config) -> Result<Vm> {
        let src = self.existing(from)?;
        let config = read_config(&src)?.with(over);
        self.make(
            name,
            &disk_of(&src, current(&src)?),
            &config,
            &format!("fork of {from}"),
        )
    }

    // Built aside under a dot-name that list skips, then renamed into place, so
    // a create killed partway never takes the name.
    fn make(&self, name: &str, disk: &Path, config: &Config, why: &str) -> Result<Vm> {
        check_name(name)?;
        config.check()?;
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
            .and_then(|()| write_config(&staged, config))
            .and_then(|()| write_note(&staged, 0, &Note::auto(why)))
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

    /// The subnet VM addresses come from: the host's range, or
    /// 172.16.0.0/16 before host/install.sh has run. Changing it moves every
    /// VM's address, but not its slot.
    pub fn subnet(&self) -> Result<Subnet> {
        let path = &self.net_file;
        match fs::read_to_string(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DEFAULT_SUBNET),
            Err(e) => Err(at(path)(e)),
            Ok(text) => text.trim().parse().map_err(|why| invalid(path, why)),
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
    fn lock_slots(&self) -> Result<File> {
        let path = self.root.join("slots.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(at(&path))?;
        file.lock().map_err(at(&path))?;
        Ok(file)
    }

    // A VM being deleted has let go of its slot. A slot that can't be read
    // fails the create: taking it as free could hand it out twice.
    fn taken_slots(&self) -> Result<HashSet<u32>> {
        let vms = self.vms();
        let mut taken = HashSet::new();
        for entry in fs::read_dir(&vms).map_err(at(&vms))? {
            let entry = entry.map_err(at(&vms))?;
            let name = entry.file_name();
            // Following symlinks, as finding a VM does.
            let dir = entry.path().is_dir();
            if !dir || name.to_str().is_some_and(|n| n.starts_with(".del-")) {
                continue;
            }
            // A VM deleted since the listing reads as no slot.
            taken.extend(read_slot(&entry.path())?);
        }
        Ok(taken)
    }

    pub fn get(&self, name: &str) -> Result<Vm> {
        read_vm(name, &self.existing(name)?)
    }

    /// The sizes and kernel a VM's directory names, without the defaults.
    pub fn config(&self, name: &str) -> Result<Config> {
        read_config(&self.existing(name)?)
    }

    /// What the VM boots with.
    pub fn machine(&self, name: &str) -> Result<Machine> {
        let config = self.config(name)?;
        config.check()?;
        Ok(Machine {
            cpus: config.cpus.unwrap_or(DEFAULT_CPUS),
            mem_mib: config.mem_mib.unwrap_or(DEFAULT_MEM_MIB),
            kernel: config.kernel.unwrap_or_else(|| self.default_kernel()),
        })
    }

    /// The host's kernel, for every VM that doesn't name its own.
    pub fn default_kernel(&self) -> PathBuf {
        self.root.join("kernel").join("vmlinux")
    }

    /// Where the VM's Firecracker listens, while it runs.
    pub fn api_socket(&self, name: &str) -> Result<PathBuf> {
        Ok(api_socket(&self.existing(name)?))
    }

    /// Where the host reaches the VM's guest through Firecracker, while it runs.
    pub fn vsock(&self, name: &str) -> Result<PathBuf> {
        Ok(vsock(&self.existing(name)?))
    }

    /// Blocks until nothing holds the VM: what a stop waits on.
    pub fn wait_idle(&self, name: &str) -> Result<()> {
        let dir = self.existing(name)?;
        let path = dir.join("lock");
        let file = open_lock(&path, name)?;
        file.lock().map_err(at(&path))
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
        let from = current(&dir)?;
        let generation = next_generation(&dir)?;
        let disk = disk_of(&dir, generation);
        clone(&disk_of(&dir, from), &disk)?;
        let name = name.to_string();
        Ok(Run {
            from,
            generation,
            disk,
            name,
            dir,
            keep: self.keep,
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

    /// Deletes the disk generations a VM no longer needs: all but the current
    /// one, every checkpoint, and the newest few others. A run
    /// holding the VM may be using any of them, so that's `Busy`, and a dry run
    /// only says what would go.
    pub fn gc(&self, name: &str, dry_run: bool) -> Result<Freed> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        collect(name, &dir, self.keep, dry_run)
    }

    /// Freezes a copy of the current disk for `restore` to bring back. Nothing
    /// boots a checkpoint or writes to it; the VM carries on from its own disk.
    pub fn checkpoint(&self, name: &str, comment: Option<&str>) -> Result<Checkpoint> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        let current = current(&dir)?;
        let id = next_generation(&dir)?;
        clone(&disk_of(&dir, current), &disk_of(&dir, id))?;
        // Written last: a disk without its note is a leftover nothing lists.
        let note = Note {
            time: now(),
            comment: comment.map(str::to_string),
            auto: false,
        };
        write_note(&dir, id, &note)?;
        read_checkpoint(&dir, id, Some(note), false)
    }

    /// Every checkpoint and every generation in history, newest first.
    pub fn checkpoints(&self, name: &str) -> Result<Vec<Checkpoint>> {
        let dir = self.existing(name)?;
        let gens = history(&dir)?;
        let current = gens.last().copied();
        let mut out = Vec::new();
        for id in generations(&dir)? {
            let note = read_note(&dir, id)?;
            if gens.contains(&id) || note.as_ref().is_some_and(|n| !n.auto) {
                out.push(read_checkpoint(&dir, id, note, current == Some(id))?);
            }
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.id));
        Ok(out)
    }

    /// Makes a copy of checkpoint `id` the current disk. The copy goes on the
    /// end of history, so `undo` goes back to the disk from before the restore,
    /// and the checkpoint stays as it was, to restore again.
    pub fn restore(&self, name: &str, id: u64) -> Result<Vm> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        let mut gens = history(&dir)?;
        let frozen = read_note(&dir, id)?.is_some_and(|n| !n.auto);
        if !(gens.contains(&id) || frozen) || !disk_of(&dir, id).is_file() {
            return Err(Error::NoCheckpoint(name.to_string(), id));
        }
        let generation = next_generation(&dir)?;
        clone(&disk_of(&dir, id), &disk_of(&dir, generation))?;
        write_note(&dir, generation, &Note::auto(&format!("restore of {id}")))?;
        gens.push(generation);
        write_history(&dir, &gens)?;
        read_vm(name, &dir)
    }

    /// The credentials the VM gets at boot: what its allowlist names, or
    /// `creds::DEFAULT` if it has none. Names only; no value is kept here.
    pub fn creds(&self, name: &str) -> Result<Vec<Cred>> {
        let creds = read_creds(&self.existing(name)?.join("creds"))?;
        Ok(creds.unwrap_or_else(|| DEFAULT.to_vec()))
    }

    /// The credentials the VM's guest took this boot, by name: None until it
    /// has, and once its boot is over. A boot that was killed leaves its
    /// record until the next one, so this counts only while the VM is up.
    pub fn handed(&self, name: &str) -> Result<Option<Vec<Cred>>> {
        read_creds(&self.existing(name)?.join(HANDED))
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
    /// The generation this run's disk was cloned from.
    pub from: u64,
    pub generation: u64,
    pub disk: PathBuf,
    name: String,
    dir: PathBuf,
    keep: usize,
    _lock: File,
}

impl Run {
    /// What the run changed: its disk against the generation it was cloned
    /// from, read from the disks, at most `cap` paths. Measure before
    /// `commit`, which takes the run.
    pub fn changes(&self, cap: usize) -> changes::Result<Changes> {
        changes::between(
            &disk_of(&self.dir, self.from),
            &self.disk,
            cap,
            changes::TIME,
        )
    }

    /// Makes this run's disk current, then collects the generations the store
    /// doesn't keep. That gc runs under the run's lock, so it never races one.
    pub fn commit(self) -> Result<Vm> {
        // Firecracker leaves the guest's writes in the page cache, and history
        // mustn't point at a disk a power cut could tear.
        File::open(&self.disk)
            .and_then(|f| f.sync_all())
            .map_err(at(&self.disk))?;
        write_note(&self.dir, self.generation, &Note::auto("run"))?;
        let mut gens = history(&self.dir)?;
        gens.push(self.generation);
        write_history(&self.dir, &gens)?;
        // The commit stands either way; the next one tries again.
        if let Err(e) = collect(&self.name, &self.dir, self.keep, false) {
            eprintln!("runner: {} is committed, but gc failed: {e}", self.name);
        }
        read_vm(&self.name, &self.dir)
    }

    /// Ends a run that never got going, with its disk: nothing wrote to it,
    /// so there's nothing to look at.
    pub fn discard(self) {
        if let Err(e) = fs::remove_file(&self.disk) {
            eprintln!("runner: couldn't remove {}: {e}", self.disk.display());
        }
    }

    /// The run's lock again, for the process that runs the VM to hold too:
    /// the VM stays held until both have exited.
    pub fn hold(&self) -> Result<File> {
        self._lock.try_clone().map_err(at(&self.dir.join("lock")))
    }

    pub fn api_socket(&self) -> PathBuf {
        api_socket(&self.dir)
    }

    pub fn vsock(&self) -> PathBuf {
        vsock(&self.dir)
    }

    /// Where the guest asks for its credentials: Firecracker passes a guest's
    /// connection to host port P on to `vsock.sock_P`.
    pub fn creds_socket(&self) -> PathBuf {
        let mut path = vsock(&self.dir).into_os_string();
        path.push(format!("_{}", creds::PORT));
        path.into()
    }

    /// Where this boot notes which credentials its guest took.
    pub fn handed(&self) -> Handed {
        Handed {
            dir: self.dir.clone(),
        }
    }
}

const HANDED: &str = "handed";

/// A boot's note of which credentials its guest took, for `runner status`,
/// which runs in another process. Names only.
#[derive(Debug, Clone)]
pub struct Handed {
    dir: PathBuf,
}

impl Handed {
    pub fn record(&self, creds: &[Cred]) -> Result<()> {
        let text: String = creds.iter().map(|c| format!("{c}\n")).collect();
        write_aside(&self.dir, HANDED, &text)
    }

    pub fn clear(&self) -> Result<()> {
        let path = self.dir.join(HANDED);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(at(&path)(e)),
            _ => Ok(()),
        }
    }
}

// A list of credential names, one per line, sorted; None if there's no file.
fn read_creds(path: &Path) -> Result<Option<Vec<Cred>>> {
    let text = match fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(at(path))?,
    };
    let bad = |e| at(path)(io::Error::new(io::ErrorKind::InvalidData, e));
    let mut creds = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().map_err(bad))
        .collect::<Result<Vec<Cred>>>()?;
    creds.sort();
    creds.dedup();
    Ok(Some(creds))
}

fn api_socket(dir: &Path) -> PathBuf {
    dir.join("api.sock")
}

fn vsock(dir: &Path) -> PathBuf {
    dir.join("vsock.sock")
}

// The caller holds the VM's lock. History is rewritten before any disk goes,
// so a gc killed partway leaves only disks nothing points at, which the next
// one deletes.
fn collect(name: &str, dir: &Path, keep: usize, dry_run: bool) -> Result<Freed> {
    let mut gens = history(dir)?;
    let current = current(dir)?;
    let mut loose = generations(dir)?;
    // A checkpoint stays until `restore` or a delete of the VM; only the
    // generations runs, imports and restores made are gc's to drop.
    let mut kept = Vec::new();
    for &g in &loose {
        if g == current || read_note(dir, g)?.is_some_and(|n| !n.auto) {
            kept.push(g);
        }
    }
    loose.retain(|g| !kept.contains(g));
    // In keeping order: undo's way back first, then the disks history doesn't
    // name (failed and undone runs'), each newest first. A crash loop's
    // disks can't push out what undo needs.
    let mut others = Vec::new();
    for &g in gens.iter().rev() {
        if let Some(i) = loose.iter().position(|&l| l == g) {
            others.push(loose.swap_remove(i));
        }
    }
    loose.sort_unstable_by(|a, b| b.cmp(a));
    others.extend(loose);
    let mut doomed = others.split_off(keep.min(others.len()));
    doomed.sort_unstable();
    let mut bytes = 0;
    for &g in &doomed {
        let disk = disk_of(dir, g);
        bytes += allocated(&fs::metadata(&disk).map_err(at(&disk))?);
    }
    if !dry_run && !doomed.is_empty() {
        let before = gens.len();
        gens.retain(|g| !doomed.contains(g));
        if gens.len() != before {
            write_history(dir, &gens)?;
        }
        for &g in &doomed {
            let disk = disk_of(dir, g);
            fs::remove_file(&disk).map_err(at(&disk))?;
            let note = note_of(dir, g);
            match fs::remove_file(&note) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(at(&note)(e)),
                _ => {}
            }
        }
        sync_dir(&dir.join("gens"))?;
    }
    let name = name.to_string();
    Ok(Freed {
        name,
        generations: doomed,
        bytes,
    })
}

// flock, not a pid file: the kernel drops it when the holder dies, so a
// crashed run never wedges its VM.
fn take_lock(dir: &Path, name: &str) -> Result<File> {
    let path = dir.join("lock");
    let file = open_lock(&path, name)?;
    // A process spawned by another thread holds a copy of every open fd from
    // fork to exec, so a lock just let go can look held for that instant.
    // Ask again for a moment before calling the VM busy.
    for _ in 0..LOCK_TRIES {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => std::thread::sleep(LOCK_RETRY),
            Err(TryLockError::Error(e)) => return Err(at(&path)(e)),
        }
    }
    Err(Error::Busy(name.to_string()))
}

const LOCK_TRIES: u32 = 20;
const LOCK_RETRY: Duration = Duration::from_millis(5);

fn open_lock(path: &Path, name: &str) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| match e.kind() {
            // The VM was deleted between the caller's check and here.
            io::ErrorKind::NotFound => Error::NoVm(name.to_string()),
            _ => at(path)(e),
        })
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

// No file means every default, which is what an imported VM has.
fn read_config(dir: &Path) -> Result<Config> {
    let path = dir.join("config.json");
    match fs::read(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(at(&path)(e)),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| at(&path)(io::Error::new(io::ErrorKind::InvalidData, e))),
    }
}

// Only into a VM being built, so no reader sees it half-written.
fn write_config(dir: &Path, config: &Config) -> Result<()> {
    if *config == Config::default() {
        return Ok(());
    }
    let path = dir.join("config.json");
    let mut text = serde_json::to_string(config).map_err(|e| at(&path)(io::Error::other(e)))?;
    text.push('\n');
    let mut f = File::create(&path).map_err(at(&path))?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(at(&path))
}

fn read_vm(name: &str, dir: &Path) -> Result<Vm> {
    let generation = current(dir)?;
    let disk = disk_of(dir, generation);
    let bytes = fs::metadata(&disk).map_err(at(&disk))?.len();
    let gens = dir.join("gens");
    let mut used = 0;
    for entry in fs::read_dir(&gens).map_err(at(&gens))? {
        match entry.and_then(|e| e.metadata()) {
            Ok(meta) => used += allocated(&meta),
            // A gc in another process deleted it after read_dir saw it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(at(&gens)(e)),
        }
    }
    let name = name.to_string();
    Ok(Vm {
        name,
        generation,
        bytes,
        used,
    })
}

// st_blocks is in 512-byte units whatever the filesystem's block size.
fn allocated(meta: &fs::Metadata) -> u64 {
    meta.blocks() * 512
}

fn disk_of(dir: &Path, generation: u64) -> PathBuf {
    dir.join("gens").join(format!("{generation}.ext4"))
}

fn note_of(dir: &Path, generation: u64) -> PathBuf {
    dir.join("gens").join(format!("{generation}.json"))
}

#[derive(Serialize, Deserialize)]
struct Note {
    time: DateTime<Utc>,
    comment: Option<String>,
    auto: bool,
}

impl Note {
    fn auto(why: &str) -> Note {
        let comment = Some(why.to_string());
        Note {
            time: now(),
            comment,
            auto: true,
        }
    }
}

fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(0)
}

fn write_note(dir: &Path, generation: u64, note: &Note) -> Result<()> {
    let path = note_of(dir, generation);
    let text = serde_json::to_string(note).map_err(|e| at(&path)(e.into()))?;
    write_aside(&dir.join("gens"), &format!("{generation}.json"), &text)
}

// None for a generation made before notes were written, or most failed runs'.
fn read_note(dir: &Path, generation: u64) -> Result<Option<Note>> {
    let path = note_of(dir, generation);
    let text = match fs::read(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(at(&path))?,
    };
    let note = serde_json::from_slice(&text).map_err(|e| at(&path)(e.into()))?;
    Ok(Some(note))
}

fn read_checkpoint(dir: &Path, id: u64, note: Option<Note>, current: bool) -> Result<Checkpoint> {
    let disk = disk_of(dir, id);
    let meta = fs::metadata(&disk).map_err(at(&disk))?;
    // Without a note, the disk's own time is the best guess.
    let note = match note {
        Some(note) => note,
        None => Note {
            time: DateTime::<Utc>::from(meta.modified().map_err(at(&disk))?).trunc_subsecs(0),
            comment: None,
            auto: true,
        },
    };
    Ok(Checkpoint {
        id,
        time: note.time,
        comment: note.comment,
        auto: note.auto,
        bytes: meta.len(),
        current,
    })
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

// Past every disk and note on hand, failed runs' included, so none is
// overwritten and no new disk is read with an old note.
fn next_generation(dir: &Path) -> Result<u64> {
    let mut all = numbered(dir, ".ext4")?;
    all.extend(numbered(dir, ".json")?);
    Ok(all.into_iter().max().unwrap_or(0) + 1)
}

// Every disk on hand, in no order.
fn generations(dir: &Path) -> Result<Vec<u64>> {
    numbered(dir, ".ext4")
}

fn numbered(dir: &Path, suffix: &str) -> Result<Vec<u64>> {
    let gens = dir.join("gens");
    let mut out = Vec::new();
    for entry in fs::read_dir(&gens).map_err(at(&gens))? {
        let name = entry.map_err(at(&gens))?.file_name();
        let n = name
            .to_str()
            .and_then(|s| s.strip_suffix(suffix)?.parse::<u64>().ok());
        out.extend(n);
    }
    Ok(out)
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
