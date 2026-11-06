// The filesystem is the database: each VM is a directory.
//
//   <root>/vms/<name>/gens/<n>.ext4  one disk per generation; a run writes a new one
//   <root>/vms/<name>/gens/<n>.json  its note: when it was made, a comment, whether it's a checkpoint and whose, and what an imported disk says it was built from (image.rs)
//   <root>/vms/<name>/gens/<n>.part  a running VM's checkpoint being copied, renamed to <n>.ext4 when done; or its export's copy, unlinked once open; or a rebase's disk being made
//   <root>/vms/<name>/gens/<n>.part.graft/  the files a rebase is copying into its part (graft.rs)
//   <root>/vms/<name>/history        committed generations, one per line; the last is current
//   <root>/vms/<name>/lock           flock'd by the process running the VM, one at a time
//   <root>/vms/<name>/freeze.lock    flock'd while a running VM's checkpoint is made, one at a time
//   <root>/vms/<name>/use.lock       flock'd shared by each command using the VM, and touched; exclusive by its boot as it sleeps it (idle.rs)
//   <root>/vms/<name>/config.json    sizes, kernel, idle time and URL, only where they differ from the defaults
//   <root>/vms/<name>/api.sock       Firecracker's API, while the VM runs
//   <root>/vms/<name>/vsock.sock     the guest's vsock, while the VM runs
//   <root>/vms/<name>/vsock.sock_*   where its guest takes its credentials (creds::PORT) and makes its calls (calls::PORT), while it runs
//   <root>/vms/<name>/boot.sock      where its boot takes sleep, wake, halt and park (sleep.rs), while it's up
//   <root>/vms/<name>/snapshot/      while it sleeps: Firecracker's vmstate and mem, then slept.json, the generation they run on and whether a host shutdown parked it
//   <root>/vms/<name>/creds          the credentials it gets at boot, by name; never a value
//   <root>/vms/<name>/handed         the ones its guest took this boot, by name; never a value
//   <root>/vms/<name>/slot           its network slot, unique among live VMs (net.rs)
//   <root>/vms/<name>/rollback       a run's ask that its boot not commit, naming the run
//   <root>/vms/<name>/booted         the generations of its latest boot: the one it cloned, and its own
//   <root>/vms/<name>/runs/          its headless runs: their streams, how each ended, and its result (runs.rs)
//   <root>/vms/<name>/tries.json     its latest best-of-N batch: what was asked, when, the generation its tries forked, and their names and runs (best.rs)
//   <root>/vms/.new-<name>-<pid>/    a VM being created or imported, renamed into place when done
//   <root>/vms/.del-<name>-<pid>/    a VM being deleted, renamed out of the way first
//   <root>/kernel/vmlinux            the kernel a VM boots unless its config names one
//   <root>/slots.lock                flock'd while a slot is picked
//
// The range slots carve up is the host's, in /etc/runner/net (host.rs).
use crate::calls;
use crate::creds::{self, Cred, DEFAULT};
use crate::idle::Idle;
use crate::image::{self, Build};
use crate::net::{DEFAULT_SUBNET, Net, Subnet};
use crate::url::Setting;
use chrono::{DateTime, SubsecRound, Utc};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The VM `create` forks unless told otherwise.
pub const BASE: &str = "base";

/// How many generations gc keeps besides the current one and the checkpoints.
pub const KEEP: usize = 5;
/// How many of the checkpoints runs and the agent make gc keeps per VM,
/// newest first, the two counted together. The owner's aren't counted: gc
/// keeps every one.
pub const KEEP_CHECKPOINTS: usize = 20;
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
    #[error("{0} has never booted, so it has no running disk to copy")]
    NeverBooted(String),
    #[error("{0} can't be rebased onto itself")]
    OntoItself(String),
    /// Base's current disk didn't come from an import, so a guest may have
    /// written it (Store::rebase).
    #[error(
        "base's current generation, {onto}, came from {made_by:?}, not an import, so a guest may have written it. Import base again first: runner import base --replace <disk>"
    )]
    NotImported { onto: u64, made_by: String },
    #[error("{0}")]
    BadConfig(String),
    #[error(
        "no free address left in {0}: delete a VM, or rerun host/install.sh with a wider RUNNER_NET"
    )]
    NoSlot(Subnet),
    #[error("{cmd}: {why}")]
    Command { cmd: String, why: String },
    /// A disk's stream that ended before the disk did.
    #[error("{0}")]
    Cut(String),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn at(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
    let path = path.to_path_buf();
    move |source| Error::Io { path, source }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Taken by a run before it joined the running VM, which the run's
    /// result names for a restore by hand.
    pub run: bool,
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
        let kind = match (self.auto, self.agent, self.run) {
            (true, ..) => "auto",
            (false, true, _) => "agent",
            (false, false, true) => "run",
            (false, false, false) => "checkpoint",
        };
        let comment = self.comment.as_ref().map(|c| format!("  {c:?}"));
        let comment = comment.unwrap_or_default();
        format!(
            "{mark} {}  {time}  {kind}  {} bytes{comment}",
            self.id, self.bytes
        )
    }
}

/// Who made a checkpoint, which decides how long gc keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maker {
    /// `runner checkpoint` or MCP's checkpoint_vm: kept until the VM is
    /// deleted.
    Owner,
    /// The agent in the VM, with `runner-guest checkpoint`.
    Agent,
    /// A run, before it joins the running VM.
    Run,
}

/// Where a disk brought into the store comes from.
pub enum Source<'a> {
    /// A file, which `cp` copies: a reflink where the filesystem has them,
    /// else a copy that keeps its holes.
    File(&'a Path),
    /// A stream, like stdin. Each block of it that's all zeros becomes a
    /// hole, so a sparse disk stays sparse.
    Stream(&'a mut dyn Read),
}

/// A VM's sizes, kernel, idle time and URL as stored: each one left out is
/// the default.
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
    /// Its URL on the tailnet: on, and to port 8080 in the VM, unless set (url.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Setting>,
}

impl Config {
    /// `self`, with whatever `over` sets taking its place.
    fn with(self, over: &Config) -> Config {
        Config {
            cpus: over.cpus.or(self.cpus),
            mem_mib: over.mem_mib.or(self.mem_mib),
            kernel: over.kernel.clone().or(self.kernel),
            idle: over.idle.or(self.idle),
            url: over.url.or(self.url),
        }
    }

    // Firecracker's limits, checked here so a bad size fails at create, not
    // boot, and a kernel that's there.
    fn check(&self) -> Result<()> {
        self.check_values()?;
        if let Some(kernel) = &self.kernel
            && !kernel.is_file()
        {
            let why = format!("no kernel file at {}", kernel.display());
            return Err(Error::BadConfig(why));
        }
        Ok(())
    }

    // `check`, but for the kernel only that its path is absolute: an import
    // keeps one this host lacks.
    fn check_values(&self) -> Result<()> {
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
        if self.idle == Some(Idle::After(0)) {
            return bad("a VM's idle time is at least a second, or never".to_string());
        }
        if self.url.and_then(|u| u.port) == Some(0) {
            return bad("the port a VM's URL goes to is 1 to 65535, not 0".to_string());
        }
        if let Some(kernel) = &self.kernel
            && !kernel.is_absolute()
        {
            return bad(format!(
                "the kernel path {} isn't absolute",
                kernel.display()
            ));
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

    /// Brings an ext4 disk in as a new VM, usually `base`, noted with what
    /// it says it was built from (image.rs).
    pub fn import(&self, name: &str, disk: &Path) -> Result<Vm> {
        self.import_from(name, Source::File(disk))
    }

    /// `import`, from a file or a stream.
    pub fn import_from(&self, name: &str, disk: Source) -> Result<Vm> {
        let note = |disk: &Path| Note::auto("import").built(image::read(disk));
        self.make(name, disk, &Config::default(), note)
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
        let why = format!("fork of {from}");
        let disk = disk_of(&src, current(&src)?);
        self.make(name, Source::File(&disk), &config, |_| Note::auto(&why))
    }

    /// A fork of generation `generation` of `from`, which has to be on hand,
    /// like a checkpoint: with `from`'s sizes and kernel, and whatever `over`
    /// sets in their place.
    pub fn fork_at(&self, from: &str, generation: u64, name: &str, over: &Config) -> Result<Vm> {
        let src = self.existing(from)?;
        let config = read_config(&src)?.with(over);
        let disk = disk_of(&src, generation);
        if !disk.is_file() {
            return Err(Error::NoCheckpoint(from.to_string(), generation));
        }
        let why = format!("fork of {from}'s generation {generation}");
        self.make(name, Source::File(&disk), &config, |_| Note::auto(&why))
    }

    // A new VM with `disk` as its only generation, noted as `note` says,
    // given it.
    fn make(
        &self,
        name: &str,
        disk: Source,
        config: &Config,
        note: impl FnOnce(&Path) -> Note,
    ) -> Result<Vm> {
        check_name(name)?;
        config.check()?;
        self.make_from(name, config, None, |vm: &NewVm| {
            let at = vm.disk(0);
            bring(disk, &at)?;
            vm.commit(0, &note(&at))
        })
    }

    /// Refuses a name that isn't one, or that a VM has, before anything is
    /// read for it.
    pub fn check_free(&self, name: &str) -> Result<()> {
        check_name(name)?;
        match self.vms().join(name).exists() {
            true => Err(Error::Exists(name.to_string())),
            false => Ok(()),
        }
    }

    /// A new VM `name`, with `config`, a slot of its own, the allowlist
    /// `creds` (the default if None), and whatever disks `fill` puts in it,
    /// one of them made current. Built aside under a dot-name that list
    /// skips, then renamed into place, so one that fails or is killed
    /// partway never takes the name and leaves nothing. A kernel in `config`
    /// needn't be on this host.
    pub fn make_from<E: From<Error>>(
        &self,
        name: &str,
        config: &Config,
        creds: Option<&[Cred]>,
        fill: impl FnOnce(&NewVm) -> std::result::Result<(), E>,
    ) -> std::result::Result<Vm, E> {
        check_name(name)?;
        config.check_values()?;
        let vms = self.vms();
        fs::create_dir_all(&vms).map_err(at(&vms))?;
        sweep_staging(&vms);
        let dir = vms.join(name);
        if dir.exists() {
            return Err(Error::Exists(name.to_string()).into());
        }
        let staged = vms.join(format!(".new-{name}-{}", std::process::id()));
        let made = (|| -> std::result::Result<(), E> {
            fs::create_dir(&staged)
                .and_then(|()| fs::create_dir(staged.join("gens")))
                .map_err(at(&staged))?;
            self.assign_slot(&staged)?;
            write_config(&staged, config)?;
            if let Some(creds) = creds {
                write_creds(&staged, creds)?;
            }
            fill(&NewVm {
                dir: staged.clone(),
            })?;
            // A VM exists once it has a history, which commit writes.
            if !staged.join("history").is_file() {
                let why = "no disk was made current";
                return Err(invalid(&staged.join("history"), why).into());
            }
            // rename won't replace a directory with anything in it, so a
            // create that raced this one keeps its VM. Under the slots lock,
            // or a slot scan could miss this VM between its two names.
            let _slots = self.lock_slots()?;
            fs::rename(&staged, &dir).map_err(|e| match e.kind() {
                io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists => {
                    Error::Exists(name.to_string())
                }
                _ => at(&dir)(e),
            })?;
            Ok(sync_dir(&vms)?)
        })();
        if made.is_err() {
            let _ = fs::remove_dir_all(&staged);
        }
        made?;
        Ok(read_vm(name, &dir)?)
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
        self.set_config(name, |config| config.idle = Some(idle))?;
        Ok(idle)
    }

    /// Whether the VM's URL is on, and the port in the VM it goes to.
    pub fn url(&self, name: &str) -> Result<Setting> {
        Ok(self.config(name)?.url.unwrap_or_default())
    }

    /// Sets the VM's URL. A running VM's boot reads it every second, so it
    /// counts from then on.
    pub fn set_url(&self, name: &str, url: Setting) -> Result<Setting> {
        self.set_config(name, |config| config.url = Some(url))?;
        Ok(url)
    }

    // Changes one setting in the VM's config, keeping the rest, and writes it
    // whole, so a boot reading it sees the old one or the new. An imported
    // VM's kernel needn't be on this host: only its boot needs it.
    fn set_config(&self, name: &str, change: impl FnOnce(&mut Config)) -> Result<()> {
        let dir = self.existing(name)?;
        let mut config = read_config(&dir)?;
        change(&mut config);
        config.check_values()?;
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
        })
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

    /// Where the VM's boot takes sleep, wake, halt and park (src/sleep.rs).
    pub fn boot_socket(&self, name: &str) -> Result<PathBuf> {
        Ok(boot_socket(&self.existing(name)?))
    }

    /// Where the VM's snapshot goes when it sleeps.
    pub fn snapshot(&self, name: &str) -> Result<Snapshot> {
        Ok(snapshot(&self.existing(name)?))
    }

    /// The note of the VM's snapshot, if it has one: it's sleeping, or on its
    /// way, if its boot is up, or parked, with no boot (Run::park). A boot
    /// that was killed asleep leaves one until the next boot clears it.
    pub fn sleeping(&self, name: &str) -> Result<Option<Slept>> {
        self.snapshot(name)?.slept()
    }

    /// Whether the VM is parked (Run::park), as Snapshot::parked says.
    pub fn parked(&self, name: &str) -> Result<bool> {
        Ok(self.snapshot(name)?.parked())
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

    /// Where the VM records its latest batch of best-of-N tries (best.rs).
    pub fn tries(&self, name: &str) -> Result<PathBuf> {
        Ok(self.existing(name)?.join("tries.json"))
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
    ///
    /// A VM parked at a host shutdown (Run::park) gets the run that parked it
    /// back instead, on that run's own disk, cloning nothing, for its boot
    /// to load the snapshot onto: Run::parked says so.
    pub fn begin_run(&self, name: &str) -> Result<Run> {
        let dir = self.existing(name)?;
        let lock = take_any_lock(&dir, name)?;
        // A note that won't read isn't parked (Snapshot::parked).
        let parked = snapshot(&dir).slept().ok().flatten();
        let Some(slept) = parked.filter(|s| s.parked) else {
            return fresh_run(name, dir, self.keep, lock);
        };
        // Noted by the boot that parked it; the current disk, if that's gone.
        let from = match read_booted(&dir)? {
            Some(b) if b.generation == slept.generation => b.from,
            _ => current(&dir)?,
        };
        Ok(Run {
            from,
            generation: slept.generation,
            disk: disk_of(&dir, slept.generation),
            parked: Some(slept),
            name: name.to_string(),
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
    /// one, the owner's checkpoints, the newest KEEP_CHECKPOINTS of the ones
    /// runs and the agent made, and the newest few others. A run
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
        self.checkpoint_by(name, comment, Maker::Owner)
    }

    /// `checkpoint`, noted as `by`'s.
    pub fn checkpoint_by(
        &self,
        name: &str,
        comment: Option<&str>,
        by: Maker,
    ) -> Result<Checkpoint> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        let current = current(&dir)?;
        let id = next_generation(&dir)?;
        clone(&disk_of(&dir, current), &disk_of(&dir, id))?;
        // Written last: a disk without its note is a leftover nothing lists.
        let note = Note::frozen(comment, by);
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

    /// A stopped VM's current disk and settings, and unless `checkpoints` is
    /// false, the owner's checkpoints, oldest first, every disk open, for an
    /// export to stream (export.rs). Under the VM's lock, let go before this
    /// returns: a committed disk is never written again, so the VM can start
    /// while they stream, and a delete or gc can't take an open one away.
    /// `Busy` if something holds the VM, its boot included.
    pub fn open_disks(&self, name: &str, checkpoints: bool) -> Result<Disks> {
        self.lock(name)?.open_disks(checkpoints)
    }

    /// The VM's lock, held until the Locked is dropped, so nothing boots or
    /// changes the VM meanwhile: what a move holds from its stop to its
    /// delete (moves.rs). `Busy` if something holds the VM, its boot
    /// included.
    pub fn lock(&self, name: &str) -> Result<Locked> {
        let dir = self.existing(name)?;
        let lock = take_lock(&dir, name)?;
        Ok(Locked {
            name: name.to_string(),
            dir,
            vms: self.vms(),
            _lock: lock,
        })
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

    /// Copies `disk` into a new generation of the VM, aside from its
    /// history, for finish_restore to put on top, as a restore's copy is: a
    /// disk from outside the VM, which `undo` then goes back from. It's
    /// noted as `note` once finished, with what the disk says it was built
    /// from (image.rs), and until then as the owner's checkpoint, which gc
    /// keeps; drop_restore deletes it. The VM may be up meanwhile, and its
    /// running checkpoints wait for the copy.
    pub fn stage_disk(&self, name: &str, disk: Source, note: &str) -> Result<Staged> {
        let freezer = self.hold_checkpoints(name)?;
        let from = current(&freezer.dir)?;
        let generation = freezer.copy(disk, &Note::frozen(Some(note), Maker::Owner))?;
        let build = image::read(&disk_of(&freezer.dir, generation));
        Ok(Staged {
            from,
            generation,
            note: Note::auto(note).built(build),
        })
    }

    /// Puts a disk staged by Freezer::stage_restore or stage_disk on top of
    /// history, as `restore` would have: `undo` goes back to the disk from
    /// before it. Needs the VM stopped.
    pub fn finish_restore(&self, name: &str, staged: &Staged) -> Result<Vm> {
        let dir = self.existing(name)?;
        let _lock = take_lock(&dir, name)?;
        write_note(&dir, staged.generation, &staged.note.now())?;
        let mut gens = history(&dir)?;
        gens.push(staged.generation);
        write_history(&dir, &gens)?;
        read_vm(name, &dir)
    }

    /// A new disk for the VM `name`, made from a copy of base's current disk,
    /// which `fill` writes into from the VM's own, and put on top of its
    /// history, as a restore's copy is: `undo` goes back to the disk from
    /// before it. Its settings stay as they are. `fill` gets the VM's
    /// current disk, base's, and the new one, and what it returns comes back
    /// with the VM and base's generation.
    ///
    /// Under the VM's lock all the while, which its boot holds while it's up
    /// and a run holds until it ends, so that's `Busy`. Base's current
    /// generation must be one an import made, a disk no guest has written,
    /// since `fill` may hand the copy to debugfs (graft.rs): a base that ran
    /// is `NotImported`. On any error the new disk goes and history stays as
    /// it was. One that's killed leaves a part, which the VM's next copy
    /// clears, and what `fill` staged beside it, which this clears.
    pub fn rebase<T, E: From<Error>>(
        &self,
        name: &str,
        fill: impl FnOnce(&Path, &Path, &Path) -> std::result::Result<T, E>,
    ) -> std::result::Result<(Vm, u64, T), E> {
        if name == BASE {
            return Err(Error::OntoItself(name.to_string()).into());
        }
        let dir = self.existing(name)?;
        let base = self.existing(BASE)?;
        let _lock = take_lock(&dir, name)?;
        // A stage_disk's copy, which needs no lock, takes its number first.
        let freezer = self.hold_checkpoints(name)?;
        let onto = current(&base)?;
        let note = read_note(&base, onto)?;
        if !imported(note.as_ref()) {
            let made_by = note.and_then(|n| n.comment);
            let made_by = made_by.unwrap_or_else(|| "nothing it noted".to_string());
            return Err(Error::NotImported { onto, made_by }.into());
        }
        let (from, base_disk) = (disk_of(&dir, current(&dir)?), disk_of(&base, onto));
        freezer.clear_parts()?;
        clear_staged(&dir)?;
        let why = format!("rebase onto base gen {onto}");
        let id = next_generation(&dir)?;
        // Taken first, as Freezer::copy takes one, and the owner's until
        // it's in history, so gc keeps it.
        write_note(&dir, id, &Note::frozen(Some(&why), Maker::Owner))?;
        let (part, disk) = (part_of(&dir, id), disk_of(&dir, id));
        let made = (|| -> std::result::Result<T, E> {
            clone(&base_disk, &part)?;
            // base taken away and made again since, from a disk a guest
            // wrote, would be what was cloned: asked again.
            let again = read_note(&base, onto)?;
            if current(&base)? != onto || !imported(again.as_ref()) {
                let made_by = again.and_then(|n| n.comment);
                let made_by = made_by.unwrap_or_else(|| "nothing it noted".to_string());
                return Err(Error::NotImported { onto, made_by }.into());
            }
            let filled = fill(&from, &base_disk, &part)?;
            File::open(&part)
                .and_then(|f| f.sync_all())
                .map_err(at(&part))?;
            fs::rename(&part, &disk).map_err(at(&disk))?;
            sync_dir(&dir.join("gens"))?;
            write_note(&dir, id, &Note::auto(&why).built(image::read(&disk)))?;
            let mut gens = history(&dir)?;
            gens.push(id);
            write_history(&dir, &gens)?;
            Ok(filled)
        })();
        let filled = match made {
            Ok(filled) => filled,
            Err(e) => {
                let _ = fs::remove_file(&part);
                let _ = clear_staged(&dir);
                // One that failed after it went into history is current: it stays.
                if !history(&dir).is_ok_and(|gens| gens.contains(&id)) {
                    let _ = fs::remove_file(&disk);
                    let _ = fs::remove_file(note_of(&dir, id));
                }
                return Err(e);
            }
        };
        Ok((read_vm(name, &dir)?, onto, filled))
    }

    /// What the VM's current disk says it was built from, as noted when it
    /// came in (image.rs). None if it didn't say, or came in another way.
    pub fn build(&self, name: &str) -> Result<Option<Build>> {
        let dir = self.existing(name)?;
        Ok(read_note(&dir, current(&dir)?)?.and_then(|n| n.build))
    }

    /// Deletes a staged restore, or a staged disk, that didn't go ahead.
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
        write_creds(&dir, creds).map_err(|e| match e {
            // Deleted between the check and the write.
            Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                Error::NoVm(name.to_string())
            }
            e => e,
        })
    }

    /// Removes a VM and all its disk generations. Forks of it are unaffected:
    /// each has a disk of its own.
    pub fn delete(&self, name: &str) -> Result<Vm> {
        self.lock(name)?.delete()
    }
}

/// A VM whose lock this process holds (Store::lock), until it's dropped.
#[derive(Debug)]
pub struct Locked {
    name: String,
    dir: PathBuf,
    vms: PathBuf,
    _lock: File,
}

impl Locked {
    /// As Store::open_disks, under this lock, which stays held.
    pub fn open_disks(&self, checkpoints: bool) -> Result<Disks> {
        let dir = &self.dir;
        let now = current(dir)?;
        let current = open_disk(dir, now, read_note(dir, now)?, true)?;
        open_with(dir, current, checkpoints)
    }

    /// Removes the VM and all its disk generations, as Store::delete does.
    pub fn delete(self) -> Result<Vm> {
        let (name, dir, vms) = (&self.name, &self.dir, &self.vms);
        let vm = read_vm(name, dir)?;
        // Renamed first, so the VM is gone at once and the name is free; a
        // delete killed partway leaves a dot-directory that the next create
        // from a new process sweeps away. The lock goes with the rename, still held until we return.
        // The nanos keep a leftover from colliding with the next delete of the
        // same name in a long-lived `runner mcp`, whose pid never goes away.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let doomed = vms.join(format!(".del-{name}-{nanos}-{}", std::process::id()));
        fs::rename(dir, &doomed).map_err(at(dir))?;
        sync_dir(vms)?;
        // The VM is gone either way, so a failed removal is a warning, not a
        // failure a caller would retry into "no such VM".
        if let Err(e) = fs::remove_dir_all(&doomed) {
            eprintln!("runner: {name} is deleted, but {}: {e}", doomed.display());
        }
        Ok(vm)
    }
}

/// A VM Store::make_from is building, under its staging name: where its
/// disks go, and what each one is.
#[derive(Debug)]
pub struct NewVm {
    dir: PathBuf,
}

impl NewVm {
    /// Where generation `id`'s disk goes.
    pub fn disk(&self, id: u64) -> PathBuf {
        disk_of(&self.dir, id)
    }

    /// Notes generation `id`, its disk in place, as the owner's checkpoint,
    /// made at `time` with `comment`.
    pub fn checkpoint(&self, id: u64, time: DateTime<Utc>, comment: Option<&str>) -> Result<()> {
        settle(&self.disk(id))?;
        let note = Note {
            time,
            ..Note::frozen(comment, Maker::Owner)
        };
        write_note(&self.dir, id, &note)
    }

    /// Makes generation `id`, its disk in place, the current disk, noted as
    /// made now by `why`, with what it says it was built from (image.rs).
    pub fn current(&self, id: u64, why: &str) -> Result<()> {
        let disk = self.disk(id);
        settle(&disk)?;
        self.commit(id, &Note::auto(why).built(image::read(&disk)))
    }

    fn commit(&self, id: u64, note: &Note) -> Result<()> {
        write_note(&self.dir, id, note)?;
        write_history(&self.dir, &[id])
    }
}

#[derive(Debug)]
pub struct Run {
    /// The generation this run's disk was cloned from.
    pub from: u64,
    pub generation: u64,
    pub disk: PathBuf,
    // The note of the snapshot it was parked with, if it's a parked VM's.
    parked: Option<Slept>,
    name: String,
    dir: PathBuf,
    keep: usize,
    _lock: File,
}

// A fresh copy of the current disk for a run, under the VM's lock, as
// begin_run makes it.
fn fresh_run(name: &str, dir: PathBuf, keep: usize, lock: File) -> Result<Run> {
    let Booted { from, generation } = fresh_disk(&dir)?;
    let disk = disk_of(&dir, generation);
    Ok(Run {
        from,
        generation,
        disk,
        parked: None,
        name: name.to_string(),
        dir,
        keep,
        _lock: lock,
    })
}

// The disk of a fresh run: the current one cloned, and both noted.
fn fresh_disk(dir: &Path) -> Result<Booted> {
    // An ask for an earlier boot that never took it isn't this run's.
    remove_if_there(&dir.join(ROLLBACK))?;
    let from = current(dir)?;
    let generation = next_generation(dir)?;
    let disk = disk_of(dir, generation);
    clone(&disk_of(dir, from), &disk)?;
    let booted = Booted { from, generation };
    let text = serde_json::to_string(&booted).map_err(|e| at(&dir.join(BOOTED))(e.into()))?;
    if let Err(e) = write_aside(dir, BOOTED, &text) {
        remove_if_there(&disk)?;
        return Err(e);
    }
    Ok(booted)
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
            write_note(&self.dir, staged.generation, &staged.note.now())?;
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
    /// so there's nothing to look at. A parked VM's run keeps its disk, which
    /// its guest wrote before it was parked; the VM stays parked unless its
    /// boot got as far as the load (Run::resuming).
    pub fn discard(self) {
        if self.parked.is_some() {
            return;
        }
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

    /// Where this boot takes sleep, wake, halt and park.
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

    /// The note of the snapshot this run was parked with, if it's a parked
    /// VM's, back for its boot to resume (Store::begin_run).
    pub fn parked(&self) -> Option<Slept> {
        self.parked
    }

    /// Parks this run's sleeping VM, at a host shutdown: its disk and
    /// snapshot are synced, then noted parked, so its boot can end without
    /// committing, and the VM's next boot gets this run back to resume it.
    /// Until then it's asleep, and busy as a sleeping VM is.
    pub fn park(&self) -> Result<()> {
        let snap = self.snapshot();
        let not_asleep = || invalid(&snap.dir.join(SLEPT), format!("{} isn't asleep", self.name));
        let mut slept = snap.slept()?.ok_or_else(not_asleep)?;
        // What a power cut partway through the shutdown could tear: a
        // snapshot noted parked must be all there, with its disk.
        for path in [&self.disk, &snap.vmstate(), &snap.mem()] {
            File::open(path)
                .and_then(|f| f.sync_all())
                .map_err(at(path))?;
        }
        slept.parked = true;
        snap.note(&slept)
    }

    /// Unparks this parked VM's run just before its snapshot loads, note
    /// first: once loaded, the guest runs and writes to its disk, so a boot
    /// that dies after this mustn't resume the snapshot again. The files
    /// stay for the load.
    pub fn resuming(&self) -> Result<()> {
        remove_if_there(&self.snapshot().dir.join(SLEPT))
    }

    /// Gives up on resuming this parked VM's run: its snapshot goes, its disk
    /// stays in gens/ for a look, and this becomes a fresh run of the current
    /// disk, under the same lock, as begin_run makes one.
    pub fn unpark(&mut self) -> Result<()> {
        self.snapshot().clear()?;
        let Booted { from, generation } = fresh_disk(&self.dir)?;
        self.from = from;
        self.generation = generation;
        self.disk = disk_of(&self.dir, generation);
        self.parked = None;
        Ok(())
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
        self.freeze_by(comment, Maker::Owner)
    }

    /// `freeze`, noted as `by`'s: the agent in the VM's, which asks through
    /// its boot (src/calls.rs) and which `agent_since` counts, or a run's.
    pub fn freeze_by(&self, comment: Option<&str>, by: Maker) -> Result<Checkpoint> {
        self.freeze_noted(Note::frozen(comment, by))
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
        let id = self.copy(Source::File(&disk_of(&self.dir, booted.generation)), &note)?;
        read_checkpoint(&self.dir, id, Some(note), false)
    }

    /// Copies checkpoint `id` aside for a restore that has to stop the VM
    /// first, before anything stops: a mistyped id stops nothing, and the
    /// stop's commit, whose gc may drop `id`, can't take it from the restore.
    /// The copy is noted as the owner's checkpoint, which gc keeps, until
    /// Store::finish_restore puts it in history; drop_restore deletes it.
    pub fn stage_restore(&self, id: u64) -> Result<Staged> {
        check_restorable(&self.name, &self.dir, id)?;
        let why = format!("restore of {id}");
        let note = Note::frozen(Some(&why), Maker::Owner);
        let generation = self.copy(Source::File(&disk_of(&self.dir, id)), &note)?;
        Ok(Staged {
            from: id,
            generation,
            note: Note::auto(&why),
        })
    }

    /// A copy of the disk the VM's latest boot runs, as `freeze` makes one,
    /// but open, and gone from the VM's directory before this returns: what
    /// `runner export` streams of a running VM. Nothing is noted, so no
    /// checkpoint or generation is left, however the export ends. Pause the
    /// VM first, so the copy is of one moment. A copy killed before it's
    /// unlinked leaves a part, which the VM's next copy while it runs (a
    /// checkpoint, an export or a restore) clears.
    pub fn open_copy(&self) -> Result<OpenDisk> {
        let booted = read_booted(&self.dir)?;
        let booted = booted.ok_or_else(|| Error::NeverBooted(self.name.clone()))?;
        self.clear_parts()?;
        // Made empty first, which takes the number from a boot starting
        // meanwhile, as freeze's note does.
        let part = part_of(&self.dir, next_generation(&self.dir)?);
        File::create_new(&part).map_err(at(&part))?;
        let opened = cp(&disk_of(&self.dir, booted.generation), &part)
            .and_then(|()| File::open(&part).map_err(at(&part)));
        let unlinked = fs::remove_file(&part).map_err(at(&part));
        let file = opened?;
        unlinked?;
        let meta = file.metadata().map_err(at(&part))?;
        let info = Checkpoint {
            id: booted.generation,
            time: Utc::now().trunc_subsecs(0),
            comment: None,
            auto: true,
            agent: false,
            run: false,
            bytes: meta.len(),
            current: true,
        };
        Ok(OpenDisk { info, file })
    }

    /// The VM's settings, and unless `checkpoints` is false, the owner's
    /// checkpoints, oldest first, every disk open, with `current` as its
    /// current disk: Store::open_disks, for a running VM, whose boot holds
    /// its lock. A checkpoint is never written again, and only gc and a
    /// delete, which need that lock, take one away.
    pub fn open_disks(&self, current: OpenDisk, checkpoints: bool) -> Result<Disks> {
        open_with(&self.dir, current, checkpoints)
    }

    // What a copy killed partway left.
    fn clear_parts(&self) -> Result<()> {
        for left in numbered(&self.dir, PART)? {
            let part = part_of(&self.dir, left);
            fs::remove_file(&part).map_err(at(&part))?;
            if !disk_of(&self.dir, left).exists() {
                remove_if_there(&note_of(&self.dir, left))?;
            }
        }
        Ok(())
    }

    // A copy of `from` under a new number, with its note.
    fn copy(&self, from: Source, note: &Note) -> Result<u64> {
        let gens = self.dir.join("gens");
        self.clear_parts()?;
        let id = next_generation(&self.dir)?;
        write_note(&self.dir, id, note)?;
        let (part, disk) = (part_of(&self.dir, id), disk_of(&self.dir, id));
        let made = bring(from, &part)
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

/// What Store::open_disks opened.
#[derive(Debug)]
pub struct Disks {
    /// Its sizes, kernel, idle time and URL, as stored.
    pub config: Config,
    /// Its allowlist, names only.
    pub creds: Vec<Cred>,
    pub current: OpenDisk,
    /// The owner's checkpoints, oldest first.
    pub checkpoints: Vec<OpenDisk>,
}

/// One of a VM's disks, open.
#[derive(Debug)]
pub struct OpenDisk {
    /// Its id, time and comment, as `checkpoints` lists it.
    pub info: Checkpoint,
    pub file: File,
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

    /// Whether a host shutdown parked it (Run::park). A note that won't read
    /// isn't parked, so it can't hold the VM busy for good: its next boot
    /// clears it.
    pub fn parked(&self) -> bool {
        self.slept().ok().flatten().is_some_and(|s| s.parked)
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
    /// Parked at a host shutdown (Run::park): its boot has ended, leaving the
    /// snapshot and its disk for the next boot to resume.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
}

const SLEPT: &str = "slept.json";

/// A restore's copy of a checkpoint, made while the VM still ran
/// (Freezer::stage_restore), or a disk from outside it (Store::stage_disk).
#[derive(Debug)]
pub struct Staged {
    /// The checkpoint it's a copy of; for a staged disk, the generation that
    /// was current when it was staged.
    pub from: u64,
    /// The copy's own generation.
    pub generation: u64,
    // What it's noted as once it's in history.
    note: Note,
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
    // The owner's checkpoints stay until the VM is deleted. Of the ones runs
    // and the agent made, the newest KEEP_CHECKPOINTS stay; the rest go.
    let mut made = Vec::new();
    let mut loose = Vec::new();
    for g in generations(dir)? {
        match read_note(dir, g)? {
            _ if g == current => {}
            Some(n) if !n.auto && (n.agent || n.run) => made.push((g, n.run)),
            Some(n) if !n.auto => {}
            _ => loose.push(g),
        }
    }
    made.sort_unstable_by(|a, b| b.cmp(a));
    // A run going on the VM took the newest run checkpoint before it joined,
    // and its result names it for a restore by hand. Asked after the disks were listed, so a
    // run that took one since holds the lock now, or took one this gc never
    // saw. Whatever else holds the run lock keeps one more for the moment.
    let over = made.len() > KEEP_CHECKPOINTS;
    let held = match over && run_going(dir)? {
        true => made.iter().find(|&&(_, run)| run).map(|&(g, _)| g),
        false => None,
    };
    let past = made.iter().skip(KEEP_CHECKPOINTS).map(|&(g, _)| g);
    let mut doomed: Vec<u64> = past.filter(|&g| Some(g) != held).collect();
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
    doomed.extend(others.split_off(keep.min(others.len())));
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

// Whether something holds the VM's run lock (src/runs.rs): a run going, or
// what keeps one from starting. Asked once, so one just let go can look
// held. Taking it for the asking holds off a run's start for that instant,
// which its own try_lock waits out.
fn run_going(dir: &Path) -> Result<bool> {
    let path = dir.join("runs").join("lock");
    let file = match File::open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        r => r.map_err(at(&path))?,
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(at(&path)(e)),
    }
}

// The VM's lock, for a change to it. A VM parked at a host shutdown has no
// boot to hold it, but it's asleep, and busy as a sleeping VM is: its
// snapshot runs on a disk that isn't committed, which undo, gc and the rest
// would take from under it. Only its next boot takes it (begin_run).
fn take_lock(dir: &Path, name: &str) -> Result<File> {
    let lock = take_any_lock(dir, name)?;
    if snapshot(dir).parked() {
        return Err(Error::Busy(name.to_string()));
    }
    Ok(lock)
}

// flock, not a pid file: the kernel drops it when the holder dies, so a
// crashed run never wedges its VM.
fn take_any_lock(dir: &Path, name: &str) -> Result<File> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    time: DateTime<Utc>,
    comment: Option<String>,
    auto: bool,
    // Left out unless set, so a runner from before it reads the note the same.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    agent: bool,
    // The same. A runner from before it keeps a run's checkpoint as the
    // owner's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    run: bool,
    // What an imported or staged disk says it was built from (image.rs).
    // Left out when unknown, as above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    build: Option<Build>,
}

impl Note {
    fn auto(why: &str) -> Note {
        let comment = Some(why.to_string());
        Note {
            time: now(),
            comment,
            auto: true,
            agent: false,
            run: false,
            build: None,
        }
    }

    fn built(self, build: Option<Build>) -> Note {
        Note { build, ..self }
    }

    // The same, made now.
    fn now(&self) -> Note {
        Note {
            time: now(),
            ..self.clone()
        }
    }

    // A checkpoint's, made now by `by`.
    fn frozen(comment: Option<&str>, by: Maker) -> Note {
        Note {
            time: now(),
            comment: comment.map(str::to_string),
            auto: false,
            agent: by == Maker::Agent,
            run: by == Maker::Run,
            build: None,
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

// Generation `id`'s disk, open, as `checkpoints` lists it.
fn open_disk(dir: &Path, id: u64, note: Option<Note>, current: bool) -> Result<OpenDisk> {
    let disk = disk_of(dir, id);
    let file = File::open(&disk).map_err(at(&disk))?;
    let info = read_checkpoint(dir, id, note, current)?;
    Ok(OpenDisk { info, file })
}

// `current`, with the VM's settings and, unless `checkpoints` is false, the
// owner's checkpoints, oldest first, open.
fn open_with(dir: &Path, current: OpenDisk, checkpoints: bool) -> Result<Disks> {
    let mut mine = Vec::new();
    if checkpoints {
        let now = self::current(dir)?;
        let mut ids = generations(dir)?;
        ids.sort_unstable();
        for id in ids {
            if let Some(note) = read_note(dir, id)?
                && !(note.auto || note.agent || note.run)
            {
                mine.push(open_disk(dir, id, Some(note), id == now)?);
            }
        }
    }
    let creds = read_creds(&dir.join("creds"))?.unwrap_or_else(|| DEFAULT.to_vec());
    Ok(Disks {
        config: read_config(dir)?,
        creds,
        current,
        checkpoints: mine,
    })
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
            run: false,
            build: None,
        },
    };
    Ok(Checkpoint {
        id,
        time: note.time,
        comment: note.comment,
        auto: note.auto,
        agent: note.agent,
        run: note.run,
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

// The allowlist, sorted, each name once, as set_creds leaves it.
fn write_creds(dir: &Path, creds: &[Cred]) -> Result<Vec<Cred>> {
    let mut creds = creds.to_vec();
    creds.sort();
    creds.dedup();
    let text: String = creds.iter().map(|c| format!("{c}\n")).collect();
    write_aside(dir, "creds", &text)?;
    Ok(creds)
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

// Whether a note is the one `import` writes, for a disk no guest has written.
fn imported(note: Option<&Note>) -> bool {
    note.is_some_and(|n| n.auto && n.comment.as_deref() == Some("import"))
}

// What a rebase killed partway staged beside its part (graft::staging).
fn clear_staged(dir: &Path) -> Result<()> {
    let gens = dir.join("gens");
    for entry in fs::read_dir(&gens).map_err(at(&gens))? {
        let path = entry.map_err(at(&gens))?.path();
        if path.extension().is_some_and(|e| e == "graft") {
            fs::remove_dir_all(&path).map_err(at(&path))?;
        }
    }
    Ok(())
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

// A disk into `to`, from wherever it comes.
fn bring(disk: Source, to: &Path) -> Result<()> {
    match disk {
        Source::File(from) => clone(from, to),
        Source::Stream(from) => {
            let len = write_sparse(from, to)?;
            if len == 0 {
                return Err(Error::Cut(
                    "the stream was empty, so no disk came in".into(),
                ));
            }
            match image::ext4_size(to) {
                Some(want) if want > len => Err(Error::Cut(format!(
                    "the stream ended at {len} bytes, but the ext4 superblock it starts \
                     with says {want}: was it cut off?"
                ))),
                _ => Ok(()),
            }
        }
    }
}

// A stream into a new file at `to`, leaving a hole wherever a block is all
// zeros, so a sparse disk that comes in as a stream stays sparse. As clone
// leaves a copy: writable, and synced. Returns its length.
fn write_sparse(from: &mut dyn Read, to: &Path) -> Result<u64> {
    const BLOCK: usize = 4096;
    let mut file = File::create(to).map_err(at(to))?;
    let mut buf = vec![0; 1 << 20];
    let mut len = 0u64;
    loop {
        let n = fill(from, &mut buf).map_err(at(to))?;
        if n == 0 {
            break;
        }
        // Runs of data blocks are written whole, and runs of zeros skipped.
        let zeros = |b: &[u8]| b.iter().fold(0, |a, &x| a | x) == 0;
        let next = |i: usize| (i + BLOCK).min(n);
        let mut start = 0;
        while start < n {
            let hole = zeros(&buf[start..next(start)]);
            let mut stop = next(start);
            while stop < n && zeros(&buf[stop..next(stop)]) == hole {
                stop = next(stop);
            }
            let run = &buf[start..stop];
            match hole {
                true => file.seek(SeekFrom::Current(run.len() as i64)).map(drop),
                false => file.write_all(run),
            }
            .map_err(at(to))?;
            start = stop;
        }
        len += n as u64;
    }
    // A stream that ends in zeros ends in a hole, which only the length
    // keeps.
    file.set_len(len)
        .and_then(|()| file.set_permissions(fs::Permissions::from_mode(0o644)))
        .and_then(|()| file.sync_all())
        .map_err(at(to))?;
    Ok(len)
}

// Reads until `buf` is full or the stream ends: a pipe gives what it has.
fn fill(from: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match from.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

// A disk written in place, left as clone leaves a copy: writable, and synced.
fn settle(disk: &Path) -> Result<()> {
    fs::set_permissions(disk, fs::Permissions::from_mode(0o644))
        .and_then(|()| File::open(disk)?.sync_all())
        .map_err(at(disk))
}

// GNU cp does the copying: a reflink on XFS or btrfs, which takes constant
// time and no space until one side writes, else a copy that keeps the disk's
// holes, so a sparse image stays small. Every generation is writable whatever
// the source's mode, and synced before anything points at it.
fn clone(from: &Path, to: &Path) -> Result<()> {
    cp(from, to)?;
    fs::set_permissions(to, fs::Permissions::from_mode(0o644)).map_err(at(to))?;
    File::open(to).and_then(|f| f.sync_all()).map_err(at(to))
}

// Only the copy, as clone makes it: a reflink, else a sparse copy.
fn cp(from: &Path, to: &Path) -> Result<()> {
    let out = Command::new("cp")
        .args(["--reflink=auto", "--sparse=always", "--"])
        .args([from, to])
        .output()
        .map_err(at(to))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(at(to)(io::Error::other(why)));
    }
    Ok(())
}
