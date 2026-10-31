// `runner checkpoint` and `restore`, on running VMs too, as the CLI, MCP and
// runs that join a running VM use them. A stopped VM's are the store's own.
//
// A running VM writes as it goes to the disk generation its boot runs
// (Store::booted), which only its shutdown commits. Its checkpoint runs `sync`
// in the guest, so what it wrote is on that disk, pauses the VM through
// Firecracker's API, copies the disk as every checkpoint does, and resumes the
// VM. On XFS or btrfs the copy is a reflink and the pause a few milliseconds;
// elsewhere the VM stays paused for the whole copy.
//
// Restoring a running VM stops it, which commits its disk as any clean
// shutdown does, so `undo` goes back to it, then restores and starts it again.
use crate::exec;
use crate::firecracker::{self, Api};
use crate::proto::Request;
use crate::runs;
use crate::store::{self, Checkpoint, Store, Vm};
use crate::systemd::{self, State};
use std::time::Duration;

// A guest flushing a lot of writes takes a while; one stuck longer than this
// fails the checkpoint rather than hang it.
const SYNC_WAIT: Duration = Duration::from_secs(120);
// What of sync's output an error shows.
const SYNC_OUTPUT: usize = 4 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Exec(#[from] exec::Error),
    #[error(transparent)]
    Firecracker(#[from] firecracker::Error),
    #[error(transparent)]
    Runs(#[from] runs::Error),
    #[error("{vm} is {state}: try again once it's running or stopped")]
    NotNow { vm: String, state: State },
    #[error("`sync` in {vm} {how}, so a checkpoint could miss what it last wrote: {stderr}")]
    Sync {
        vm: String,
        how: String,
        stderr: String,
    },
    #[error("{vm} is still paused after its checkpoint: {source}. `runner stop {vm}` stops it")]
    StillPaused {
        vm: String,
        source: firecracker::Error,
    },
    #[error("{vm} is at generation {generation}, restored, but didn't start again: {source}")]
    NotStarted {
        vm: String,
        generation: u64,
        source: systemd::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Freezes a copy of the VM's disk, with an optional comment, for `restore`
/// to bring back: its current disk if it's stopped, or the one it runs if it's
/// running, which carries on.
pub fn checkpoint(store: &Store, name: &str, comment: Option<&str>) -> Result<Checkpoint> {
    // A stopped VM needs no systemd, so this works on any machine.
    match store.checkpoint(name, comment) {
        Err(store::Error::Busy(_)) => {}
        done => return Ok(done?),
    }
    match systemd::status(store, name)?.state {
        State::Running => running(store, name, comment),
        // Held, but not by a boot: another runner command, gone in a moment.
        State::Stopped => Ok(store.checkpoint(name, comment)?),
        state => Err(not_now(name, state)),
    }
}

fn running(store: &Store, name: &str, comment: Option<&str>) -> Result<Checkpoint> {
    let freezer = store.hold_checkpoints(name)?;
    sync(store, name)?;
    let api = Api::new(store.api_socket(name)?);
    api.pause()?;
    let frozen = freezer.freeze(comment);
    if let Err(source) = api.resume() {
        let vm = name.to_string();
        return Err(Error::StillPaused { vm, source });
    }
    Ok(frozen?)
}

// What the guest wrote, onto the disk Firecracker gives it. A guest without
// runner-guest has no way to, so its checkpoint fails here.
fn sync(store: &Store, name: &str) -> Result<()> {
    let conn = exec::reach(store, name)?;
    let req = Request {
        argv: vec!["sync".to_string()],
        env: Vec::new(),
        cwd: None,
        // The guest's own user, root, sees every filesystem.
        user: None,
        tty: None,
    };
    let done = exec::capture(conn, &req, b"", SYNC_WAIT, SYNC_OUTPUT)?;
    if done.exit_code == Some(0) {
        return Ok(());
    }
    let how = match (done.timed_out, done.exit_code, done.signal) {
        (true, ..) => format!("took over {}s", SYNC_WAIT.as_secs()),
        (_, Some(code), _) => format!("exited {code}"),
        (_, _, Some(sig)) => format!("was killed by signal {sig}"),
        _ => "never said how it ended".to_string(),
    };
    Err(Error::Sync {
        vm: name.to_string(),
        how,
        stderr: done.stderr.trim().to_string(),
    })
}

/// Makes a copy of checkpoint `id` the VM's current disk. A running VM is
/// stopped first and started again after. Refused while a run is going, which
/// stopping the VM would end.
pub fn restore(store: &Store, name: &str, id: u64) -> Result<Vm> {
    match store.restore(name, id) {
        Err(store::Error::Busy(_)) => {}
        done => return Ok(done?),
    }
    let _no_run = runs::hold(store, name)?;
    restore_held(store, name, id)
}

/// `restore`, for a caller holding the VM's run lock: a run rolling back.
pub fn restore_held(store: &Store, name: &str, id: u64) -> Result<Vm> {
    // Before anything stops: a mistyped id leaves the VM as it was.
    if !store.checkpoints(name)?.iter().any(|c| c.id == id) {
        return Err(store::Error::NoCheckpoint(name.to_string(), id).into());
    }
    match systemd::status(store, name)?.state {
        State::Stopped => Ok(store.restore(name, id)?),
        State::Running => {
            systemd::stop(store, name)?;
            // Started again either way: it was running when asked.
            let restored = store.restore(name, id);
            let started = systemd::start(store, name);
            let vm = restored?;
            started.map_err(|source| Error::NotStarted {
                vm: name.to_string(),
                generation: vm.generation,
                source,
            })?;
            Ok(vm)
        }
        state => Err(not_now(name, state)),
    }
}

fn not_now(name: &str, state: State) -> Error {
    Error::NotNow {
        vm: name.to_string(),
        state,
    }
}
