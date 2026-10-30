// What a run changed, measured on the host from the two disks themselves: the
// generation the run started from and the one it wrote. The agent is never
// asked. The disks are read in userspace by ext4-view, never mounted, so a disk
// that a guest with root made hostile meets a parser, not the host's kernel,
// and reading one needs no root.
//
// Nothing in a disk is taken on trust. A guest with root can write any inode
// field, times included, straight to its block device, so files of equal size
// are compared byte for byte. For the same reason the walk has a budget: a
// directory linked into itself or fanned out through links would otherwise
// never end, or never end in time.
use ext4_view::{Ext4, Ext4Error, File, Metadata, Path as InPath, PathBuf as InPathBuf};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How many changed paths a result lists before it only counts the rest.
pub const CAP: usize = 200;

const MAX_DEPTH: usize = 256;
const MAX_ENTRIES: u64 = 10_000_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{disk}: {source}")]
    Read { disk: PathBuf, source: Ext4Error },
    #[error("{disk}: {why}; does a directory contain itself?")]
    Unbounded { disk: PathBuf, why: String },
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
    /// An absolute path inside the VM. A backslash is doubled and a byte that
    /// isn't UTF-8 is written \xNN.
    pub path: String,
    pub change: Kind,
    /// For a directory added or deleted whole, or one that took a file's place
    /// or gave its place to one: how many entries are under it, unlisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Changes {
    /// Each directory's entries in name order, a directory's changes right
    /// after it. Modified means a different type, mode, owner, file contents
    /// or link target. Extended attributes, file capabilities included, aren't
    /// read.
    pub paths: Vec<Change>,
    /// How many more paths changed than are listed.
    pub more: u64,
}

/// The paths that differ between two ext4 disks, listing at most `cap`.
pub fn between(before: &Path, after: &Path, cap: usize) -> Result<Changes> {
    let (before, after) = (Disk::load(before)?, Disk::load(after)?);
    // Each byte of data is read about once per disk, so twice the space both
    // disks take on the host is more than an honest walk reads.
    let bytes = 2 * (before.bytes + after.bytes);
    let mut walk = Walk {
        before,
        after,
        cap,
        budget: Budget {
            entries: MAX_ENTRIES,
            bytes,
        },
        out: Changes::default(),
    };
    walk.dir(&InPath::ROOT.into(), 0)?;
    Ok(walk.out)
}

struct Disk<'a> {
    fs: Ext4,
    path: &'a Path,
    /// Space the disk takes on the host, not its apparent size.
    bytes: u64,
}

impl<'a> Disk<'a> {
    fn load(path: &'a Path) -> Result<Self> {
        let fs = Ext4::load_from_path(path).map_err(read(path))?;
        let bytes = std::fs::metadata(path).map_or(0, |m| m.blocks() * 512);
        Ok(Disk { fs, path, bytes })
    }

    fn entries(&self, dir: &InPathBuf, budget: &mut Budget) -> Result<BTreeMap<Vec<u8>, Metadata>> {
        let mut out = BTreeMap::new();
        for entry in self.fs.read_dir(dir).map_err(read(self.path))? {
            let entry = entry.map_err(read(self.path))?;
            let name = entry.file_name().as_ref().to_vec();
            if name != b"." && name != b".." {
                budget.entry().map_err(|why| self.unbounded(why))?;
                out.insert(name, entry.metadata().map_err(read(self.path))?);
            }
        }
        Ok(out)
    }

    // Everything under `dir`, counted.
    fn count(&self, dir: &InPathBuf, depth: usize, budget: &mut Budget) -> Result<u64> {
        if depth > MAX_DEPTH {
            return Err(self.unbounded(format!("more than {MAX_DEPTH} directories deep")));
        }
        let mut n = 0;
        for (name, m) in self.entries(dir, budget)? {
            n += 1;
            if m.is_dir() {
                n += self.count(&dir.join(name), depth + 1, budget)?;
            }
        }
        Ok(n)
    }

    fn open(&self, path: &InPathBuf) -> Result<File> {
        self.fs.open(path).map_err(read(self.path))
    }

    fn link(&self, path: &InPathBuf) -> Result<InPathBuf> {
        self.fs.read_link(path).map_err(read(self.path))
    }

    fn unbounded(&self, why: String) -> Error {
        let disk = self.path.to_path_buf();
        Error::Unbounded { disk, why }
    }
}

fn read(disk: &Path) -> impl Fn(Ext4Error) -> Error + use<> {
    let disk = disk.to_path_buf();
    move |source| Error::Read {
        disk: disk.clone(),
        source,
    }
}

struct Budget {
    entries: u64,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> std::result::Result<(), String> {
        self.entries = self
            .entries
            .checked_sub(1)
            .ok_or(format!("more than {MAX_ENTRIES} entries"))?;
        Ok(())
    }

    fn read(&mut self, n: u64) -> std::result::Result<(), String> {
        self.bytes = self
            .bytes
            .checked_sub(n)
            .ok_or("more bytes to compare than both disks hold".to_string())?;
        Ok(())
    }
}

struct Walk<'a> {
    before: Disk<'a>,
    after: Disk<'a>,
    cap: usize,
    budget: Budget,
    out: Changes,
}

impl Walk<'_> {
    // A directory on both disks. Limits that run out name the run's disk,
    // `after`, which is the one a guest could have shaped.
    fn dir(&mut self, dir: &InPathBuf, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            let why = format!("more than {MAX_DEPTH} directories deep");
            return Err(self.after.unbounded(why));
        }
        let old = self.before.entries(dir, &mut self.budget)?;
        let new = self.after.entries(dir, &mut self.budget)?;
        let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
        for name in names {
            let path = dir.join(name);
            let (o, n) = (old.get(name), new.get(name));
            // A directory on one side only is counted, not walked into.
            let lone = |m: Option<&Metadata>, other: Option<&Metadata>| {
                m.is_some_and(Metadata::is_dir) && !other.is_some_and(Metadata::is_dir)
            };
            let contains = if lone(o, n) {
                Some(self.before.count(&path, depth + 1, &mut self.budget)?)
            } else if lone(n, o) {
                Some(self.after.count(&path, depth + 1, &mut self.budget)?)
            } else {
                None
            };
            let change = match (o, n) {
                (Some(o), Some(n)) => (!self.same(&path, o, n)?).then_some(Kind::Modified),
                (Some(_), None) => Some(Kind::Deleted),
                _ => Some(Kind::Added),
            };
            if let Some(change) = change {
                self.note(&path, change, contains);
            }
            if o.is_some_and(Metadata::is_dir) && n.is_some_and(Metadata::is_dir) {
                self.dir(&path, depth + 1)?;
            }
        }
        Ok(())
    }

    fn same(&mut self, path: &InPathBuf, old: &Metadata, new: &Metadata) -> Result<bool> {
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
            if old.len() != new.len() {
                return Ok(false);
            }
            self.budget
                .read(2 * old.len())
                .map_err(|why| self.after.unbounded(why))?;
            same_bytes(self.before.open(path)?, self.after.open(path)?)
                .map_err(|(which, e)| read([self.before.path, self.after.path][which])(e))
        } else {
            // A device's numbers aren't readable here, so it counts as changed
            // whenever its inode does.
            Ok(old == new)
        }
    }

    fn note(&mut self, path: &InPathBuf, change: Kind, contains: Option<u64>) {
        if self.out.paths.len() < self.cap {
            let path = shown(path.as_ref());
            self.out.paths.push(Change {
                path,
                change,
                contains,
            });
        } else {
            self.out.more += 1;
        }
    }
}

// Names are bytes and JSON is text. Escaping the backslash too keeps two
// different names from ever printing the same.
fn shown(path: &[u8]) -> String {
    let mut out = String::new();
    for chunk in path.utf8_chunks() {
        out.push_str(&chunk.valid().replace('\\', r"\\"));
        for b in chunk.invalid() {
            out.push_str(&format!(r"\x{b:02x}"));
        }
    }
    out
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
