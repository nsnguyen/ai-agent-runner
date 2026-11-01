// Headless runs: `runner run` starts Claude Code in a VM as `claude -p` with
// stream-json output, detached, and gives back the run's id at once; `runner
// output` and MCP's run_output read what it streams, by byte offset.
//
// Each run is driven by a process of its own, `runner drive`, in a session of
// its own with no terminal, so a dropped SSH connection or a closed laptop
// can't end it. It holds the VM's run lock, handed down by the command that
// started it, so a VM has one run at a time, and the kernel lets go of the
// lock if the process dies. It writes the stream to the VM's directory on the
// host, not to the VM's disk, so it can be read with the VM off and outlasts
// a crash of the VM:
//
//   <vm>/runs/lock          flock'd by the process driving the VM's run
//   <vm>/runs/<n>/ask.json  what was asked, written as the run starts
//   <vm>/runs/<n>/stream    claude's stdout, stream-json, as it came
//   <vm>/runs/<n>/stderr    claude's stderr
//   <vm>/runs/<n>/log       what the driving process itself printed
//   <vm>/runs/<n>/cancel    there once someone has asked to cancel the run,
//                           until Claude Code is done
//   <vm>/runs/<n>/result.json  what the runner measured once it was over
//   <vm>/runs/<n>/end.json  how it ended, written last
//
// A run on a stopped VM boots it, which runs a new disk generation as every
// boot does, and shuts it down at the end: a clean exit commits that
// generation, and anything else rolls it back. Rollback is the default: the
// run asks boot for it once the guest is up (Store::ask_rollback) and takes
// the ask back only when Claude Code has exited 0, so a VM shut down under
// the run, by a host reboot, `runner stop` or the agent itself, rolls back
// too. A run on a running VM joins it as it is, and commits nothing, but
// takes a checkpoint of it first (checkpoint.rs). A clean exit leaves the VM
// running on; anything else restores that checkpoint, which stops the VM and
// starts it again, ending whatever else ran in it.
//
// Once Claude Code is done, and a VM the run booted is down, the process
// driving the run measures what the run changed on its disk (changes.rs), in
// its own process, never `runner mcp`'s, since a disk a guest crafted could
// still take it down. The result (result.rs) keeps that apart from what the
// agent says it did, and is written just before the end, so a run that's
// over has its result. Measuring reads the run's disk whole, so the run looks
// to be running a while after Claude Code exits, until it's measured or a
// cancel stops the measuring. A cancel that stopped Claude Code doesn't: a
// cancelled run is measured too.
use crate::changes;
use crate::checkpoint;
use crate::exec::{self, Streamed};
use crate::firecracker::{self, Api};
use crate::idle::InUse;
use crate::proto::{Exit, Request};
use crate::result::{AgentSays, Changed, Outcome, RunResult};
use crate::store::{self, Booted, Store};
use crate::systemd::{self, State as VmState};
use chrono::{DateTime, SubsecRound, Utc};
use rmcp::schemars;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// What claude is run with, before `--resume`. The prompt goes on its stdin.
// The VM is the sandbox, and a headless run has no one to approve a tool.
pub const FLAGS: [&str; 5] = [
    "-p",
    "--verbose",
    "--output-format",
    "stream-json",
    "--dangerously-skip-permissions",
];
/// The most of a stream one run_output gives back.
pub const OUTPUT_CAP: usize = 64 * 1024;
/// The longest run_output waits for an event.
pub const MAX_WAIT: Duration = Duration::from_secs(45);

// How much of claude's stderr a finished run shows, from the end.
const STDERR_TAIL: usize = 4 * 1024;
// How much of its prompt a run shows.
const PROMPT_SHOWN: usize = 200;
// Claude Code's time to exit after SIGTERM, before SIGKILL.
const GRACE: Duration = Duration::from_secs(10);
// A guest that took Ctrl-Alt-Del is down within `runner stop`'s timeout.
const HALT_WAIT: Duration = Duration::from_secs(systemd::STOP_TIMEOUT_SECS as u64);
// Claude Code's grace, its kill, and the VM shutting down, with room to spare.
// Measuring a cancelled run's disk comes after, so on a big disk the run can
// still be running when this is up.
const CANCEL_WAIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(100);

const ASK: &str = "ask.json";
const STREAM: &str = "stream";
const STDERR: &str = "stderr";
const LOG: &str = "log";
const CANCEL: &str = "cancel";
const RESULT: &str = "result.json";
const END: &str = "end.json";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Exec(#[from] exec::Error),
    #[error("{0:?} isn't a run id: use the one `runner run` printed, like dev.1")]
    BadId(String),
    #[error("no run {0}")]
    NoRun(String),
    #[error("the prompt is empty")]
    NoPrompt,
    #[error("{vm} already has a run going, {run}: one run per VM; `runner cancel {run}` ends it")]
    Busy { vm: String, run: String },
    #[error("{0} already has a run starting: one run per VM")]
    Starting(String),
    #[error("{0} is shutting down: start the run once it's stopped")]
    Stopping(String),
    #[error("{0} stopped before the run could join it")]
    Gone(String),
    #[error("checkpointing {vm} before the run: {why}")]
    Checkpoint { vm: String, why: String },
    #[error("{id} has already ended: {how}")]
    Ended { id: String, how: String },
    #[error("{0} is still running: its result comes once it ends")]
    StillRunning(String),
    #[error("{id} has no result: {how}")]
    NoResult { id: String, how: String },
    #[error("{id}'s stream has only {len} bytes, so there's nothing at {from}")]
    PastEnd { id: String, len: u64, from: u64 },
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(what: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Claude Code is running, or the VM is booting for it or shutting down
    /// after it, or the runner is measuring what the run changed.
    Running,
    /// Claude Code exited: exit_code says how, or signal if one killed it.
    Exited,
    /// Cancelled before Claude Code exited on its own.
    Cancelled,
    /// It couldn't run, or lost its VM partway: error says why.
    Failed,
    /// The process driving it died before it could say how it ended.
    Lost,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            State::Running => "running",
            State::Exited => "exited",
            State::Cancelled => "cancelled",
            State::Failed => "failed",
            State::Lost => "lost",
        })
    }
}

/// A headless run, as `runs` lists it and run_agent and run_output give it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AgentRun {
    /// `<vm>.<n>`: what run_output and cancel_run take.
    pub id: String,
    pub vm: String,
    pub state: State,
    /// True if the VM was stopped, so the run boots it and shuts it down
    /// after; false if the run joined it running.
    pub boots: bool,
    pub started: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<DateTime<Utc>>,
    /// Claude Code's exit code, if it exited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that killed Claude Code, if one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// For a run that booted its VM, once the VM is down: whether the disk
    /// generation it ran was committed or rolled back. For one that joined
    /// it running: rolled back if it restored the checkpoint it took.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// What went wrong, if anything did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Untrusted: the end of what Claude Code wrote to stderr, once the run
    /// is over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// The stream's size so far, in bytes.
    pub bytes: u64,
    /// The prompt, cut at 200 characters.
    pub prompt: String,
}

// What was asked, as the command that started the run wrote it.
#[derive(Serialize, Deserialize)]
struct Ask {
    prompt: String,
    dir: String,
    resume: Option<String>,
    user: String,
    boots: bool,
    started: DateTime<Utc>,
}

// How the run ended, as the process driving it wrote it.
#[derive(Default, Serialize, Deserialize)]
struct End {
    ended: DateTime<Utc>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    cancelled: bool,
    outcome: Option<Outcome>,
    error: Option<String>,
}

impl End {
    fn state(&self) -> State {
        match self {
            End {
                cancelled: true, ..
            } => State::Cancelled,
            End {
                exit_code: None,
                signal: None,
                ..
            } => State::Failed,
            _ => State::Exited,
        }
    }
}

/// A run's id: the VM's name, a dot, and the run's number in that VM.
pub fn id(vm: &str, n: u64) -> String {
    format!("{vm}.{n}")
}

/// The VM's name and the run's number, from a run's id.
pub fn parse_id(id: &str) -> Result<(String, u64)> {
    let bad = || Error::BadId(id.to_string());
    let (vm, n) = id.rsplit_once('.').ok_or_else(bad)?;
    store::check_name(vm).map_err(|_| bad())?;
    let digits = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    if !digits || n.starts_with('0') {
        return Err(bad());
    }
    Ok((vm.to_string(), n.parse().map_err(|_| bad())?))
}

/// What `runner run` asks for.
pub struct Start<'a> {
    pub vm: &'a str,
    pub prompt: &'a str,
    /// Where in the VM claude runs.
    pub dir: &'a str,
    /// A Claude Code session to carry on.
    pub resume: Option<&'a str>,
    /// Who claude runs as.
    pub user: &'a str,
}

/// Starts a run and returns it at once, while `runner drive`, from `exe`,
/// takes it from there. Refused while the VM has a run going.
pub fn start(store: &Store, exe: &Path, s: &Start) -> Result<AgentRun> {
    if s.prompt.trim().is_empty() {
        return Err(Error::NoPrompt);
    }
    let runs = store.runs(s.vm)?;
    // An idle sleep holds the run lock while it lasts: wait it out, then
    // wake the VM.
    let _in_use = InUse::take(store, s.vm)?;
    let lock = hold(store, s.vm)?;
    let boots = match systemd::status(store, s.vm)?.state {
        VmState::Stopped => true,
        VmState::Booting | VmState::Running => false,
        // Joined awake, as a running VM is.
        VmState::Sleeping => {
            systemd::wake(store, s.vm)?;
            false
        }
        VmState::Stopping => return Err(Error::Stopping(s.vm.to_string())),
    };
    // Under the lock, so no other run takes the number.
    let n = newest(&runs)?.unwrap_or(0) + 1;
    let dir = runs.join(n.to_string());
    let made = |what: &str| io_err(format!("making {}", dir.join(what).display()));
    fs::create_dir(&dir).map_err(made(""))?;
    // Before the ask, so every run there is to read has a stream.
    File::create(dir.join(STREAM)).map_err(made(STREAM))?;
    let log = File::create(dir.join(LOG)).map_err(made(LOG))?;
    let ask = Ask {
        prompt: s.prompt.to_string(),
        dir: s.dir.to_string(),
        resume: s.resume.map(str::to_string),
        user: s.user.to_string(),
        boots,
        started: now(),
    };
    write_json(&dir, ASK, &ask)?;
    if let Err(e) = spawn(exe, store.root(), s.vm, n, lock, log) {
        let why = format!("couldn't start runner drive: {e}");
        let end = End {
            ended: now(),
            error: Some(why.clone()),
            ..End::default()
        };
        write_json(&dir, END, &end)?;
        return Err(io_err("starting runner drive")(e));
    }
    load(store, s.vm, n)
}

/// The VM's run lock, for as long as the file is open: no run starts on the
/// VM meanwhile. Refused while a run holds it.
pub fn hold(store: &Store, vm: &str) -> Result<File> {
    let runs = store.runs(vm)?;
    fs::create_dir_all(&runs).map_err(io_err(format!("making {}", runs.display())))?;
    if let Some(lock) = lock(&runs)? {
        return Ok(lock);
    }
    // Unless it's the newest, the run holding the lock is one starting.
    let newest = match newest(&runs)? {
        Some(n) => Some(load(store, vm, n)?),
        None => None,
    };
    let vm = vm.to_string();
    Err(match newest {
        Some(run) if run.state == State::Running => Error::Busy { vm, run: run.id },
        _ => Error::Starting(vm),
    })
}

// `runner drive` for run `n`, holding the run lock as its stdin, in a
// session of its own without a terminal, so nothing that ends the caller's
// session, like ssh hanging up, reaches it. Its output goes to the run's log,
// never the caller's, so `ssh host runner run` returns at once.
fn spawn(exe: &Path, root: &Path, vm: &str, n: u64, lock: File, log: File) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.arg("--root")
        .arg(std::path::absolute(root)?)
        .args(["drive", vm, &n.to_string()])
        .stdin(lock)
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe, so fine between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = cmd.spawn()?;
    // Reaped here, so a long-lived `runner mcp` gathers no zombies; a command
    // that exits first leaves that to init.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Drives run `n` of `vm` to its end: `runner drive`, which `start` spawns
/// with the VM's run lock as its stdin. `program` is claude.
pub fn drive(store: &Store, vm: &str, n: u64, program: &str) -> Result<()> {
    let began = Instant::now();
    let run_id = id(vm, n);
    let dir = store.runs(vm)?.join(n.to_string());
    let ask: Ask = read_json(&dir.join(ASK))?.ok_or_else(|| Error::NoRun(run_id.clone()))?;
    let mut gens = Gens::default();
    let ran = run_claude(store, vm, &run_id, &dir, &ask, program, &mut gens);
    let mut end = End::default();
    match ran {
        Ok(Streamed { exit, stopped }) => {
            end.cancelled = stopped;
            match exit {
                Some(Exit::Code(code)) => end.exit_code = Some(code),
                Some(Exit::Signal(sig)) => end.signal = Some(sig),
                None => {}
            }
        }
        Err(e) => end.error = Some(e.to_string()),
    }
    // Claude Code is done, so a cancel asked for by now was for it, even one
    // whose run then failed: only a cancel asked for after that stops the
    // measuring.
    let cancel = dir.join(CANCEL);
    match fs::remove_file(&cancel) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            eprintln!("runner: removing {}: {e}", cancel.display());
        }
        _ => {}
    }
    // A disk the VM may still be writing isn't measured.
    let mut down = true;
    if ask.boots {
        let clean = end.exit_code == Some(0) && !end.cancelled && end.error.is_none();
        if let Err(e) = shut_down(store, vm, &run_id, clean) {
            eprintln!("runner: {e}");
            end.error.get_or_insert(e.to_string());
            down = false;
        }
        // The generation the VM booted from stays current unless boot
        // committed the run's.
        match gens.from.map(|from| (from, store.get(vm))) {
            Some((from, Ok(after))) => {
                let committed = after.generation != from;
                end.outcome = Some(if committed {
                    Outcome::Committed
                } else {
                    Outcome::RolledBack
                });
                if clean && !committed {
                    end.error.get_or_insert(format!(
                        "Claude Code exited 0, but {vm} went down before the run ended, or \
                         didn't shut down cleanly, so its disk was rolled back: `runner logs \
                         {vm}` says which"
                    ));
                }
            }
            Some((_, Err(e))) => {
                end.error.get_or_insert(e.to_string());
            }
            None => {}
        }
    } else if let Some(id) = gens.checkpoint
        && gens.ran
        && !(end.exit_code == Some(0) && !end.cancelled && end.error.is_none())
    {
        // Under the run lock this process holds, so no run starts on the VM
        // while it's down.
        match checkpoint::restore_held(store, vm, id) {
            Ok(_) => end.outcome = Some(Outcome::RolledBack),
            Err(e) => {
                let why = format!("restoring checkpoint {id} failed: {e}");
                eprintln!("runner: {why}");
                end.error = Some(match end.error.take() {
                    Some(was) => format!("{was}; {why}"),
                    None => why,
                });
            }
        }
    }
    end.ended = now();
    let took = began.elapsed();
    let result = RunResult {
        exit_code: end.exit_code,
        duration_ms: took.as_millis().try_into().unwrap_or(u64::MAX),
        outcome: end.outcome,
        from_generation: gens.booted.map(|b| b.from).or(gens.from),
        generation: gens.booted.map(|b| b.generation),
        checkpoint: gens.checkpoint,
        changed: changed(store, vm, &ask, &gens, down, &|| cancel.exists()),
        agent_says: agent_says(&dir.join(STREAM)),
    };
    // Without its end, a run would look lost, how it ended and all.
    if let Err(e) = write_json(&dir, RESULT, &result) {
        eprintln!("runner: {e}");
    }
    write_json(&dir, END, &end)
}

// The generations of a run that boots its VM, as far as it got, or the
// checkpoint one that joined it took.
#[derive(Default)]
struct Gens {
    // Current once the VM started, and so unless boot commits the run's.
    from: Option<u64>,
    // What boot noted once its guest answered: what it cloned, and its own.
    booted: Option<Booted>,
    // Taken of a running VM before the run, to restore if the run fails.
    checkpoint: Option<u64>,
    // Whether claude started: a run cancelled or failed before then restores
    // nothing, since stopping the VM would end its sessions for nothing.
    ran: bool,
}

// What the run changed on its disk, measured from the disks, unless `stop`
// says to give up.
fn changed(
    store: &Store,
    vm: &str,
    ask: &Ask,
    gens: &Gens,
    down: bool,
    stop: &dyn Fn() -> bool,
) -> Changed {
    let unmeasured = |error: String| Changed::Unmeasured { error };
    if !ask.boots {
        return unmeasured(format!(
            "the run joined {vm} running, so there's no disk from before it to measure against"
        ));
    }
    let Some(booted) = gens.booted else {
        return unmeasured(format!(
            "{vm}'s guest never answered the run, so which generation it booted isn't known"
        ));
    };
    if !down {
        return unmeasured(format!(
            "{vm} may still be running on the run's disk, generation {}",
            booted.generation
        ));
    }
    let (before, after) = match (
        store.disk(vm, booted.from),
        store.disk(vm, booted.generation),
    ) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(e), _) | (_, Err(e)) => return unmeasured(e.to_string()),
    };
    // A disk a guest crafted that panics the reader fails the measuring, not
    // the run.
    let measured = panic::catch_unwind(AssertUnwindSafe(|| {
        changes::between_until(&before, &after, changes::CAP, changes::TIME, stop)
    }));
    match measured {
        Ok(measured) => measured.into(),
        Err(_) => unmeasured("measuring the disks panicked: the run's log says where".to_string()),
    }
}

// What the agent says it did, from its stream; nothing if that can't be read.
fn agent_says(stream: &Path) -> AgentSays {
    let read = File::open(stream).and_then(|f| AgentSays::from_stream(BufReader::new(f)));
    read.unwrap_or_else(|e| {
        eprintln!("runner: reading {}: {e}", stream.display());
        AgentSays::new(None)
    })
}

// Boots the VM if the run boots it, then runs claude there to its end, its
// stdout into the stream, noting a booted VM's generations in `gens`.
fn run_claude(
    store: &Store,
    vm: &str,
    run: &str,
    dir: &Path,
    ask: &Ask,
    program: &str,
    gens: &mut Gens,
) -> Result<Streamed> {
    let cancel = dir.join(CANCEL);
    let cancelled = || cancel.exists();
    let stopped = Streamed {
        exit: None,
        stopped: true,
    };
    if cancelled() {
        return Ok(stopped);
    }
    // A joined VM that has stopped since isn't started again: then the run
    // would have to shut it down, which nobody asked for.
    let joinable = ask.boots || systemd::status(store, vm)?.state != VmState::Stopped;
    if !joinable {
        return Err(Error::Gone(vm.to_string()));
    }
    if ask.boots {
        systemd::start(store, vm)?;
        gens.from = Some(store.get(vm)?.generation);
    }
    // A guest that answers means boot is past begin_run, which drops any
    // ask from before it and notes this boot's generations, and holds the VM
    // until it has shut down.
    let mut conn = exec::reach(store, vm)?;
    // Once the guest answers, a VM that was booting when the run joined it is
    // up, so it can be checkpointed. Then a fresh connection: the VM was
    // paused under this one.
    if !ask.boots {
        let made = checkpoint::checkpoint(store, vm, Some(&format!("before run {run}")));
        let made = made.map_err(|e| Error::Checkpoint {
            vm: vm.to_string(),
            why: e.to_string(),
        })?;
        gens.checkpoint = Some(made.id);
        conn = exec::reach(store, vm)?;
    }
    if ask.boots {
        gens.booted = store.booted(vm)?;
        store.ask_rollback(vm, &format!("run {run}"))?;
    }
    if cancelled() {
        return Ok(stopped);
    }
    let mut argv = vec![program.to_string()];
    argv.extend(FLAGS.map(String::from));
    if let Some(session) = &ask.resume {
        argv.extend(["--resume".to_string(), session.clone()]);
    }
    let req = Request {
        argv,
        env: Vec::new(),
        cwd: Some(ask.dir.clone()),
        user: Some(ask.user.clone()),
        tty: None,
    };
    let append = |name: &str| {
        let path = dir.join(name);
        let file = OpenOptions::new().append(true).create(true).open(&path);
        file.map_err(io_err(format!("opening {}", path.display())))
    };
    let (mut out, mut err) = (append(STREAM)?, append(STDERR)?);
    let prompt = ask.prompt.as_bytes();
    gens.ran = true;
    let streamed = exec::stream(conn, &req, prompt, &mut out, &mut err, cancelled, GRACE);
    // The guest couldn't start claude, so nothing ran.
    if let Err(exec::Error::Guest(_)) = &streamed {
        gens.ran = false;
    }
    let streamed = streamed?;
    // So a host crash after the end is written can't lose the stream's tail.
    out.sync_all().map_err(io_err("syncing the stream"))?;
    Ok(streamed)
}

// Shuts down a VM the run booted by Ctrl-Alt-Del straight to Firecracker, as
// the unit's ExecStop, `runner halt`, does. Only a clean run takes back its
// ask to roll back first. A guest still up after HALT_WAIT is stopped through
// systemd, which kills it past its timeout, so nothing commits.
fn shut_down(store: &Store, vm: &str, run: &str, clean: bool) -> Result<()> {
    if clean {
        store.withdraw_rollback(vm)?;
    } else {
        store.ask_rollback(vm, &format!("run {run}"))?;
    }
    let sock = store.api_socket(vm)?;
    match Api::new(&sock).ctrl_alt_del() {
        Ok(()) | Err(firecracker::Error::NotListening(_)) => {}
        // The guest may be on its way down anyway.
        Err(e) => eprintln!("runner: {e}"),
    }
    let deadline = Instant::now() + HALT_WAIT;
    // Boot holds the VM until it has committed or rolled back. With
    // Firecracker gone, boot is finishing, so that's worth waiting out.
    while store.held(vm)? {
        if Instant::now() > deadline && UnixStream::connect(&sock).is_ok() {
            systemd::stop(store, vm)?;
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
    // The unit still runs its ExecStop once boot has exited. Waited out, so a
    // run right after this one finds the VM stopped.
    let deadline = Instant::now() + HALT_WAIT;
    while systemd::status(store, vm)?.state != VmState::Stopped && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// A VM's runs, newest first.
pub fn list(store: &Store, vm: &str) -> Result<Vec<AgentRun>> {
    let runs = store.runs(vm)?;
    let mut all = Vec::new();
    for n in numbered(&runs)?.into_iter().rev() {
        match load(store, vm, n) {
            Ok(run) => all.push(run),
            // A start killed before it wrote the ask.
            Err(Error::NoRun(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(all)
}

/// The run with this id.
pub fn get(store: &Store, id: &str) -> Result<AgentRun> {
    let (vm, n) = parse_id(id)?;
    load(store, &vm, n)
}

/// What the runner measured of a run that's over, with what the agent says
/// kept apart. Refused while the run goes on.
pub fn result(store: &Store, id: &str) -> Result<RunResult> {
    let (vm, n) = parse_id(id)?;
    let run = load(store, &vm, n)?;
    if run.state == State::Running {
        return Err(Error::StillRunning(run.id));
    }
    let measured = read_result(store, &vm, n)?;
    measured.ok_or_else(|| Error::NoResult {
        how: describe(&run),
        id: run.id,
    })
}

// None for a run that's going, lost, or never got as far as running.
fn read_result(store: &Store, vm: &str, n: u64) -> Result<Option<RunResult>> {
    read_json(&store.runs(vm)?.join(n.to_string()).join(RESULT))
}

fn load(store: &Store, vm: &str, n: u64) -> Result<AgentRun> {
    let runs = store.runs(vm)?;
    let dir = runs.join(n.to_string());
    let run_id = id(vm, n);
    let ask: Ask = read_json(&dir.join(ASK))?.ok_or_else(|| Error::NoRun(run_id.clone()))?;
    let bytes = size(&dir.join(STREAM))?;
    let mut end: Option<End> = read_json(&dir.join(END))?;
    // The process writes the end before it lets go of the lock, so a run with
    // no end and no one driving it is lost, unless it ended since the read.
    let mut going = false;
    if end.is_none() {
        going = held(&runs, n)?;
        if !going {
            end = read_json(&dir.join(END))?;
        }
    }
    let state = match &end {
        Some(end) => end.state(),
        None if going => State::Running,
        None => State::Lost,
    };
    let stderr = match state {
        State::Running => None,
        _ => tail(&dir.join(STDERR), STDERR_TAIL)?,
    };
    let mut end = end.unwrap_or_default();
    if state == State::Lost {
        let log = dir.join(LOG);
        end.error = Some(format!(
            "the process driving it died: {} may say why",
            log.display()
        ));
    }
    let ended = (state != State::Running && state != State::Lost).then_some(end.ended);
    Ok(AgentRun {
        id: run_id,
        vm: vm.to_string(),
        state,
        boots: ask.boots,
        started: ask.started,
        ended,
        exit_code: end.exit_code,
        signal: end.signal,
        outcome: end.outcome,
        error: end.error,
        stderr,
        bytes,
        prompt: shown(&ask.prompt),
    })
}

// Whether run `n` is the newest and something holds the run lock: the
// process driving it, or the command starting it.
fn held(runs: &Path, n: u64) -> Result<bool> {
    if newest(runs)? != Some(n) {
        return Ok(false);
    }
    let path = runs.join("lock");
    let file = match File::open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        r => r.map_err(io_err(format!("opening {}", path.display())))?,
    };
    // Asked once: a lock just let go can look held for an instant
    // (store::try_lock), and then the run looks to be going a moment longer.
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(io_err(format!("locking {}", path.display()))(e)),
    }
}

// The run lock, or None if a run holds it.
fn lock(runs: &Path) -> Result<Option<File>> {
    let path = runs.join("lock");
    let what = || format!("locking {}", path.display());
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_err(what()))?;
    let took = store::try_lock(&file).map_err(io_err(what()))?;
    Ok(took.then_some(file))
}

/// Copies the run's stream from byte `from` on to `out`: its whole lines,
/// and with `follow`, more as they come until the run is over. Returns the
/// run as it stood at the last read.
pub fn output(
    store: &Store,
    id: &str,
    from: u64,
    follow: bool,
    out: &mut impl Write,
) -> Result<AgentRun> {
    let (vm, n) = parse_id(id)?;
    let path = store.runs(&vm)?.join(n.to_string()).join(STREAM);
    let what = || format!("reading {}", path.display());
    let mut at = from;
    loop {
        let run = load(store, &vm, n)?;
        if at > run.bytes {
            return Err(past_end(&run, at));
        }
        let ended = run.state != State::Running;
        let mut file = File::open(&path).map_err(io_err(what()))?;
        let end = readable_end(&mut file, at, ended).map_err(io_err(what()))?;
        file.seek(SeekFrom::Start(at)).map_err(io_err(what()))?;
        io::copy(&mut file.take(end - at), out)
            .and_then(|_| out.flush())
            .map_err(io_err("writing the stream"))?;
        at = end;
        if ended || !follow {
            return Ok(run);
        }
        std::thread::sleep(POLL);
    }
}

/// What one run_output gives back.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Events {
    pub run: AgentRun,
    /// Untrusted: the run's stream-json events from the cursor on, in order,
    /// as Claude Code wrote them. A line that isn't JSON comes as a string,
    /// and one longer than 64 KiB as {"runner_cut": true, "bytes": n} in its
    /// place.
    pub events: Vec<Value>,
    /// Where the next read starts: pass it back as the cursor.
    pub cursor: u64,
    /// True if more is there already, past what fit in this reply.
    pub more: bool,
    /// Once the run is over, in the reply that reaches the end of its stream:
    /// what the runner measured, with what the agent says kept apart, as
    /// `runner result` gives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RunResult>,
}

/// The run's events from byte `cursor` on, as many as fit in OUTPUT_CAP.
/// While the run goes on with nothing new, waits up to `wait` (MAX_WAIT at
/// most) for something.
pub fn events(store: &Store, id: &str, cursor: u64, wait: Duration) -> Result<Events> {
    let (vm, n) = parse_id(id)?;
    let path = store.runs(&vm)?.join(n.to_string()).join(STREAM);
    let deadline = Instant::now() + wait.min(MAX_WAIT);
    loop {
        let run = load(store, &vm, n)?;
        if cursor > run.bytes {
            return Err(past_end(&run, cursor));
        }
        let ended = run.state != State::Running;
        let got = lines(&path, cursor, OUTPUT_CAP, ended)
            .map_err(io_err(format!("reading {}", path.display())))?;
        if !got.lines.is_empty() || ended || Instant::now() >= deadline {
            let more = got.next < got.end;
            // A result that can't be read mustn't cost the events.
            let result = if ended && !more {
                read_result(store, &vm, n).unwrap_or_else(|e| {
                    eprintln!("runner: {e}");
                    None
                })
            } else {
                None
            };
            return Ok(Events {
                run,
                events: got.lines.iter().filter_map(event).collect(),
                cursor: got.next,
                more,
                result,
            });
        }
        std::thread::sleep(POLL);
    }
}

fn past_end(run: &AgentRun, from: u64) -> Error {
    Error::PastEnd {
        id: run.id.clone(),
        len: run.bytes,
        from,
    }
}

// A blank line is no event.
fn event(line: &Line) -> Option<Value> {
    match line {
        Line::Cut(bytes) => Some(json!({"runner_cut": true, "bytes": bytes})),
        Line::Whole(b) => {
            let b = b.strip_suffix(b"\n").unwrap_or(b);
            if b.iter().all(u8::is_ascii_whitespace) {
                return None;
            }
            let text = || Value::String(String::from_utf8_lossy(b).into_owned());
            Some(serde_json::from_slice(b).unwrap_or_else(|_| text()))
        }
    }
}

/// Asks the run to stop, and returns it once it has ended, or as it stands
/// if it's still going after two minutes.
pub fn cancel(store: &Store, id: &str) -> Result<AgentRun> {
    let (vm, n) = parse_id(id)?;
    let run = load(store, &vm, n)?;
    if run.state != State::Running {
        let how = describe(&run);
        return Err(Error::Ended { id: run.id, how });
    }
    let path = store.runs(&vm)?.join(n.to_string()).join(CANCEL);
    fs::write(&path, "").map_err(io_err(format!("writing {}", path.display())))?;
    let deadline = Instant::now() + CANCEL_WAIT;
    loop {
        let run = load(store, &vm, n)?;
        if run.state != State::Running || Instant::now() > deadline {
            return Ok(run);
        }
        std::thread::sleep(POLL);
    }
}

/// How a run stands, in a few words, like `exited 0, committed`. Once it's
/// over: what became of the disk, and what went wrong, if anything did.
pub fn describe(run: &AgentRun) -> String {
    let state = match (run.state, run.exit_code, run.signal) {
        (State::Running, ..) => return State::Running.to_string(),
        // Nothing is known of the disk, nor ever will be.
        (State::Lost, ..) => "lost".to_string(),
        (State::Exited, Some(code), _) => format!("exited {code}"),
        (State::Exited, None, Some(sig)) => format!("killed by signal {sig}"),
        (state, ..) => state.to_string(),
    };
    let disk = match (run.outcome, run.boots, run.state) {
        (_, _, State::Lost) => "",
        (Some(Outcome::Committed), ..) => ", committed",
        (Some(Outcome::RolledBack), ..) => ", rolled back",
        (None, false, _) => ", joined",
        (None, true, _) => "",
    };
    let error = run.error.as_deref().map(|e| {
        let lines: Vec<&str> = e.lines().collect();
        format!(": {}", lines.join(" "))
    });
    format!("{state}{disk}{}", error.unwrap_or_default())
}

/// A line of a run's stream, as a read gives it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// The line, with its newline, unless it's the last of a run that ended
    /// without one.
    Whole(Vec<u8>),
    /// A line too long for the read, left out: how many bytes it took.
    Cut(u64),
}

/// One read of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lines {
    pub lines: Vec<Line>,
    /// Where the next read starts: just past the last line here.
    pub next: u64,
    /// Where what's readable now ends; past `next` if `cap` stopped the read.
    pub end: u64,
}

/// Reads whole lines of the stream at `path` from byte `from` on, as many as
/// fit in `cap` bytes; a first line longer than that comes alone, as `Cut`. A
/// line without its newline yet is left for a later read, unless the run has
/// `ended` and nothing will finish it.
pub fn lines(path: &Path, from: u64, cap: usize, ended: bool) -> io::Result<Lines> {
    let mut file = File::open(path)?;
    let end = readable_end(&mut file, from, ended)?;
    file.seek(SeekFrom::Start(from))?;
    let mut r = BufReader::new(file.take(end - from));
    let (mut lines, mut next, mut used) = (Vec::new(), from, 0);
    while next < end {
        let mut line = Vec::new();
        let got = (&mut r)
            .take((cap - used) as u64)
            .read_until(b'\n', &mut line)?;
        if got > 0 && (line.ends_with(b"\n") || next + got as u64 == end) {
            used += got;
            next += got as u64;
            lines.push(Line::Whole(line));
            continue;
        }
        // Longer than there's room for: alone and cut, or left for the next.
        if lines.is_empty() {
            let rest = r.skip_until(b'\n')?;
            let bytes = (got + rest) as u64;
            next += bytes;
            lines.push(Line::Cut(bytes));
        }
        break;
    }
    Ok(Lines { lines, next, end })
}

// Where the whole lines past `from` end: just after the stream's last
// newline, or at its end once the run is over and nothing will finish its
// last line. Read back from the end, a block at a time.
fn readable_end(file: &mut File, from: u64, ended: bool) -> io::Result<u64> {
    let len = file.metadata()?.len();
    if from > len {
        let why = format!("the stream has only {len} bytes, so there's nothing at {from}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
    }
    if ended {
        return Ok(len);
    }
    let mut buf = vec![0; 64 * 1024];
    let mut at = len;
    while at > from {
        let size = ((at - from) as usize).min(buf.len());
        let start = at - size as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..size])?;
        if let Some(i) = buf[..size].iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        at = start;
    }
    Ok(from)
}

// The last `cap` bytes of a file as text, from a character's start; None if
// it's empty or missing.
fn tail(path: &Path, cap: usize) -> Result<Option<String>> {
    let what = || format!("reading {}", path.display());
    let mut file = match File::open(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(io_err(what()))?,
    };
    let len = file.metadata().map_err(io_err(what()))?.len();
    let start = len.saturating_sub(cap as u64);
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(start))
        .and_then(|_| file.read_to_end(&mut bytes))
        .map_err(io_err(what()))?;
    // A character the cut split in two goes.
    let skip = bytes.iter().take_while(|&&b| b & 0xC0 == 0x80).count();
    let text = String::from_utf8_lossy(&bytes[skip..]).into_owned();
    Ok((!text.is_empty()).then_some(text))
}

fn shown(prompt: &str) -> String {
    match prompt.char_indices().nth(PROMPT_SHOWN) {
        Some((i, _)) => format!("{}…", &prompt[..i]),
        None => prompt.to_string(),
    }
}

fn size(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        r => Ok(r
            .map_err(io_err(format!("reading {}", path.display())))?
            .len()),
    }
}

// The runs on hand, by number, lowest first.
fn numbered(runs: &Path) -> Result<Vec<u64>> {
    let what = || format!("reading {}", runs.display());
    let entries = match fs::read_dir(runs) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        r => r.map_err(io_err(what()))?,
    };
    let mut all = Vec::new();
    for entry in entries {
        let name = entry.map_err(io_err(what()))?.file_name();
        all.extend(name.to_str().and_then(|n| n.parse::<u64>().ok()));
    }
    all.sort_unstable();
    Ok(all)
}

fn newest(runs: &Path) -> Result<Option<u64>> {
    Ok(numbered(runs)?.last().copied())
}

// None if there's no file.
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let what = || format!("reading {}", path.display());
    let bytes = match fs::read(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(io_err(what()))?,
    };
    let parsed = serde_json::from_slice(&bytes).map_err(|e| io_err(what())(e.into()))?;
    Ok(Some(parsed))
}

fn write_json(dir: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let text =
        serde_json::to_string(value).map_err(|e| io_err(format!("writing {name}"))(e.into()))?;
    Ok(store::write_aside(dir, name, &text)?)
}

fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(0)
}
