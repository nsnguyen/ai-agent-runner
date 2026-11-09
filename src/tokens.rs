// Bearer tokens for `runner mcp --http` (src/mcp.rs). `runner mcp-http
// token` makes one from 32 random bytes, prints it once, and keeps only its
// SHA-256, in <root>/mcp-http/tokens: a line each, the hash in hex, then
// when it was made and how. OAuth's (src/oauth.rs) go in the same file, with
// fields past those: `client=<id>`, `kind=access` or `kind=refresh`,
// `until=<unix seconds>`, and `spent` on a refresh token that's been used.
// A refresh token is never a bearer token, nor is one past its `until`. A
// line that doesn't start with a hash is skipped. The directory is 0700 and
// its files 0600, all runner's. The server reads the file at each request,
// so a revoke takes hold at once, and compares hashes in constant time. No
// token is ever logged.
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: {1}")]
    Io(String, #[source] io::Error),
    #[error(
        "{dir} isn't this user's, so the server, which runs as its owner, couldn't read what's made here: run it as that user, like `sudo -u runner runner mcp-http token`"
    )]
    NotMine { dir: String },
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(what: impl std::fmt::Display) -> impl FnOnce(io::Error) -> Error {
    let what = what.to_string();
    move |e| Error::Io(what, e)
}

/// Where the tokens live.
pub fn dir(root: &Path) -> PathBuf {
    root.join("mcp-http")
}

/// The file of hashes, a line each.
pub fn file(root: &Path) -> PathBuf {
    dir(root).join(TOKENS)
}

const TOKENS: &str = "tokens";

// Held while a file here is rewritten, so two at once lose neither's change.
fn lock(root: &Path) -> Result<File> {
    let path = dir(root).join("lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(io_err(format!("opening {}", path.display())))?;
    file.lock()
        .map_err(io_err(format!("locking {}", path.display())))?;
    Ok(file)
}

/// Runs `f` with the directory made and locked: what changes a file here.
pub fn locked<T>(root: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    ready(root)?;
    let _held = lock(root)?;
    f()
}

// The directory, made 0700 if it isn't there, and this user's, as the root
// is: the server runs as the root's owner, runner, and reads the file as it.
// Anyone else, root above all, is refused before anything is made.
fn ready(root: &Path) -> Result<()> {
    mine(root)?;
    let dir = dir(root);
    match DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_err(format!("making {}", dir.display()))(e)),
    }
    mine(&dir)
}

/// Whether `path` is this user's: what reads the store has to be.
pub fn mine(path: &Path) -> Result<()> {
    let meta = fs::metadata(path).map_err(io_err(format!("reading {}", path.display())))?;
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(Error::NotMine {
            dir: path.display().to_string(),
        });
    }
    Ok(())
}

/// The hash a token is kept and checked by.
pub fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Whether two hashes are the same, in a time that doesn't say how near.
pub fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = (a.iter().zip(b.iter())).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// `N` bytes from the kernel's random source.
pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(io_err("reading /dev/urandom"))?;
    Ok(bytes)
}

/// A file here's text, empty when there's none.
pub fn read(root: &Path, name: &str) -> Result<String> {
    let path = dir(root).join(name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io_err(format!("reading {}", path.display()))(e)),
    }
}

/// Writes a file here anew, 0600, in one move. Held under `locked`.
pub fn write(root: &Path, name: &str, text: &str) -> Result<()> {
    let path = dir(root).join(name);
    let new = dir(root).join(format!("{name}.new"));
    let what = format!("writing {}", path.display());
    let _ = fs::remove_file(&new);
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&new)
        .map_err(io_err(&what))?;
    f.write_all(text.as_bytes()).map_err(io_err(&what))?;
    f.sync_all().map_err(io_err(&what))?;
    fs::rename(&new, &path).map_err(io_err(&what))
}

/// Removes a file here, if it's there. Held under `locked`.
pub fn remove(root: &Path, name: &str) -> Result<bool> {
    let path = dir(root).join(name);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err(format!("removing {}", path.display()))(e)),
    }
}

/// When, for a token's line.
pub fn stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A new token: 32 bytes from the kernel's random source, in hex, with its
/// hash added to the file, noted with when and `how`, like "cli". The token
/// itself is only returned.
pub fn mint(root: &Path, how: &str) -> Result<String> {
    ready(root)?;
    let token = hex(&random::<32>()?);
    let how: String = how.split_whitespace().collect::<Vec<_>>().join("-");
    let line = format!("{} {} {how}", hex(&hash(&token)), stamp());
    edit(root, |lines| lines.push(line))?;
    Ok(token)
}

/// Runs `f` on the file's lines, locked, and writes back what it leaves.
pub fn edit<T>(root: &Path, f: impl FnOnce(&mut Vec<String>) -> T) -> Result<T> {
    locked(root, || {
        let text = read(root, TOKENS)?;
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let out = f(&mut lines);
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        write(root, TOKENS, &text)?;
        Ok(out)
    })
}

/// Drops every token, and says how many there were.
pub fn revoke(root: &Path) -> Result<usize> {
    mine(root)?;
    if !dir(root).exists() {
        return Ok(0);
    }
    edit(root, |lines| {
        let n = lines.iter().filter(|l| Entry::parse(l).is_some()).count();
        lines.clear();
        n
    })
}

/// What a token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A bearer token: the CLI's, or an OAuth access token.
    Access,
    /// An OAuth refresh token, which only buys a new pair.
    Refresh,
}

/// A line of the file, read.
#[derive(Debug)]
pub struct Entry<'a> {
    pub hash: [u8; 32],
    /// The OAuth client's, or none for the CLI's.
    pub client: Option<&'a str>,
    pub kind: Kind,
    /// When it stops being good, if it does.
    pub until: Option<i64>,
    /// A refresh token that's been used.
    pub spent: bool,
}

impl<'a> Entry<'a> {
    pub fn parse(line: &'a str) -> Option<Entry<'a>> {
        let mut words = line.split_whitespace();
        let hash = unhex(words.next()?)?;
        let mut entry = Entry {
            hash,
            client: None,
            kind: Kind::Access,
            until: None,
            spent: false,
        };
        for word in words {
            match word.split_once('=') {
                Some(("client", id)) => entry.client = Some(id),
                Some(("kind", "refresh")) => entry.kind = Kind::Refresh,
                // An unreadable time is one long past.
                Some(("until", t)) => entry.until = Some(t.parse().unwrap_or(0)),
                None if word == "spent" => entry.spent = true,
                _ => {}
            }
        }
        Some(entry)
    }

    /// Whether it's still good at `now`.
    pub fn live(&self, now: i64) -> bool {
        self.until.is_none_or(|until| now < until)
    }
}

/// Who a bearer token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    /// One `runner mcp-http token` made.
    Cli,
    /// An OAuth client's access token.
    Client(String),
}

/// Who `token` is, if the file holds it as a live bearer token at `now`.
/// Every hash is compared, each in constant time, so how long it takes says
/// nothing of how near a guess came.
pub fn check(root: &Path, token: &str, now: i64) -> Result<Option<Holder>> {
    let presented = hash(token);
    let text = read(root, TOKENS)?;
    let mut found = None;
    for line in text.lines() {
        let Some(entry) = Entry::parse(line) else {
            continue;
        };
        if same(&entry.hash, &presented) {
            found = Some(entry);
        }
    }
    Ok(found
        .filter(|e| e.kind == Kind::Access && e.live(now))
        .map(|e| {
            e.client
                .map_or(Holder::Cli, |c| Holder::Client(c.to_string()))
        }))
}
