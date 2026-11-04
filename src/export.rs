// `runner export`: a VM as one stream, to keep anywhere. It's a tar that GNU
// tar reads, every entry under the VM's name:
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
// The tar crate writes it, from files the store opened and let go of, under
// the VM's lock (Store::open_disks), or a running VM's under its freezer
// (Freezer::open_disks), so the VM can start, stop, or be deleted while it
// streams.
//
// A running VM's current disk is a copy, made as its checkpoint is
// (checkpoint.rs): `sync` in the guest, a pause, a copy of the disk its boot
// runs, a resume. It's the disk as a power cut right after the sync would
// leave it. The copy is unlinked once open (Freezer::open_copy), so no
// checkpoint or generation is left, and the VM carries on while it streams.
// A sleeping VM is woken for it, as for a checkpoint.
use crate::checkpoint;
use crate::creds::Cred;
use crate::idle::InUse;
use crate::runs;
use crate::store::{self, Config, Disks, Store};
use crate::systemd::{self, State};
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use std::io::{self, BufWriter, Write};
use tar::{Builder, EntryType, Header};

/// The manifest's format: an import reads it before the rest, and refuses
/// one it doesn't know. A field an older import wouldn't know, in the
/// manifest or its config (which denies unknown fields), bumps it.
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
    #[error(transparent)]
    Checkpoint(#[from] checkpoint::Error),
    #[error(transparent)]
    Runs(#[from] runs::Error),
    #[error("{vm} is {state}: try again once it's running or stopped")]
    NotNow { vm: String, state: State },
    #[error(
        "run {run} is going in {vm}, and its disk may roll back: export it once the run is over"
    )]
    RunGoing { vm: String, run: String },
    #[error(
        "something holds {0}'s run lock: a run starting, a restore, a sleep or another \
         export. Export it once that's over"
    )]
    RunStarting(String),
    #[error("exporting {vm}: {source}")]
    Stream { vm: String, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Writes the VM `name` to `out` as one tar stream, with the owner's
/// checkpoints unless `checkpoints` is false. A running VM's disk is a copy
/// made with it paused, and it carries on; a sleeping one is woken first.
/// One booting or stopping, one with a run going, and a stopped one another
/// command holds are refused before anything is written.
pub fn export(store: &Store, name: &str, checkpoints: bool, out: impl Write) -> Result<Exported> {
    let disks = open(store, name, checkpoints)?;
    write(name, disks, out)
}

// Every disk the export streams, open, and nothing of the VM's held once
// it returns.
fn open(store: &Store, name: &str, checkpoints: bool) -> Result<Disks> {
    // Before the look, so an idle sleep can't start between it and the sync.
    let _in_use = InUse::take(store, name)?;
    // A stopped VM needs no systemd, so this works on any machine. Its boot
    // holds the VM's lock while it's up, which open_disks goes by.
    match store.open_disks(name, checkpoints) {
        Err(store::Error::Busy(_)) => {}
        done => return Ok(done?),
    }
    // A run's disk may roll back when it ends. Held until the copy is made,
    // and before the look, so no sleep starts after it: a sleep takes it too.
    let _no_run = runs::hold(store, name).map_err(|e| match e {
        runs::Error::Busy { vm, run } => Error::RunGoing { vm, run },
        runs::Error::Starting(vm) => Error::RunStarting(vm),
        e => e.into(),
    })?;
    // Only systemd tells running from sleeping.
    match systemd::status(store, name)?.state {
        State::Running => {}
        // Its guest syncs its disk first, which it can only do awake.
        State::Sleeping => {
            systemd::wake(store, name)?;
        }
        // Held, but not by a boot, or stopped since: as a stopped VM.
        State::Stopped => return Ok(store.open_disks(name, checkpoints)?),
        state => {
            let vm = name.to_string();
            return Err(Error::NotNow { vm, state });
        }
    }
    let freezer = store.hold_checkpoints(name)?;
    let current = checkpoint::paused(store, name, &freezer, |f| f.open_copy())?;
    Ok(freezer.open_disks(current, checkpoints)?)
}

// Big writes, so a disk goes out in few syscalls.
const BUF: usize = 1 << 20;

/// Writes `disks`, as Store::open_disks opened them, to `out` as the export
/// of `name`. A failure partway leaves the stream cut short, never ended as
/// a whole one is.
pub fn write(name: &str, disks: Disks, out: impl Write) -> Result<Exported> {
    let entry = |rest: &str| format!("{name}/{rest}");
    let Disks {
        config,
        creds,
        current,
        checkpoints,
    } = disks;
    let disk = Disk {
        path: entry("disk.ext4"),
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
