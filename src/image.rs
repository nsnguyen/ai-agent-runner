// A base image's record of what it was built from, and bringing a disk in as
// a VM. images/base/build.sh writes the record into the image as
// /.runner/build: a fingerprint of what the image is made from, which
// `build.sh --fingerprint` prints for a checkout without building, and the
// commit it was built at. `runner import` reads it from the disk it brings
// in, in userspace with ext4-view as src/changes.rs reads disks, and keeps it
// in the new generation's note, where `runner doctor` shows base's. A check
// that compares the two can tell when base needs rebuilding.
//
// The record is a few `key=value` lines, `#` starting a comment:
//
//   fingerprint=<sha256, 64 lowercase hex>
//   commit=<the commit, -dirty if the tree had changes>
//
// A guest with root can rewrite its own copy, so in a VM's disk it's a hint,
// not a record. Only what parses is kept: a fingerprint that isn't one makes
// the build unknown.
use crate::store::{self, Source, Store, Vm};
use crate::systemd::{self, State};
use ext4_view::Ext4;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Where in an image its record is.
pub const RECORD: &str = "/.runner/build";

// Longer than any record build.sh writes.
const MAX_BYTES: usize = 4096;

/// What an image says it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    /// build.sh's fingerprint of what the image is made from: the files
    /// under images/base and the runner-guest binary.
    pub fingerprint: String,
    /// The commit it was built at, `-dirty` if the tree had changes; None
    /// if build.sh couldn't tell.
    pub commit: Option<String>,
}

/// What the ext4 disk at `disk` says it was built from. None if it has no
/// record, the record doesn't say, or the disk can't be read as ext4.
pub fn read(disk: &Path) -> Option<Build> {
    let fs = Ext4::load_from_path(disk).ok()?;
    let mut file = fs.open(RECORD).ok()?;
    let meta = file.metadata();
    if !meta.file_type().is_regular_file() || meta.len() > MAX_BYTES as u64 {
        return None;
    }
    let mut text = vec![0; MAX_BYTES];
    let mut n = 0;
    while n < text.len() {
        match file.read_bytes(&mut text[n..]).ok()? {
            0 => break,
            k => n += k,
        }
    }
    parse(std::str::from_utf8(&text[..n]).ok()?)
}

/// How long the ext4 filesystem at the start of `disk` says it is, from its
/// superblock: what a disk that came in as a stream should be at least. None
/// if it doesn't start with one.
pub fn ext4_size(disk: &Path) -> Option<u64> {
    let mut sb = [0; 1024];
    let mut file = File::open(disk).ok()?;
    file.seek(SeekFrom::Start(1024)).ok()?;
    file.read_exact(&mut sb).ok()?;
    let u32_at = |i: usize| u32::from_le_bytes(sb[i..i + 4].try_into().unwrap());
    if u16::from_le_bytes([sb[0x38], sb[0x39]]) != 0xEF53 {
        return None;
    }
    let mut blocks = u64::from(u32_at(0x04));
    // The high half counts only with the 64bit feature.
    if u32_at(0x60) & 0x80 != 0 {
        blocks |= u64::from(u32_at(0x150)) << 32;
    }
    let log = u32_at(0x18);
    if log > 6 {
        return None;
    }
    blocks.checked_mul(1024 << log)
}

fn parse(text: &str) -> Option<Build> {
    let (mut fingerprint, mut commit) = (None, None);
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some(("fingerprint", v)) => fingerprint = Some(v),
            Some(("commit", v)) => commit = Some(v),
            _ => {}
        }
    }
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let fingerprint = fingerprint.filter(|f| f.len() == 64 && f.bytes().all(hex))?;
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"._-".contains(&b);
    let commit = commit.filter(|c| (1..=64).contains(&c.len()) && c.bytes().all(plain));
    Some(Build {
        fingerprint: fingerprint.to_string(),
        commit: commit.map(str::to_string),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error("a VM named {0} already exists: --replace puts the disk on top of its history")]
    Exists(String),
    #[error("{vm} is {state}: `runner stop {vm}` first, then import")]
    Up { vm: String, state: State },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Brings `disk` in as the VM `name`, usually `base`, noted with what it
/// says it was built from. Without `replace`, a name that exists is refused.
/// With it, the disk goes on top of that VM's history as a new generation
/// and its settings stay, so `runner undo` brings the old disk back; that's
/// refused while the VM is up, as undo is, and a name that doesn't exist is
/// imported. Forks keep their own disks either way.
pub fn import(store: &Store, name: &str, disk: Source, replace: bool) -> Result<Vm> {
    let exists = |e| match e {
        store::Error::Exists(name) => Error::Exists(name),
        e => Error::Store(e),
    };
    if !replace {
        return store.import_from(name, disk).map_err(exists);
    }
    match store.get(name) {
        Err(store::Error::NoVm(_)) => return store.import_from(name, disk).map_err(exists),
        r => r?,
    };
    // Asked before the copy, which can take minutes; the VM's lock, which
    // its boot holds while it's up, is what finish_restore goes by.
    let state = systemd::status(store, name)?.state;
    if state != State::Stopped {
        let vm = name.to_string();
        return Err(Error::Up { vm, state });
    }
    let staged = store.stage_disk(name, disk, "import")?;
    let finished = store.finish_restore(name, &staged);
    // One that failed after it went into history is current: it stays.
    let current = store.get(name).map(|vm| vm.generation);
    if finished.is_err() && current.is_ok_and(|g| g != staged.generation) {
        let generation = staged.generation;
        if let Err(e) = store.drop_restore(name, staged) {
            eprintln!("runner: couldn't remove {name}'s generation {generation}: {e}");
        }
    }
    Ok(finished?)
}
