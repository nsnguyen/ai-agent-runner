// Bearer tokens for `runner mcp --http` (src/mcp.rs). `runner mcp-http
// token` makes one from 32 random bytes, prints it once, and keeps only its
// SHA-256, in <root>/mcp-http/tokens: a line each, the hash in hex, then
// when it was made and how, words a reader past the hash leaves alone. The
// directory is 0700 and the file 0600, both runner's. The server reads the
// file at each request, so a revoke takes hold at once, and compares hashes
// in constant time. No token is ever logged. #177's OAuth mints tokens
// into the same file.
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
        "{dir} isn't this user's, so the server, which runs as its owner, couldn't read a token made here: run it as that user, like `sudo -u runner runner mcp-http token`"
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
    dir(root).join("tokens")
}

// Held while the file is rewritten, so two at once lose neither's token.
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

fn mine(path: &Path) -> Result<()> {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

// The file's text, empty when there's none.
fn read(root: &Path) -> Result<String> {
    let path = file(root);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io_err(format!("reading {}", path.display()))(e)),
    }
}

// Writes the file anew, 0600, in one move.
fn write(root: &Path, text: &str) -> Result<()> {
    let path = file(root);
    let new = dir(root).join("tokens.new");
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

/// A new token: 32 bytes from the kernel's random source, in hex, with its
/// hash added to the file, noted with when and `how`, like "cli". The token
/// itself is only returned.
pub fn mint(root: &Path, how: &str) -> Result<String> {
    ready(root)?;
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(io_err("reading /dev/urandom"))?;
    let token = hex(&bytes);
    let _held = lock(root)?;
    let mut text = read(root)?;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let made = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let how: String = how.split_whitespace().collect::<Vec<_>>().join("-");
    text.push_str(&format!("{} {made} {how}\n", hex(&hash(&token))));
    write(root, &text)?;
    Ok(token)
}

/// Drops every token, and says how many there were.
pub fn revoke(root: &Path) -> Result<usize> {
    mine(root)?;
    if !dir(root).exists() {
        return Ok(0);
    }
    ready(root)?;
    let _held = lock(root)?;
    let n = hashes(&read(root)?).len();
    write(root, "")?;
    Ok(n)
}

// Each line's hash; a line that doesn't start with one is skipped.
fn hashes(text: &str) -> Vec<[u8; 32]> {
    (text.lines())
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(unhex)
        .collect()
}

/// Whether `token` is one the file holds. Every hash is compared, each in
/// constant time, so how long it takes says nothing of how near a guess
/// came.
pub fn check(root: &Path, token: &str) -> Result<bool> {
    let presented = hash(token);
    let mut found = 0u8;
    for kept in hashes(&read(root)?) {
        let diff = (kept.iter().zip(presented.iter())).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        found |= u8::from(diff == 0);
    }
    Ok(std::hint::black_box(found) == 1)
}
