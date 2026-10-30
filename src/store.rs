// The filesystem is the database: each VM is a directory.
//
//   <root>/vms/<name>/gens/<n>.ext4  one disk per generation; a run writes a new one
//   <root>/vms/<name>/gens/<n>.json  its note: when it was made, a comment, and whether it's a checkpoint
//   <root>/vms/<name>/history        committed generations, one per line; the last is current
//   <root>/vms/<name>/lock           flock'd by the process running the VM, one at a time
//   <root>/vms/.new-<name>-<pid>/    a VM being created, renamed into place when done
//   <root>/vms/.del-<name>-<pid>/    a VM being deleted, renamed out of the way first
use chrono::{DateTime, SubsecRound, Utc};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
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
    #[error("{0} has no checkpoint {1}")]
    NoCheckpoint(String, u64),
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
        self.make(name, disk, "import")
    }

    /// A new VM whose disk is a copy-on-write fork of `from`'s current disk.
    pub fn fork(&self, from: &str, name: &str) -> Result<Vm> {
        let src = self.existing(from)?;
        self.make(
            name,
            &disk_of(&src, current(&src)?),
            &format!("fork of {from}"),
        )
    }

    // Built aside under a dot-name that list skips, then renamed into place, so
    // a create killed partway never takes the name.
    fn make(&self, name: &str, disk: &Path, why: &str) -> Result<Vm> {
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
            .and_then(|()| clone(disk, &disk_of(&staged, 0)))
            .and_then(|()| write_note(&staged, 0, &Note::auto(why)))
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
        if !gens.contains(&id) && !frozen {
            return Err(Error::NoCheckpoint(name.to_string(), id));
        }
        let generation = next_generation(&dir)?;
        clone(&disk_of(&dir, id), &disk_of(&dir, generation))?;
        write_note(&dir, generation, &Note::auto(&format!("restore of {id}")))?;
        gens.push(generation);
        write_history(&dir, &gens)?;
        read_vm(name, &dir)
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
        write_note(&self.dir, self.generation, &Note::auto("run"))?;
        let mut gens = history(&self.dir)?;
        gens.push(self.generation);
        write_history(&self.dir, &gens)?;
        read_vm(&self.name, &self.dir)
    }
}

// flock, not a pid file: the kernel drops it when the holder dies, so a
// crashed run never wedges its VM.
fn take_lock(dir: &Path, name: &str) -> Result<File> {
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
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::Busy(name.to_string())),
        Err(TryLockError::Error(e)) => Err(at(&path)(e)),
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
    let text = serde_json::to_vec(note).map_err(|e| at(&path)(e.into()))?;
    write_aside(&path, &text)
}

// None for a generation made before notes were written, or a failed run's.
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
    write_aside(&dir.join("history"), text.as_bytes())
}

// Written aside, synced and renamed, so even a host crash leaves either the old
// file or the new.
fn write_aside(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = File::create(&tmp).map_err(at(&tmp))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(at(&tmp))?;
    fs::rename(&tmp, path).map_err(at(path))?;
    let dir = path.parent().unwrap_or(Path::new("."));
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
    Ok(generations(dir)?.into_iter().max().unwrap_or(0) + 1)
}

// Every disk on hand, in no order.
fn generations(dir: &Path) -> Result<Vec<u64>> {
    let gens = dir.join("gens");
    let mut out = Vec::new();
    for entry in fs::read_dir(&gens).map_err(at(&gens))? {
        let name = entry.map_err(at(&gens))?.file_name();
        let n = name
            .to_str()
            .and_then(|s| s.strip_suffix(".ext4")?.parse::<u64>().ok());
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
