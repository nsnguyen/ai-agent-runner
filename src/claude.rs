// `runner claude <vm>`: Claude Code running in the VM, on this terminal. A new
// VM is made from base and a stopped one started, then the terminal attaches
// to the VM's tmux session named claude (src/sessions.rs), which tmux makes
// to run claude only if it isn't there. So a dropped connection detaches it,
// the next `runner claude` reattaches, and a VM never runs two. Claude Code
// gets the VM's credentials as every command does (src/creds.rs).
use crate::exec;
use crate::proto::{Exit, Request, WinSize};
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
}

pub type Result<T> = std::result::Result<T, Error>;

/// What `runner claude` does to VMs. `Host` is the real one; tests fake it.
pub trait Vms {
    fn exists(&self, vm: &str) -> Result<bool>;
    /// Makes `vm` a fork of `from`.
    fn create(&self, vm: &str, from: &str) -> Result<()>;
    fn running(&self, vm: &str) -> Result<bool>;
    fn start(&self, vm: &str) -> Result<()>;
    fn has_session(&self, vm: &str, user: &str, session: &str) -> Result<bool>;
    /// This terminal on `req` in the VM, until it exits or detaches.
    fn attach(&self, vm: &str, req: &Request) -> Result<Exit>;
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

    fn running(&self, vm: &str) -> Result<bool> {
        Ok(systemd::status(self.0, vm)?.state == State::Running)
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
}
