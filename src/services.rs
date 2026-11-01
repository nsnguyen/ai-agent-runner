// Services, like a sprite's: a command systemd keeps running in a VM, from
// now and every boot on, started again a second after it exits. `runner
// service add`, `list`, `logs` and `rm`, and MCP's add_service, list_services
// and remove_service. Each is a unit file of the runner's in the guest's
// /etc/systemd/system, written and driven by a short script that runs as
// root over the guest protocol (src/proto.rs). The disk keeps the unit, so a
// service is there at every boot, and sleep keeps its process.
//
// A unit runs its program directly, not through a shell, as the given user in
// the given directory, with systemd's environment: HOME, USER and the same
// PATH runner exec has, and no credentials, which would have to be on the disk.
use crate::exec::{self, Captured};
use crate::proto::Request;
use crate::store::Store;
use rmcp::schemars;
use serde::Serialize;
use std::time::Duration;

/// Where the units go in the guest.
pub const UNIT_DIR: &str = "/etc/systemd/system";
/// What every service's unit is named for, so `list` finds only the runner's.
pub const PREFIX: &str = "runner-svc-";

// Long enough for a stop's TimeoutStopSec and its kill, with room to spare.
const TIMEOUT: Duration = Duration::from_secs(60);
// A listed service is a few hundred bytes.
const CAP: usize = 1 << 20;
// What systemd reads at the start of ExecStart= as a prefix, not the program.
const PREFIXES: &[char] = &['-', '@', ':', '+', '!', '|'];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{0:?} isn't a service name: use up to 32 of A-Z, a-z, 0-9, _ and -, not starting with -"
    )]
    BadName(String),
    #[error("no command: give the program and its arguments")]
    NoCommand,
    #[error("{program:?} can't be a service's program: {why}")]
    BadProgram { program: String, why: &'static str },
    #[error("a service's command can't hold a NUL byte")]
    Nul,
    #[error("{dir:?} can't be a service's directory: {why}")]
    BadDir { dir: String, why: &'static str },
    #[error("{0:?} isn't a user name")]
    BadUser(String),
    #[error("{vm} already has a service named {name}: remove it first")]
    Exists { vm: String, name: String },
    #[error("no service {name} in {vm}")]
    NoService { vm: String, name: String },
    /// What add needs in the guest isn't there: the user or the directory.
    #[error("{why} in {vm}")]
    Missing { vm: String, why: String },
    #[error("{name} didn't start in {vm}, so it was removed:\n{why}")]
    NotStarted {
        vm: String,
        name: String,
        why: String,
    },
    #[error("services in {vm}: {why}")]
    Failed { vm: String, why: String },
    #[error(transparent)]
    Exec(#[from] exec::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A service to add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    /// The program and its arguments. A relative program with a / in it, like
    /// ./serve, is taken from `dir`; a bare name is looked up in PATH.
    pub argv: Vec<String>,
    /// Where it runs: absolute, and there in the guest.
    pub dir: String,
    /// Who it runs as.
    pub user: String,
}

/// A service in a VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Service {
    pub name: String,
    /// Its systemd unit in the VM, for `journalctl -u` and `systemctl status`.
    pub unit: String,
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// Where it runs.
    pub dir: String,
    /// Who it runs as.
    pub user: String,
    /// systemd's ActiveState: active, activating, deactivating, failed or inactive.
    pub state: String,
    /// systemd's SubState, like running, auto-restart, dead or failed.
    pub sub_state: String,
}

/// Where services' scripts run as root. `InVm` is the real one; tests run the
/// same scripts on their own host, on a directory of their own.
pub trait Guest {
    /// The VM's name, for messages.
    fn vm(&self) -> &str;
    /// Where the units go: UNIT_DIR in a VM.
    fn unit_dir(&self) -> String;
    /// Runs `argv` as root to its end, with `stdin`, then closed, as its input.
    fn run(&self, argv: &[String], stdin: &[u8]) -> exec::Result<Captured>;
}

/// A VM's guest, started first if the VM is stopped, or woken if it sleeps:
/// its services are on its disk, and only the running VM can read or change
/// them.
pub struct InVm<'a> {
    pub store: &'a Store,
    pub vm: &'a str,
}

impl Guest for InVm<'_> {
    fn vm(&self) -> &str {
        self.vm
    }

    fn unit_dir(&self) -> String {
        UNIT_DIR.to_string()
    }

    fn run(&self, argv: &[String], stdin: &[u8]) -> exec::Result<Captured> {
        // In use until the command is done.
        let exec::Conn {
            stream,
            in_use: _in_use,
        } = exec::open(self.store, self.vm)?;
        exec::capture(stream, &as_root(argv.to_vec()), stdin, TIMEOUT, CAP)
    }
}

/// A request that runs `argv` as root, the guest agent's own user.
pub fn as_root(argv: Vec<String>) -> Request {
    Request {
        argv,
        env: Vec::new(),
        cwd: None,
        user: None,
        tty: None,
    }
}

/// The unit a service runs as.
pub fn unit_name(name: &str) -> String {
    format!("{PREFIX}{name}.service")
}

/// Names `add` takes and the rest look up: sessions' rules, which make a
/// plain unit name.
pub fn check_name(name: &str) -> Result<()> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let ok = (1..=32).contains(&name.len()) && !name.starts_with('-') && name.bytes().all(plain);
    if ok {
        Ok(())
    } else {
        Err(Error::BadName(name.to_string()))
    }
}

/// The unit file for `spec`, refusing anything systemd would read some other
/// way than as given.
pub fn render_unit(spec: &Spec) -> Result<String> {
    Ok(prepare(spec)?.1)
}

// The service's directory, normalized, and its unit.
fn prepare(spec: &Spec) -> Result<(String, String)> {
    check_name(&spec.name)?;
    check_user(&spec.user)?;
    let dir = check_dir(&spec.dir)?;
    let (program, args) = spec.argv.split_first().ok_or(Error::NoCommand)?;
    if spec.argv.iter().any(|w| w.contains('\0')) {
        return Err(Error::Nul);
    }
    let program = check_program(program, &dir)?;
    let argv: Vec<String> = std::iter::once(program).chain(args.to_vec()).collect();
    let name = &spec.name;
    // StartLimitIntervalSec=0: systemd's start limit counts restarts, and
    // would give up on a service that crash-loops for 5 seconds, say a dev
    // server started before its dependencies are installed, until the next
    // boot. Type=exec: a start fails if the program can't be run, so add can
    // say so. TimeoutStopSec: a service that ignores SIGTERM can't hold the
    // VM's shutdown past the 30 seconds after which it's killed and its disk
    // rolled back.
    let unit = format!(
        "# Made by `runner service add {name}`; `runner service rm` removes it.
[Unit]
Description=runner service {name}
After=network.target
StartLimitIntervalSec=0

[Service]
Type=exec
User={user}
WorkingDirectory={dir}
ExecStart={exec}
Restart=always
RestartSec=1
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
",
        user = spec.user,
        dir = dir.replace('%', "%%"),
        exec = exec_start(&argv),
    );
    Ok((dir, unit))
}

fn check_user(user: &str) -> Result<()> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"_.-".contains(&b);
    let ok =
        (1..=32).contains(&user.len()) && !user.starts_with(['-', '.']) && user.bytes().all(plain);
    if ok {
        Ok(())
    } else {
        Err(Error::BadUser(user.to_string()))
    }
}

// An absolute path without `.` or empty parts. systemd fails a unit whose
// WorkingDirectory= has `..` in it, and strips the whitespace a line ends
// with. Quotes and backslashes it would take as they are, but they're left
// out, as in a program.
fn check_dir(dir: &str) -> Result<String> {
    let bad = |why| {
        Err(Error::BadDir {
            dir: dir.to_string(),
            why,
        })
    };
    if !dir.starts_with('/') {
        return bad("it has to be an absolute path");
    }
    if dir.chars().any(|c| c.is_control() || "\"'\\".contains(c)) {
        return bad("systemd won't take control characters, quotes or backslashes in one");
    }
    let parts: Vec<&str> = dir
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return bad("systemd won't take .. in one");
    }
    if parts.iter().any(|p| p.len() > 255) || dir.len() > 4095 {
        return bad("it's too long for a path");
    }
    let norm = format!("/{}", parts.join("/"));
    if norm.ends_with(char::is_whitespace) {
        return bad("systemd drops the whitespace it ends with");
    }
    Ok(norm)
}

// The program as ExecStart= runs it: a name looked up in PATH, or an absolute
// path, a relative one taken from `dir`. systemd takes no quotes, backslashes
// or control characters in it, replaces no variables, and does expand
// specifiers, so `$` and `%` are left out too, the directory's included. A
// first word of `;`, even quoted, separates commands.
fn check_program(program: &str, dir: &str) -> Result<String> {
    let bad = |why| {
        Err(Error::BadProgram {
            program: program.to_string(),
            why,
        })
    };
    if program.is_empty() {
        return bad("it's empty");
    }
    if program
        .chars()
        .any(|c| c.is_control() || "\"'\\$%".contains(c))
    {
        return bad("systemd won't take quotes, backslashes, $, % or control characters in one");
    }
    if !program.contains('/') {
        if program == "." || program == ".." {
            return bad("it's a directory");
        }
        if program == ";" {
            return bad("systemd reads it as the end of a command");
        }
        if program.starts_with(PREFIXES) {
            return bad("systemd reads a leading -, @, :, +, ! or | as a prefix");
        }
        if program.len() > 255 {
            return bad("it's too long for a file name");
        }
        return Ok(program.to_string());
    }
    let path = if program.starts_with('/') {
        program.to_string()
    } else {
        format!("{dir}/{program}")
    };
    if path.contains(['$', '%']) {
        return bad("systemd won't take $ or % in its path, which comes from the directory");
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    Ok(format!("/{}", parts.join("/")))
}

/// ExecStart='s value for `argv`: each word in double quotes, so systemd
/// takes it whole, with `\`, `"` and control characters escaped C's way, and
/// `$` and `%` doubled, so systemd replaces no variable and expands no
/// specifier in it.
pub fn exec_start(argv: &[String]) -> String {
    let words: Vec<String> = argv.iter().map(|w| quote(w)).collect();
    words.join(" ")
}

fn quote(word: &str) -> String {
    let mut q = String::with_capacity(word.len() + 2);
    q.push('"');
    for c in word.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            '\n' => q.push_str("\\n"),
            '\t' => q.push_str("\\t"),
            '$' => q.push_str("$$"),
            '%' => q.push_str("%%"),
            c if c.is_ascii_control() => q.push_str(&format!("\\x{:02x}", c as u32)),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// The words of an ExecStart= value `exec_start` made. None for one it
/// didn't, as a unit edited by hand may have.
pub fn parse_exec_start(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.chars();
    loop {
        if chars.next()? != '"' {
            return None;
        }
        let mut word = String::new();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    '\\' => word.push('\\'),
                    '"' => word.push('"'),
                    'n' => word.push('\n'),
                    't' => word.push('\t'),
                    'x' => {
                        let hex: String = [chars.next()?, chars.next()?].iter().collect();
                        let b = u8::from_str_radix(&hex, 16).ok().filter(u8::is_ascii)?;
                        word.push(b as char);
                    }
                    _ => return None,
                },
                c @ ('$' | '%') => {
                    if chars.next()? != c {
                        return None;
                    }
                    word.push(c);
                }
                c => word.push(c),
            }
        }
        words.push(word);
        match chars.next() {
            None => return Some(words),
            Some(' ') => {}
            Some(_) => return None,
        }
    }
}

// Each script runs as `sh -c SCRIPT sh ARGS...`, as root, its first argument
// the unit directory.

// $2 the unit, $3 the service, $4 its user, $5 its directory; the unit's text
// on stdin. Exits 3 if the name is taken, 4 if the user or directory isn't
// there, and 5, having removed the unit, with the journal's say on stderr,
// if it didn't start.
const ADD: &str = r#"d=$1 unit=$2 user=$4 dir=$5
f=$d/$unit
[ -e "$f" ] && exit 3
id -u -- "$user" >/dev/null 2>&1 || { echo "no user $user" >&2; exit 4; }
[ -d "$dir" ] || { echo "no directory $dir" >&2; exit 4; }
since=$(date +%s)
(set -C; cat > "$f") || exit 1
systemctl daemon-reload || { rm -f -- "$f"; exit 1; }
said=$(systemctl enable --now "$unit" 2>&1) && exit 0
journalctl --sync >/dev/null 2>&1
log=$(journalctl -u "$unit" --since "@$since" -n 20 --no-pager -o cat 2>/dev/null)
systemctl disable --now "$unit" >/dev/null 2>&1
rm -f -- "$f"
systemctl daemon-reload
systemctl reset-failed "$unit" >/dev/null 2>&1
printf '%s\n' "${log:-$said}" >&2
exit 5
"#;

// Each of the runner's units: Unit=, its state from systemd, and the lines
// add wrote that say what it runs, then a blank line.
const LIST: &str = r#"for f in "$1"/runner-svc-*.service; do
  [ -f "$f" ] || continue
  unit=${f##*/}
  echo "Unit=$unit"
  systemctl show -p ActiveState -p SubState "$unit"
  grep -E '^(User|WorkingDirectory|ExecStart)=' "$f"
  echo
done
exit 0
"#;

// $2 the unit. Exits 3 if it isn't there.
const REMOVE: &str = r#"f=$1/$2
[ -e "$f" ] || exit 3
systemctl disable --now "$2" >/dev/null 2>&1
rm -f -- "$f" || exit 1
systemctl daemon-reload || exit 1
systemctl reset-failed "$2" >/dev/null 2>&1
exit 0
"#;

// $2 the unit, $3 the VM, $4 the service, then journalctl's options.
const LOGS: &str = r#"[ -e "$1/$2" ] || { echo "runner: no service $4 in $3" >&2; exit 3; }
unit=$2
shift 4
exec journalctl -u "$unit" --no-pager "$@"
"#;

fn script(body: &str, args: &[&str]) -> Vec<String> {
    ["sh", "-c", body, "sh"]
        .iter()
        .chain(args)
        .map(|s| s.to_string())
        .collect()
}

/// Writes the service's unit, then enables and starts it. Returns once its
/// program has started, which a crash a moment later doesn't undo: `list`
/// and `logs` show that. A program that can't start leaves no unit behind,
/// and the error has the journal's last lines.
pub fn add(guest: &impl Guest, spec: &Spec) -> Result<Service> {
    let (dir, unit) = prepare(spec)?;
    let (vm, name) = (guest.vm(), spec.name.as_str());
    let (units, unit_file) = (guest.unit_dir(), unit_name(name));
    let args = [units.as_str(), &unit_file, name, &spec.user, &dir];
    let got = guest.run(&script(ADD, &args), unit.as_bytes())?;
    match exit_code(vm, "adding it", &got)? {
        0 => {}
        3 => return Err(exists(vm, name)),
        4 => {
            return Err(Error::Missing {
                vm: vm.to_string(),
                why: got.stderr.trim().to_string(),
            });
        }
        5 => {
            return Err(Error::NotStarted {
                vm: vm.to_string(),
                name: name.to_string(),
                why: got.stderr.trim().to_string(),
            });
        }
        code => return Err(failed(vm, "adding it", code, &got)),
    }
    let found = list(guest)?.into_iter().find(|s| s.name == name);
    found.ok_or_else(|| Error::Failed {
        vm: vm.to_string(),
        why: format!("{name} was added, but isn't listed"),
    })
}

/// The VM's services, by name.
pub fn list(guest: &impl Guest) -> Result<Vec<Service>> {
    let vm = guest.vm();
    let got = guest.run(&script(LIST, &[&guest.unit_dir()]), b"")?;
    match exit_code(vm, "listing them", &got)? {
        0 if got.stdout_truncated => Err(Error::Failed {
            vm: vm.to_string(),
            why: format!("the list ran over {CAP} bytes"),
        }),
        0 => Ok(parse_list(&got.stdout)),
        code => Err(failed(vm, "listing them", code, &got)),
    }
}

/// Stops the service and removes its unit, so no boot starts it again.
pub fn remove(guest: &impl Guest, name: &str) -> Result<()> {
    check_name(name)?;
    let vm = guest.vm();
    let (units, unit) = (guest.unit_dir(), unit_name(name));
    let got = guest.run(&script(REMOVE, &[&units, &unit]), b"")?;
    match exit_code(vm, "removing it", &got)? {
        0 => Ok(()),
        3 => Err(Error::NoService {
            vm: vm.to_string(),
            name: name.to_string(),
        }),
        code => Err(failed(vm, "removing it", code, &got)),
    }
}

/// What prints the service's log from the guest's journal, run as root: the
/// last `lines`, or all of it, and with `follow`, what comes after. It exits
/// 3, saying so, if there's no such service.
pub fn logs_argv(
    unit_dir: &str,
    vm: &str,
    name: &str,
    follow: bool,
    lines: Option<u32>,
) -> Result<Vec<String>> {
    check_name(name)?;
    let mut args = vec![unit_dir.to_string(), unit_name(name), vm.to_string()];
    args.push(name.to_string());
    args.extend(lines.map(|n| format!("--lines={n}")));
    args.extend(follow.then(|| "--follow".to_string()));
    Ok(script(
        LOGS,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    ))
}

/// Services from what LIST prints, by name. A block without its unit is
/// left out; a command add didn't write is one word, as it stands.
pub fn parse_list(out: &str) -> Vec<Service> {
    let mut all: Vec<Service> = out
        .split("\n\n")
        .filter_map(|block| {
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            };
            let unit = field("Unit")?;
            let name = unit.strip_prefix(PREFIX)?.strip_suffix(".service")?;
            let exec = field("ExecStart").unwrap_or_default();
            let unknown = || "unknown".to_string();
            Some(Service {
                name: name.to_string(),
                unit: unit.to_string(),
                argv: parse_exec_start(exec).unwrap_or_else(|| vec![exec.to_string()]),
                dir: field("WorkingDirectory")
                    .unwrap_or_default()
                    .replace("%%", "%"),
                user: field("User").unwrap_or_default().to_string(),
                state: field("ActiveState").map_or_else(unknown, String::from),
                sub_state: field("SubState").map_or_else(unknown, String::from),
            })
        })
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

/// A service on one line: name, state, directory and command, its words
/// quoted as a shell would need them.
pub fn row(s: &Service) -> String {
    let words: Vec<String> = s.argv.iter().map(|w| shell_word(w)).collect();
    let (state, sub, dir) = (&s.state, &s.sub_state, &s.dir);
    format!("{}  {state}/{sub}  {dir}  {}", s.name, words.join(" "))
}

fn shell_word(word: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn exists(vm: &str, name: &str) -> Error {
    Error::Exists {
        vm: vm.to_string(),
        name: name.to_string(),
    }
}

// The script's exit code; an error if it timed out or a signal ended it.
fn exit_code(vm: &str, doing: &str, got: &Captured) -> Result<i32> {
    let why = match (got.exit_code, got.signal) {
        _ if got.timed_out => format!("{doing} didn't finish in {}s", TIMEOUT.as_secs()),
        (Some(code), _) => return Ok(code),
        (None, sig) => format!("{doing} was killed by signal {}", sig.unwrap_or(0)),
    };
    Err(Error::Failed {
        vm: vm.to_string(),
        why,
    })
}

fn failed(vm: &str, doing: &str, code: i32, got: &Captured) -> Error {
    let err = got.stderr.trim();
    let why = if err.is_empty() {
        format!("{doing} exited {code}")
    } else {
        format!("{doing}: {err}")
    };
    Error::Failed {
        vm: vm.to_string(),
        why,
    }
}
