// `runner claude <vm>`: Claude Code running in the VM, on this terminal. A new
// VM is made from base and a stopped one started, then the terminal attaches
// to the VM's tmux session named claude (src/sessions.rs), which tmux makes
// to run claude only if it isn't there. So a dropped connection detaches it,
// the next `runner claude` reattaches, and a VM never runs two. Claude Code
// gets the VM's credentials as every command does (src/creds.rs).
//
// `runner claude --run dev.1` takes a headless run that's going over: once
// the run has stopped its `claude -p` (runs.rs, take_over), which leaves dev
// up, its session goes on in that same claude session, as `claude --resume`
// in the run's directory, as the run's user. From there it's an ordinary
// `runner claude` session.
//
// A run that's over is taken over from the disk its session's record is on,
// under the run user's ~/.claude/projects. One that committed or joined dev
// left it on dev's own, so dev is started if it's stopped. One that rolled
// back left it on the run's own generation, kept in dev's gens/ until gc: a
// copy of that goes on top of dev's history, as a restore's does, and dev
// boots onto it. That needs dev stopped, which would end its sessions, so
// it's refused while dev is up. Either way the record is looked for before
// claude starts, since a restore, undo or rollback since can have taken it.
// The run is marked taken then (runs::mark_taken), and from then on, as for
// one taken over while it went, its session is on dev's own disk, or in
// dev's claude session if that's there.
use crate::exec;
use crate::idle::InUse;
use crate::proto::{Exit, Request, WinSize};
use crate::result::Outcome;
use crate::runs;
use crate::sessions;
use crate::store::{self, Store};
use crate::systemd::{self, State};
use std::io::Write;
use std::time::Duration;

/// The tmux session Claude Code runs in.
pub const SESSION: &str = "claude";
/// Where a new session starts Claude Code: the base image's work directory.
pub const DIR: &str = "/home/agent/work";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Sessions(#[from] sessions::Error),
    #[error(transparent)]
    Exec(#[from] exec::Error),
    #[error(transparent)]
    Runs(#[from] runs::Error),
    #[error(
        "{id} has no Claude Code session yet, so there's none to resume: `runner runs {vm}` \
         shows it once Claude Code has named it"
    )]
    NoSession { id: String, vm: String },
    #[error(
        "{vm} already has a claude session, which the run's would replace: `runner kill {vm} \
         claude` ends it"
    )]
    SessionThere { vm: String },
    #[error(
        "{id} ended before the takeover stopped it, so nothing was opened: {how}. `runner \
         claude --run {id}` again picks it up from there"
    )]
    EndedFirst { id: String, how: String },
    #[error("{id} ended before Claude Code named a session, so there's none to resume: {how}")]
    NeverNamed { id: String, how: String },
    #[error(
        "{id} is taken over already: its session goes on in {vm}'s claude session, which \
         `runner claude {vm}` reattaches"
    )]
    TakenOver { id: String, vm: String },
    #[error(
        "{id} is over, but what became of its disk isn't known, so neither is its session: {how}"
    )]
    NoDisk { id: String, how: String },
    #[error(
        "{id} rolled back, so taking it over puts its disk back on {vm}, which needs {vm} \
         stopped and would end its sessions: `runner stop {vm}` first"
    )]
    Up { id: String, vm: String },
    #[error(
        "{id} rolled back, and its disk, {vm}'s generation {generation}, is gone: gc dropped it \
         at a later commit, and the session with it"
    )]
    Dropped {
        id: String,
        vm: String,
        generation: u64,
    },
    /// `since` says how it can have gone.
    #[error(
        "{id}'s session isn't on {vm}'s disk: {user} has no ~/.claude/projects/*/{session}.jsonl \
         there, {since}"
    )]
    NotOnDisk {
        id: String,
        vm: String,
        user: String,
        session: String,
        since: String,
    },
    #[error("looking for {session}'s record in {vm}: {why}")]
    Look {
        vm: String,
        session: String,
        why: String,
    },
    /// `resume` is the command that carries the session on by hand.
    #[error(
        "{id} is still stopping: once `runner runs {vm}` says it's taken over, {resume} \
         carries its session on"
    )]
    StillStopping {
        id: String,
        vm: String,
        resume: String,
    },
    #[error(
        "{id} is taken over, but {vm} got a claude session while it stopped: `runner kill {vm} \
         claude` ends that, then {resume} carries the run's session on"
    )]
    SessionOpened {
        id: String,
        vm: String,
        resume: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

// What carries a run's session on by hand.
fn resume(run: &Run, session: &str) -> String {
    let (vm, dir, user) = (&run.vm, &run.dir, &run.user);
    format!("`runner claude {vm} --dir {dir} --user {user} -- --resume {session}`")
}

/// What `runner claude` does to VMs. `Host` is the real one; tests fake it.
pub trait Vms {
    fn exists(&self, vm: &str) -> Result<bool>;
    /// Makes `vm` a fork of `from`.
    fn create(&self, vm: &str, from: &str) -> Result<()>;
    /// Up, awake or asleep: it may have sessions.
    fn running(&self, vm: &str) -> Result<bool>;
    fn start(&self, vm: &str) -> Result<()>;
    fn has_session(&self, vm: &str, user: &str, session: &str) -> Result<bool>;
    /// This terminal on `req` in the VM, until it exits or detaches.
    fn attach(&self, vm: &str, req: &Request) -> Result<Exit>;
    /// The headless run with this id.
    fn run(&self, id: &str) -> Result<Run>;
    /// Stops the run's Claude Code, leaving its VM up, and returns the run
    /// once it has ended, or as it stands after two minutes.
    fn take_over(&self, id: &str) -> Result<Run>;
    /// Whether the VM still has this generation's disk: gc drops it.
    fn kept(&self, vm: &str, generation: u64) -> Result<bool>;
    /// Puts a copy of this generation on top of the stopped VM's history,
    /// for run `run`'s takeover, and returns the generation that was current
    /// before and the copy's. Refused while a run is going on the VM, then
    /// while the VM is up (Up).
    fn put_back(&self, vm: &str, generation: u64, run: &str) -> Result<(u64, u64)>;
    /// Whether `user` has Claude Code's record of `session` in the running
    /// VM, under ~/.claude/projects.
    fn has_record(&self, vm: &str, user: &str, session: &str) -> Result<bool>;
    /// Marks the run, over, as taken over (runs::mark_taken).
    fn mark_taken(&self, id: &str) -> Result<()>;
}

/// A headless run, as a takeover needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: String,
    pub vm: String,
    pub state: runs::State,
    /// How it stands, in a few words: runs::describe.
    pub how: String,
    /// Its Claude Code session, once Claude Code has named it.
    pub session: Option<String>,
    /// Where its Claude Code runs, and as whom.
    pub dir: String,
    pub user: String,
    /// Whether it booted its VM, or joined it running.
    pub boots: bool,
    /// What became of its disk, once it's over: runs::AgentRun's.
    pub outcome: Option<Outcome>,
    /// The generation it booted, once it's over: its result's.
    pub generation: Option<u64>,
    /// Taken over already after it ended (runs::mark_taken).
    pub taken: bool,
}

impl From<runs::Placed> for Run {
    fn from(p: runs::Placed) -> Run {
        Run {
            how: runs::describe(&p.run),
            id: p.run.id,
            vm: p.run.vm,
            state: p.run.state,
            session: p.run.session,
            dir: p.dir,
            user: p.user,
            boots: p.run.boots,
            outcome: p.run.outcome,
            generation: p.generation,
            taken: p.taken,
        }
    }
}

// Where a run that's over left its session's record.
enum Disk {
    // The VM's own: it committed, or joined the VM, or it was taken over.
    Own,
    // Its own generation, rolled back.
    Run(u64),
}

/// One `runner claude --run`.
pub struct TakeOver {
    /// The run's id, like dev.1.
    pub run: String,
    /// For claude, after `--resume <session>`.
    pub args: Vec<String>,
    /// For the guest's terminal: TERM.
    pub env: Vec<(String, String)>,
    pub size: WinSize,
}

/// One `runner claude`.
pub struct Claude {
    pub vm: String,
    /// What a new VM forks: base unless given.
    pub from: Option<String>,
    /// Where a new session starts claude.
    pub dir: String,
    pub user: String,
    /// claude and its arguments.
    pub argv: Vec<String>,
    /// For the guest's terminal: TERM.
    pub env: Vec<(String, String)>,
    pub size: WinSize,
}

/// Makes the VM if it's new and starts it if it's stopped, then attaches this
/// terminal to its claude session. What it can't do as asked goes to `notes`.
pub fn open(vms: &impl Vms, c: &Claude, notes: &mut impl Write) -> Result<Exit> {
    let vm = &c.vm;
    let made = !vms.exists(vm)?;
    if made {
        let from = c.from.as_deref().unwrap_or(store::BASE);
        vms.create(vm, from)?;
        note(notes, &format!("made {vm} from {from}"));
    } else if let Some(from) = &c.from {
        note(
            notes,
            &format!("{vm} already exists, so it isn't made from {from}"),
        );
    }
    // One just made or started has no sessions yet.
    let running = !made && vms.running(vm)?;
    if !running {
        vms.start(vm)?;
    }
    let found = running && vms.has_session(vm, &c.user, SESSION)?;
    // Usually claude, though it could be a shell someone named claude.
    if found && c.argv.len() > 1 {
        let args = c.argv[1..].join(" ");
        note(
            notes,
            &format!("{vm} already has a claude session, so this attaches to it without {args}"),
        );
    }
    // The guest refuses a directory that isn't there, where tmux would start
    // in the home one instead. Attaching needs none.
    let req = Request {
        argv: sessions::attach_or_run(SESSION, &c.dir, &c.argv),
        env: c.env.clone(),
        cwd: (!found).then(|| c.dir.clone()),
        user: Some(c.user.clone()),
        tty: Some(c.size),
    };
    vms.attach(vm, &req)
}

/// Takes a run that's going over: checks it has a session to resume and
/// that its VM has no claude session for it to clash with, then stops its
/// Claude Code, leaving the VM up, and attaches this terminal to `claude
/// --resume <session>` in the VM's claude session, in the run's directory,
/// as its user. Nothing stops unless every check passes. A run that's over
/// is picked up from the disk it ended on (pick_up).
pub fn take_over(vms: &impl Vms, t: &TakeOver, notes: &mut impl Write) -> Result<Exit> {
    let run = vms.run(&t.run)?;
    if run.state != runs::State::Running {
        return pick_up(vms, t, run, notes);
    }
    let Some(session) = run.session.clone() else {
        return Err(Error::NoSession {
            id: run.id,
            vm: run.vm,
        });
    };
    if vms.has_session(&run.vm, &run.user, SESSION)? {
        return Err(Error::SessionThere { vm: run.vm });
    }
    note(notes, &format!("stopping {}'s claude -p", run.id));
    let ended = vms.take_over(&run.id)?;
    match ended.state {
        runs::State::TakenOver => {}
        runs::State::Running => {
            return Err(Error::StillStopping {
                resume: resume(&run, &session),
                id: run.id,
                vm: run.vm,
            });
        }
        _ => {
            return Err(Error::EndedFirst {
                id: ended.id,
                how: ended.how,
            });
        }
    }
    // Stopping can take a while, and tmux would attach to a session opened
    // meanwhile as if it were the run's.
    if vms.has_session(&run.vm, &run.user, SESSION)? {
        return Err(Error::SessionOpened {
            resume: resume(&run, &session),
            id: run.id,
            vm: run.vm,
        });
    }
    let vm = &run.vm;
    note(
        notes,
        &format!(
            "{} is taken over: its session goes on in {vm}'s claude session, which `runner \
             claude {vm}` reattaches",
            run.id
        ),
    );
    resume_in(vms, t, &run, &session)
}

// A run that's over: its session goes on from the disk it ended on, with the
// VM up on that disk. Refused before anything starts or changes unless that
// disk is known and there, and once it is, unless the record is on it. Then
// the run is marked taken, so a takeover after this one goes on from the
// VM's own disk, as for a run taken over while it went.
fn pick_up(vms: &impl Vms, t: &TakeOver, run: Run, notes: &mut impl Write) -> Result<Exit> {
    let (id, vm) = (run.id.clone(), run.vm.clone());
    if run.state == runs::State::Lost {
        return Err(Error::NoDisk { id, how: run.how });
    }
    let Some(session) = run.session.clone() else {
        return Err(Error::NeverNamed { id, how: run.how });
    };
    let taken = run.state == runs::State::TakenOver || run.taken;
    let disk = match (taken, run.boots, run.outcome, run.generation) {
        (true, ..) => Disk::Own,
        (_, _, Some(Outcome::RolledBack), Some(generation)) => Disk::Run(generation),
        (_, false, ..) | (_, true, Some(Outcome::Committed), _) => Disk::Own,
        _ => return Err(Error::NoDisk { id, how: run.how }),
    };
    // How the record can have gone, if it isn't there.
    let since = match disk {
        // tmux would attach to a claude session that's there as if it were
        // the run's; one just started has none. A takeover's own is there to
        // reattach to.
        Disk::Own => {
            if !vms.running(&vm)? {
                vms.start(&vm)?;
            } else if vms.has_session(&vm, &run.user, SESSION)? {
                return Err(match taken {
                    true => Error::TakenOver { id, vm },
                    false => Error::SessionThere { vm },
                });
            }
            "so a restore, undo or rollback since has taken it".to_string()
        }
        Disk::Run(generation) => {
            if !vms.kept(&vm, generation)? {
                return Err(Error::Dropped { id, vm, generation });
            }
            let (before, put) = vms.put_back(&vm, generation, &id)?;
            let back = format!("`runner restore {vm} {before}`");
            note(
                notes,
                &format!(
                    "put {id}'s disk, generation {generation}, back on {vm} as generation \
                     {put}, over generation {before}: {back} brings that back"
                ),
            );
            // The session is on dev's own disk from here on, whatever comes.
            mark(vms, &id, notes);
            vms.start(&vm)?;
            format!(
                "so Claude Code never wrote it on the run's disk, which {vm} runs on now: {back} \
                 brings back the disk from before"
            )
        }
    };
    if !vms.has_record(&vm, &run.user, &session)? {
        return Err(Error::NotOnDisk {
            id,
            vm,
            user: run.user,
            session,
            since,
        });
    }
    if !taken && matches!(disk, Disk::Own) {
        mark(vms, &id, notes);
    }
    note(
        notes,
        &format!(
            "{id}'s session goes on in {vm}'s claude session, which `runner claude {vm}` \
             reattaches"
        ),
    );
    resume_in(vms, t, &run, &session)
}

// Without the mark, a takeover after this one would go by how the run ended:
// worth a note, not the session.
fn mark(vms: &impl Vms, id: &str, notes: &mut impl Write) {
    if let Err(e) = vms.mark_taken(id) {
        note(
            notes,
            &format!(
                "couldn't mark {id} taken over, so taking it over again goes by how it ended: {e}"
            ),
        );
    }
}

// This terminal on `claude --resume <session>` and the arguments after `--`,
// in the VM's claude session, in the run's directory, as its user.
fn resume_in(vms: &impl Vms, t: &TakeOver, run: &Run, session: &str) -> Result<Exit> {
    let argv: Vec<String> = ["claude", "--resume", session]
        .into_iter()
        .map(String::from)
        .chain(t.args.iter().cloned())
        .collect();
    let req = Request {
        argv: sessions::attach_or_run(SESSION, &run.dir, &argv),
        env: t.env.clone(),
        cwd: Some(run.dir.clone()),
        user: Some(run.user.clone()),
        tty: Some(t.size),
    };
    vms.attach(&run.vm, &req)
}

// Said before tmux takes the screen, and seen again after a detach.
fn note(notes: &mut impl Write, what: &str) {
    let _ = writeln!(notes, "runner: {what}");
}

/// The VMs in a store, under systemd.
pub struct Host<'a>(pub &'a Store);

impl Vms for Host<'_> {
    fn exists(&self, vm: &str) -> Result<bool> {
        match self.0.get(vm) {
            Ok(_) => Ok(true),
            Err(store::Error::NoVm(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    // A VM another `runner claude` made first is the one this one wanted.
    fn create(&self, vm: &str, from: &str) -> Result<()> {
        match self.0.fork(from, vm) {
            Ok(_) | Err(store::Error::Exists(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // A sleeping VM has its sessions too, and looking for one wakes it.
    fn running(&self, vm: &str) -> Result<bool> {
        let state = systemd::status(self.0, vm)?.state;
        Ok(matches!(state, State::Running | State::Sleeping))
    }

    fn start(&self, vm: &str) -> Result<()> {
        systemd::start(self.0, vm)?;
        Ok(())
    }

    fn has_session(&self, vm: &str, user: &str, session: &str) -> Result<bool> {
        Ok(sessions::exists(self.0, vm, user, session)?)
    }

    fn attach(&self, vm: &str, req: &Request) -> Result<Exit> {
        // In use until claude's terminal is let go.
        let exec::Conn {
            stream,
            in_use: _in_use,
        } = exec::open(self.0, vm)?;
        Ok(exec::attach(stream, req)?)
    }

    fn run(&self, id: &str) -> Result<Run> {
        Ok(runs::placed(self.0, id)?.into())
    }

    fn take_over(&self, id: &str) -> Result<Run> {
        runs::take_over(self.0, id)?;
        self.run(id)
    }

    fn kept(&self, vm: &str, generation: u64) -> Result<bool> {
        Ok(self.0.disk(vm, generation)?.is_file())
    }

    // Under the run lock, as a restore is: a run starting meanwhile would
    // boot the disk from before. Asked first, so a run that booted the VM
    // isn't met with advice to stop it.
    fn put_back(&self, vm: &str, generation: u64, run: &str) -> Result<(u64, u64)> {
        // An idle sleep holds the run lock while it lasts: wait it out.
        let _in_use = InUse::take(self.0, vm)?;
        let _no_run = runs::hold(self.0, vm)?;
        if systemd::status(self.0, vm)?.state != State::Stopped {
            return Err(Error::Up {
                id: run.to_string(),
                vm: vm.to_string(),
            });
        }
        let before = self.0.get(vm)?.generation;
        let why = format!("takeover of {run}");
        match self.0.restore_generation(vm, generation, &why) {
            Ok(put) => Ok((before, put.generation)),
            Err(store::Error::NoGeneration(..)) => Err(Error::Dropped {
                id: run.to_string(),
                vm: vm.to_string(),
                generation,
            }),
            Err(e) => Err(e.into()),
        }
    }

    // The session is a UUID (runs::session), passed as an argument, never
    // as the script's text.
    fn has_record(&self, vm: &str, user: &str, session: &str) -> Result<bool> {
        let exec::Conn {
            stream,
            in_use: _in_use,
        } = exec::open(self.0, vm)?;
        let req = Request {
            argv: ["sh", "-c", RECORD, "sh", session]
                .map(String::from)
                .to_vec(),
            env: Vec::new(),
            cwd: None,
            user: Some(user.to_string()),
            tty: None,
        };
        let got = exec::capture(stream, &req, b"", LOOK_WAIT, 4 << 10)?;
        let why = match (got.timed_out, got.exit_code, got.signal) {
            (false, Some(0), _) => return Ok(true),
            (false, Some(1), _) => return Ok(false),
            (true, ..) => format!("no answer in {}s", LOOK_WAIT.as_secs()),
            (_, Some(code), _) => format!("sh exited {code}: {}", got.stderr.trim()),
            (_, None, sig) => format!("sh was killed by signal {}", sig.unwrap_or(0)),
        };
        Err(Error::Look {
            vm: vm.to_string(),
            session: session.to_string(),
            why,
        })
    }

    fn mark_taken(&self, id: &str) -> Result<()> {
        Ok(runs::mark_taken(self.0, id)?)
    }
}

/// For `sh -c`: exits 0 if the user has Claude Code's record of session $1,
/// 1 if not. Claude Code keeps each session as <session>.jsonl in a
/// directory per project under ~/.claude/projects.
pub const RECORD: &str = r#"for f in "$HOME"/.claude/projects/*/"$1".jsonl; do
  [ -f "$f" ] && exit 0
done
exit 1"#;
// How long the look may take.
const LOOK_WAIT: Duration = Duration::from_secs(30);
