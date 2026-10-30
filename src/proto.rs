// The protocol between the host and runner-guest, the agent inside each VM.
//
// The host reaches the guest on vsock PORT, through Firecracker's vsock
// socket (`CONNECT <PORT>`). A connection runs one command:
//
//   both sides   hello: MAGIC then VERSION, as a u32 big-endian, 8 bytes
//   host         Request, then any ToGuest frames
//   guest        FromGuest frames, ending with Exit or Error
//
// After the hello, every message is a frame: a u32 big-endian length, then
// that many bytes of postcard. A peer whose hello doesn't match is refused
// before any frame, so a host and guest built apart say so instead of
// misreading each other. Bump VERSION on any change below the hello; the
// hello itself never changes shape.
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// The guest agent's vsock port: 22, ssh's port, in place of sshd.
pub const PORT: u32 = 10022;
pub const VERSION: u32 = 1;
/// Starts every hello, so a wrong port or a stray peer fails fast.
pub const MAGIC: [u8; 4] = *b"rgst";
/// The largest frame either side sends or accepts, header excluded.
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the other end isn't runner-guest or a runner host")]
    NotPeer,
    #[error("the other end speaks protocol v{theirs}, this end speaks v{ours}")]
    Version { ours: u32, theirs: u32 },
    #[error("a frame of {0} bytes is over the {MAX_FRAME}-byte limit")]
    TooBig(usize),
    #[error("the connection closed partway through a frame")]
    Truncated,
    #[error("bad frame: {0}")]
    Decode(#[from] postcard::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What to run. The host's first frame, and its only one of this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The program and its arguments; the program is looked up in PATH.
    pub argv: Vec<String>,
    /// Set over a clean environment: PATH, HOME, USER, LOGNAME and SHELL
    /// for the user, plus TERM with a PTY.
    pub env: Vec<(String, String)>,
    /// Defaults to the user's home, or / if it has none.
    pub cwd: Option<String>,
    /// Defaults to the agent's own user, root.
    pub user: Option<String>,
    /// Some runs the command on a new PTY of this size, as its controlling
    /// terminal. Its stdout and stderr then both come back as Stdout.
    pub tty: Option<WinSize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
}

/// The host's frames after the Request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToGuest {
    Stdin(Vec<u8>),
    /// Closes the command's stdin. Ignored with a PTY, as ssh -tt does;
    /// send the terminal's EOF character instead.
    CloseStdin,
    /// Resizes the PTY, which signals SIGWINCH. Ignored without one.
    Resize(WinSize),
    /// Sends a signal to the command's process group.
    Signal(i32),
}

/// The guest's frames. Exit or Error is the last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromGuest {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(Exit),
    /// The command couldn't start, and why.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exit {
    Code(i32),
    /// Killed by this signal.
    Signal(i32),
}

pub fn hello(w: &mut impl Write) -> io::Result<()> {
    let mut buf = [0; 8];
    buf[..4].copy_from_slice(&MAGIC);
    buf[4..].copy_from_slice(&VERSION.to_be_bytes());
    w.write_all(&buf)?;
    w.flush()
}

/// Reads the peer's hello and checks it matches this end's.
pub fn check_hello(r: &mut impl Read) -> Result<()> {
    let mut buf = [0; 8];
    r.read_exact(&mut buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => Error::NotPeer,
        _ => e.into(),
    })?;
    if buf[..4] != MAGIC {
        return Err(Error::NotPeer);
    }
    let theirs = u32::from_be_bytes(buf[4..].try_into().unwrap());
    if theirs != VERSION {
        return Err(Error::Version {
            ours: VERSION,
            theirs,
        });
    }
    Ok(())
}

/// Writes one frame with a single write, so frames from threads sharing a
/// writer don't interleave.
pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<()> {
    let mut buf = postcard::to_extend(msg, vec![0; 4])?;
    let len = buf.len() - 4;
    if len > MAX_FRAME {
        return Err(Error::TooBig(len));
    }
    buf[..4].copy_from_slice(&(len as u32).to_be_bytes());
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame. None means the peer closed cleanly between frames.
pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> Result<Option<T>> {
    let mut head = [0; 4];
    let got = fill(r, &mut head)?;
    if got == 0 {
        return Ok(None);
    }
    if got < head.len() {
        return Err(Error::Truncated);
    }
    let len = u32::from_be_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(Error::TooBig(len));
    }
    let mut body = vec![0; len];
    if fill(r, &mut body)? < len {
        return Err(Error::Truncated);
    }
    match postcard::take_from_bytes(&body)? {
        (msg, []) => Ok(Some(msg)),
        _ => Err(postcard::Error::DeserializeBadEncoding.into()),
    }
}

// Like read_exact, but says how much it got before the end.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}
