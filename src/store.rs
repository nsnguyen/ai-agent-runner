// The filesystem is the database: each VM is a directory.
//
//   <root>/vms/<name>/gens/<n>.ext4  one disk per generation; a run writes a new one
//   <root>/vms/<name>/history        committed generations, one per line; the last is current
//   <root>/vms/<name>/lock           flock'd by the process running the VM, one at a time
//   <root>/vms/<name>/config.json    sizes and kernel, only where they differ from the host's
//   <root>/vms/<name>/api.sock       Firecracker's API, while the VM runs
//   <root>/vms/<name>/vsock.sock     the guest's vsock, while the VM runs
//   <root>/vms/<name>/creds          the credentials it gets at boot, by name; never a value
//   <root>/vms/.new-<name>-<pid>/    a VM being created, renamed into place when done
//   <root>/vms/.del-<name>-<pid>/    a VM being deleted, renamed out of the way first
//   <root>/kernel/vmlinux            the kernel a VM boots unless its config names one
use crate::creds::{Cred, DEFAULT};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The VM `create` forks unless told otherwise.
pub const BASE: &str = "base";

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
    #[error("{0}")]
    BadConfig(String),
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
        self.make(name, disk, &Config::default())
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
        self.make(name, &disk_of(&src, current(&src)?), &config)
    }

    // Built aside under a dot-name that list skips, then renamed into place, so
    // a create killed partway never takes the name.
    fn make(&self, name: &str, disk: &Path, config: &Config) -> Result<Vm> {
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
            .and_then(|()| clone(disk, &disk_of(&staged, 0)))
            .and_then(|()| write_config(&staged, config))
            .and_then(|()| write_history(&staged, &[0]))
            // rename won't replace a directory with anything in it, so a
            // create that raced this one keeps its VM.
            .and_then(|()| {
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
    _lock: File,
}

impl Run {
    pub fn commit(self) -> Result<Vm> {
        // Firecracker leaves the guest's writes in the page cache, and history
        // mustn't point at a disk a power cut could tear.
        File::open(&self.disk)
            .and_then(|f| f.sync_all())
            .map_err(at(&self.disk))?;
        let mut gens = history(&self.dir)?;
        gens.push(self.generation);
        write_history(&self.dir, &gens)?;
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
        self.dir.join("vsock.sock")
    }
}

fn api_socket(dir: &Path) -> PathBuf {
    dir.join("api.sock")
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
