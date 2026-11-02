// Sleep when idle: a VM's boot puts it to sleep (src/sleep.rs) once nothing
// has used it for its idle time, ten minutes unless `runner idle` or
// `create --idle` says otherwise, and every command that needs its guest
// wakes it first (exec::open).
//
// Use is what the host can see. A command holds the VM's use lock, shared,
// from before it looks at the VM until its connection to the guest closes,
// and touches the lock's file as it takes it and as it lets go, so a use too
// short for the boot to catch still leaves its mark. An open `runner proxy`
// holds it through `runner hold` on the host (exec::hold). Every few seconds
// the boot looks: the lock held or touched since its last look, a headless run
// going, or, once the time is otherwise up, a terminal attached to a tmux
// session in the guest each count as use. A detached session doesn't: it
// sleeps with the rest of the VM's memory and is there on waking. Once none
// has been seen for the idle time, the boot puts the VM to sleep holding the
// lock exclusive, so a command that comes meanwhile waits for the sleep, then
// sees the VM sleeping and wakes it.
use crate::store::{self, Store};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};

/// How long a VM goes unused before its boot puts it to sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Idle {
    /// This many seconds.
    After(u32),
    Never,
}

impl Default for Idle {
    /// Ten minutes.
    fn default() -> Idle {
        Idle::After(10 * 60)
    }
}

impl Idle {
    /// None for never.
    pub fn after(self) -> Option<Duration> {
        match self {
            Idle::After(secs) => Some(Duration::from_secs(secs.into())),
            Idle::Never => None,
        }
    }
}

/// Minutes, like `10`; with a unit, like `90s`, `10m` or `2h`; or `never`.
impl FromStr for Idle {
    type Err = String;

    fn from_str(text: &str) -> Result<Idle, String> {
        if text == "never" {
            return Ok(Idle::Never);
        }
        let digits = text.trim_end_matches(['s', 'm', 'h']);
        let per = match &text[digits.len()..] {
            "" | "m" => 60,
            "s" => 1,
            "h" => 60 * 60,
            _ => 0,
        };
        let secs = Some(digits)
            .filter(|d| per > 0 && !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|d| d.parse::<u32>().ok())
            .and_then(|n| n.checked_mul(per))
            .filter(|&secs| secs > 0);
        secs.map(Idle::After).ok_or_else(|| {
            format!(
                "{text:?} isn't an idle time: use minutes, like 10, or 90s, 10m or 2h, or never"
            )
        })
    }
}

/// In the largest unit it's a whole number of.
impl fmt::Display for Idle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Idle::Never => f.write_str("never"),
            Idle::After(secs) if secs % 3600 == 0 => write!(f, "{}h", secs / 3600),
            Idle::After(secs) if secs % 60 == 0 => write!(f, "{}m", secs / 60),
            Idle::After(secs) => write!(f, "{secs}s"),
        }
    }
}

/// When the VM was last seen in use, on a clock the caller gives.
#[derive(Debug, Clone, Copy)]
pub struct Timer {
    since: Instant,
}

impl Timer {
    /// Started now: a VM that just booted or woke has a whole idle time.
    pub fn new(now: Instant) -> Timer {
        Timer { since: now }
    }

    /// Seen in use: the idle time starts again.
    pub fn used(&mut self, now: Instant) {
        self.since = self.since.max(now);
    }

    /// Whether the idle time has passed unused.
    pub fn due(&self, idle: Idle, now: Instant) -> bool {
        idle.after()
            .is_some_and(|after| now.saturating_duration_since(self.since) >= after)
    }
}

/// What a look asks, cheapest first.
pub trait Probe {
    /// A command holds the VM, or has since the last look.
    fn used(&mut self) -> bool;
    /// A headless run is going.
    fn running(&mut self) -> bool;
    /// A terminal is attached to a tmux session in the guest. Asked only once
    /// the time is otherwise up.
    fn attached(&mut self) -> bool;
}

/// What a look found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// In use: the idle time starts again.
    Busy,
    /// Unused, but not for long enough yet.
    Wait,
    /// Unused for the idle time: put it to sleep.
    Sleep,
}

/// One look at the VM, at `now`.
pub fn look(timer: &mut Timer, idle: Idle, now: Instant, probe: &mut impl Probe) -> Look {
    if idle == Idle::Never {
        return Look::Wait;
    }
    if probe.used() || probe.running() {
        timer.used(now);
        return Look::Busy;
    }
    if !timer.due(idle, now) {
        return Look::Wait;
    }
    if probe.attached() {
        timer.used(now);
        return Look::Busy;
    }
    Look::Sleep
}

/// A command's use of a VM: the VM's use lock, shared, until it's dropped.
#[derive(Debug)]
pub struct InUse {
    file: File,
}

impl InUse {
    /// Waits out a sleep under way, then holds the VM awake, as far as idling
    /// goes, until dropped. A command takes it before it looks at the VM.
    pub fn take(store: &Store, name: &str) -> store::Result<InUse> {
        let path = store.use_lock(name)?;
        let file = open(&path).map_err(store::at(&path))?;
        file.lock_shared().map_err(store::at(&path))?;
        touch(&file);
        Ok(InUse { file })
    }
}

impl Drop for InUse {
    // Touched before the lock goes, so a look that finds it free sees the
    // touch too.
    fn drop(&mut self) {
        touch(&self.file);
    }
}

/// A boot's side of the use lock.
#[derive(Debug)]
pub struct Uses {
    path: PathBuf,
    file: File,
    /// The file's mtime at the last look that found the lock free.
    seen: Option<SystemTime>,
}

impl Uses {
    pub fn open(store: &Store, name: &str) -> store::Result<Uses> {
        let path = store.use_lock(name)?;
        let file = open(&path).map_err(store::at(&path))?;
        let seen = mtime(&file);
        Ok(Uses { path, file, seen })
    }

    /// The lock, exclusive, if no command holds it and none has touched it
    /// since the last look; None if one has. Held until dropped.
    pub fn quiet(&mut self) -> io::Result<Option<Quiet>> {
        let file = self.file.try_clone().map_err(|e| self.err(e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(self.err(e)),
        }
        // From here, dropping it lets go.
        let quiet = Quiet { file };
        let now = mtime(&self.file);
        if now != self.seen {
            self.seen = now;
            return Ok(None);
        }
        Ok(Some(quiet))
    }

    fn err(&self, e: io::Error) -> io::Error {
        io::Error::new(e.kind(), format!("{}: {e}", self.path.display()))
    }
}

/// The use lock held exclusive: no command starts on the VM until it's
/// dropped.
#[derive(Debug)]
pub struct Quiet {
    file: File,
}

impl Drop for Quiet {
    fn drop(&mut self) {
        let _ = self.file.unlock();
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

fn mtime(file: &File) -> Option<SystemTime> {
    file.metadata().and_then(|m| m.modified()).ok()
}
