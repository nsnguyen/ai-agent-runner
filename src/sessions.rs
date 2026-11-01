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
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// What `tmux list-sessions -F` prints for each session. tmux keeps `:` and
/// `.` out of names, so the name goes last and the command can hold anything.
pub const LIST_FORMAT: &str =
    "#{session_created}:#{session_attached}:#{pane_current_command}:#{session_name}";
/// What prints a line for each terminal attached to a tmux session on any
/// user's tmux server in the guest, run as root, who can reach them all:
/// what a VM's boot asks before it sleeps the VM for want of use.
pub const ATTACHED: [&str; 3] = [
    "sh",
    "-c",
    r#"for s in "${TMUX_TMPDIR:-/tmp}"/tmux-*/*; do [ -S "$s" ] && tmux -S "$s" list-clients -F x 2>/dev/null; done; exit 0"#,
];
/// How many of a pane's last lines read_session returns unless told.
pub const DEFAULT_LINES: u32 = 100;
/// tmux keeps 2000 lines of history unless its config says otherwise.
pub const MAX_LINES: u32 = 2000;

// A tmux command that doesn't attach is quick; one that hangs is a stuck VM.
const TIMEOUT: Duration = Duration::from_secs(30);
// A boot holds off every command on its VM while it asks, so it asks briefly.
const ASK_WAIT: Duration = Duration::from_secs(5);
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

// tmux won't start on a TERM the guest has no terminfo for, as a newer
// terminal's own often is, where a plain shell only gets by. `-u` because guest
// commands get no LANG, and tmux would otherwise draw lines as ASCII.
const ON_A_KNOWN_TERM: &str =
    r#"infocmp "$TERM" >/dev/null 2>&1 || export TERM=xterm-256color; exec tmux -u "$@""#;

/// What attaches a terminal to the session, making it first if `create`. It
/// starts the user's shell as a login shell, as tmux does.
pub fn attach_argv(session: &str, create: bool) -> Vec<String> {
    let (verb, flag, name) = if create {
        ("new-session", "-As", session.to_string())
    } else {
        ("attach-session", "-t", target(session))
    };
    on_a_known_term(&[verb, flag, &name])
}

/// What attaches a terminal to the session, first making it to run `argv` in
/// `dir` if it isn't there. tmux decides, so two at once make one session.
/// With more than one word, `argv` runs as it is, with no shell to split it.
pub fn attach_or_run(session: &str, dir: &str, argv: &[String]) -> Vec<String> {
    let dir = literal(dir);
    let head = ["new-session", "-A", "-s", session, "-c", &dir, "--"];
    let mut all = on_a_known_term(&head);
    all.extend(argv.iter().map(|word| literal(word)));
    all
}

// tmux ends its command at a word ending in `;`, even past `--`, unless a `\`
// comes before that `;`, which tmux then drops.
fn literal(word: &str) -> String {
    match word.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => word.to_string(),
    }
}

fn on_a_known_term(args: &[&str]) -> Vec<String> {
    ["sh", "-c", ON_A_KNOWN_TERM, "sh"]
        .iter()
        .chain(args)
        .map(|s| s.to_string())
        .collect()
}

/// Whether the session is there. A VM that isn't running has none.
pub fn exists(store: &Store, vm: &str, user: &str, session: &str) -> Result<bool> {
    check_name(session)?;
    let args = ["has-session", "-t", &target(session)];
    Ok(tmux(store, vm, user, &args, 64 << 10)?.is_some())
}

/// Every session of `user`'s in the VM, by name. A VM that isn't running has
/// none, and isn't started to look.
pub fn list(store: &Store, vm: &str, user: &str) -> Result<Vec<Session>> {
    let args = ["list-sessions", "-F", LIST_FORMAT];
    let listed = tmux(store, vm, user, &args, 64 << 10)?;
    Ok(listed.map_or_else(Vec::new, |out| parse_list(&out)))
}

/// Whether a terminal is attached to any session in the guest at the other
/// end of `conn`, of any user's.
pub fn attached(conn: UnixStream) -> Result<bool> {
    let req = Request {
        argv: ATTACHED.map(String::from).to_vec(),
        env: Vec::new(),
        cwd: None,
        user: None,
        tty: None,
    };
    let got = exec::capture(conn, &req, b"", ASK_WAIT, 64 << 10)?;
    match (got.timed_out, got.exit_code) {
        (false, Some(0)) => Ok(!got.stdout.is_empty()),
        _ => Err(Error::Tmux {
            vm: "the guest".to_string(),
            why: format!(
                "asking whether a terminal is attached: {}",
                if got.timed_out {
                    format!("no answer in {}s", ASK_WAIT.as_secs())
                } else {
                    got.stderr.trim().to_string()
                }
            ),
        }),
    }
}

/// Ends the session and everything running in it.
pub fn kill(store: &Store, vm: &str, user: &str, session: &str) -> Result<()> {
    check_name(session)?;
    let args = ["kill-session", "-t", &target(session)];
    let killed = tmux(store, vm, user, &args, 64 << 10)?;
    killed.map(drop).ok_or_else(|| no_session(vm, session))
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
    let captured = tmux(store, vm, user, &args, CAPTURE_CAP)?;
    let pane = captured.map(|text| tail(&text, lines as usize, exec::OUTPUT_CAP));
    pane.ok_or_else(|| no_session(vm, session))
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

// Runs tmux in a running VM for its stdout. None if what it was asked about
// isn't there: the session, the user's tmux server, or the running VM.
fn tmux(store: &Store, vm: &str, user: &str, args: &[&str], cap: usize) -> Result<Option<String>> {
    let Some(conn) = exec::open_running(store, vm)? else {
        return Ok(None);
    };
    // In use until tmux is done.
    let exec::Conn {
        stream: conn,
        in_use: _in_use,
    } = conn;
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
        (Some(0), _) => return Ok(Some(got.stdout)),
        _ if nothing_there(err) => return Ok(None),
        _ if !err.is_empty() => err.to_string(),
        (Some(code), _) => format!("{} exited {code}", args[0]),
        (None, sig) => format!("{} was killed by signal {}", args[0], sig.unwrap_or(0)),
    };
    Err(Error::Tmux {
        vm: vm.to_string(),
        why,
    })
}

/// Whether tmux's error says the session, or the user's whole server, isn't
/// there. A server that's exiting as its last session ends says so in ways of
/// its own: it's gone, or has no sessions left to find.
pub fn nothing_there(stderr: &str) -> bool {
    let gone = [
        "no server running",
        "server exited unexpectedly",
        "lost server",
        "can't find session",
        "no current target",
    ];
    // Not "Permission denied": that's a server it can't reach, not none.
    let no_socket = stderr.starts_with("error connecting")
        && (stderr.ends_with("(No such file or directory)")
            || stderr.ends_with("(Connection refused)"));
    no_socket || gone.iter().any(|said| stderr.starts_with(said))
}

fn no_session(vm: &str, session: &str) -> Error {
    Error::NoSession {
        vm: vm.to_string(),
        session: session.to_string(),
    }
}
