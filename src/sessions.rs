// Detachable sessions: tmux in the guest, under `runner shell --session`,
// `sessions`, `attach` and `kill`, and MCP's list_sessions, read_session and
// kill_session. Each tmux call is an ordinary command over the guest protocol
// (src/proto.rs), run as the user, so sessions live on that user's own tmux
// server and `tmux ls` in the VM shows the same ones.
//
// tmux daemonizes its server into a session of its own, so the hang-up a
// dropped connection sends reaches only the client: the session detaches.
use crate::exec;
use crate::proto::Request;
use crate::store::Store;
use chrono::{DateTime, Utc};
use rmcp::schemars;
use serde::Serialize;
use std::time::Duration;

/// What `tmux list-sessions -F` prints for each session. tmux keeps `:` and
/// `.` out of names, so the name goes last and the command can hold anything.
pub const LIST_FORMAT: &str =
    "#{session_created}:#{session_attached}:#{pane_current_command}:#{session_name}";
/// How many of a pane's last lines read_session returns unless told.
pub const DEFAULT_LINES: u32 = 100;
/// tmux keeps 2000 lines of history unless its config says otherwise.
pub const MAX_LINES: u32 = 2000;

// A tmux command that doesn't attach is quick; one that hangs is a stuck VM.
const TIMEOUT: Duration = Duration::from_secs(30);
// Lines no wider than the pane, so 2000 of them stay well under this.
const CAPTURE_CAP: usize = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{0:?} isn't a session name: use up to 32 of A-Z, a-z, 0-9, _ and -, not starting with -"
    )]
    BadName(String),
    #[error("no session {session} in {vm}")]
    NoSession { vm: String, session: String },
    #[error("lines must be 1 to {MAX_LINES}, not {0}")]
    BadLines(u32),
    #[error("tmux in {vm}: {why}")]
    Tmux { vm: String, why: String },
    #[error(transparent)]
    Exec(#[from] exec::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A tmux session in a VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Session {
    pub name: String,
    /// What its active pane runs now, like bash or vim.
    pub command: String,
    /// When it was made, by the VM's clock.
    pub started: DateTime<Utc>,
    /// True while a terminal is on it.
    pub attached: bool,
}

/// The end of a session's active pane, as text without colour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Pane {
    /// Its last lines, each ending in a newline, without the blank rows under
    /// the cursor.
    pub text: String,
    /// True if fewer lines than asked fit in the 32 KiB kept.
    pub truncated: bool,
}

/// Names the runner makes and looks up: tmux's own rules, narrowed so a name
/// never reads as a flag or a tmux target's `=`, `:` or `.`.
pub fn check_name(name: &str) -> Result<()> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let ok = (1..=32).contains(&name.len()) && !name.starts_with('-') && name.bytes().all(plain);
    if ok {
        Ok(())
    } else {
        Err(Error::BadName(name.to_string()))
    }
}

// The session exactly: tmux matches a bare name by its start.
fn target(session: &str) -> String {
    format!("={session}")
}

/// What attaches a terminal to the session, making it first if `create`. It
/// starts the user's shell as a login shell, as tmux does. `-u` because guest
/// commands get no LANG, and tmux would otherwise draw lines as ASCII.
pub fn attach_argv(session: &str, create: bool) -> Vec<String> {
    let (verb, flag, name) = if create {
        ("new-session", "-As", session.to_string())
    } else {
        ("attach-session", "-t", target(session))
    };
    ["tmux", "-u", verb, flag, &name].map(String::from).to_vec()
}

/// Every session of `user`'s in the VM, by name. A VM that isn't running has
/// none, and isn't started to look.
pub fn list(store: &Store, vm: &str, user: &str) -> Result<Vec<Session>> {
    let args = ["list-sessions", "-F", LIST_FORMAT];
    Ok(match tmux(store, vm, user, &args, 64 << 10)? {
        Some(Ran::Ok(stdout)) => parse_list(&stdout),
        _ => Vec::new(),
    })
}

/// Ends the session and everything running in it.
pub fn kill(store: &Store, vm: &str, user: &str, session: &str) -> Result<()> {
    check_name(session)?;
    let args = ["kill-session", "-t", &target(session)];
    match tmux(store, vm, user, &args, 64 << 10)? {
        Some(Ran::Ok(_)) => Ok(()),
        _ => Err(no_session(vm, session)),
    }
}

/// The last `lines` of the session's active pane, reaching back into tmux's
/// history past what's on screen.
pub fn read(store: &Store, vm: &str, user: &str, session: &str, lines: u32) -> Result<Pane> {
    check_name(session)?;
    if !(1..=MAX_LINES).contains(&lines) {
        return Err(Error::BadLines(lines));
    }
    // The trailing colon makes it the session's current window, active pane.
    let pane = format!("{}:", target(session));
    let start = format!("-{lines}");
    let args = ["capture-pane", "-p", "-t", &pane, "-S", &start];
    match tmux(store, vm, user, &args, CAPTURE_CAP)? {
        Some(Ran::Ok(text)) => Ok(tail(&text, lines as usize, exec::OUTPUT_CAP)),
        _ => Err(no_session(vm, session)),
    }
}

/// Sessions from `tmux list-sessions -F LIST_FORMAT`. A line that doesn't
/// parse is left out.
pub fn parse_list(out: &str) -> Vec<Session> {
    out.split('\n').filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Session> {
    let (rest, name) = line.rsplit_once(':')?;
    let mut fields = rest.splitn(3, ':');
    let created: i64 = fields.next()?.parse().ok()?;
    let clients: u32 = fields.next()?.parse().ok()?;
    let command = fields.next()?;
    Some(Session {
        name: name.to_string(),
        command: command.to_string(),
        started: DateTime::from_timestamp(created, 0)?,
        attached: clients > 0,
    })
}

/// The last `lines` lines of a captured pane, after dropping the blank rows
/// under its cursor, in at most `cap` bytes. Past the cap, whole lines go from
/// the front; a single line over the cap keeps its end.
pub fn tail(pane: &str, lines: usize, cap: usize) -> Pane {
    let mut all: Vec<&str> = pane.split('\n').collect();
    while all.last().is_some_and(|l| l.is_empty()) {
        all.pop();
    }
    let kept = &all[all.len().saturating_sub(lines)..];
    let text: String = kept.iter().map(|l| format!("{l}\n")).collect();
    if text.len() <= cap {
        return Pane {
            text,
            truncated: false,
        };
    }
    let mut from = text.len() - cap;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    // Back to the start of a line, unless that's the last line, which keeps
    // what fits.
    let last = text[..text.len() - 1].rfind('\n').map_or(0, |i| i + 1);
    if from < last
        && let Some(nl) = text.as_bytes()[from - 1..].iter().position(|&b| b == b'\n')
    {
        from += nl;
    }
    Pane {
        text: text[from..].to_string(),
        truncated: true,
    }
}

// How a tmux command that didn't fail went. tmux says "no server running" or
// "error connecting" when the user has no sessions at all.
enum Ran {
    Ok(String),
    NoServer,
    NoSession,
}

// Runs tmux in a running VM; None if the VM isn't running.
fn tmux(store: &Store, vm: &str, user: &str, args: &[&str], cap: usize) -> Result<Option<Ran>> {
    let Some(conn) = exec::open_running(store, vm)? else {
        return Ok(None);
    };
    let req = Request {
        argv: ["tmux"].iter().chain(args).map(|s| s.to_string()).collect(),
        env: Vec::new(),
        cwd: None,
        user: Some(user.to_string()),
        tty: None,
    };
    let got = exec::capture(conn, &req, b"", TIMEOUT, cap)?;
    let err = got.stderr.trim();
    let why = match (got.exit_code, got.signal) {
        _ if got.timed_out => format!("{} didn't finish in {}s", args[0], TIMEOUT.as_secs()),
        (Some(0), _) if got.stdout_truncated => format!("{} printed over {cap} bytes", args[0]),
        (Some(0), _) => return Ok(Some(Ran::Ok(got.stdout))),
        _ if err.starts_with("no server running") || err.starts_with("error connecting") => {
            return Ok(Some(Ran::NoServer));
        }
        _ if err.starts_with("can't find session") => return Ok(Some(Ran::NoSession)),
        _ if !err.is_empty() => err.to_string(),
        (Some(code), _) => format!("{} exited {code}", args[0]),
        (None, sig) => format!("{} was killed by signal {}", args[0], sig.unwrap_or(0)),
    };
    Err(Error::Tmux {
        vm: vm.to_string(),
        why,
    })
}

fn no_session(vm: &str, session: &str) -> Error {
    Error::NoSession {
        vm: vm.to_string(),
        session: session.to_string(),
    }
}
