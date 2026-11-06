// The host powers itself off once it has been idle for its idle time, an
// hour on a host client/aws.sh set up and never on one it didn't
// (host/install.sh keeps it in /etc/runner/host-idle). On EC2 an OS
// power-off stops the instance, so only its disks cost until
// `client/aws.sh start`. It's any host shutdown: a sleeping VM is parked
// and sleeps on through it (src/sleep.rs), and a running one shuts down
// cleanly.
//
// Use is what keeps the host up: a VM running, booting or stopping (one
// stopped or asleep isn't); a login by any user but the one the look runs
// as, runner, whose own SSH sessions, like a `runner mcp` left open on the
// laptop, don't count; and a runner command at work. Every command, and
// every MCP call, holds <root>/busy.lock shared while it works, and touches
// the file as it takes it and as it lets go (`Busy`), so the lock's mtime is
// when one last let go. The look touches it too whenever it sees use, so its
// mtime is when the host was last seen in use, and nothing else needs to
// keep that.
//
// runner-host-idle.timer has `runner host-idle` look every minute. Once
// nothing has used the host for its idle time since it was last seen in
// use, or since it booted if that's later, so a host just started gets the
// whole idle time, the look says why on stdout, which is its journal, and
// powers off through logind, as runner, which install.sh's polkit rule
// allows and nothing more. The decision is `decide`, from what the look saw,
// so it's tested without systemd.
use crate::idle::Idle;
use crate::store::{self, Store};
use crate::systemd::{self, State};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// The polkit action logind asks about for a power-off with no one else
/// logged in: all install.sh's rule lets runner do.
pub const POWER_OFF: &str = "org.freedesktop.login1.power-off";

/// The lock each runner command holds, shared, while it works.
pub fn busy_lock(root: &Path) -> PathBuf {
    root.join("busy.lock")
}

/// What a look saw on the host.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Each VM's name and state.
    pub vms: Vec<(String, State)>,
    /// The user of each login session, as loginctl lists them.
    pub logins: Vec<String>,
    /// A runner command holds the busy lock.
    pub busy: bool,
}

/// When the host was last seen in use, or booted if that's later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    since: SystemTime,
}

impl Clock {
    /// From the host's boot and the last use the busy lock marks.
    pub fn new(booted: SystemTime, used: Option<SystemTime>) -> Clock {
        let since = used.map_or(booted, |used| used.max(booted));
        Clock { since }
    }

    pub fn since(&self) -> SystemTime {
        self.since
    }
}

/// What a look found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The idle time is never: the host stays up.
    Never,
    /// In use, by what it names: the clock starts again.
    Used(String),
    /// Idle, but not for its idle time yet: this much is left.
    Wait(Duration),
    /// Idle for its idle time, as it says: power off.
    Off(String),
}

/// One look, at `now`, by the look running as `me`.
pub fn decide(clock: &mut Clock, idle: Idle, me: &str, seen: &Seen, now: SystemTime) -> Verdict {
    let Some(after) = idle.after() else {
        return Verdict::Never;
    };
    let up = seen
        .vms
        .iter()
        .find(|(_, state)| matches!(state, State::Running | State::Booting | State::Stopping));
    let used = if let Some((vm, state)) = up {
        Some(format!("{vm} is {state}"))
    } else if let Some(user) = seen.logins.iter().find(|user| *user != me) {
        Some(format!("{user} is logged in"))
    } else if seen.busy {
        Some("a runner command is at work".to_string())
    } else {
        None
    };
    if let Some(used) = used {
        clock.since = clock.since.max(now);
        return Verdict::Used(used);
    }
    // A clock that went back reads as just used.
    let idle_for = now.duration_since(clock.since).unwrap_or_default();
    if idle_for < after {
        return Verdict::Wait(after - idle_for);
    }
    let since = chrono::DateTime::<chrono::Utc>::from(clock.since);
    let since = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Verdict::Off(format!(
        "idle since {since}, longer than the host's idle time, {idle}: every VM stopped or asleep, no one but {me} logged in, and no runner command at work"
    ))
}

/// A runner command at work: the busy lock, shared, until it's dropped.
#[derive(Debug)]
pub struct Busy {
    file: File,
}

impl Busy {
    /// Waits out a look that holds it, then holds the host up until dropped.
    /// None if the lock can't be had, as in a root that isn't there yet: the
    /// command goes on without it, and `runner doctor` says when a host's
    /// lock won't open.
    pub fn take(root: &Path) -> Option<Busy> {
        let file = open(&busy_lock(root)).ok()?;
        file.lock_shared().ok()?;
        touch(&file);
        Some(Busy { file })
    }
}

impl Drop for Busy {
    // Touched before the lock goes, so a look that finds it free sees the
    // touch too.
    fn drop(&mut self) {
        touch(&self.file);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {why}. host/install.sh sets it from RUNNER_HOST_IDLE")]
    Setting { path: String, why: String },
    #[error("{path}: {source}")]
    Io { path: String, source: io::Error },
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error("{0}")]
    Failed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The host's idle time from `path`, as host/install.sh keeps it: never
/// when it isn't there.
pub fn setting(path: &Path) -> Result<Idle> {
    match std::fs::read_to_string(path) {
        Ok(text) => text.trim().parse().map_err(|why| Error::Setting {
            path: path.display().to_string(),
            why,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Idle::Never),
        Err(e) => Err(io_at(path)(e)),
    }
}

/// `runner host-idle`: one look at the host, with its idle time from
/// `setting` and its boot from `uptime` (/proc/uptime), and a power-off
/// if it's due.
pub fn look(store: &Store, setting: &Path, uptime: &Path) -> Result<Verdict> {
    let idle = self::setting(setting)?;
    if idle == Idle::Never {
        return Ok(Verdict::Never);
    }
    let path = busy_lock(store.root());
    let file = open(&path).map_err(io_at(&path))?;
    // Held exclusive from here, so no command starts between what the look
    // saw and the power-off.
    let held = match file.try_lock() {
        Ok(()) => Some(Unlock(&file)),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(e)) => return Err(io_at(&path)(e)),
    };
    let mut vms = Vec::new();
    for vm in store.list()? {
        let state = systemd::state(store, &vm.name)?;
        vms.push((vm.name, state));
    }
    let seen = Seen {
        vms,
        logins: logins()?,
        busy: held.is_none(),
    };
    let now = SystemTime::now();
    let up = std::fs::read_to_string(uptime).map_err(io_at(uptime))?;
    let booted = parse_uptime(&up)
        .and_then(|up| now.checked_sub(up))
        .ok_or_else(|| Error::Failed(format!("{}: no uptime in {up:?}", uptime.display())))?;
    let used = file.metadata().and_then(|m| m.modified()).ok();
    let mut clock = Clock::new(booted, used);
    let verdict = decide(&mut clock, idle, &whoami()?, &seen, now);
    match &verdict {
        Verdict::Used(_) => touch(&file),
        Verdict::Off(why) => {
            println!("{why}: powering off");
            power_off()?;
        }
        Verdict::Wait(_) | Verdict::Never => {}
    }
    drop(held);
    Ok(verdict)
}

/// How long the host has been up, from /proc/uptime's text.
pub fn parse_uptime(text: &str) -> Option<Duration> {
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

/// The user of each line `loginctl list-sessions --no-legend` prints, whose
/// first three columns are the session, its user's uid, and its user, in
/// every systemd from 219 on. A line that isn't one, like "No sessions.", is
/// skipped.
pub fn parse_sessions(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (_session, uid, user) = (words.next()?, words.next()?, words.next()?);
            uid.parse::<u32>().ok()?;
            Some(user.to_string())
        })
        .collect()
}

fn logins() -> Result<Vec<String>> {
    let out = run("loginctl", &["list-sessions", "--no-legend"])?;
    Ok(parse_sessions(&out))
}

fn power_off() -> Result<()> {
    run("systemctl", &["poweroff"]).map(drop)
}

// stdin closed, so neither asks for a password.
fn run(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Failed(format!("running {prog}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let command = [prog].iter().chain(args).copied().collect::<Vec<_>>();
        return Err(Error::Failed(format!(
            "{} failed: {}",
            command.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// The user this runs as: runner, under its unit.
fn whoami() -> Result<String> {
    let uid = nix::unistd::geteuid();
    match nix::unistd::User::from_uid(uid) {
        Ok(Some(user)) => Ok(user.name),
        Ok(None) => Err(Error::Failed(format!(
            "uid {uid} isn't in the passwd database"
        ))),
        Err(e) => Err(Error::Failed(format!("uid {uid}: {e}"))),
    }
}

// Unlocks on drop, for the look's exclusive hold.
struct Unlock<'a>(&'a File);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

// Best effort: a use whose touch is lost is still seen while it holds the
// lock.
fn touch(file: &File) {
    let _ = file.set_modified(SystemTime::now());
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}
