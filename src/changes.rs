// What a run changed, measured on the host from the two disks themselves: the
// generation the run started from and the one it wrote. The agent is never
// asked. The disks are read in userspace by ext4-view, never mounted, so a disk
// that a guest with root made hostile meets a parser, not the host's kernel,
// and reading one needs no root.
use ext4_view::{Ext4, Ext4Error, File, Metadata, Path as InPath, PathBuf as InPathBuf};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How many changed paths a result lists before it only counts the rest.
pub const CAP: usize = 200;

// Bounds on the walk. A guest can link a directory into itself, and without
// them the walk would never end.
const MAX_DEPTH: usize = 256;
const MAX_ENTRIES: u64 = 10_000_000;
// A file this big whose inode changed counts as modified without reading it.
const MAX_COMPARED: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{disk}: {source}")]
    Read { disk: PathBuf, source: Ext4Error },
    #[error("{after} against {before}: {why}; does a directory contain itself?")]
    Unbounded {
        before: PathBuf,
        after: PathBuf,
        why: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Change {
    /// An absolute path inside the VM.
    pub path: String,
    pub change: Kind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Changes {
    /// Each directory's entries in name order, a directory's contents right
    /// after it. Modified means a different type, mode, owner, file contents
    /// or link target.
    pub paths: Vec<Change>,
    /// How many more paths changed than are listed.
    pub more: u64,
}

/// The paths that differ between two ext4 disks, listing at most `cap`.
pub fn between(before: &Path, after: &Path, cap: usize) -> Result<Changes> {
    let (before, after) = (Disk::load(before)?, Disk::load(after)?);
    let mut walk = Walk {
        cloned: before.fs.uuid() == after.fs.uuid(),
        before,
        after,
        cap,
        seen: 0,
        out: Changes::default(),
    };
    walk.dir(&InPath::ROOT.into(), true, true, 0)?;
    Ok(walk.out)
}

struct Disk<'a> {
    fs: Ext4,
    path: &'a Path,
}

impl<'a> Disk<'a> {
    fn load(path: &'a Path) -> Result<Self> {
        let fs = Ext4::load_from_path(path).map_err(read(path))?;
        Ok(Disk { fs, path })
    }

    fn entries(&self, dir: &InPathBuf) -> Result<BTreeMap<Vec<u8>, Metadata>> {
        let mut out = BTreeMap::new();
        for entry in self.fs.read_dir(dir).map_err(read(self.path))? {
            let entry = entry.map_err(read(self.path))?;
            let name = entry.file_name().as_ref().to_vec();
            if name != b"." && name != b".." {
                out.insert(name, entry.metadata().map_err(read(self.path))?);
            }
        }
        Ok(out)
    }

    fn open(&self, path: &InPathBuf) -> Result<File> {
        self.fs.open(path).map_err(read(self.path))
    }

    fn link(&self, path: &InPathBuf) -> Result<InPathBuf> {
        self.fs.read_link(path).map_err(read(self.path))
    }
}

fn read(disk: &Path) -> impl Fn(Ext4Error) -> Error + use<> {
    let disk = disk.to_path_buf();
    move |source| Error::Read {
        disk: disk.clone(),
        source,
    }
}

struct Walk<'a> {
    // One disk is a copy of the other, as a run's is of its last generation.
    cloned: bool,
    before: Disk<'a>,
    after: Disk<'a>,
    cap: usize,
    seen: u64,
    out: Changes,
}

impl Walk<'_> {
    // `before` and `after` say whether `dir` is a directory on each disk; on a
    // disk where it isn't, everything under it was added or deleted.
    fn dir(&mut self, dir: &InPathBuf, before: bool, after: bool, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(self.unbounded(format!("more than {MAX_DEPTH} directories deep")));
        }
        let old = if before {
            self.before.entries(dir)?
        } else {
            BTreeMap::new()
        };
        let new = if after {
            self.after.entries(dir)?
        } else {
            BTreeMap::new()
        };
        self.seen += (old.len() + new.len()) as u64;
        if self.seen > MAX_ENTRIES {
            return Err(self.unbounded(format!("more than {MAX_ENTRIES} entries")));
        }
        let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
        for name in names {
            let path = dir.join(name);
            let (o, n) = (old.get(name), new.get(name));
            let change = match (o, n) {
                (Some(o), Some(n)) => (!self.same(&path, o, n)?).then_some(Kind::Modified),
                (Some(_), None) => Some(Kind::Deleted),
                _ => Some(Kind::Added),
            };
            if let Some(change) = change {
                self.note(&path, change);
            }
            let is_dir = |m: Option<&Metadata>| m.is_some_and(Metadata::is_dir);
            if is_dir(o) || is_dir(n) {
                self.dir(&path, is_dir(o), is_dir(n), depth + 1)?;
            }
        }
        Ok(())
    }

    fn same(&self, path: &InPathBuf, old: &Metadata, new: &Metadata) -> Result<bool> {
        // On a cloned disk, an inode that matches in full, change time
        // included, which no syscall can set, is one the run didn't touch. That
        // is most of a disk, so most of it is never read. Between unrelated
        // disks it proves nothing: mkfs can stamp two files alike.
        if self.cloned && old == new {
            return Ok(true);
        }
        let shape = |m: &Metadata| (m.file_type(), m.mode(), m.uid(), m.gid());
        if shape(old) != shape(new) {
            return Ok(false);
        }
        let kind = old.file_type();
        if kind.is_dir() {
            // Its entries are walked on their own.
            Ok(true)
        } else if kind.is_symlink() {
            Ok(self.before.link(path)? == self.after.link(path)?)
        } else if kind.is_regular_file() {
            if old.len() != new.len() || old.len() > MAX_COMPARED {
                return Ok(false);
            }
            same_bytes(self.before.open(path)?, self.after.open(path)?)
                .map_err(|(which, e)| read([self.before.path, self.after.path][which])(e))
        } else {
            // A device's numbers aren't readable here, so any inode change counts.
            Ok(false)
        }
    }

    fn note(&mut self, path: &InPathBuf, change: Kind) {
        if self.out.paths.len() < self.cap {
            let path = String::from_utf8_lossy(path.as_ref()).into_owned();
            self.out.paths.push(Change { path, change });
        } else {
            self.out.more += 1;
        }
    }

    fn unbounded(&self, why: String) -> Error {
        let before = self.before.path.to_path_buf();
        let after = self.after.path.to_path_buf();
        Error::Unbounded { before, after, why }
    }
}

// Two files of the same length, read side by side. The error says which side.
fn same_bytes(mut a: File, mut b: File) -> std::result::Result<bool, (usize, Ext4Error)> {
    let (mut x, mut y) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let n = fill(&mut a, &mut x).map_err(|e| (0, e))?;
        let m = fill(&mut b, &mut y).map_err(|e| (1, e))?;
        if x[..n] != y[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn fill(file: &mut File, buf: &mut [u8]) -> std::result::Result<usize, Ext4Error> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_bytes(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
