// Copying directory trees out of one VM's disk into another's (#156), so
// `runner rebase` (#157) can put a VM's /home into a fresh copy of base.
// Nothing is mounted, and nothing needs root.
//
// The source, a disk a guest with root wrote, is only read, by ext4-view, as
// src/changes.rs reads a run's disks and within its limits. The target is
// always a fresh copy of base's disk, which no guest has written, and only
// e2fsprogs writes it: resize2fs first when the VM's disk is bigger than
// base's (Store::rebase), then debugfs. So they, which parse a whole
// filesystem in C, never open a disk a guest made.
//
// From the source, debugfs gets file contents, names and link targets.
// Contents go through files staged on the host. Names and targets go into
// its commands, which debugfs reads a line at a time and splits as a shell
// would, so each goes in double quotes, and one holding a byte that would end
// the line or the quotes (a newline, a carriage return or a double quote) is
// skipped and named in what the copy returns. A newline let through would
// start a command of the guest's, and debugfs runs a line starting with `!`
// in a shell on the host. So does one that's too long: debugfs runs what's
// past its line buffer as a command of its own. Every path the copy writes is
// absolute, and below one the caller gave it is made of the source's names
// alone, through directories the copy made, so nothing lands anywhere else.
//
// Files go through the host a batch at a time, never the whole tree: a
// batch's files are staged sparse next to the target (`staging`), written in
// by one debugfs, then deleted. debugfs finds a path by reading each
// directory on it from the start, so a command per file in a directory of N
// takes time in N squared. A file's mode, owner and times go in by the inode
// number its write gave, in a second debugfs, with no path to look up.
//
// Each entry keeps its type, contents, permission bits, owner and group, and
// its access and modification times to the nanosecond. A hard link comes out
// as separate copies, and extended attributes, file capabilities and ACLs
// among them, aren't copied: ext4-view gives neither. Nor are inode flags, or
// the change and creation times, which are the copy's. Sockets, devices and
// fifos are skipped and named: ext4-view doesn't read a device's numbers,
// and debugfs's mknod takes a name in its working directory, not a path. So
// is an entry whose inode ext4-view refuses as corrupt, which includes, in
// ext4-view 1.0, any time before 1970 or past 2038 (`touch -d 2040-01-01`).
use crate::changes::{self, Budget, Disk, Limits, fill, read, shown};
use crate::host;
use ext4_view::{Ext4Error, Metadata, PathBuf as InPathBuf};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

// The longest command debugfs is given. It reads a line into a buffer of
// libc's BUFSIZ, 8192 bytes with glibc, and runs the rest as a line of its
// own, so no line comes near that. A path that would need more is skipped.
const MAX_LINE: usize = 4096;

// How many bytes of files a batch stages on the host, and how many commands
// it holds, before debugfs writes it. A file bigger than that is a batch of
// its own.
const BATCH_BYTES: u64 = 64 << 20;
const BATCH_LINES: usize = 20_000;

const CHUNK: usize = 1 << 16;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading a disk failed, or the source ran past a limit.
    #[error(transparent)]
    Disk(#[from] changes::Error),
    #[error("{path}: {why}")]
    Path { path: String, why: &'static str },
    #[error("{disk}: the copy would write into the disk it reads")]
    Same { disk: PathBuf },
    #[error("{disk}: {path} is there, and isn't a directory")]
    Parent { disk: PathBuf, path: String },
    #[error("{disk}: no room for the copy: {why}")]
    Full { disk: PathBuf, why: String },
    #[error("{disk}: debugfs: {why}")]
    Debugfs { disk: PathBuf, why: String },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

fn at(path: &Path) -> impl Fn(io::Error) -> Error + use<> {
    let path = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}

/// Why a path under those given wasn't copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// Its name, or its link's target, holds a byte debugfs can't take in
    /// a command: a newline, a carriage return or a double quote.
    Name,
    /// Its path, or its link's target, is too long for a debugfs command.
    Long,
    /// It's a socket, a device or a fifo.
    Special,
    /// Its inode, or its link's target, is one ext4-view refuses, like one
    /// with a time before 1970 or past 2038.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Skip {
    /// An absolute path inside the VM, written as src/changes.rs writes
    /// one. A directory skipped is skipped whole.
    pub path: String,
    pub why: Why,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Copied {
    /// What wasn't copied, in the order the copy met it: each directory's
    /// entries in name order.
    pub skipped: Vec<Skip>,
    /// How many more were skipped than are listed.
    pub more: u64,
}

/// Copies each of `paths`, absolute paths apart from each other, from the
/// ext4 disk `from` into the ext4 disk `to`, replacing what `to` has there
/// and leaving the rest of it alone. A path `from` doesn't have goes from
/// `to` as well. A parent `to` lacks is made as `from` has it.
///
/// debugfs reads `to`, so it must be a disk no guest has written, like a
/// fresh copy of base's. On an error `to` may be half written: delete it.
/// The copy stages files in `staging(to)` and deletes it, whether it fails
/// or not; one that was killed leaves it, for the next copy into `to`, or
/// whoever deletes `to`, to delete.
pub fn copy(from: &Path, to: &Path, paths: &[&str], limits: Limits) -> Result<Copied> {
    let paths = checked(paths)?;
    let (a, b) = (stat(from)?, stat(to)?);
    if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
        let disk = to.to_path_buf();
        return Err(Error::Same { disk });
    }
    let never = || false;
    let mut budget = Budget::new(limits, "copying", &never);
    let src = Disk::load(from)?;
    let mut batch = Batch::new(from, to)?;
    // Everything the copy reads of `to`, read before debugfs writes it.
    let mut parents = Vec::new();
    {
        let dst = Disk::load(to)?;
        for path in &paths {
            parents.push(missing(&dst, path)?);
            let at = in_path(path);
            match dst.fs.symlink_metadata(&at) {
                Ok(m) => remove(&dst, &at, path, &m, depth(path), &mut budget, &mut batch)?,
                Err(Ext4Error::NotFound) => {}
                Err(e) => return Err(read(to)(e).into()),
            }
        }
    }
    let mut copy = Copier {
        src,
        budget,
        batch,
        made: BTreeSet::new(),
        listed: 0,
        out: Copied::default(),
    };
    for (path, missing) in paths.iter().zip(parents) {
        copy.path(path, missing)?;
    }
    copy.batch.flush(&copy.budget)?;
    Ok(copy.out)
}

/// Where a copy into `to` stages the source's files: a directory beside it.
pub fn staging(to: &Path) -> PathBuf {
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(".graft");
    to.with_file_name(name)
}

fn stat(disk: &Path) -> Result<fs::Metadata> {
    fs::metadata(disk).map_err(at(disk))
}

// Whether debugfs takes these bytes inside double quotes as they are.
fn plain(bytes: &[u8]) -> bool {
    !bytes.is_empty() && !bytes.iter().any(|b| matches!(b, b'\n' | b'\r' | b'"' | 0))
}

// The paths, each absolute, plain and apart from the others.
fn checked(paths: &[&str]) -> Result<Vec<Vec<u8>>> {
    let bad = |path: &str, why| {
        let path = path.to_string();
        Err(Error::Path { path, why })
    };
    for path in paths {
        let Some(rest) = path.strip_prefix('/') else {
            return bad(path, "isn't an absolute path");
        };
        if rest.is_empty() {
            return bad(path, "is the whole disk");
        }
        for name in rest.split('/') {
            if matches!(name, "" | "." | "..") {
                return bad(path, "has an empty name, . or ..");
            }
            if !plain(name.as_bytes()) || name.len() > 255 {
                return bad(path, "has a name debugfs can't take");
            }
        }
        if path.len() > MAX_LINE / 2 {
            return bad(path, "is too long for debugfs");
        }
    }
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            if under(a.as_bytes(), b.as_bytes()) || under(b.as_bytes(), a.as_bytes()) {
                return bad(b, "is or holds another path to copy");
            }
        }
    }
    Ok(paths.iter().map(|p| p.as_bytes().to_vec()).collect())
}

// Whether `path` is `root` or under it.
fn under(path: &[u8], root: &[u8]) -> bool {
    path == root || path.strip_prefix(root).is_some_and(|r| r.starts_with(b"/"))
}

// How many directories deep a path is, / being 0.
fn depth(path: &[u8]) -> usize {
    path.iter().filter(|&&b| b == b'/').count()
}

// A path `checked` passed, as ext4-view takes it.
fn in_path(path: &[u8]) -> InPathBuf {
    InPathBuf::try_from(path).expect("a checked path is a valid ext4 path")
}

// The parents of `path` that `to` lacks, outermost first. One that's there
// must be a directory, so nothing the copy writes goes through a link.
fn missing(dst: &Disk, path: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let ends = path.iter().enumerate().skip(1).filter(|&(_, &b)| b == b'/');
    for (end, _) in ends {
        let parent = &path[..end];
        if !out.is_empty() {
            out.push(parent.to_vec());
            continue;
        }
        match dst.fs.symlink_metadata(&in_path(parent)) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                let disk = dst.path.to_path_buf();
                let path = shown(parent);
                return Err(Error::Parent { disk, path });
            }
            Err(Ext4Error::NotFound) => out.push(parent.to_vec()),
            Err(e) => return Err(read(dst.path)(e).into()),
        }
    }
    Ok(out)
}

// What `to` has at `path`, removed: a directory's entries before it.
fn remove(
    dst: &Disk,
    at: &InPathBuf,
    path: &[u8],
    m: &Metadata,
    depth: usize,
    budget: &mut Budget,
    batch: &mut Batch,
) -> Result<()> {
    let line = |verb| {
        let line = cmd(verb, &[path]);
        if !plain(&path[1..]) || line.len() > MAX_LINE {
            let path = shown(path);
            let why = "has a name debugfs can't take, so the copy can't replace it";
            return Err(Error::Path { path, why });
        }
        Ok(line)
    };
    if !m.is_dir() {
        batch.push(line("rm")?);
        return Ok(());
    }
    budget.deep(depth).map_err(|why| dst.unbounded(why))?;
    for (name, kid) in dst.entries(at, budget)? {
        let kid_path = [path, b"/", &name].concat();
        remove(
            dst,
            &at.join(&name),
            &kid_path,
            &kid,
            depth + 1,
            budget,
            batch,
        )?;
    }
    batch.push(line("rmdir")?);
    Ok(())
}

struct Copier<'a> {
    src: Disk<'a>,
    budget: Budget<'a>,
    batch: Batch,
    // The parents this copy made.
    made: BTreeSet<Vec<u8>>,
    // The bytes of the skipped paths listed so far.
    listed: usize,
    out: Copied,
}

impl Copier<'_> {
    // One of the paths given, with the parents the target lacks.
    fn path(&mut self, path: &[u8], missing: Vec<Vec<u8>>) -> Result<()> {
        let at = in_path(path);
        let m = match self.src.fs.symlink_metadata(&at) {
            Ok(m) => m,
            // Nothing there, so nothing there in the target either.
            Err(Ext4Error::NotFound | Ext4Error::NotADirectory) => return Ok(()),
            Err(Ext4Error::Corrupt(_)) => {
                self.skip(path, Why::Unreadable);
                return Ok(());
            }
            Err(e) => return Err(read(self.src.path)(e).into()),
        };
        let mut made = Vec::new();
        for dir in missing {
            if self.made.insert(dir.clone()) {
                let m = self.src.fs.metadata(&in_path(&dir));
                let m = m.map_err(read(self.src.path))?;
                self.batch.push(cmd("mkdir", &[&dir]));
                made.push((dir, m));
            }
        }
        self.entry(&at, path, &m, depth(path))?;
        for (dir, m) in made.iter().rev() {
            self.batch.attrs(dir, m);
        }
        Ok(())
    }

    fn entry(&mut self, at: &InPathBuf, path: &[u8], m: &Metadata, depth: usize) -> Result<()> {
        if let Some(why) = self.make(at, path, m, depth)? {
            self.skip(path, why);
            return Ok(());
        }
        self.batch.attrs(path, m);
        if self.batch.full() {
            self.batch.flush(&self.budget)?;
        }
        Ok(())
    }

    // The commands that make the entry, a directory's with its entries
    // after, or why there can't be any.
    fn make(
        &mut self,
        at: &InPathBuf,
        path: &[u8],
        m: &Metadata,
        depth: usize,
    ) -> Result<Option<Why>> {
        let kind = m.file_type();
        if kind.is_dir() {
            let line = cmd("mkdir", &[path]);
            if line.len() > MAX_LINE {
                return Ok(Some(Why::Long));
            }
            let deep = self.budget.deep(depth);
            deep.map_err(|why| self.src.unbounded(why))?;
            self.batch.push(line);
            for (name, kid) in self.src.entries_as_read(at, &mut self.budget)? {
                let kid_path = [path, b"/", &name].concat();
                match kid {
                    _ if !plain(&name) => self.skip(&kid_path, Why::Name),
                    _ if kid_path.len() > MAX_LINE => self.skip(&kid_path, Why::Long),
                    Err(Ext4Error::Corrupt(_)) => self.skip(&kid_path, Why::Unreadable),
                    Err(e) => return Err(read(self.src.path)(e).into()),
                    Ok(kid) => self.entry(&at.join(&name), &kid_path, &kid, depth + 1)?,
                }
            }
        } else if kind.is_symlink() {
            // ext4-view reads a link whole, at whatever length its inode
            // claims.
            if m.len() > changes::MAX_LINK {
                return Ok(Some(Why::Long));
            }
            let to = match self.src.fs.read_link(at) {
                Ok(to) => to,
                // A target with a name longer than 255 bytes, say.
                Err(Ext4Error::Corrupt(_)) => return Ok(Some(Why::Unreadable)),
                Err(e) => return Err(read(self.src.path)(e).into()),
            };
            let to = to.as_ref();
            if !plain(to) {
                return Ok(Some(Why::Name));
            }
            let line = cmd("symlink", &[path, to]);
            if line.len() > MAX_LINE {
                return Ok(Some(Why::Long));
            }
            self.batch.push(line);
        } else if kind.is_regular_file() {
            let staged = self.batch.next_file();
            let line = cmd("write", &[staged.as_os_str().as_encoded_bytes(), path]);
            if line.len() > MAX_LINE {
                return Ok(Some(Why::Long));
            }
            self.batch.make_room(m.len(), &self.budget)?;
            self.batch.stage(&staged, &self.src, at, &self.budget)?;
            self.batch.push(line);
        } else {
            return Ok(Some(Why::Special));
        }
        Ok(None)
    }

    // Once a path doesn't fit, the rest are only counted, so the list stays
    // in order.
    fn skip(&mut self, path: &[u8], why: Why) {
        let out = &mut self.out;
        if out.more == 0 && out.skipped.len() < changes::CAP {
            let path = shown(path);
            if self.listed + path.len() <= changes::PATH_BYTES {
                self.listed += path.len();
                out.skipped.push(Skip { path, why });
                return;
            }
        }
        out.more += 1;
    }
}

// A debugfs command with its arguments, each in double quotes.
fn cmd(verb: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut line = verb.as_bytes().to_vec();
    for arg in args {
        line.extend(b" \"");
        line.extend(*arg);
        line.push(b'"');
    }
    line
}

// What debugfs writes next, and the files it writes in from, in a directory
// next to the target, which goes once the copy's done.
struct Batch {
    src: PathBuf,
    disk: PathBuf,
    dir: PathBuf,
    script: Vec<u8>,
    lines: usize,
    // Each file written in this batch's attributes, in the order written,
    // for the second debugfs to set by inode number.
    later: Vec<Vec<(&'static str, String)>>,
    staged: Vec<PathBuf>,
    bytes: u64,
    files: u64,
}

impl Batch {
    fn new(src: &Path, disk: &Path) -> Result<Batch> {
        let dir = staging(disk);
        if !plain(dir.as_os_str().as_encoded_bytes()) {
            let path = dir.display().to_string();
            return Err(Error::Path {
                path,
                why: "has a byte debugfs can't take",
            });
        }
        // What a copy that died left.
        let _ = fs::remove_dir_all(&dir);
        let mut mkdir = fs::DirBuilder::new();
        mkdir.mode(0o700).create(&dir).map_err(at(&dir))?;
        Ok(Batch {
            src: src.to_path_buf(),
            disk: disk.to_path_buf(),
            dir,
            script: Vec::new(),
            lines: 0,
            later: Vec::new(),
            staged: Vec::new(),
            bytes: 0,
            files: 0,
        })
    }

    fn push(&mut self, line: Vec<u8>) {
        self.script.extend(line);
        self.script.push(b'\n');
        self.lines += 1;
    }

    // An entry's mode, owner and times, set as the source has them: a
    // file's by its inode number, once its batch is written.
    fn attrs(&mut self, path: &[u8], m: &Metadata) {
        let kind: u32 = if m.is_dir() {
            0o040000
        } else if m.is_symlink() {
            0o120000
        } else {
            0o100000
        };
        let mode = kind | u32::from(m.mode());
        let (atime, mtime) = (m.accessed(), m.modified());
        let fields = [
            ("mode", format!("0{mode:o}")),
            ("uid", m.uid().to_string()),
            ("gid", m.gid().to_string()),
            ("atime", format!("@{}", atime.seconds())),
            (
                "atime_extra",
                extra(atime.seconds(), atime.nanoseconds()).to_string(),
            ),
            ("mtime", format!("@{}", mtime.seconds())),
            (
                "mtime_extra",
                extra(mtime.seconds(), mtime.nanoseconds()).to_string(),
            ),
        ];
        if m.file_type().is_regular_file() {
            self.lines += fields.len();
            self.later.push(fields.into());
            return;
        }
        for (field, value) in fields {
            let mut line = cmd("sif", &[path]);
            line.extend(format!(" {field} {value}").as_bytes());
            self.push(line);
        }
    }

    fn full(&self) -> bool {
        self.lines >= BATCH_LINES || self.bytes >= BATCH_BYTES
    }

    // Where the next file is staged.
    fn next_file(&self) -> PathBuf {
        self.dir.join(self.files.to_string())
    }

    // Writes out what's staged first if a file of `len` bytes would take the
    // batch past its size.
    fn make_room(&mut self, len: u64, budget: &Budget) -> Result<()> {
        if self.bytes > 0 && self.bytes.saturating_add(len) > BATCH_BYTES {
            self.flush(budget)?;
        }
        Ok(())
    }

    // A file of the source's, staged at `to` with its runs of zeros left as
    // holes, which debugfs keeps.
    fn stage(&mut self, to: &Path, src: &Disk, at: &InPathBuf, budget: &Budget) -> Result<()> {
        let mut open = OpenOptions::new();
        let out = open.write(true).create_new(true).mode(0o600).open(to);
        let out = out.map_err(self::at(to))?;
        self.staged.push(to.to_path_buf());
        self.files += 1;
        let mut file = src.open(at)?;
        let mut buf = vec![0; CHUNK];
        let mut len = 0;
        loop {
            budget.in_time().map_err(|why| src.unbounded(why))?;
            let n = fill(&mut file, &mut buf).map_err(read(src.path))?;
            if n == 0 {
                break;
            }
            if buf[..n].iter().any(|&b| b != 0) {
                out.write_all_at(&buf[..n], len).map_err(self::at(to))?;
            }
            len += n as u64;
        }
        out.set_len(len).map_err(self::at(to))?;
        self.bytes += len;
        Ok(())
    }

    // Has debugfs run what's in the batch, then deletes what it staged.
    fn flush(&mut self, budget: &Budget) -> Result<()> {
        if self.lines == 0 {
            return Ok(());
        }
        let said = self.debugfs(&self.script, budget)?;
        // Each write says the inode it made on a line of its own, which no
        // name can start, since none holds a newline or a carriage return.
        let made: Vec<&[u8]> = (said.split(|&b| b == b'\n'))
            .filter_map(|l| l.strip_prefix(b"Allocated inode: "))
            .collect();
        if made.len() != self.later.len() {
            let (n, m) = (self.later.len(), made.len());
            let why = format!("wrote {n} files but made {m} inodes");
            let disk = self.disk.clone();
            return Err(Error::Debugfs { disk, why });
        }
        let mut sifs = Vec::new();
        for (ino, fields) in made.iter().zip(&self.later) {
            let ino = std::str::from_utf8(ino).ok();
            let Some(ino) = ino.and_then(|n| n.trim().parse::<u32>().ok()) else {
                let why = "made an inode it didn't number".to_string();
                let disk = self.disk.clone();
                return Err(Error::Debugfs { disk, why });
            };
            for (field, value) in fields {
                sifs.extend(format!("sif <{ino}> {field} {value}\n").as_bytes());
            }
        }
        if !sifs.is_empty() {
            self.debugfs(&sifs, budget)?;
        }
        for file in self.staged.drain(..) {
            fs::remove_file(&file).map_err(at(&file))?;
        }
        self.script.clear();
        self.later.clear();
        self.lines = 0;
        self.bytes = 0;
        Ok(())
    }

    // Runs `script` through debugfs on the target, and what it printed, or
    // an error if it said one.
    fn debugfs(&self, script: &[u8], budget: &Budget) -> Result<Vec<u8>> {
        let path = self.dir.join("script");
        fs::write(&path, script).map_err(at(&path))?;
        // Files, not pipes, so debugfs never waits on them.
        let (outs, errs) = (self.dir.join("stdout"), self.dir.join("stderr"));
        let out = File::create(&outs).map_err(at(&outs))?;
        let err = File::create(&errs).map_err(at(&errs))?;
        let mut debugfs = Command::new(host::DEBUGFS)
            .arg("-w")
            .arg("-f")
            .arg(&path)
            .arg(&self.disk)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(at(Path::new(host::DEBUGFS)))?;
        let status = loop {
            if let Some(status) = debugfs.try_wait().map_err(at(&self.disk))? {
                break status;
            }
            if let Err(why) = budget.in_time() {
                let _ = debugfs.kill();
                let _ = debugfs.wait();
                let disk = self.src.clone();
                return Err(changes::Error::Unbounded { disk, why }.into());
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let said = fs::read(&errs).map_err(at(&errs))?;
        let said = String::from_utf8_lossy(&said);
        let mut errors = said
            .lines()
            .map(str::trim)
            // Its banner, `debugfs 1.47.0 (5-Feb-2023)`, comes on every run.
            .filter(|l| !l.is_empty() && !(l.starts_with("debugfs ") && l.ends_with(')')));
        let disk = self.disk.clone();
        if let Some(first) = errors.clone().next() {
            if let Some(full) = errors.find(|l| l.contains("Could not allocate")) {
                let why = full.to_string();
                return Err(Error::Full { disk, why });
            }
            let why = first.to_string();
            return Err(Error::Debugfs { disk, why });
        }
        if !status.success() {
            let why = match status.code() {
                Some(code) => format!("exited {code}"),
                None => format!("killed by signal {}", status.signal().unwrap_or(0)),
            };
            return Err(Error::Debugfs { disk, why });
        }
        fs::read(&outs).map_err(at(&outs))
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// A time's `_extra` field as ext4 keeps it: the nanoseconds over two bits
// that carry the seconds past the 32 in the inode's own field, which holds
// them signed.
fn extra(secs: i64, nanos: u32) -> u32 {
    let epoch = ((secs - i64::from(secs as i32)) >> 32) & 0x3;
    (nanos << 2) | epoch as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_never_holds_a_byte_that_ends_it_or_its_quotes() {
        for bad in [&b"a\nb"[..], b"a\rb", b"a\"b", b"a\0b", b""] {
            assert!(!plain(bad), "{bad:?}");
        }
        for good in [&b"a b"[..], b"'", b"\\", b"!x", b"#x", b"\xff", b"\t"] {
            assert!(plain(good), "{good:?}");
        }
        assert_eq!(cmd("symlink", &[b"/a b", b"c"]), b"symlink \"/a b\" \"c\"");
    }

    // ext4 reads a time back as its field, signed, plus the extra's two low
    // bits times 2^32 (ext4-view's Timestamp::from_raw).
    #[test]
    fn a_time_past_2038_keeps_its_epoch_bits() {
        for (secs, nanos) in [
            (1_500_000_000, 1000),
            (-5, 7),
            (i64::from(i32::MAX) + 1, 0),
            ((1 << 33) + 5, 999_999_999),
        ] {
            let x = extra(secs, nanos);
            let back = i64::from(secs as u32 as i32) + (i64::from(x & 3) << 32);
            assert_eq!((back, x >> 2), (secs, nanos));
        }
    }
}
