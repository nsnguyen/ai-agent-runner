// The filesystem is the database: each VM is a directory.
//
//   <root>/vms/<name>/gens/<n>.ext4  one disk per generation; a run writes a new one
//   <root>/vms/<name>/gens/<n>.json  its note: when it was made, a comment, and whether it's a checkpoint
//   <root>/vms/<name>/gens/<n>.part  a running VM's checkpoint being copied, renamed to <n>.ext4 when done
//   <root>/vms/<name>/history        committed generations, one per line; the last is current
//   <root>/vms/<name>/lock           flock'd by the process running the VM, one at a time
//   <root>/vms/<name>/freeze.lock    flock'd while a running VM's checkpoint is made, one at a time
//   <root>/vms/<name>/use.lock       flock'd shared by each command using the VM, and touched; exclusive by its boot as it sleeps it (idle.rs)
//   <root>/vms/<name>/config.json    sizes, kernel and idle time, only where they differ from the defaults
//   <root>/vms/<name>/api.sock       Firecracker's API, while the VM runs
//   <root>/vms/<name>/vsock.sock     the guest's vsock, while the VM runs
//   <root>/vms/<name>/vsock.sock_*   where its guest takes its credentials (creds::PORT) and makes its calls (calls::PORT), while it runs
//   <root>/vms/<name>/boot.sock      where its boot takes sleep, wake and halt (sleep.rs), while it's up
//   <root>/vms/<name>/snapshot/      while it sleeps: Firecracker's vmstate and mem, then slept.json, the generation they run on
//   <root>/vms/<name>/creds          the credentials it gets at boot, by name; never a value
//   <root>/vms/<name>/handed         the ones its guest took this boot, by name; never a value
//   <root>/vms/<name>/slot           its network slot, unique among live VMs (net.rs)
//   <root>/vms/<name>/rollback       a run's ask that its boot not commit, naming the run
//   <root>/vms/<name>/booted         the generations of its latest boot: the one it cloned, and its own
//   <root>/vms/<name>/runs/          its headless runs: their streams, how each ended, and its result (runs.rs)
//   <root>/vms/.new-<name>-<pid>/    a VM being created, renamed into place when done
//   <root>/vms/.del-<name>-<pid>/    a VM being deleted, renamed out of the way first
//   <root>/kernel/vmlinux            the kernel a VM boots unless its config names one
//   <root>/slots.lock                flock'd while a slot is picked
//
// The range slots carve up is the host's, in /etc/runner/net (host.rs).
use crate::calls;
use crate::creds::{self, Cred, DEFAULT};
use crate::idle::Idle;
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
    #[error("{0} is busy: it's running or sleeping, or another runner command holds it")]
    Busy(String),
    #[error("{0} has no earlier generation to go back to")]
    NothingToUndo(String),
    #[error("{0} has no checkpoint {1}")]
    NoCheckpoint(String, u64),
    #[error("{0} has never booted, so it has no running disk to checkpoint")]
    NeverBooted(String),
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

pub(crate) fn at(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
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
    /// Made by the agent in the VM, with `runner-guest checkpoint`.
    pub agent: bool,
    /// Size of its disk.
    pub bytes: u64,
    /// The disk the next boot starts from.
    pub current: bool,
}

impl Checkpoint {
    /// One line, as `runner checkpoints` and `runner-guest checkpoints` list
    /// it. The comment is quoted, so one with a newline in it still takes
    /// one line.
    pub fn row(&self) -> String {
        let mark = if self.current { '*' } else { ' ' };
        let time = self.time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let kind = match (self.auto, self.agent) {
            (true, _) => "auto",
            (false, true) => "agent",
            (false, false) => "checkpoint",
        };
        let comment = self.comment.as_ref().map(|c| format!("  {c:?}"));
        let comment = comment.unwrap_or_default();
        format!(
            "{mark} {}  {time}  {kind}  {} bytes{comment}",
            self.id, self.bytes
        )
    }
}

/// A VM's sizes, kernel and idle time as stored: each one left out is the
/// default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<PathBuf>,
    /// How long it goes unused before it sleeps (idle.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<Idle>,
}

impl Config {
    /// `self`, with whatever `over` sets taking its place.
    fn with(self, over: &Config) -> Config {
        Config {
            cpus: over.cpus.or(self.cpus),
            mem_mib: over.mem_mib.or(self.mem_mib),
            kernel: over.kernel.clone().or(self.kernel),
            idle: over.idle.or(self.idle),
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

    /// How long the VM goes unused before its boot puts it to sleep.
    pub fn idle(&self, name: &str) -> Result<Idle> {
        Ok(self.config(name)?.idle.unwrap_or_default())
    }

    /// Sets how long the VM goes unused before it sleeps. A running VM's boot
    /// reads it at each look, so it counts from now on.
    pub fn set_idle(&self, name: &str, idle: Idle) -> Result<Idle> {
        let dir = self.existing(name)?;
        let config = Config {
            idle: Some(idle),
            ..read_config(&dir)?
        };
        let path = dir.join(CONFIG);
        let mut text =
            serde_json::to_string(&config).map_err(|e| at(&path)(io::Error::other(e)))?;
        text.push('\n');
        write_aside(&dir, CONFIG, &text).map_err(|e| match e {
            // Deleted between the check and the write.
            Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                Error::NoVm(name.to_string())
            }
            e => e,
        })?;
        Ok(idle)
    }

    /// The lock each command using the VM holds (idle.rs).
    pub fn use_lock(&self, name: &str) -> Result<PathBuf> {
        Ok(self.existing(name)?.join("use.lock"))
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

    /// Where the VM's boot takes sleep, wake and halt (src/sleep.rs).
    pub fn boot_socket(&self, name: &str) -> Result<PathBuf> {
        Ok(boot_socket(&self.existing(name)?))
    }

    /// Where the VM's snapshot goes when it sleeps.
    pub fn snapshot(&self, name: &str) -> Result<Snapshot> {
        Ok(snapshot(&self.existing(name)?))
    }

    /// The note of the VM's snapshot, if it has one: it's sleeping, or on its
    /// way, if its boot is up. A boot that was killed asleep leaves one until
    /// the next boot clears it.
    pub fn sleeping(&self, name: &str) -> Result<Option<Slept>> {
        self.snapshot(name)?.slept()
    }

    /// Blocks until nothing holds the VM: what a stop waits on.
    pub fn wait_idle(&self, name: &str) -> Result<()> {
        let dir = self.existing(name)?;
        let path = dir.join("lock");
        let file = open_lock(&path, name)?;
        file.lock().map_err(at(&path))
    }

    /// Whether something holds the VM now: a boot, or a command changing it.
    /// Asked once: a lock that's just been let go can look held for an
    /// instant (take_lock), so a caller that must know asks again.
    pub fn held(&self, name: &str) -> Result<bool> {
        let path = self.existing(name)?.join("lock");
        match open_lock(&path, name)?.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(at(&path)(e)),
        }
    }

    /// Where the VM's headless runs keep their records (runs.rs).
    pub fn runs(&self, name: &str) -> Result<PathBuf> {
        Ok(self.existing(name)?.join("runs"))
    }

    /// Asks the boot running the VM to roll back, not commit, however its
    /// guest shuts down: what a run holds until it ends cleanly. `by` names
    /// who asked, for boot's note. The next boot drops an ask no boot took.
    pub fn ask_rollback(&self, name: &str, by: &str) -> Result<()> {
        write_aside(&self.existing(name)?, ROLLBACK, &format!("{by}\n"))
    }

    /// The generations begin_run last noted, for a headless run in another
    /// process to know which disk its boot runs; the disk may be gone since,
    /// if that boot never got going. None if the VM never booted.
    pub fn booted(&self, name: &str) -> Result<Option<Booted>> {
        read_booted(&self.existing(name)?)
    }

    /// The disk of one of the VM's generations, whether or not it's still
    /// there.
    pub fn disk(&self, name: &str, generation: u64) -> Result<PathBuf> {
        Ok(disk_of(&self.existing(name)?, generation))
    }

    /// Takes back an ask_rollback, so a clean shutdown commits again.
    pub fn withdraw_rollback(&self, name: &str) -> Result<()> {
        remove_if_there(&self.existing(name)?.join(ROLLBACK))
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
    /// from, and notes both generations (Store::booted). Only `Run::commit`
    /// makes it current; dropping the run keeps the old one current and
    /// leaves the new disk for a look.
    pub fn begin_run(&self, name: &str) -> Result<Run> {
        let dir = self.existing(name)?;
        let lock = take_lock(&dir, name)?;
        // An ask for an earlier boot that never took it isn't this run's.
        remove_if_there(&dir.join(ROLLBACK))?;
        let from = current(&dir)?;
        let generation = next_generation(&dir)?;
        let disk = disk_of(&dir, generation);
        clone(&disk_of(&dir, from), &disk)?;
        let booted = Booted { from, generation };
        let text = serde_json::to_string(&booted).map_err(|e| at(&dir.join(BOOTED))(e.into()))?;
        if let Err(e) = write_aside(&dir, BOOTED, &text) {
            remove_if_there(&disk)?;
            return Err(e);
        }
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
        let note = Note::frozen(comment, false);
        write_note(&dir, id, &note)?;
        read_checkpoint(&dir, id, Some(note), false)
    }

    /// Waits until no other checkpoint of a running VM is being made, then
    /// holds off the rest until the Freezer is dropped. A running VM's boot
    /// holds the VM's own lock, so its checkpoints take turns on this one.
    pub fn hold_checkpoints(&self, name: &str) -> Result<Freezer> {
        let dir = self.existing(name)?;
        let path = dir.join(FREEZE_LOCK);
        let lock = open_lock(&path, name)?;
        lock.lock().map_err(at(&path))?;
        let name = name.to_string();
        Ok(Freezer {
            name,
            dir,
            _lock: lock,
        })
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
        check_restorable(name, &dir, id)?;
        let generation = next_generation(&dir)?;
        clone(&disk_of(&dir, id), &disk_of(&dir, generation))?;
        write_note(&dir, generation, &Note::auto(&format!("restore of {id}")))?;
        gens.push(generation);
        write_history(&dir, &gens)?;
        read_vm(name, &dir)
    }

    /// Puts a restore staged by Freezer::stage_restore on top of history, as
    /// `restore` would have: `undo` goes back to the disk from before it.
    /// Needs the VM stopped.
    pub fn finish_restore(&self, name: &str, staged: &Staged) -> Result<Vm> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        let note = Note::auto(&format!("restore of {}", staged.from));
        write_note(&dir, staged.generation, &note)?;
        let mut gens = history(&dir)?;
        gens.push(staged.generation);
        write_history(&dir, &gens)?;
        read_vm(name, &dir)
    }

    /// Deletes a staged restore that didn't go ahead.
    pub fn drop_restore(&self, name: &str, staged: Staged) -> Result<()> {
        let dir = self.existing(name)?;
        remove_if_there(&disk_of(&dir, staged.generation))?;
        remove_if_there(&note_of(&dir, staged.generation))
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
    /// Makes this run's disk current, then collects the generations the store
    /// doesn't keep. That gc runs under the run's lock, so it never races one.
    pub fn commit(self) -> Result<Vm> {
        self.commit_then(None)
    }

    /// `commit`, then, if given, a restore staged while the VM ran, on top,
    /// as Store::finish_restore would put it: `undo` goes back to this run's
    /// disk. Both go into history at once, under this run's lock: a guest's
    /// own restore (src/calls.rs), which nothing can come between.
    pub fn commit_then(self, restore: Option<&Staged>) -> Result<Vm> {
        // Firecracker leaves the guest's writes in the page cache, and history
        // mustn't point at a disk a power cut could tear.
        File::open(&self.disk)
            .and_then(|f| f.sync_all())
            .map_err(at(&self.disk))?;
        write_note(&self.dir, self.generation, &Note::auto("run"))?;
        let mut gens = history(&self.dir)?;
        gens.push(self.generation);
        if let Some(staged) = restore {
            let note = Note::auto(&format!("restore of {}", staged.from));
            write_note(&self.dir, staged.generation, &note)?;
            gens.push(staged.generation);
        }
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

    /// Who asked for this run to roll back (Store::ask_rollback), if anyone
    /// did. Taking it clears the ask.
    pub fn take_rollback(&self) -> Result<Option<String>> {
        let path = self.dir.join(ROLLBACK);
        let by = match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            r => r.map_err(at(&path))?,
        };
        remove_if_there(&path)?;
        Ok(Some(by.trim().to_string()))
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
        port_socket(&self.dir, creds::PORT)
    }

    /// Where the guest makes its calls: checkpoint, list, restore.
    pub fn calls_socket(&self) -> PathBuf {
        port_socket(&self.dir, calls::PORT)
    }

    /// Where this boot takes sleep, wake and halt.
    pub fn boot_socket(&self) -> PathBuf {
        boot_socket(&self.dir)
    }

    /// Where this boot's VM sleeps.
    pub fn snapshot(&self) -> Snapshot {
        snapshot(&self.dir)
    }

    /// Where this boot notes which credentials its guest took.
    pub fn handed(&self) -> Handed {
        Handed {
            dir: self.dir.clone(),
        }
    }
}

/// A running VM's checkpoints, one at a time (Store::hold_checkpoints).
#[derive(Debug)]
pub struct Freezer {
    name: String,
    dir: PathBuf,
    _lock: File,
}

impl Freezer {
    /// Freezes a copy of the disk the VM's latest boot runs (Store::booted),
    /// for `restore` to bring back: a running VM's checkpoint, as
    /// Store::checkpoint is a stopped one's. Pause the VM first, so the copy
    /// is of one moment. The VM carries on from its own disk, which it
    /// commits or rolls back at shutdown as it would have.
    ///
    /// The note goes first, which takes the generation's number, so a boot
    /// that starts while the copy goes on, Firecracker having been killed,
    /// can't take it too. The copy is made aside and renamed into place, so
    /// nothing lists a half-made one, and the next checkpoint clears what one
    /// killed partway left.
    pub fn freeze(&self, comment: Option<&str>) -> Result<Checkpoint> {
        self.freeze_noted(Note::frozen(comment, false))
    }

    /// `freeze`, for the agent in the VM, which asks through its boot
    /// (src/calls.rs): noted as the agent's, which `agent_since` counts.
    /// gc keeps it as it keeps every checkpoint.
    pub fn freeze_for_agent(&self, comment: Option<&str>) -> Result<Checkpoint> {
        self.freeze_noted(Note::frozen(comment, true))
    }

    /// When each checkpoint the agent made after `since` was made, oldest
    /// first. Under the freezer, so a count and the checkpoint it allows
    /// can't be split by another.
    pub fn agent_since(&self, since: DateTime<Utc>) -> Result<Vec<DateTime<Utc>>> {
        let mut times = Vec::new();
        for id in generations(&self.dir)? {
            if let Some(note) = read_note(&self.dir, id)?
                && note.agent
                && !note.auto
                && note.time > since
            {
                times.push(note.time);
            }
        }
        times.sort();
        Ok(times)
    }

    fn freeze_noted(&self, note: Note) -> Result<Checkpoint> {
        let booted = read_booted(&self.dir)?;
        let booted = booted.ok_or_else(|| Error::NeverBooted(self.name.clone()))?;
        let id = self.copy(booted.generation, &note)?;
        read_checkpoint(&self.dir, id, Some(note), false)
    }

    /// Copies checkpoint `id` aside for a restore that has to stop the VM
    /// first, before anything stops: a mistyped id stops nothing, and the
    /// stop's commit, whose gc may drop `id`, can't take it from the restore.
    /// The copy is noted as a checkpoint, which gc keeps, until
    /// Store::finish_restore puts it in history; drop_restore deletes it.
    pub fn stage_restore(&self, id: u64) -> Result<Staged> {
        check_restorable(&self.name, &self.dir, id)?;
        let note = Note::frozen(Some(&format!("restore of {id}")), false);
        let generation = self.copy(id, &note)?;
        Ok(Staged {
            from: id,
            generation,
        })
    }

    // A copy of generation `from` under a new number, with its note.
    fn copy(&self, from: u64, note: &Note) -> Result<u64> {
        let gens = self.dir.join("gens");
        for left in numbered(&self.dir, PART)? {
            let part = part_of(&self.dir, left);
            fs::remove_file(&part).map_err(at(&part))?;
            if !disk_of(&self.dir, left).exists() {
                remove_if_there(&note_of(&self.dir, left))?;
            }
        }
        let id = next_generation(&self.dir)?;
        write_note(&self.dir, id, note)?;
        let (part, disk) = (part_of(&self.dir, id), disk_of(&self.dir, id));
        let made = clone(&disk_of(&self.dir, from), &part)
            .and_then(|()| fs::rename(&part, &disk).map_err(at(&disk)))
            .and_then(|()| sync_dir(&gens));
        if let Err(e) = made {
            let _ = fs::remove_file(&part);
            let _ = fs::remove_file(note_of(&self.dir, id));
            return Err(e);
        }
        Ok(id)
    }
}

/// A sleeping VM's snapshot (src/sleep.rs): Firecracker's state of it and
/// its guest's memory, and a note, written first, that says the VM is asleep
/// or on its way. Only the boot that took it can wake it: it runs on that
/// boot's disk. The memory holds what the guest held, its credentials too,
/// so only the runner user can read it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    dir: PathBuf,
}

impl Snapshot {
    /// The VM's vCPUs and devices.
    pub fn vmstate(&self) -> PathBuf {
        self.dir.join("vmstate")
    }

    /// The guest's memory, as big as the VM's.
    pub fn mem(&self) -> PathBuf {
        self.dir.join("mem")
    }

    /// Clears the last snapshot, if one is left, for a new one. Its memory
    /// file is unlinked, not written over: a guest woken from it still reads
    /// its pages from it (firecracker::Api::load).
    pub fn make_room(&self) -> Result<()> {
        self.clear()?;
        fs::create_dir_all(&self.dir)
            .and_then(|()| fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700)))
            .map_err(at(&self.dir))
    }

    /// Notes the VM asleep, before Firecracker writes the snapshot.
    pub fn note(&self, slept: &Slept) -> Result<()> {
        let path = self.dir.join(SLEPT);
        let text = serde_json::to_string(slept).map_err(|e| at(&path)(e.into()))?;
        write_aside(&self.dir, SLEPT, &text)
    }

    /// The note, if the VM is asleep or on its way.
    pub fn slept(&self) -> Result<Option<Slept>> {
        let path = self.dir.join(SLEPT);
        let text = match fs::read(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            r => r.map_err(at(&path))?,
        };
        let slept = serde_json::from_slice(&text).map_err(|e| at(&path)(e.into()))?;
        Ok(Some(slept))
    }

    /// Deletes it, note first, so a snapshot cleared partway isn't asleep.
    pub fn clear(&self) -> Result<()> {
        remove_if_there(&self.dir.join(SLEPT))?;
        remove_if_there(&self.vmstate())?;
        remove_if_there(&self.mem())
    }
}

/// A snapshot's note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slept {
    /// The disk generation its guest runs on: its boot's own (Booted).
    pub generation: u64,
    /// When it went to sleep.
    pub at: DateTime<Utc>,
}

const SLEPT: &str = "slept.json";

/// A restore's copy of a checkpoint, made while the VM still ran
/// (Freezer::stage_restore).
#[derive(Debug)]
pub struct Staged {
    /// The checkpoint it's a copy of.
    pub from: u64,
    /// The copy's own generation.
    pub generation: u64,
}

// What `restore` takes: a checkpoint, or a generation in history, whose disk
// is there.
fn check_restorable(name: &str, dir: &Path, id: u64) -> Result<()> {
    let frozen = read_note(dir, id)?.is_some_and(|n| !n.auto);
    if !(history(dir)?.contains(&id) || frozen) || !disk_of(dir, id).is_file() {
        return Err(Error::NoCheckpoint(name.to_string(), id));
    }
    Ok(())
}

const HANDED: &str = "handed";
const ROLLBACK: &str = "rollback";
const BOOTED: &str = "booted";
const FREEZE_LOCK: &str = "freeze.lock";
const PART: &str = ".part";

/// A boot's generations (Store::booted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Booted {
    /// The generation it cloned.
    pub from: u64,
    /// Its own: the one it runs, and commits if the VM shuts down cleanly.
    pub generation: u64,
}

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
        remove_if_there(&self.dir.join(HANDED))
    }
}

fn remove_if_there(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(at(path)(e)),
        _ => Ok(()),
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

fn boot_socket(dir: &Path) -> PathBuf {
    dir.join("boot.sock")
}

fn snapshot(dir: &Path) -> Snapshot {
    Snapshot {
        dir: dir.join("snapshot"),
    }
}

fn port_socket(dir: &Path, port: u32) -> PathBuf {
    let mut path = vsock(dir).into_os_string();
    path.push(format!("_{port}"));
    path.into()
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
    if try_lock(&file).map_err(at(&path))? {
        Ok(file)
    } else {
        Err(Error::Busy(name.to_string()))
    }
}

/// Takes `file`'s flock unless something else holds it; false if it does.
// A process spawned by another thread holds a copy of every open fd from
// fork to exec, so a lock just let go can look held for that instant. Ask
// again for a moment before calling it held.
pub(crate) fn try_lock(file: &File) -> io::Result<bool> {
    for _ in 0..LOCK_TRIES {
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => std::thread::sleep(LOCK_RETRY),
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(false)
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
const CONFIG: &str = "config.json";

fn read_config(dir: &Path) -> Result<Config> {
    let path = dir.join(CONFIG);
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
    let path = dir.join(CONFIG);
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

fn part_of(dir: &Path, generation: u64) -> PathBuf {
    dir.join("gens").join(format!("{generation}{PART}"))
}

fn read_booted(dir: &Path) -> Result<Option<Booted>> {
    let path = dir.join(BOOTED);
    let text = match fs::read(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(at(&path))?,
    };
    let booted = serde_json::from_slice(&text).map_err(|e| at(&path)(e.into()))?;
    Ok(Some(booted))
}

#[derive(Serialize, Deserialize)]
struct Note {
    time: DateTime<Utc>,
    comment: Option<String>,
    auto: bool,
    // Left out unless set, so a runner from before it reads the note the same.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    agent: bool,
}

impl Note {
    fn auto(why: &str) -> Note {
        let comment = Some(why.to_string());
        Note {
            time: now(),
            comment,
            auto: true,
            agent: false,
        }
    }

    // A checkpoint's: kept by gc, made now.
    fn frozen(comment: Option<&str>, agent: bool) -> Note {
        Note {
            time: now(),
            comment: comment.map(str::to_string),
            auto: false,
            agent,
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
            agent: false,
        },
    };
    Ok(Checkpoint {
        id,
        time: note.time,
        comment: note.comment,
        auto: note.auto,
        agent: note.agent,
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
pub(crate) fn write_aside(dir: &Path, name: &str, text: &str) -> Result<()> {
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
    all.extend(numbered(dir, PART)?);
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
