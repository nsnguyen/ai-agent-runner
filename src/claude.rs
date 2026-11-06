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
use crate::exec;
use crate::proto::{Exit, Request, WinSize};
use crate::runs;
use crate::sessions;
use crate::store::{self, Store};
use crate::systemd::{self, State};
use std::io::Write;

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
    #[error("{id} has already ended: {how}")]
    Ended { id: String, how: String },
    #[error(
        "{id}'s Claude Code ended on its own before the takeover stopped it, so nothing was \
         opened: {how}"
    )]
    EndedFirst { id: String, how: String },
    #[error(
        "{id} is still stopping: once `runner runs {vm}` says it's taken over, `runner claude \
         {vm} --dir {dir} -- --resume {session}` carries its session on"
    )]
    StillStopping {
        id: String,
        vm: String,
        dir: String,
        session: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

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
        }
    }
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
/// as its user. Nothing stops unless every check passes.
pub fn take_over(vms: &impl Vms, t: &TakeOver, notes: &mut impl Write) -> Result<Exit> {
    let run = vms.run(&t.run)?;
    if run.state != runs::State::Running {
        return Err(Error::Ended {
            id: run.id,
            how: run.how,
        });
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
                id: run.id,
                vm: run.vm,
                dir: run.dir,
                session,
            });
        }
        _ => {
            return Err(Error::EndedFirst {
                id: ended.id,
                how: ended.how,
            });
        }
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
    let argv: Vec<String> = ["claude", "--resume", &session]
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
    vms.attach(vm, &req)
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
}
