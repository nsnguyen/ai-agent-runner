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
// Restoring a running VM copies the checkpoint first, then stops the VM, which
// commits its disk as any clean shutdown does, so `undo` goes back to it, puts
// the copy on top of history, and starts the VM again. A VM already being
// stopped is restored the same way once it's down, and left stopped.
//
// The agent in a VM checkpoints it too, through its boot (src/calls.rs), as
// often as AGENT_PER_HOUR allows.
use crate::exec;
use crate::firecracker::{self, Api};
use crate::idle::InUse;
use crate::proto::Request;
use crate::runs;
use crate::store::{self, Checkpoint, Freezer, Store, Vm};
use crate::systemd::{self, State};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use std::time::Duration;

/// How many checkpoints the agent in a VM may make of it in any hour. gc
/// keeps every checkpoint, so this bounds how fast the agent's add up, not
/// how much space they take in all.
pub const AGENT_PER_HOUR: usize = 10;

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
    #[error(
        "the agent's checkpoints of {vm} are capped at {AGENT_PER_HOUR} an hour: \
         the next can be made at {}", next.to_rfc3339_opts(SecondsFormat::Secs, true)
    )]
    Capped { vm: String, next: DateTime<Utc> },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Freezes a copy of the VM's disk, with an optional comment, for `restore`
/// to bring back: its current disk if it's stopped, or the one it runs if it's
/// running, which carries on.
pub fn checkpoint(store: &Store, name: &str, comment: Option<&str>) -> Result<Checkpoint> {
    // Before the look, so an idle sleep can't start between it and the sync.
    let _in_use = InUse::take(store, name)?;
    // A stopped VM needs no systemd, so this works on any machine.
    match store.checkpoint(name, comment) {
        Err(store::Error::Busy(_)) => {}
        done => return Ok(done?),
    }
    match systemd::status(store, name)?.state {
        State::Running => running(store, name, comment),
        // Its guest syncs its disk first, which it can only do awake.
        State::Sleeping => {
            systemd::wake(store, name)?;
            running(store, name, comment)
        }
        // Held, but not by a boot: another runner command, gone in a moment.
        State::Stopped => Ok(store.checkpoint(name, comment)?),
        state => Err(not_now(name, state)),
    }
}

/// The agent's own checkpoint of the running VM it's in, which its boot
/// makes for it (src/calls.rs): as a running VM's checkpoint is made, noted
/// as the agent's, and refused past AGENT_PER_HOUR in the last hour.
pub fn for_agent(store: &Store, name: &str, comment: Option<&str>) -> Result<Checkpoint> {
    let freezer = store.hold_checkpoints(name)?;
    let hour = TimeDelta::hours(1);
    let made = freezer.agent_since(Utc::now() - hour)?;
    if made.len() >= AGENT_PER_HOUR {
        // The one that drops out of the hour first makes room.
        let next = made[made.len() - AGENT_PER_HOUR] + hour;
        return Err(Error::Capped {
            vm: name.to_string(),
            next,
        });
    }
    paused(store, name, &freezer, |f| f.freeze_for_agent(comment))
}

fn running(store: &Store, name: &str, comment: Option<&str>) -> Result<Checkpoint> {
    let freezer = store.hold_checkpoints(name)?;
    paused(store, name, &freezer, |f| f.freeze(comment))
}

// Syncs the guest, then freezes its disk with the VM paused. From the pause
// to the resume, every way out resumes the VM. `runner halt` waits on the
// freezer, then resumes the VM too, for one whose checkpoint was killed with
// it paused.
fn paused(
    store: &Store,
    name: &str,
    freezer: &Freezer,
    freeze: impl FnOnce(&Freezer) -> store::Result<Checkpoint>,
) -> Result<Checkpoint> {
    sync(store, name)?;
    let api = Api::new(store.api_socket(name)?);
    let _held = HoldSignals::new();
    let frozen = match api.pause() {
        // A refusal can come after the pause took, as a timeout can.
        Err(e) => Err(Error::from(e)),
        Ok(()) => freeze(freezer).map_err(Error::from),
    };
    match api.resume() {
        Err(source) if frozen.is_ok() => Err(Error::StillPaused {
            vm: name.to_string(),
            source,
        }),
        _ => frozen,
    }
}

// Ignores Ctrl-C, a dropped ssh connection and a polite kill until dropped,
// so none leaves the VM paused. On ext4 that's the whole copy.
struct HoldSignals(Vec<(Signal, SigAction)>);

impl HoldSignals {
    const SIGNALS: [Signal; 3] = [Signal::SIGINT, Signal::SIGHUP, Signal::SIGTERM];

    fn new() -> HoldSignals {
        let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        let mut was = Vec::new();
        for sig in Self::SIGNALS {
            // SAFETY: SIG_IGN runs no code of ours in a signal handler.
            if let Ok(old) = unsafe { sigaction(sig, &ignore) } {
                was.push((sig, old));
            }
        }
        HoldSignals(was)
    }
}

impl Drop for HoldSignals {
    fn drop(&mut self) {
        for (sig, old) in &self.0 {
            // SAFETY: puts back what was there before.
            let _ = unsafe { sigaction(*sig, old) };
        }
    }
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
/// stopped first and started again after; a stopping one is waited out and
/// left stopped. A sleeping one is restored as a running one is: its
/// snapshot runs on the disk the restore replaces, so the stop wakes it to
/// shut down, and it starts again awake. Refused while a run is going, which
/// stopping the VM would end.
pub fn restore(store: &Store, name: &str, id: u64) -> Result<Vm> {
    // An idle sleep holds the run lock while it lasts: wait it out.
    let _in_use = InUse::take(store, name)?;
    match store.restore(name, id) {
        Err(store::Error::Busy(_)) => {}
        done => return Ok(done?),
    }
    let _no_run = runs::hold(store, name)?;
    restore_held(store, name, id)
}

// `restore`, once it holds the VM's run lock.
fn restore_held(store: &Store, name: &str, id: u64) -> Result<Vm> {
    let state = systemd::status(store, name)?.state;
    match state {
        State::Stopped => return Ok(store.restore(name, id)?),
        State::Running | State::Stopping | State::Sleeping => {}
        state => return Err(not_now(name, state)),
    }
    // The freezer goes before the stop, whose halt waits on it.
    let staged = store.hold_checkpoints(name)?.stage_restore(id)?;
    // On a VM being stopped, `systemctl stop` waits for that stop instead.
    if let Err(e) = systemd::stop(store, name) {
        drop_staged(store, name, staged);
        return Err(e.into());
    }
    // A restore that fails here found it started by someone else since.
    let restored = store.finish_restore(name, &staged);
    if restored.is_err() {
        drop_staged(store, name, staged);
    }
    // One that was stopping was asked to stop, so it stays down.
    if state == State::Stopping {
        return Ok(restored?);
    }
    // Started again either way: it was running when asked.
    let started = systemd::start(store, name);
    let vm = restored?;
    started.map_err(|source| Error::NotStarted {
        vm: name.to_string(),
        generation: vm.generation,
        source,
    })?;
    Ok(vm)
}

// A copy left would be listed as a checkpoint: harmless, so only noted.
fn drop_staged(store: &Store, name: &str, staged: store::Staged) {
    let generation = staged.generation;
    if let Err(e) = store.drop_restore(name, staged) {
        eprintln!("runner: couldn't remove {name}'s generation {generation}: {e}");
    }
}

fn not_now(name: &str, state: State) -> Error {
    Error::NotNow {
        vm: name.to_string(),
        state,
    }
}
