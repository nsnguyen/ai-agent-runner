// What a run changed, measured on the host from the two disks themselves: the
// generation the run started from and the one it wrote. The agent is never
// asked. The disks are read in userspace by ext4-view, never mounted, so a disk
// that a guest with root made hostile meets a parser, not the host's kernel,
// and reading one needs no root.
//
// Nothing in a disk is taken on trust. A guest with root can write any inode
// field, times included, straight to its block device, so files of equal size
// are compared byte for byte. For the same reason the walk has limits: a
// directory linked into itself or fanned out through links would otherwise
// never end, or never end in time. The time limit also covers ext4-view
// finding each path from the root, a scan per lookup in a directory with no
// hash index, as mkfs.ext4 -d leaves them.
//
// What the VM's boot writes on its own, like its journal, is left out of the
// list and only counted (BOOT_ONLY), so the agent's changes stand out. What
// the agent's containers write is its own, but thousands of paths of it, so
// each directory on SUMMED is one entry with a count.
use ext4_view::{Ext4, Ext4Error, File, Metadata, Path as InPath, PathBuf as InPathBuf};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How many changed paths a result lists before it only counts the rest.
pub const CAP: usize = 200;

/// How many bytes of paths a result lists before it only counts the rest: a
/// guest names its paths, and can make each one long.
pub const PATH_BYTES: usize = 32 * 1024;

/// How long measuring may take before it gives up.
pub const TIME: Duration = Duration::from_secs(600);

const MAX_DEPTH: usize = 256;
const MAX_ENTRIES: u64 = 10_000_000;

/// How far reading a disk a guest wrote may go: how many directories deep,
/// how many entries in all, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub depth: usize,
    pub entries: u64,
    pub time: Duration,
}

/// What measuring a run may read, which a copy out of a disk
/// (src/graft.rs) keeps to as well.
pub const LIMITS: Limits = Limits {
    depth: MAX_DEPTH,
    entries: MAX_ENTRIES,
    time: TIME,
};
// Longer than any path a link can hold.
pub(crate) const MAX_LINK: u64 = 4096;
// How often measuring asks whether to stop, in entries and blocks read.
const ASK_EVERY: u32 = 1024;

/// What a boot of the base image (images/base) writes on its own, so a run's
/// result leaves it out and only counts it (#107). Found by booting a copy of
/// the image's systemd in a container and measuring its disk before and
/// after, as a run does, plus systemd-random-seed, which a VM runs and a
/// container skips. Each line says which boots write it. Nothing under /home,
/// where the agent works, is on it.
pub const BOOT_ONLY: &[(&str, Part)] = &[
    // systemd-update-done's stamp, at the first boot: the image has none.
    ("/etc/.updated", Part::File),
    // containerd's first start, at a VM's first boot, makes these empty:
    // /etc/cni/net.d, and /opt/containerd's bin and lib. Seen on base's own
    // containerd in 2026-11-08's milestone check.
    ("/etc/cni", Part::Dirs),
    // The VM's own id, written at its first boot: base ships it empty.
    ("/etc/machine-id", Part::File),
    // The first boot's systemd-tmpfiles links it to ../proc/self/mounts.
    ("/etc/mtab", Part::File),
    // As /etc/cni.
    ("/opt/containerd", Part::Dirs),
    // Every boot empties /tmp (tmpfiles' tmp.conf), so what the last run left
    // there shows as deleted. What this run adds is listed.
    ("/tmp", Part::Deleted),
    // X11's socket directories, which every boot's tmpfiles makes (x11.conf).
    ("/tmp/.ICE-unix", Part::Empty),
    ("/tmp/.X11-unix", Part::Empty),
    ("/tmp/.XIM-unix", Part::Empty),
    ("/tmp/.font-unix", Part::Empty),
    // As /etc/.updated.
    ("/var/.updated", Part::File),
    // The dpkg-db-backup and apt-daily-upgrade timers' backups. They're daily
    // and persistent, so a boot a day after the last one runs them.
    ("/var/backups", Part::All),
    // The apt-daily timer, twice a day: apt's caches, and its lock.
    ("/var/cache/apt/pkgcache.bin", Part::File),
    ("/var/cache/apt/srcpkgcache.bin", Part::File),
    ("/var/lib/apt/daily_lock", Part::File),
    // ldconfig.service's cache, at the first boot.
    ("/var/cache/ldconfig/aux-cache", Part::File),
    // containerd's and dockerd's state (SUMMED). Each start rewrites these
    // bolt databases (#170), measured on base's own Docker started twice on a
    // copy of its disk. The rest of these files their first start writes, at
    // a VM's first boot, beside empty directories (`made`), measured on
    // Docker 29.9 and containerd 2.4 started on empty directories.
    (
        "/var/lib/containerd/io.containerd.grpc.v1.introspection/uuid",
        Part::File,
    ),
    (
        "/var/lib/containerd/io.containerd.metadata.v1.bolt/meta.db",
        Part::File,
    ),
    // The first boot's tmpfiles links it to /etc/machine-id (dbus.conf).
    ("/var/lib/dbus/machine-id", Part::File),
    // As containerd's.
    ("/var/lib/docker/buildkit/cache.db", Part::File),
    (
        "/var/lib/docker/buildkit/containerd-overlayfs/metadata_v2.db",
        Part::File,
    ),
    (
        "/var/lib/docker/buildkit/containerd-overlayfs/workerid",
        Part::File,
    ),
    ("/var/lib/docker/buildkit/history_c8d.db", Part::File),
    ("/var/lib/docker/engine-id", Part::File),
    ("/var/lib/docker/image/identity-cache.db", Part::File),
    ("/var/lib/docker/network/files/local-kv.db", Part::File),
    ("/var/lib/docker/volumes/backingFsBlockDev", Part::File),
    ("/var/lib/docker/volumes/metadata.db", Part::File),
    // The first boot's tmpfiles makes it. A user who lingers is listed.
    ("/var/lib/systemd/linger", Part::Empty),
    // systemd-random-seed's seed, saved at every boot and shutdown.
    ("/var/lib/systemd/random-seed", Part::File),
    // Each timer's last run: stamped at the first boot, and when it runs.
    ("/var/lib/systemd/timers", Part::All),
    // journald's journal: every boot writes it, and rotates it.
    ("/var/log/journal", Part::All),
    // systemd-update-utmp's records of every boot and shutdown.
    ("/var/log/wtmp", Part::File),
];

/// Where Docker and containerd keep what the agent's images, containers and
/// volumes write: a run that pulls one image changes thousands of paths
/// there. The agent's work, so not left out as the boot's is, but each of
/// these is one entry in a result, counting what changed under it (#171).
pub const SUMMED: &[&str] = &["/var/lib/containerd", "/var/lib/docker"];

// The directory on SUMMED that a path is under, if any.
fn summed_under(path: &[u8]) -> Option<&'static str> {
    let under =
        |at: &str| (path.strip_prefix(at.as_bytes())).is_some_and(|rest| rest.starts_with(b"/"));
    SUMMED.iter().copied().find(|at| under(at))
}

/// Which changes to a path on BOOT_ONLY are the boot's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Any, to it or anything under it.
    All,
    /// To it, but not to a directory put in its place.
    File,
    /// It added as an empty directory.
    Empty,
    /// It added with only empty directories under it, or none.
    Dirs,
    /// Deletions under it.
    Deleted,
}

// Whether BOOT_ONLY leaves the path out when it's added holding only
// directories.
fn only_dirs(path: &[u8]) -> bool {
    BOOT_ONLY
        .iter()
        .any(|&(at, part)| part == Part::Dirs && at.as_bytes() == path)
}

// Whether a change is one only the boot makes.
fn boot_only(path: &[u8], change: Kind, contains: Option<u64>) -> bool {
    BOOT_ONLY.iter().any(|&(at, part)| {
        let at = at.as_bytes();
        let under = path
            .strip_prefix(at)
            .is_some_and(|rest| rest.starts_with(b"/"));
        match part {
            Part::All => path == at || under,
            Part::File => path == at && contains.is_none(),
            Part::Empty => path == at && change == Kind::Added && contains == Some(0),
            Part::Deleted => under && change == Kind::Deleted,
            // Only a walk under it can tell (Walk::dir).
            Part::Dirs => false,
        }
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{disk}: {source}")]
    Read { disk: PathBuf, source: Ext4Error },
    #[error("{disk}: {why}")]
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
    /// For a directory on SUMMED, where containers keep their images, layers
    /// and volumes: how many paths changed under it, none of them listed. A
    /// directory added or deleted whole there counts with all that's in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_under: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Changes {
    /// Each directory's entries in name order, a directory's changes right
    /// after it. Modified means a different type, mode, owner, file contents
    /// or link target. Extended attributes, file capabilities included, aren't
    /// read. A directory on SUMMED, Docker's and containerd's state, is one
    /// entry for everything that changed under it, with `changed_under`.
    pub paths: Vec<Change>,
    /// How many more paths changed than are listed, besides those counted
    /// in `boot_only`.
    pub more: u64,
    /// How many changed paths are left out because only the VM's boot writes
    /// them, like its journal and systemd's state. Never one under /home.
    #[serde(default)]
    pub boot_only: u64,
}

/// The paths that differ between two ext4 disks, listing at most `cap`, or an
/// error once measuring takes longer than `time`.
pub fn between(before: &Path, after: &Path, cap: usize, time: Duration) -> Result<Changes> {
    between_until(before, after, cap, time, &|| false)
}

/// As `between`, but an error too as soon as `stop` says to.
pub fn between_until(
    before: &Path,
    after: &Path,
    cap: usize,
    time: Duration,
    stop: &dyn Fn() -> bool,
) -> Result<Changes> {
    let limits = Limits { time, ..LIMITS };
    let mut walk = Walk {
        before: Disk::load(before)?,
        after: Disk::load(after)?,
        cap,
        budget: Budget::new(limits, "measuring", stop),
        listed: 0,
        summing: None,
        out: Changes::default(),
    };
    walk.dir(&InPath::ROOT.into(), 0)?;
    Ok(walk.out)
}

pub(crate) struct Disk<'a> {
    pub(crate) fs: Ext4,
    pub(crate) path: &'a Path,
}

impl<'a> Disk<'a> {
    pub(crate) fn load(path: &'a Path) -> Result<Self> {
        let fs = Ext4::load_from_path(path).map_err(read(path))?;
        Ok(Disk { fs, path })
    }

    // Each entry in `dir` but . and .., by name.
    pub(crate) fn entries(
        &self,
        dir: &InPathBuf,
        budget: &mut Budget,
    ) -> Result<BTreeMap<Vec<u8>, Metadata>> {
        let entries = self.entries_as_read(dir, budget)?.into_iter();
        let read = read(self.path);
        entries
            .map(|(name, m)| Ok((name, m.map_err(&read)?)))
            .collect()
    }

    // As `entries`, with each inode ext4-view couldn't read as its error.
    pub(crate) fn entries_as_read(
        &self,
        dir: &InPathBuf,
        budget: &mut Budget,
    ) -> Result<BTreeMap<Vec<u8>, std::result::Result<Metadata, Ext4Error>>> {
        let mut out = BTreeMap::new();
        for entry in self.fs.read_dir(dir).map_err(read(self.path))? {
            let entry = entry.map_err(read(self.path))?;
            let name = entry.file_name().as_ref().to_vec();
            if name != b"." && name != b".." {
                budget.entry().map_err(|why| self.unbounded(why))?;
                out.insert(name, entry.metadata());
            }
        }
        Ok(out)
    }

    // Everything under `dir`, counted.
    fn count(&self, dir: &InPathBuf, depth: usize, budget: &mut Budget) -> Result<u64> {
        budget.deep(depth).map_err(|why| self.unbounded(why))?;
        let mut n = 0;
        for (name, m) in self.entries(dir, budget)? {
            n += 1;
            if m.is_dir() {
                n += self.count(&dir.join(name), depth + 1, budget)?;
            }
        }
        Ok(n)
    }

    pub(crate) fn open(&self, path: &InPathBuf) -> Result<File> {
        self.fs.open(path).map_err(read(self.path))
    }

    pub(crate) fn link(&self, path: &InPathBuf) -> Result<InPathBuf> {
        self.fs.read_link(path).map_err(read(self.path))
    }

    pub(crate) fn unbounded(&self, why: String) -> Error {
        let disk = self.path.to_path_buf();
        Error::Unbounded { disk, why }
    }
}

pub(crate) fn read(disk: &Path) -> impl Fn(Ext4Error) -> Error + use<> {
    let disk = disk.to_path_buf();
    move |source| Error::Read {
        disk: disk.clone(),
        source,
    }
}

// What's left of the limits as a walk goes. Each error says which limit ran
// out.
pub(crate) struct Budget<'a> {
    limits: Limits,
    // What the walk is for, as its errors say it: "measuring".
    doing: &'static str,
    entries: u64,
    until: Instant,
    stop: &'a dyn Fn() -> bool,
    // How often it's been asked whether to go on.
    asked: Cell<u32>,
}

impl<'a> Budget<'a> {
    pub(crate) fn new(limits: Limits, doing: &'static str, stop: &'a dyn Fn() -> bool) -> Self {
        Budget {
            limits,
            doing,
            entries: limits.entries,
            until: Instant::now() + limits.time,
            stop,
            asked: Cell::new(0),
        }
    }

    pub(crate) fn deep(&self, depth: usize) -> std::result::Result<(), String> {
        if depth > self.limits.depth {
            let max = self.limits.depth;
            return Err(format!(
                "more than {max} directories deep; does a directory contain itself?"
            ));
        }
        Ok(())
    }

    pub(crate) fn entry(&mut self) -> std::result::Result<(), String> {
        let max = self.limits.entries;
        self.entries = (self.entries.checked_sub(1)).ok_or(format!("more than {max} entries"))?;
        self.in_time()
    }

    pub(crate) fn in_time(&self) -> std::result::Result<(), String> {
        if Instant::now() >= self.until {
            let secs = self.limits.time.as_secs_f32();
            return Err(format!("{} took over {secs}s", self.doing));
        }
        let asked = self.asked.get();
        self.asked.set(asked.wrapping_add(1));
        if asked.is_multiple_of(ASK_EVERY) && (self.stop)() {
            return Err(format!("{} was stopped", self.doing));
        }
        Ok(())
    }
}

struct Walk<'a> {
    before: Disk<'a>,
    after: Disk<'a>,
    cap: usize,
    budget: Budget<'a>,
    // The bytes of the paths listed so far.
    listed: usize,
    // The directory on SUMMED whose entry the paths under it go in, and where
    // that entry is in the list: None once it's counted in `more` instead.
    summing: Option<(&'static str, Option<usize>)>,
    out: Changes,
}

impl Walk<'_> {
    // A directory on both disks. Limits that run out name the run's disk,
    // `after`, which is the one a guest could have shaped.
    fn dir(&mut self, dir: &InPathBuf, depth: usize) -> Result<()> {
        let deep = self.budget.deep(depth);
        deep.map_err(|why| self.after.unbounded(why))?;
        let old = self.before.entries(dir, &mut self.budget)?;
        let new = self.after.entries(dir, &mut self.budget)?;
        let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
        for name in names {
            let path = dir.join(name);
            let (o, n) = (old.get(name), new.get(name));
            let made = o.is_none() && n.is_some_and(Metadata::is_dir);
            if let Some(root) = SUMMED
                .iter()
                .find(|at| made && at.as_bytes() == path.as_ref())
            {
                self.made(root, &path, depth + 1)?;
                continue;
            }
            if made && only_dirs(path.as_ref()) && self.empty_tree(&path, depth + 1)? {
                self.out.boot_only += 1;
                continue;
            }
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

    fn same(&self, path: &InPathBuf, old: &Metadata, new: &Metadata) -> Result<bool> {
        let shape = |m: &Metadata| (m.file_type(), m.mode(), m.uid(), m.gid());
        if shape(old) != shape(new) {
            return Ok(false);
        }
        let kind = old.file_type();
        if kind.is_dir() {
            // Its entries are walked on their own.
            Ok(true)
        } else if old.len() != new.len() {
            Ok(false)
        } else if kind.is_symlink() {
            // ext4-view reads a link whole, at whatever length its inode
            // claims, and no real link is this long.
            if old.len() > MAX_LINK {
                return Ok(false);
            }
            Ok(self.before.link(path)? == self.after.link(path)?)
        } else if kind.is_regular_file() {
            self.same_bytes(path)
        } else {
            // A device's numbers aren't readable here, so it counts as changed
            // whenever its inode does.
            Ok(old == new)
        }
    }

    // A file of the same length on both disks, read side by side.
    fn same_bytes(&self, path: &InPathBuf) -> Result<bool> {
        let (mut a, mut b) = (self.before.open(path)?, self.after.open(path)?);
        let (mut x, mut y) = (vec![0; 1 << 16], vec![0; 1 << 16]);
        loop {
            let late = |why| self.after.unbounded(why);
            self.budget.in_time().map_err(late)?;
            let n = fill(&mut a, &mut x).map_err(read(self.before.path))?;
            let m = fill(&mut b, &mut y).map_err(read(self.after.path))?;
            if x[..n] != y[..m] {
                return Ok(false);
            }
            if n == 0 {
                return Ok(true);
            }
        }
    }

    // A directory on SUMMED that the run made whole, as dockerd and
    // containerd do at their first start, at a VM's first boot. What the
    // agent's first run put in it beside theirs is summed; if nothing, the
    // directory is the boot's, one path added whole.
    fn made(&mut self, root: &'static str, path: &InPathBuf, depth: usize) -> Result<()> {
        let (boot, agent) = self.split(path, depth)?;
        if agent == 0 {
            self.out.boot_only += 1;
        } else {
            self.out.boot_only += boot;
            self.sum(root, Kind::Added, agent);
        }
        Ok(())
    }

    // Whether a directory holds nothing but directories, all the way down.
    fn empty_tree(&mut self, dir: &InPathBuf, depth: usize) -> Result<bool> {
        let deep = self.budget.deep(depth);
        deep.map_err(|why| self.after.unbounded(why))?;
        for (name, m) in self.after.entries(dir, &mut self.budget)? {
            if !m.is_dir() || !self.empty_tree(&dir.join(name), depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // How many paths under a directory `made` walks are the boot's, and how
    // many the agent's. An empty directory, or a file on BOOT_ONLY, is the
    // boot's. Any other file is the agent's, and so is a directory with any
    // of the agent's under it.
    fn split(&mut self, dir: &InPathBuf, depth: usize) -> Result<(u64, u64)> {
        let deep = self.budget.deep(depth);
        deep.map_err(|why| self.after.unbounded(why))?;
        let (mut boot, mut agent) = (0, 0);
        for (name, m) in self.after.entries(dir, &mut self.budget)? {
            let path = dir.join(name);
            if m.is_dir() {
                let (b, a) = self.split(&path, depth + 1)?;
                boot += b;
                agent += a;
                if a == 0 {
                    boot += 1;
                } else {
                    agent += 1;
                }
            } else if boot_only(path.as_ref(), Kind::Added, None) {
                boot += 1;
            } else {
                agent += 1;
            }
        }
        Ok((boot, agent))
    }

    // The boot's own changes are only counted, and take no room in the list.
    // Those under a directory on SUMMED are counted in its one entry.
    fn note(&mut self, path: &InPathBuf, change: Kind, contains: Option<u64>) {
        if boot_only(path.as_ref(), change, contains) {
            self.out.boot_only += 1;
        } else if let Some(root) = summed_under(path.as_ref()) {
            self.sum(root, Kind::Modified, 1 + contains.unwrap_or(0));
        } else {
            let root = SUMMED.iter().find(|at| at.as_bytes() == path.as_ref());
            let path = shown(path.as_ref());
            let listed = self.list(Change {
                path,
                change,
                contains,
                changed_under: None,
            });
            // Its own change, a new mode say: what changed under it joins it.
            if let Some(root) = root {
                self.summing = Some((root, listed.then(|| self.out.paths.len() - 1)));
            }
        }
    }

    // `paths` more under `root`, in its entry: the walk comes to everything
    // under a directory together, so the entry is where the first of them
    // would be listed. If it didn't fit, it's one path in `more`, however
    // many are under it.
    fn sum(&mut self, root: &'static str, change: Kind, paths: u64) {
        if let Some((at, entry)) = self.summing
            && at == root
        {
            if let Some(i) = entry {
                let n = self.out.paths[i].changed_under.get_or_insert(0);
                *n = n.saturating_add(paths);
            }
            return;
        }
        let entry = Change {
            path: root.to_string(),
            change,
            contains: None,
            changed_under: Some(paths),
        };
        let listed = self.list(entry);
        self.summing = Some((root, listed.then(|| self.out.paths.len() - 1)));
    }

    // Lists a change, if it fits. Once one doesn't, the rest are only
    // counted, so the list stays in order.
    fn list(&mut self, change: Change) -> bool {
        let fits = self.out.paths.len() < self.cap && self.listed + change.path.len() <= PATH_BYTES;
        if self.out.more == 0 && fits {
            self.listed += change.path.len();
            self.out.paths.push(change);
            return true;
        }
        self.out.more += 1;
        false
    }
}

// Names are bytes and JSON is text. Escaping the backslash too keeps two
// different names from ever printing the same.
pub(crate) fn shown(path: &[u8]) -> String {
    let mut out = String::new();
    for chunk in path.utf8_chunks() {
        out.push_str(&chunk.valid().replace('\\', r"\\"));
        for b in chunk.invalid() {
            out.push_str(&format!(r"\x{b:02x}"));
        }
    }
    out
}

pub(crate) fn fill(file: &mut File, buf: &mut [u8]) -> std::result::Result<usize, Ext4Error> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_bytes(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
