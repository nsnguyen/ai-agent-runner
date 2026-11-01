// The guest's calls to its host: the agent in a VM checkpoints, lists and
// restores its own VM with `runner-guest checkpoint`, `checkpoints` and
// `restore`, which `runner boot` serves for that VM alone.
//
// The guest connects to the host (CID 2) on vsock PORT, which Firecracker
// passes on to `vsock.sock_<PORT>` in the VM's directory, where boot listens.
// A connection makes one call, framed as src/proto.rs frames, with a hello of
// its own:
//
//   both sides   hello: MAGIC then VERSION, as a u32 big-endian, 8 bytes
//   guest        Call
//   host         Answer
//
// Nothing in a call names a VM, so a guest can only reach its own, and a
// call this host doesn't know is refused. Calls take turns: boot serves one
// connection at a time, and one that's quiet past CALL_WAIT is dropped.
//
// A restore is copied while the VM runs and answered, then boot asks the VM
// to shut down once the guest has hung up. At the clean shutdown, boot
// commits the disk the VM ran, puts the copy on top, and exits with
// boot::RESTART, which the unit boots again (src/systemd.rs).
use crate::checkpoint;
use crate::firecracker::Api;
use crate::proto;
use crate::runs;
use crate::store::{Checkpoint, Staged, Store};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The host's vsock port the guest makes its calls on.
pub const PORT: u32 = 10024;
/// Bump on any change to Call, Answer or store::Checkpoint.
pub const VERSION: u32 = 1;
/// Starts every hello, apart from proto::MAGIC, so a peer on the wrong port
/// fails at once.
pub const MAGIC: [u8; 4] = *b"rcal";
/// The longest comment a call's checkpoint takes, in bytes.
pub const MAX_COMMENT: usize = 1024;

// A guest that connects says what it wants at once.
const CALL_WAIT: Duration = Duration::from_secs(10);
// How long a restore waits for the guest to take its answer and hang up
// before the VM goes down anyway.
const HANG_UP_WAIT: Duration = Duration::from_secs(2);

/// What the guest asks for, of its own VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Call {
    /// Freeze a copy of the VM's disk, which carries on.
    Checkpoint { comment: Option<String> },
    /// The VM's checkpoints, newest first, as `runner checkpoints` lists them.
    Checkpoints,
    /// Reboot the VM onto a copy of checkpoint `id`.
    Restore { id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Made(Checkpoint),
    Listed(Vec<Checkpoint>),
    /// Checkpoint `id` is copied as `generation`, and the VM goes down once
    /// the guest hangs up, then boots again onto it.
    Restoring {
        id: u64,
        generation: u64,
    },
    /// Why not.
    Refused(String),
}

/// The guest's side of one call, on a connection to the host's PORT.
pub fn ask<C: Read + Write>(conn: &mut C, call: &Call) -> proto::Result<Answer> {
    proto::hello_with(conn, MAGIC, VERSION)?;
    proto::check_hello_with(conn, MAGIC, VERSION)?;
    proto::write_frame(conn, call)?;
    proto::read_frame(conn)?.ok_or(proto::Error::Truncated)
}

/// A restore the guest asked for, waiting on its VM to go down. It holds the
/// VM's run lock until dropped, so no run starts that the reboot would end.
#[derive(Debug)]
pub struct Restore {
    pub staged: Staged,
    _no_run: File,
}

// What the VM's boot hears from its server about a restore.
#[derive(Default)]
struct Asked {
    restore: Option<Restore>,
    // The VM is down: a restore staged now would be left to no one.
    closed: bool,
}

/// The host's side, which `runner boot` runs for its VM while the VM runs.
/// Dropping it stops it and removes its socket, without waiting out a call
/// in flight: one whose VM is gone fails on its own.
pub struct Calls {
    sock: PathBuf,
    stop: Arc<AtomicBool>,
    asked: Arc<Mutex<Asked>>,
}

impl Calls {
    /// Serves the guest's calls on `sock`, Firecracker's for PORT, for VM
    /// `name` alone. Only the runner user can connect; Firecracker connects
    /// for the guest.
    pub fn start(store: &Store, name: &str, sock: &Path) -> io::Result<Calls> {
        let listener = UnixListener::bind(sock)?;
        fs::set_permissions(sock, Permissions::from_mode(0o600))?;
        let calls = Calls {
            sock: sock.to_path_buf(),
            stop: Arc::new(AtomicBool::new(false)),
            asked: Arc::default(),
        };
        let host = Host {
            store: store.clone(),
            name: name.to_string(),
            asked: calls.asked.clone(),
        };
        let stopped = calls.stop.clone();
        let serve = move || {
            for conn in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                let served = conn.map_err(proto::Error::from).and_then(|c| host.serve(c));
                if let Err(e) = served {
                    note(format_args!("{}'s guest call: {e}", host.name));
                }
            }
        };
        std::thread::Builder::new().spawn(serve)?;
        Ok(calls)
    }

    /// The restore the guest asked for, if it did, for boot to finish once
    /// the VM is down. A restore asked for after this is refused.
    pub fn close(&self) -> Option<Restore> {
        let mut asked = self.asked.lock().unwrap();
        asked.closed = true;
        asked.restore.take()
    }
}

impl Drop for Calls {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the thread if it's waiting for a call; one in a call sees
        // the flag after.
        let _ = UnixStream::connect(&self.sock);
        let _ = fs::remove_file(&self.sock);
    }
}

struct Host {
    store: Store,
    name: String,
    asked: Arc<Mutex<Asked>>,
}

impl Host {
    fn serve(&self, mut conn: UnixStream) -> proto::Result<()> {
        conn.set_read_timeout(Some(CALL_WAIT))?;
        conn.set_write_timeout(Some(CALL_WAIT))?;
        proto::hello_with(&mut conn, MAGIC, VERSION)?;
        proto::check_hello_with(&mut conn, MAGIC, VERSION)?;
        let call = match proto::read_frame::<Call>(&mut conn) {
            Ok(Some(call)) => call,
            Ok(None) => return Ok(()),
            // Framed right, but not a call this host has: a newer guest's.
            Err(proto::Error::Decode(_)) => {
                let why = "this runner host doesn't know that call: runner-guest is newer than \
                           the runner, so ask for the runner to be updated";
                return proto::write_frame(&mut conn, &Answer::Refused(why.into()));
            }
            Err(e) => return Err(e),
        };
        let answer = match call {
            Call::Checkpoint { comment } => self.checkpoint(comment.as_deref()),
            Call::Checkpoints => match self.store.checkpoints(&self.name) {
                Ok(listed) => Answer::Listed(listed),
                Err(e) => Answer::Refused(e.to_string()),
            },
            Call::Restore { id } => self.stage(id),
        };
        let restoring = matches!(answer, Answer::Restoring { .. });
        let sent = proto::write_frame(&mut conn, &answer);
        if restoring {
            // Down even if the guest didn't hear: what it asked for is staged.
            self.reboot(&mut conn);
        }
        sent
    }

    fn checkpoint(&self, comment: Option<&str>) -> Answer {
        if comment.is_some_and(|c| c.len() > MAX_COMMENT) {
            return Answer::Refused(format!(
                "a checkpoint's comment can be up to {MAX_COMMENT} bytes"
            ));
        }
        match checkpoint::for_agent(&self.store, &self.name, comment) {
            Ok(made) => {
                note(format_args!(
                    "{}'s guest made checkpoint {}",
                    self.name, made.id
                ));
                Answer::Made(made)
            }
            Err(e) => Answer::Refused(e.to_string()),
        }
    }

    // Copies the checkpoint while the VM runs, so a wrong id reboots nothing,
    // and leaves it for boot to finish.
    fn stage(&self, id: u64) -> Answer {
        let down = || Answer::Refused(format!("{} is shutting down", self.name));
        {
            let asked = self.asked.lock().unwrap();
            if asked.closed {
                return down();
            }
            if let Some(r) = &asked.restore {
                let from = r.staged.from;
                return Answer::Refused(format!(
                    "{} is already rebooting to restore checkpoint {from}",
                    self.name
                ));
            }
        }
        let no_run = match runs::hold(&self.store, &self.name) {
            Ok(lock) => lock,
            Err(e) => {
                return Answer::Refused(format!(
                    "{e}. Restoring would end that run, so it's refused until the run is over"
                ));
            }
        };
        let staged = self
            .store
            .hold_checkpoints(&self.name)
            .and_then(|freezer| freezer.stage_restore(id));
        let staged = match staged {
            Ok(staged) => staged,
            Err(e) => return Answer::Refused(e.to_string()),
        };
        let generation = staged.generation;
        let mut asked = self.asked.lock().unwrap();
        if asked.closed {
            drop(asked);
            self.drop_staged(staged);
            return down();
        }
        asked.restore = Some(Restore {
            staged,
            _no_run: no_run,
        });
        note(format_args!(
            "{}'s guest asked to restore checkpoint {id}, copied as generation {generation}",
            self.name
        ));
        Answer::Restoring { id, generation }
    }

    // Waits for the guest to hang up, so its answer is out, then asks the VM
    // to shut down, as `runner stop` does. If it can't, the restore is
    // dropped: the VM carries on as it was.
    fn reboot(&self, conn: &mut UnixStream) {
        let _ = conn.set_read_timeout(Some(HANG_UP_WAIT));
        let _ = io::copy(conn, &mut io::sink());
        let asked = self
            .store
            .api_socket(&self.name)
            .map_err(|e| e.to_string())
            .and_then(|sock| Api::new(sock).ctrl_alt_del().map_err(|e| e.to_string()));
        let Err(e) = asked else {
            note(format_args!(
                "sent Ctrl-Alt-Del to {} for its restore",
                self.name
            ));
            return;
        };
        note(format_args!(
            "couldn't shut {} down for its restore, so it's dropped: {e}",
            self.name
        ));
        let restore = self.asked.lock().unwrap().restore.take();
        if let Some(restore) = restore {
            self.drop_staged(restore.staged);
        }
    }

    fn drop_staged(&self, staged: Staged) {
        let generation = staged.generation;
        if let Err(e) = self.store.drop_restore(&self.name, staged) {
            note(format_args!(
                "couldn't remove {}'s generation {generation}: {e}",
                self.name
            ));
        }
    }
}

// As boot's notes: Firecracker shares this stderr and makes it non-blocking.
fn note(msg: std::fmt::Arguments) {
    let _ = writeln!(io::stderr(), "runner: {msg}");
}
