// Best of N: `runner run dev --n 3` forks dev into dev-1, dev-2 and dev-3 and
// starts the prompt in each as an ordinary headless run (runs.rs), all at
// once. Each fork is new and stopped, so its run boots it and commits only a
// clean exit, and its result measures what that try changed against the disk
// it forked, which is dev's. dev itself isn't touched, and has no run.
//
// A stopped dev's tries fork its current disk. A running or sleeping one is
// checkpointed first, as a run that joins it is (checkpoint::for_run), and
// the tries fork that checkpoint, so they get what dev has now, not its last
// commit. They get the default credentials, as every fork does, so N agents
// can't all push with dev's, and dev's config otherwise.
//
// Making them holds dev's run lock, so it's refused while dev has a run
// going, and two batches can't race. Each check comes before anything is
// made: N is 2 to 4, the names are free, the host has room for N more of dev,
// and no batch of dev's is on hand. A batch is on hand while any of its tries
// is still a VM. dev records the batch in its directory, in tries.json: what
// was asked, when, the generation the tries forked, and their names and runs,
// for `runner tries` and `keep` to read.
//
// `runner tries dev` reads that record, and each try's run and result as
// runs.rs keeps them, into one table to pick from; a try deleted by hand is
// gone, and the rest still show.
//
// `runner keep dev-2` puts dev-2's current disk on top of dev's history, as
// a restore puts a checkpoint's copy (checkpoint::put_disk), so `undo` goes
// back to dev from before, and deletes every try of the batch; `keep dev
// --none` only deletes them. dev keeps its name and settings. Everything is
// checked before anything changes: a try still at the generation it forked,
// whose run rolled back or never got going, still has dev's disk, so it
// can't be the one kept, nor can one whose run is going; and no try can be
// up, but for one a run booted, which a cancel shuts down. The one kept is
// held by its lock from then on. Tries whose runs are going are cancelled
// first, then the disk goes on dev, which is stopped for it if it's up and
// started again after, then the tries go, and with them dev's record of the
// batch. It holds dev's run lock throughout, as a batch's start does, so
// it's refused while dev has a run going.
use crate::checkpoint;
use crate::host;
use crate::idle::InUse;
use crate::result::{Changed, Outcome, RunResult};
use crate::runs::{self, AgentRun, Start};
use crate::store::{self, Config, DEFAULT_MEM_MIB, Source, Store};
use crate::systemd::{self, State};
use chrono::{DateTime, SubsecRound, Utc};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// The fewest tries a batch has.
pub const MIN: u32 = 2;
/// The most tries a batch has.
pub const MAX: u32 = 4;
/// What the host keeps free past what a batch's tries take, in MiB.
pub const SPARE_MIB: u64 = 1024;
/// How many characters of what an agent says a row of `table` shows.
pub const SAYS_WIDTH: usize = 60;
// How often `wait` looks again: each look reads every try's result.
const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Runs(#[from] runs::Error),
    #[error(transparent)]
    Checkpoint(#[from] checkpoint::Error),
    #[error("--n {0} is out of range: a batch is {MIN} to {MAX} tries")]
    BadN(u32),
    #[error(
        "a VM named {name} already exists, and {vm}'s {n} tries take the names {vm}-1 to {vm}-{n}: \
         delete {name} first"
    )]
    Taken { vm: String, n: u32, name: String },
    #[error(
        "{vm} has tries on hand from best of {n} at {when}: {left}. One batch at a time: \
         `runner keep {vm} --none` drops them, or `runner keep` the best"
    )]
    OnHand {
        vm: String,
        n: usize,
        when: String,
        left: String,
    },
    #[error(
        "{n} tries of {vm} take {need} MiB, {n} times its {each}, and this host has {free} MiB free, \
         less {SPARE_MIB} to spare: {fits}"
    )]
    NoRoom {
        vm: String,
        n: u32,
        each: u64,
        need: u64,
        free: u64,
        fits: String,
    },
    #[error("reading this host's free memory: {0}")]
    Mem(io::Error),
    #[error("{name}'s run didn't start: {source}. {started}")]
    NotStarted {
        name: String,
        started: String,
        source: Box<runs::Error>,
    },
    #[error(
        "{0} isn't a try: keep takes one of a best of N's tries, like dev-2, or with --none the \
         VM they forked"
    )]
    NotATry(String),
    #[error("{0} has no tries on hand: `runner tries {0}` shows its latest batch")]
    NoTries(String),
    #[error("{run} is still going: wait for it (`runner tries {vm} --wait`), or keep another")]
    StillGoing { vm: String, run: String },
    #[error("{how}, so {name} still has {vm}'s disk: keep a try whose run committed")]
    Unchanged {
        vm: String,
        name: String,
        how: String,
    },
    #[error("{name} never ran, so it still has {vm}'s disk: keep a try whose run committed")]
    NeverRan { vm: String, name: String },
    #[error("{name} is {state}: stop it first (`runner stop {name}`), then keep")]
    Up { name: String, state: State },
    #[error("{run} was still going two minutes after its cancel: keep again once it has ended")]
    NotCancelled { run: String },
    #[error("{said}but {vm} didn't start again: {source}. `runner start {vm}` starts it")]
    KeptNotStarted {
        said: String,
        vm: String,
        source: systemd::Error,
    },
    #[error("{}", said.trim_end())]
    NotDeleted { said: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// A VM's batch of tries, as it records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    /// What each try was asked.
    pub prompt: String,
    /// Where in each try Claude Code runs.
    pub dir: String,
    /// The Claude Code session each try carries on.
    pub resume: Option<String>,
    pub started: DateTime<Utc>,
    /// The VM's generation that every try forked.
    pub generation: u64,
    /// That generation, if it's the checkpoint taken of the VM running or
    /// sleeping; None if it's the VM's current disk.
    pub checkpoint: Option<u64>,
    pub tries: Vec<Try>,
}

/// One try of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Try {
    /// The fork: the VM's name, a dash, and the try's number.
    pub vm: String,
    /// Its run's id, once that has started.
    pub run: Option<String>,
}

/// The tries' names: the VM's, a dash, and 1 to `n`.
pub fn names(vm: &str, n: u32) -> Vec<String> {
    (1..=n).map(|i| format!("{vm}-{i}")).collect()
}

/// Forks `s.vm` `n` times and starts `s` in each fork, returning each run as
/// it starts. Refused, with nothing made, unless every try can be made.
pub fn start(store: &Store, exe: &Path, s: &Start, n: u32) -> Result<Vec<AgentRun>> {
    if !(MIN..=MAX).contains(&n) {
        return Err(Error::BadN(n));
    }
    if s.prompt.trim().is_empty() {
        return Err(runs::Error::NoPrompt.into());
    }
    let vm = s.vm;
    let each = u64::from(store.config(vm)?.mem_mib.unwrap_or(DEFAULT_MEM_MIB));
    // As a run does: an idle sleep holds the run lock while it lasts.
    let _in_use = InUse::take(store, vm)?;
    let _lock = runs::hold(store, vm)?;
    if let Some(on_hand) = on_hand(store, vm)? {
        return Err(on_hand);
    }
    let names = names(vm, n);
    for name in &names {
        store.check_free(name).map_err(|e| match e {
            store::Error::Exists(name) => Error::Taken {
                vm: vm.to_string(),
                n,
                name,
            },
            e => e.into(),
        })?;
    }
    room(vm, n, each)?;
    // A sleeping VM is woken for its checkpoint, and one booting or stopping
    // is refused.
    let checkpoint = match systemd::state(store, vm)? {
        State::Stopped => None,
        _ => {
            let comment = format!("before best of {n}");
            Some(checkpoint::for_run(store, vm, Some(&comment))?.id)
        }
    };
    let generation = match checkpoint {
        Some(id) => id,
        None => store.get(vm)?.generation,
    };
    fork_all(store, vm, generation, &names)?;
    let mut batch = Batch {
        prompt: s.prompt.to_string(),
        dir: s.dir.to_string(),
        resume: s.resume.map(str::to_string),
        started: Utc::now().trunc_subsecs(0),
        generation,
        checkpoint,
        tries: (names.iter())
            .map(|name| Try {
                vm: name.clone(),
                run: None,
            })
            .collect(),
    };
    if let Err(e) = record(store, vm, &batch) {
        drop_forks(store, &names);
        return Err(e);
    }
    let mut started = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let one = Start { vm: name, ..*s };
        match runs::start(store, exe, &one) {
            Ok(run) => {
                batch.tries[i].run = Some(run.id.clone());
                started.push(run);
            }
            // The tries whose runs started go on; the rest go.
            Err(source) => {
                drop_forks(store, &names[i..]);
                batch.tries.truncate(i);
                record(store, vm, &batch)?;
                let ids: Vec<&str> = started.iter().map(|r| r.id.as_str()).collect();
                let started = match ids.is_empty() {
                    true => "Every try was deleted".to_string(),
                    false => format!("{} started, and the tries after were deleted", and(&ids)),
                };
                return Err(Error::NotStarted {
                    name: name.clone(),
                    started,
                    source: Box::new(source),
                });
            }
        }
    }
    record(store, vm, &batch)?;
    Ok(started)
}

/// The VM's latest batch, as it recorded it; None if it never had one.
pub fn tries(store: &Store, vm: &str) -> Result<Option<Batch>> {
    let path = store.tries(vm)?;
    let bytes = match fs::read(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(store::at(&path))?,
    };
    let bad =
        |e: serde_json::Error| store::at(&path)(io::Error::new(io::ErrorKind::InvalidData, e));
    Ok(Some(serde_json::from_slice(&bytes).map_err(bad)?))
}

/// A VM's latest batch as it stands, for `runner tries` and list_tries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Standing {
    /// The VM the tries forked.
    pub vm: String,
    /// What each try was asked. Left out, with the rest of the batch's
    /// fields, if the VM never had a batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// When the batch started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    /// The VM's generation that every try forked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// That generation, if it's the checkpoint taken of the VM running or
    /// sleeping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<u64>,
    /// The tries in order, none if the VM never had a batch.
    pub tries: Vec<Tried>,
}

/// One try as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Tried {
    /// The fork: the VM's name, a dash, and the try's number.
    pub vm: String,
    /// Its run's id, like dev-1.1; null if its run never started.
    pub id: Option<String>,
    /// The fork, or its run, isn't on this host any more: deleted, or moved
    /// to another host, since. Then there's no run or result to show.
    pub gone: bool,
    /// Its run, as list_runs gives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<AgentRun>,
    /// Once its run is over, what the runner measured, as `runner result`
    /// prints it, with agent_says kept apart: untrusted. Left out while the
    /// run goes on, and for one lost or that never got as far as running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RunResult>,
}

/// The VM's latest batch as it stands: each try's run, and once that's over,
/// its result. A try whose fork or run has been deleted is gone.
pub fn standing(store: &Store, vm: &str) -> Result<Standing> {
    let Some(batch) = tries(store, vm)? else {
        return Ok(Standing {
            vm: vm.to_string(),
            prompt: None,
            started: None,
            generation: None,
            checkpoint: None,
            tries: Vec::new(),
        });
    };
    let tries = (batch.tries.iter())
        .map(|t| tried(store, t))
        .collect::<Result<_>>()?;
    Ok(Standing {
        vm: vm.to_string(),
        prompt: Some(batch.prompt),
        started: Some(batch.started),
        generation: Some(batch.generation),
        checkpoint: batch.checkpoint,
        tries,
    })
}

/// As `standing`, once no try's run is going.
pub fn wait(store: &Store, vm: &str) -> Result<Standing> {
    loop {
        let now = standing(store, vm)?;
        let going = (now.tries.iter())
            .filter_map(|t| t.run.as_ref())
            .any(|r| r.state == runs::State::Running);
        if !going {
            return Ok(now);
        }
        std::thread::sleep(POLL);
    }
}

/// What `keep` did, for `runner keep` and keep_try.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Kept {
    /// The VM the tries forked.
    pub vm: String,
    /// The try whose disk the VM has now. Left out, with run and session,
    /// when none was kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
    /// The kept try's newest run whose work is on the disk kept: one that
    /// committed, joined it running, or was taken over. The one best of N
    /// started, unless another did one of those in it since. Left out if
    /// none is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// That run's Claude Code session, which run_agent's resume takes to
    /// carry its conversation on in the VM. Left out if it never named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The VM's current generation: the kept try's disk, on top of its
    /// history, or as it was.
    pub generation: u64,
    /// The runs of tries still going, which were cancelled first.
    pub cancelled: Vec<String>,
    /// The tries deleted, the kept one too.
    pub deleted: Vec<String>,
}

impl Kept {
    /// What `runner keep` prints: a line on the VM's disk, then the runs
    /// cancelled, the tries deleted, and the session to carry on.
    pub fn said(&self) -> String {
        let vm = &self.vm;
        let g = self.generation;
        let mut out = match &self.kept {
            Some(name) => format!(
                "kept {name} on {vm}: generation {g} is current, and `runner undo {vm}` goes back\n"
            ),
            None => format!("{vm} is as it was: generation {g} is current\n"),
        };
        let names = |v: &[String]| and(&v.iter().map(String::as_str).collect::<Vec<_>>());
        if !self.cancelled.is_empty() {
            let _ = writeln!(out, "cancelled {}", names(&self.cancelled));
        }
        if !self.deleted.is_empty() {
            let _ = writeln!(out, "deleted {}", names(&self.deleted));
        }
        let _ = match (&self.run, &self.session) {
            (Some(run), Some(s)) => writeln!(
                out,
                "session {s}: `runner run {vm} --resume {s} <prompt>` carries on {run}'s conversation"
            ),
            (Some(run), None) => writeln!(out, "{run} named no Claude Code session to carry on"),
            _ => Ok(()),
        };
        out
    }
}

/// Keeps the try `name`: its current disk goes on top of the history of the
/// VM it forked, and every try of that batch is deleted. With `none`, `name`
/// is that VM, whose batch's tries are deleted, and it's left as it was.
/// Refused, with nothing changed, unless every try can go and the one kept
/// has a disk of its own.
pub fn keep(store: &Store, name: &str, none: bool) -> Result<Kept> {
    let (vm, winner) = match none {
        true => (name.to_string(), None),
        false => (forked(store, name)?, Some(name)),
    };
    // As a run does: an idle sleep holds the run lock while it lasts.
    let _in_use = InUse::take(store, &vm)?;
    let _lock = runs::hold(store, &vm)?;
    let names: Vec<String> = (tries(store, &vm)?.into_iter())
        .flat_map(|b| b.tries)
        .map(|t| t.vm)
        .filter(|t| store.get(t).is_ok())
        .collect();
    if names.is_empty() {
        return Err(Error::NoTries(vm));
    }
    // Each try's newest run, and the runs to cancel.
    let mut cancel = Vec::new();
    let mut kept_run = None;
    for t in &names {
        let all = runs::list(store, t)?;
        let run = all.first();
        let going = run.filter(|r| r.state == runs::State::Running);
        if Some(t.as_str()) == winner {
            winnable(store, &vm, t, run)?;
            kept_run = all.into_iter().find(on_disk);
        // A run that booted its try shuts it down once cancelled; one that
        // joined it running leaves it up, as any other try that's up.
        } else if let Some(run) = going.filter(|r| r.boots) {
            cancel.push(run.id.clone());
        } else {
            stopped(store, t)?;
        }
    }
    let held = match winner {
        Some(w) => {
            // put_disk refuses it too, but only once runs are cancelled.
            let state = systemd::state(store, &vm)?;
            if state == State::Booting {
                return Err(checkpoint::Error::NotNow { vm, state }.into());
            }
            // Busy if it was started since: nothing boots it from here on.
            Some((w, store.lock(w)?))
        }
        None => None,
    };
    let mut kept = Kept {
        vm: vm.clone(),
        kept: winner.map(str::to_string),
        run: kept_run.as_ref().map(|r| r.id.clone()),
        session: kept_run.and_then(|r| r.session),
        generation: 0,
        cancelled: Vec::new(),
        deleted: Vec::new(),
    };
    // All at once, as they started: each waits out Claude Code's grace and
    // its VM's shutdown.
    let cancels: Vec<_> = std::thread::scope(|s| {
        let asked: Vec<_> = (cancel.iter())
            .map(|id| s.spawn(move || runs::cancel(store, id)))
            .collect();
        let joined = asked.into_iter().map(|h| h.join());
        joined
            .map(|r| r.unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    for (id, done) in cancel.into_iter().zip(cancels) {
        match done {
            Ok(run) if run.state == runs::State::Running => {
                return Err(Error::NotCancelled { run: id });
            }
            Ok(_) => kept.cancelled.push(id),
            // It ended on its own meanwhile.
            Err(runs::Error::Ended { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // dev is back up first, if it was up: the kept disk is copied already,
    // and the kept try is held until it goes.
    let restarted = match held {
        None => {
            kept.generation = store.get(&vm)?.generation;
            Ok(())
        }
        Some((w, ref held)) => {
            let from = held.disk()?;
            let note = format!("keep of {w}");
            let stage = || store.stage_disk(&vm, Source::File(&from), &note);
            match checkpoint::put_disk(store, &vm, stage) {
                Ok(on) => {
                    kept.generation = on.generation;
                    Ok(())
                }
                Err(checkpoint::Error::NotStarted {
                    generation, source, ..
                }) => {
                    kept.generation = generation;
                    Err(source)
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    let mut left = Vec::new();
    for t in names.iter().filter(|t| Some(t.as_str()) != winner) {
        match store.delete(t) {
            Ok(_) => kept.deleted.push(t.clone()),
            Err(e) => left.push(format!("{t} ({e})")),
        }
    }
    if let Some((w, held)) = held {
        match held.delete() {
            Ok(_) => kept.deleted.push(w.to_string()),
            Err(e) => left.push(format!("{w} ({e})")),
        }
    }
    kept.deleted
        .sort_by_key(|t| names.iter().position(|n| n == t));
    let mut said = kept.said();
    if left.is_empty() {
        // Done with: a VM made later under a try's name isn't one.
        drop_record(store, &vm)?;
    } else {
        let left: Vec<&str> = left.iter().map(String::as_str).collect();
        let _ = writeln!(
            said,
            "couldn't delete {}: `runner delete` each by hand",
            and(&left)
        );
    }
    match restarted {
        Err(source) => Err(Error::KeptNotStarted { said, vm, source }),
        Ok(()) if !left.is_empty() => Err(Error::NotDeleted { said }),
        Ok(()) => Ok(kept),
    }
}

// The VM whose latest batch has the try `name`: the name less its dash and
// number, as `names` makes them.
fn forked(store: &Store, name: &str) -> Result<String> {
    let not = || Error::NotATry(name.to_string());
    let (vm, _) = name.rsplit_once('-').ok_or_else(not)?;
    let batch = match tries(store, vm) {
        Err(Error::Store(store::Error::NoVm(_) | store::Error::BadName(_))) => return Err(not()),
        r => r?,
    };
    let has = batch.is_some_and(|b| b.tries.iter().any(|t| t.vm == name));
    match has {
        true => Ok(vm.to_string()),
        false => Err(not()),
    }
}

// Whether the run's work, and so its Claude Code session, is on its VM's
// disk: it committed, joined the VM running, or was taken over, which
// leaves the VM up on the run's disk; a clean stop commits those two. A
// rolled-back run's went with the disk it ran.
fn on_disk(run: &AgentRun) -> bool {
    run.outcome == Some(Outcome::Committed) || !run.boots || run.state == runs::State::TakenOver
}

// Whether the try `name`, whose newest run is `run`, can be kept: no run is
// going, it's stopped, and it has a disk of its own, not still generation 0,
// the copy of the VM's that it forked. A rollback, or a run that never got
// going, leaves it there.
fn winnable(store: &Store, vm: &str, name: &str, run: Option<&AgentRun>) -> Result<()> {
    let (vm, name) = (vm.to_string(), name.to_string());
    if let Some(run) = run.filter(|r| r.state == runs::State::Running) {
        let run = run.id.clone();
        return Err(Error::StillGoing { vm, run });
    }
    stopped(store, &name)?;
    let forked = store.get(&name)?.generation == 0;
    match run {
        None => Err(Error::NeverRan { vm, name }),
        Some(run) if forked => Err(Error::Unchanged {
            how: match run.outcome {
                Some(Outcome::RolledBack) => format!("{} rolled back", run.id),
                _ => format!("{} {}", run.id, runs::describe(run)),
            },
            vm,
            name,
        }),
        Some(_) => Ok(()),
    }
}

// Refused unless `name` is stopped.
fn stopped(store: &Store, name: &str) -> Result<()> {
    match systemd::state(store, name)? {
        State::Stopped => Ok(()),
        state => Err(Error::Up {
            name: name.to_string(),
            state,
        }),
    }
}

impl Standing {
    /// One table, a header and then a row per try: its run, how it stands,
    /// its exit code, how long it took, how many paths it changed, and the
    /// first line of what its agent says, quoted and cut at SAYS_WIDTH
    /// characters. A running try's time is so far, at `now`.
    pub fn table(&self, now: DateTime<Utc>) -> String {
        let head = ["RUN", "STATE", "EXIT", "TOOK", "CHANGED"];
        let mut rows = vec![(head.map(String::from), "AGENT SAYS (UNTRUSTED)".to_string())];
        rows.extend(self.tries.iter().map(|t| t.row(now)));
        let width = |i: usize| rows.iter().map(|(r, _)| r[i].chars().count()).max();
        let widths = [0, 1, 2, 3, 4].map(|i| width(i).unwrap_or(0));
        let mut out = String::new();
        for (cells, says) in &rows {
            for (cell, w) in cells.iter().zip(widths) {
                let _ = write!(out, "{cell:<w$}  ");
            }
            out.push_str(says);
            out.push('\n');
        }
        out
    }
}

impl Tried {
    // Its cells, and what its agent says.
    fn row(&self, now: DateTime<Utc>) -> ([String; 5], String) {
        let id = self.id.clone().unwrap_or_else(|| self.vm.clone());
        let dash = || "-".to_string();
        let Some(run) = &self.run else {
            let state = if self.gone { "gone" } else { "no run" };
            return ([id, state.to_string(), dash(), dash(), dash()], dash());
        };
        let exit = run.exit_code.map_or_else(dash, |code| code.to_string());
        let since = |to: DateTime<Utc>| {
            let ms = (to - run.started).num_milliseconds();
            took(u64::try_from(ms).unwrap_or(0))
        };
        let took = match (&self.result, run.ended, run.state) {
            (Some(result), ..) => took(result.duration_ms),
            (None, Some(ended), _) => since(ended),
            (None, None, runs::State::Running) => since(now),
            // Lost: when it ended isn't known.
            (None, None, _) => dash(),
        };
        let changed = match self.result.as_ref().map(|r| &r.changed) {
            Some(Changed::Measured(c)) => (c.paths.len() as u64 + c.more).to_string(),
            _ => dash(),
        };
        let says = (self.result.as_ref())
            .and_then(|r| r.agent_says.text())
            .and_then(first_line)
            .unwrap_or_else(dash);
        let cells = [id, runs::describe(run), exit, took, changed];
        (cells, says)
    }
}

// "42s", "2m14s", "1h03m".
fn took(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

// The first line with something on it, cut at SAYS_WIDTH characters and
// quoted, so control characters show escaped rather than reach the terminal.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= SAYS_WIDTH {
        return Some(format!("{line:?}"));
    }
    let mut cut: String = line.chars().take(SAYS_WIDTH - 1).collect();
    cut.push('…');
    Some(format!("{cut:?}"))
}

// A try as it stands: gone if its fork or run has been deleted, even while
// it's read. One the record has no run for may have one all the same, if
// `run --n` died before it noted it: a fork's first run is <vm>.1.
fn tried(store: &Store, t: &Try) -> Result<Tried> {
    let mut tried = Tried {
        vm: t.vm.clone(),
        id: t.run.clone(),
        gone: false,
        run: None,
        result: None,
    };
    let id = t.run.clone().unwrap_or_else(|| runs::id(&t.vm, 1));
    let run = match runs::get(store, &id) {
        Err(runs::Error::Store(store::Error::NoVm(_))) => {
            tried.gone = true;
            return Ok(tried);
        }
        // Its run was deleted, or it never had one.
        Err(runs::Error::NoRun(_)) => {
            tried.gone = t.run.is_some();
            return Ok(tried);
        }
        r => r?,
    };
    tried.id = Some(id.clone());
    if run.state != runs::State::Running {
        tried.result = match runs::result(store, &id) {
            Err(runs::Error::NoResult { .. } | runs::Error::StillRunning(_)) => None,
            Err(runs::Error::Store(store::Error::NoVm(_)) | runs::Error::NoRun(_)) => {
                tried.gone = true;
                return Ok(tried);
            }
            r => Some(r?),
        };
    }
    tried.run = Some(run);
    Ok(tried)
}

// Why a new batch can't start while one of the VM's is on hand: some of its
// tries are still VMs.
fn on_hand(store: &Store, vm: &str) -> Result<Option<Error>> {
    let Some(batch) = tries(store, vm)? else {
        return Ok(None);
    };
    let left: Vec<&str> = (batch.tries.iter())
        .map(|t| t.vm.as_str())
        .filter(|name| store.get(name).is_ok())
        .collect();
    if left.is_empty() {
        return Ok(None);
    }
    Ok(Some(Error::OnHand {
        vm: vm.to_string(),
        n: batch.tries.len(),
        when: (batch.started).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        left: and(&left),
    }))
}

// Refused unless the host's free memory, less SPARE_MIB, holds `n` VMs of
// `each` MiB.
fn room(vm: &str, n: u32, each: u64) -> Result<()> {
    let free = host::mem_available().map_err(Error::Mem)? >> 20;
    let need = each * u64::from(n);
    let spare = free.saturating_sub(SPARE_MIB);
    if need <= spare {
        return Ok(());
    }
    let fit = spare / each.max(1);
    let fits = match fit >= u64::from(MIN) {
        true => format!("--n {fit} fits"),
        false => "not even two fit".to_string(),
    };
    Err(Error::NoRoom {
        vm: vm.to_string(),
        n,
        each,
        need,
        free,
        fits,
    })
}

// Each try, a fork of `generation` of `vm`; on a failure, none of them.
fn fork_all(store: &Store, vm: &str, generation: u64, names: &[String]) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        if let Err(e) = store.fork_at(vm, generation, name, &Config::default()) {
            drop_forks(store, &names[..i]);
            return Err(e.into());
        }
    }
    Ok(())
}

// Tries made that won't run. Each is new and stopped, so a delete takes it.
fn drop_forks(store: &Store, names: &[String]) {
    for name in names {
        if let Err(e) = store.delete(name) {
            eprintln!("runner: deleting {name}: {e}");
        }
    }
}

fn record(store: &Store, vm: &str, batch: &Batch) -> Result<()> {
    let path = store.tries(vm)?;
    let text = serde_json::to_string(batch).map_err(|e| store::at(&path)(e.into()))?;
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Err(store::at(&path)(io::ErrorKind::InvalidInput.into()).into());
    };
    Ok(store::write_aside(dir, name, &text)?)
}

// Clears the VM's batch once keep has deleted its tries.
fn drop_record(store: &Store, vm: &str) -> Result<()> {
    let path = store.tries(vm)?;
    match fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(store::at(&path)(e).into()),
        _ => Ok(()),
    }
}

// "a", "a and b", "a, b and c".
fn and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
