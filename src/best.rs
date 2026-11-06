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
use crate::checkpoint;
use crate::host;
use crate::idle::InUse;
use crate::runs::{self, AgentRun, Start};
use crate::store::{self, Config, DEFAULT_MEM_MIB, Store};
use crate::systemd::{self, State};
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

/// The fewest tries a batch has.
pub const MIN: u32 = 2;
/// The most tries a batch has.
pub const MAX: u32 = 4;
/// What the host keeps free past what a batch's tries take, in MiB.
pub const SPARE_MIB: u64 = 1024;

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
        "a VM named {name} already exists, and {vm}'s tries are named {vm}-1 on: delete {name} first"
    )]
    Taken { vm: String, name: String },
    #[error(
        "{vm} has tries on hand from best of {n} at {when}: {left}. One batch at a time: delete them first"
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
    #[error("{vm}'s tries are made, but {name}'s run didn't start: {source}. {started}")]
    NotStarted {
        vm: String,
        name: String,
        started: String,
        source: Box<runs::Error>,
    },
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
    record(store, vm, &batch)?;
    let mut started = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let one = Start { vm: name, ..*s };
        match runs::start(store, exe, &one) {
            Ok(run) => {
                batch.tries[i].run = Some(run.id.clone());
                started.push(run);
            }
            Err(source) => {
                record(store, vm, &batch)?;
                let ids: Vec<&str> = started.iter().map(|r| r.id.as_str()).collect();
                return Err(Error::NotStarted {
                    vm: vm.to_string(),
                    name: name.clone(),
                    started: match ids.is_empty() {
                        true => "None of their runs started".to_string(),
                        false => format!("{} started", and(&ids)),
                    },
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
            for made in &names[..i] {
                if let Err(e) = store.delete(made) {
                    eprintln!("runner: deleting {made}: {e}");
                }
            }
            return Err(e.into());
        }
    }
    Ok(())
}

fn record(store: &Store, vm: &str, batch: &Batch) -> Result<()> {
    let path = store.tries(vm)?;
    let text = serde_json::to_string(batch).map_err(|e| store::at(&path)(e.into()))?;
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Err(store::at(&path)(io::ErrorKind::InvalidInput.into()).into());
    };
    Ok(store::write_aside(dir, name, &text)?)
}

// "a", "a and b", "a, b and c".
fn and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
