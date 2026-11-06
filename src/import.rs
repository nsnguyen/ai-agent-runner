// `runner import` of an export: the stream export.rs writes, back as a new
// VM, on this host or another. The tar crate reads it here, in Rust; tar the
// program never runs over a stream from elsewhere. Only what the manifest
// names is taken, in the order export writes it, each a plain or sparse
// file, and written to names the store makes (gens/<id>.ext4), seeking over
// holes, so a sparse disk stays sparse. Then tar's end, two zero blocks,
// and nothing after but zeros. The VM is built under a staging name and
// renamed into place last (Store::make_from), so a stream that's cut short
// or refused leaves nothing and doesn't take the name.
//
// What comes in: the current disk, as a generation past every checkpoint's
// id; the owner's checkpoints under the ids, times and comments they had;
// and the config. Services are on the disk. What doesn't: the slot, which
// is picked here, so a new address and URL port; undo's history; and the
// allowlist, which is the default unless the caller says, as a fork doesn't
// take its source's.
use crate::creds::Cred;
use crate::export::{FORMAT, MANIFEST, Manifest};
use crate::store::{self, NewVm, Store, Vm};
use serde::Deserialize;
use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::PathBuf;
use std::rc::Rc;
use tar::{Archive, Entries, Entry, EntryType};

/// What came in.
#[derive(Debug)]
pub struct Imported {
    pub vm: Vm,
    pub manifest: Manifest,
    /// Its allowlist.
    pub creds: Vec<Cred>,
    /// The kernel its config names, if this host hasn't got it.
    pub kernel_missing: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(
        "the export is format {0}, and this runner reads formats 1 to {FORMAT}: upgrade the runner"
    )]
    Format(u32),
    #[error("the stream ended at {at} bytes, in {within}: was it cut off?")]
    Cut { at: u64, within: String },
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// Far past any export's.
const MAX_MANIFEST: u64 = 16 << 20;
const BLOCK: u64 = 512;
const MAX_ID: u64 = u32::MAX as u64;

/// Whether a stream that starts with `head` is an export: it starts as a
/// tar header does, with ustar's magic at 257. Its checksum is the tar
/// crate's to check, so a header with a byte flipped is refused, not taken
/// for a disk. A raw ext4 disk starts with 1024 bytes of boot block, zeros
/// as mkfs leaves them, then the superblock with its magic at 1080, so it
/// never is one.
pub fn is_export(head: &[u8]) -> bool {
    matches!(head.get(257..263), Some(b"ustar\0" | b"ustar "))
}

/// What a stream that starts with `head` is compressed with, if it starts
/// as gzip, zstd, xz or bzip2 does: an export or a disk to decompress
/// first.
pub fn compressed(head: &[u8]) -> Option<&'static str> {
    const MAGIC: [(&[u8], &str); 4] = [
        (&[0x1f, 0x8b], "gzip"),
        (&[0x28, 0xb5, 0x2f, 0xfd], "zstd"),
        (&[0xfd, b'7', b'z', b'X', b'Z', 0], "xz"),
        (b"BZh", "bzip2"),
    ];
    MAGIC
        .iter()
        .find(|(magic, _)| head.starts_with(magic))
        .map(|&(_, kind)| kind)
}

/// Reads the export on `stream` in as the new VM `name`, with the allowlist
/// `creds`, or the default if None. A name that's taken is refused before a
/// byte is read.
pub fn import(
    store: &Store,
    name: &str,
    stream: impl Read,
    creds: Option<&[Cred]>,
) -> Result<Imported> {
    store.check_free(name)?;
    let shared = Shared(Rc::new(RefCell::new(Counted {
        inner: stream,
        at: 0,
        ended: false,
    })));
    let mut archive = Archive::new(shared.clone());
    let mut entries = archive.entries().map_err(|e| shared.failed(MANIFEST, e))?;
    let manifest = read_manifest(&mut entries, &shared)?;
    let named = Named::of(&manifest)?;
    let vm = store.make_from(name, &manifest.config, creds, |vm: &NewVm| {
        let src = &manifest.name;
        let mut take = |i: usize, id: u64| take(&mut entries, &shared, &named, i, &vm.disk(id));
        take(0, named.current)?;
        vm.current(named.current, &format!("import of {src}'s export"))?;
        for (i, c) in manifest.checkpoints.iter().enumerate() {
            take(i + 1, c.id)?;
            vm.checkpoint(c.id, c.time, c.comment.as_deref())?;
        }
        end(&mut entries, &shared)
    })?;
    let creds = store.creds(name)?;
    let kernel_missing = manifest.config.kernel.clone().filter(|k| !k.is_file());
    Ok(Imported {
        vm,
        manifest,
        creds,
        kernel_missing,
    })
}

// The first entry, `<name>/manifest.json`: its format first, then the rest.
fn read_manifest<R: Read>(
    entries: &mut Entries<Shared<R>>,
    shared: &Shared<R>,
) -> Result<Manifest> {
    let not_one =
        || refused("the stream is a tar, but not an export: it doesn't start with a manifest");
    let mut entry = next(entries, shared, MANIFEST)?.ok_or_else(not_one)?;
    let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
    let Some(dir) = path.strip_suffix(&format!("/{MANIFEST}")) else {
        return Err(refused(format!(
            "the stream starts with {path:?}, not an export's manifest"
        )));
    };
    if entry.header().entry_type() != EntryType::Regular {
        return Err(not_a_file(&path, &entry));
    }
    if entry.size() > MAX_MANIFEST {
        let n = entry.size();
        return Err(refused(format!(
            "{path} is {n} bytes, far past any manifest"
        )));
    }
    let mut json = Vec::new();
    entry
        .read_to_end(&mut json)
        .map_err(|e| shared.failed(&path, e))?;
    let bad = |e: serde_json::Error| refused(format!("{path}: {e}"));
    #[derive(Deserialize)]
    struct Format {
        format: u32,
    }
    let Format { format } = serde_json::from_slice(&json).map_err(bad)?;
    if !(1..=FORMAT).contains(&format) {
        return Err(Error::Format(format));
    }
    let manifest: Manifest = serde_json::from_slice(&json).map_err(bad)?;
    if manifest.name != dir {
        let name = &manifest.name;
        return Err(refused(format!(
            "the manifest names the VM {name:?}, but it's at {path:?}"
        )));
    }
    Ok(manifest)
}

// The entries the manifest names, in the stream's order: the current disk,
// then each checkpoint. And the generation the current disk takes, past
// every checkpoint's id.
struct Named {
    paths: Vec<String>,
    bytes: Vec<u64>,
    current: u64,
}

impl Named {
    // Each named as export names it, under the VM's name, so no name in the
    // stream is anything but plain.
    fn of(m: &Manifest) -> Result<Named> {
        let name = &m.name;
        if store::check_name(name).is_err() {
            return Err(refused(format!(
                "the manifest names the VM {name:?}, which isn't a VM name"
            )));
        }
        let check = |got: &str, want: &str, what: &str| match got == want {
            true => Ok(()),
            false => Err(refused(format!(
                "the manifest names {got:?} for {what}, where an export has {want:?}"
            ))),
        };
        let mut paths = vec![format!("{name}/disk.ext4")];
        let mut bytes = vec![m.disk.bytes];
        check(&m.disk.path, &paths[0], "its disk")?;
        let mut ids = Vec::new();
        for c in &m.checkpoints {
            let want = format!("{name}/checkpoints/{}.ext4", c.id);
            check(&c.path, &want, &format!("checkpoint {}", c.id))?;
            if ids.contains(&c.id) {
                return Err(refused(format!(
                    "the manifest names checkpoint {} twice",
                    c.id
                )));
            }
            ids.push(c.id);
            paths.push(want);
            bytes.push(c.bytes);
        }
        // Generations count up from 0, one per run or checkpoint, so an id
        // past this is no VM's, and would leave no room after it.
        let top = ids.iter().max().copied();
        if let Some(top) = top.filter(|&id| id > MAX_ID) {
            return Err(refused(format!(
                "the manifest names checkpoint {top}, past any VM's generations"
            )));
        }
        let current = top.map_or(0, |top| top + 1);
        Ok(Named {
            paths,
            bytes,
            current,
        })
    }
}

// Entry `i` of what's named, written to `to`.
fn take<R: Read>(
    entries: &mut Entries<Shared<R>>,
    shared: &Shared<R>,
    named: &Named,
    i: usize,
    to: &std::path::Path,
) -> Result<()> {
    let want = &named.paths[i];
    let Some(mut entry) = next(entries, shared, want)? else {
        return Err(match shared.ended() {
            true => shared.cut(want),
            false => refused(format!(
                "the stream's tar ends before {want}, which its manifest names"
            )),
        });
    };
    let path = entry.path_bytes();
    if *path != *want.as_bytes() {
        let got = String::from_utf8_lossy(&path).into_owned();
        return Err(match named.paths.contains(&got) {
            true => refused(format!(
                "{got:?} is out of order: the manifest names {want:?} next"
            )),
            false => refused(format!("the manifest doesn't name {got:?}")),
        });
    }
    if !matches!(
        entry.header().entry_type(),
        EntryType::Regular | EntryType::GNUSparse
    ) {
        return Err(not_a_file(want, &entry));
    }
    let (got, bytes) = (entry.size(), named.bytes[i]);
    if got != bytes {
        return Err(refused(format!(
            "{want} is {got} bytes, but the manifest says {bytes} bytes"
        )));
    }
    // Writes each extent, and seeks over each hole.
    entry.unpack(to).map_err(|e| shared.failed(want, e))?;
    Ok(())
}

// Tar's end, two zero blocks, after the last entry; then nothing but zeros,
// as GNU tar pads a record.
fn end<R: Read>(entries: &mut Entries<Shared<R>>, shared: &Shared<R>) -> Result<()> {
    const END: &str = "tar's end";
    let past = || refused("the stream goes on past the export's end with more than zeros");
    // A block that isn't one of tar's headers isn't its end either.
    let after = entries.next().transpose().map_err(|e| {
        match shared.ended() || e.raw_os_error().is_some() {
            true => shared.failed(END, e),
            false => past(),
        }
    })?;
    if let Some(entry) = after {
        let got = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        return Err(refused(format!("the manifest doesn't name {got:?}")));
    }
    // The tar crate reads only the first zero block, and takes a stream
    // that stops at a block boundary as ended too: either way, the second
    // block is still to read.
    let mut more = shared.clone();
    let mut zeros = [0; BLOCK as usize];
    more.read_exact(&mut zeros)
        .map_err(|e| shared.failed(END, e))?;
    if zeros.iter().any(|&b| b != 0) {
        return Err(past());
    }
    let mut buf = vec![0; 64 << 10];
    loop {
        match more.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) if buf[..n].iter().all(|&b| b == 0) => {}
            Ok(_) => return Err(past()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(shared.failed(END, e)),
        }
    }
}

fn next<'a, R: Read>(
    entries: &mut Entries<'a, Shared<R>>,
    shared: &Shared<R>,
    within: &str,
) -> Result<Option<Entry<'a, Shared<R>>>> {
    entries
        .next()
        .transpose()
        .map_err(|e| shared.failed(within, e))
}

fn not_a_file<R: Read>(path: &str, entry: &Entry<R>) -> Error {
    let kind = match entry.header().entry_type() {
        EntryType::Symlink => "a symlink",
        EntryType::Link => "a hard link",
        EntryType::Directory => "a directory",
        EntryType::GNUSparse => "a sparse file",
        _ => "something else",
    };
    refused(format!("{path} isn't a file: it's {kind}"))
}

fn refused(why: impl Into<String>) -> Error {
    Error::Refused(why.into())
}

// The stream, which the tar crate reads through one handle and `end`
// through another, counting what's been read and whether it ran out.
struct Shared<R>(Rc<RefCell<Counted<R>>>);

struct Counted<R> {
    inner: R,
    at: u64,
    ended: bool,
}

impl<R> Clone for Shared<R> {
    fn clone(&self) -> Self {
        Shared(Rc::clone(&self.0))
    }
}

impl<R> Shared<R> {
    fn at(&self) -> u64 {
        self.0.borrow().at
    }

    fn ended(&self) -> bool {
        self.0.borrow().ended
    }

    fn cut(&self, within: &str) -> Error {
        let at = self.at();
        let within = within.to_string();
        Error::Cut { at, within }
    }

    // What `e`, from reading `within`, says: a cut if the stream ran out,
    // else the error, every cause on one line.
    fn failed(&self, within: &str, e: io::Error) -> Error {
        if self.ended() {
            return self.cut(within);
        }
        let mut why = format!("{within}: {e}");
        let mut cause = std::error::Error::source(&e);
        while let Some(c) = cause {
            let _ = write!(why, ": {c}");
            cause = c.source();
        }
        refused(why)
    }
}

impl<R: Read> Read for Shared<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut s = self.0.borrow_mut();
        let n = s.inner.read(buf)?;
        if n == 0 && !buf.is_empty() {
            s.ended = true;
        }
        s.at += n as u64;
        Ok(n)
    }
}
