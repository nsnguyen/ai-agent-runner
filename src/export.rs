// `runner export`: a stopped VM as one stream, to keep anywhere. It's a tar
// that GNU tar reads, every entry under the VM's name:
//
//   <vm>/manifest.json            first: the format, its settings, and what each disk is
//   <vm>/disk.ext4                its current disk
//   <vm>/checkpoints/<id>.ext4    each of the owner's checkpoints, oldest first
//
// Each disk goes as a GNU sparse file, holding only the extents SEEK_DATA
// finds, so a 20 GiB disk with 4.6 GB on it streams about 4.6 GB. Space the
// guest freed that the host's file still holds goes as data. Services are
// units on the disk, so they come with it. Left out: undo's older
// generations, runs' and the agent's checkpoints, `runs/`, the slot, and the
// sleep snapshot, which holds the guest's memory and credentials.
//
// The tar crate writes it, from files the store opened under the VM's lock
// and let go (Store::open_disks), so the VM can start, or be deleted, while
// it streams.
use crate::creds::Cred;
use crate::store::{self, Config, Disks, Store};
use crate::systemd::{self, State};
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use std::io::{self, BufWriter, Write};
use tar::{Builder, EntryType, Header};

/// The manifest's format: an import refuses one it doesn't know.
pub const FORMAT: u32 = 1;

/// The stream's first entry, under the VM's name.
pub const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// The VM's name where it was exported, which every entry is under.
    pub name: String,
    /// When.
    pub exported: DateTime<Utc>,
    /// Its sizes, kernel path, idle time and URL setting, as stored: each one
    /// left out is the default.
    pub config: Config,
    /// The credentials its allowlist names. Names only; no value is kept.
    pub creds: Vec<Cred>,
    /// Its current disk.
    pub disk: Disk,
    /// The owner's checkpoints, oldest first; none with --no-checkpoints.
    pub checkpoints: Vec<Saved>,
}

/// A VM's current disk in the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    /// Its entry.
    pub path: String,
    /// Its generation in the VM's history.
    pub generation: u64,
    /// Its size, holes and all.
    pub bytes: u64,
}

/// A checkpoint in the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// What `runner restore` takes.
    pub id: u64,
    /// When it was made.
    pub time: DateTime<Utc>,
    /// What it was made with.
    pub comment: Option<String>,
    /// Its entry.
    pub path: String,
    /// Its disk's size, holes and all.
    pub bytes: u64,
}

/// What went out.
#[derive(Debug)]
pub struct Exported {
    pub manifest: Manifest,
    /// The stream's length.
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error("{vm} is {state}: stop it first, with `runner stop {vm}`")]
    Up { vm: String, state: State },
    #[error("exporting {vm}: {source}")]
    Stream { vm: String, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Writes the stopped VM `name` to `out` as one tar stream, with the owner's
/// checkpoints unless `checkpoints` is false. A VM that's up, or that
/// another command holds, is refused before anything is written.
pub fn export(store: &Store, name: &str, checkpoints: bool, out: impl Write) -> Result<Exported> {
    // Asked first, as import --replace does: only systemd tells running
    // from sleeping. Its boot holds the VM's lock while it's up, which
    // open_disks then goes by.
    let state = systemd::status(store, name)?.state;
    if state != State::Stopped {
        let vm = name.to_string();
        return Err(Error::Up { vm, state });
    }
    let disks = store.open_disks(name, checkpoints)?;
    stream(name, disks, out)
}

// Big writes, so a disk goes out in few syscalls.
const BUF: usize = 1 << 20;

fn stream(name: &str, disks: Disks, out: impl Write) -> Result<Exported> {
    let entry = |rest: &str| format!("{name}/{rest}");
    let Disks {
        config,
        creds,
        current,
        checkpoints,
    } = disks;
    let disk = Disk {
        path: entry("disk.ext4"),
        generation: current.info.id,
        bytes: current.info.bytes,
    };
    let saved = checkpoints.iter().map(|c| Saved {
        id: c.info.id,
        time: c.info.time,
        comment: c.info.comment.clone(),
        path: entry(&format!("checkpoints/{}.ext4", c.info.id)),
        bytes: c.info.bytes,
    });
    let manifest = Manifest {
        format: FORMAT,
        name: name.to_string(),
        exported: Utc::now().trunc_subsecs(0),
        config,
        creds,
        disk,
        checkpoints: saved.collect(),
    };
    let paths =
        std::iter::once(&manifest.disk.path).chain(manifest.checkpoints.iter().map(|c| &c.path));
    let files = std::iter::once(current.file).chain(checkpoints.into_iter().map(|c| c.file));
    let mut tar = Builder::new(Gate {
        out: BufWriter::with_capacity(BUF, out),
        bytes: 0,
        shut: false,
    });
    let written = (|| {
        append_manifest(&mut tar, &manifest)?;
        for (path, mut file) in paths.zip(files) {
            let at = |e: io::Error| io::Error::new(e.kind(), format!("{path}: {e}"));
            tar.append_file(path, &mut file).map_err(at)?;
        }
        tar.finish()?;
        tar.get_mut().flush()
    })();
    match written {
        Ok(()) => {
            let bytes = tar.get_ref().bytes;
            Ok(Exported { manifest, bytes })
        }
        Err(source) => {
            // The Builder ends the stream as it's dropped, which would make
            // one cut short look whole to a reader that doesn't count.
            tar.get_mut().shut = true;
            let vm = name.to_string();
            Err(Error::Stream { vm, source })
        }
    }
}

fn append_manifest(tar: &mut Builder<impl Write>, manifest: &Manifest) -> io::Result<()> {
    let mut json = serde_json::to_vec_pretty(manifest)?;
    json.push(b'\n');
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(manifest.exported.timestamp().max(0) as u64);
    let path = format!("{}/{MANIFEST}", manifest.name);
    tar.append_data(&mut header, path, json.as_slice())
}

// Where the stream goes: counts what goes out, and once shut, takes
// nothing more.
struct Gate<W: Write> {
    out: BufWriter<W>,
    bytes: u64,
    shut: bool,
}

impl<W: Write> Write for Gate<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.shut {
            return Err(io::Error::other("the export failed"));
        }
        let n = self.out.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.shut {
            return Ok(());
        }
        self.out.flush()
    }
}
