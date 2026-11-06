// `runner sleep` and `wake`: a running VM's memory and devices go to a
// Firecracker snapshot in its directory and Firecracker stops, so the VM
// takes no CPU and no RAM; waking loads the snapshot into a fresh
// Firecracker, and every process in the VM carries on where it was.
//
// The VM's boot does both, since it runs Firecracker (src/boot.rs). It stays
// up while the VM sleeps, holding the VM's lock, its tap and the sockets its
// guest takes credentials and makes calls on, so a wake only starts
// Firecracker and loads. It takes asks on `<vm>/boot.sock`, one at a time,
// from `runner sleep`, `wake`, `start` and `halt`. Only the runner user can
// connect. A connection makes one ask, framed as src/proto.rs frames, with a
// hello of its own:
//
//   both sides   hello: MAGIC then VERSION, as a u32 big-endian, 8 bytes
//   asker        Ask
//   boot         Reply
//
// Both ends are this binary, but a boot outlives an upgrade of it, so asks
// and replies are only added at the end, as with src/calls.rs.
//
// A snapshot is only good with the disk its boot runs: the generation that
// boot hasn't committed yet, which the snapshot's note names (store::Slept).
// The boot holds the VM's lock all the while, so nothing commits, undoes or
// collects around it. `restore` stops the VM first (src/checkpoint.rs), and
// a sleeping VM's stop wakes it to shut it down cleanly. A boot that dies
// asleep leaves its snapshot for the next boot to clear.
//
// But a host shutdown parks it: runner-park.service, which stops before the
// VMs' units, runs `runner park --all`, and the boot of each sleeping VM syncs
// its snapshot and disk, notes them parked (store::Run::park), and ends
// without committing. Its unit stops with it, and its next boot, at a
// `start` or `wake`, gets that run back and loads the snapshot onto that disk,
// with its tap and sockets where they were, and sets the guest's clock, as a
// wake does. One that can't load boots the last committed disk instead, and
// the parked disk stays for a look (src/boot.rs). Until then the store holds
// a parked VM busy itself, as a sleeping VM's boot does, and status says
// sleeping. Only a host shutdown parks: `stop`, `restore` and the rest wake a
// sleeping VM, and resume a parked one, to shut it down cleanly.
use crate::proto;
use crate::store::{self, Store};
use serde::{Deserialize, Serialize};
use std::fs::{self, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Starts every hello, apart from proto's and calls', so a peer on the
/// wrong socket fails at once.
pub const MAGIC: [u8; 4] = *b"rbot";
/// Changes only when an ask or a reply can't be added at the end.
pub const VERSION: u32 = 1;

// An asker says what it wants at once.
const ASK_WAIT: Duration = Duration::from_secs(10);
// A sleep writes the VM's whole memory, and a halt's wake waits on the guest.
const REPLY_WAIT: Duration = Duration::from_secs(15 * 60);

/// What a VM's boot is asked to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    /// Snapshot the running VM and stop its Firecracker. Done at once if
    /// it's asleep.
    Sleep,
    /// Load the snapshot into a fresh Firecracker. Done at once if it's awake.
    Wake,
    /// Shut it down, cleanly: wake it if it's asleep, then Ctrl-Alt-Del, as
    /// `runner halt` does. Sleeps are refused from then on.
    Halt,
    /// The host is shutting down: if it's asleep, park it, and end the boot.
    /// One awake is refused, and sleeps are refused from then on, so its
    /// unit's stop shuts it down cleanly.
    Park,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Done,
    /// Why not.
    Refused(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(
        "{0} has no boot to ask: it's stopped, or it booted under a runner from before \
         sleep and wake, which a stop and start brings up to date"
    )]
    NoBoot(String),
    #[error("{0}")]
    Refused(String),
    #[error("asking {vm}'s boot: {source}")]
    Talk { vm: String, source: proto::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Puts the VM to sleep, through its boot.
pub fn sleep(store: &Store, name: &str) -> Result<()> {
    ask(store, name, Ask::Sleep)
}

/// Wakes the VM, through its boot.
pub fn wake(store: &Store, name: &str) -> Result<()> {
    ask(store, name, Ask::Wake)
}

/// Parks the sleeping VM, through its boot, and returns once that boot has
/// let go of it. Refused if it's awake.
pub fn park(store: &Store, name: &str) -> Result<()> {
    ask(store, name, Ask::Park)?;
    store.wait_idle(name)?;
    Ok(())
}

/// Asks the VM's boot, and waits for it to be done.
pub fn ask(store: &Store, name: &str, ask: Ask) -> Result<()> {
    let sock = store.boot_socket(name)?;
    let talk = |source: proto::Error| Error::Talk {
        vm: name.to_string(),
        source,
    };
    let mut conn = UnixStream::connect(&sock).map_err(|e| match e.kind() {
        // No socket, or one left by a boot that's gone.
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            Error::NoBoot(name.to_string())
        }
        _ => talk(e.into()),
    })?;
    let asked = conn
        .set_read_timeout(Some(REPLY_WAIT))
        .and_then(|()| conn.set_write_timeout(Some(ASK_WAIT)))
        .map_err(proto::Error::from)
        .and_then(|()| proto::hello_with(&mut conn, MAGIC, VERSION).map_err(Into::into))
        .and_then(|()| proto::check_hello_with(&mut conn, MAGIC, VERSION))
        .and_then(|()| proto::write_frame(&mut conn, &ask))
        .and_then(|()| proto::read_frame::<Reply>(&mut conn));
    match asked.map_err(talk)? {
        Some(Reply::Done) => Ok(()),
        Some(Reply::Refused(why)) => Err(Error::Refused(why)),
        // The boot ended before it answered.
        None => Err(talk(proto::Error::Truncated)),
    }
}

/// A boot's side: takes asks on its socket, one at a time, and answers each
/// with `answer`, for as long as it lives. Dropping it stops it and removes
/// its socket, without waiting out an ask in flight.
pub struct Asks {
    sock: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Asks {
    /// Serves `sock`, which only this user can connect to.
    pub fn serve(sock: &Path, answer: impl Fn(Ask) -> Reply + Send + 'static) -> io::Result<Asks> {
        let listener = UnixListener::bind(sock)?;
        fs::set_permissions(sock, Permissions::from_mode(0o600))?;
        let asks = Asks {
            sock: sock.to_path_buf(),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let stopped = asks.stop.clone();
        let serve = move || {
            for conn in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                let served = conn
                    .map_err(proto::Error::from)
                    .and_then(|c| take(c, &answer));
                if let Err(e) = served {
                    note(format_args!("an ask of this boot: {e}"));
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        };
        std::thread::Builder::new().spawn(serve)?;
        Ok(asks)
    }
}

impl Drop for Asks {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the thread if it's waiting for an ask.
        let _ = UnixStream::connect(&self.sock);
        let _ = fs::remove_file(&self.sock);
    }
}

fn take(mut conn: UnixStream, answer: &impl Fn(Ask) -> Reply) -> proto::Result<()> {
    conn.set_read_timeout(Some(ASK_WAIT))?;
    conn.set_write_timeout(Some(ASK_WAIT))?;
    proto::hello_with(&mut conn, MAGIC, VERSION)?;
    proto::check_hello_with(&mut conn, MAGIC, VERSION)?;
    let ask = match proto::read_frame::<Ask>(&mut conn) {
        Ok(Some(ask)) => ask,
        Ok(None) => return Ok(()),
        // Framed right, but not an ask this boot has: a newer runner's.
        Err(proto::Error::Decode(_)) => {
            let why = "this VM's boot doesn't know that ask: it's older than the runner \
                       asking, which a stop and start brings up to date";
            return proto::write_frame(&mut conn, &Reply::Refused(why.into()));
        }
        Err(e) => return Err(e),
    };
    proto::write_frame(&mut conn, &answer(ask))
}

// As boot's notes: Firecracker shares this stderr and makes it non-blocking.
fn note(msg: std::fmt::Arguments) {
    let _ = writeln!(io::stderr(), "runner: {msg}");
}
