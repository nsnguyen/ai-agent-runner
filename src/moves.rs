// `runner move <vm> <host>`: a VM to another host, with everything its
// export carries (export.rs): its disk, the owner's checkpoints, its
// settings and services, and its allowlist, which a move keeps, since the
// move is the owner's own and not a stream from elsewhere. It keeps its
// name; the other host picks its slot, so its address and URL port change.
//
// On the VM's host, holding the VM's run lock so no run starts: the other
// host is asked, through its MCP as peers::Remote reaches it, whether it
// has a VM by that name. Then a VM that's up is stopped, which a clean
// shutdown commits, and, holding the VM's lock until the delete so nothing
// starts it, its export streams into `ssh <host> runner import <vm> -
// --creds …`, with this host's own key, which that host's `runner peer`
// lets through (peers.rs). The bytes go host to host, never through the
// laptop. Only once that import has exited 0 is the VM deleted here; then
// one that was up is started there, awake.
//
// An import builds its VM aside and renames it in last (import.rs), so one
// that fails leaves nothing there, and the VM here is as it was, started
// again if it was up. So does anything else that goes wrong once it's
// stopped, but a stop that wasn't clean: what the VM wrote since it booted
// wasn't committed, so it stays here, stopped, for a look.
//
// It all runs in one thread, with a runtime of its own for the other host's
// MCP, so a move once begun goes to its end: `runner mcp` waits for it as
// it shuts down, and `runner move` ignores the terminal going away.
use crate::export::{self, Exported};
use crate::idle::InUse;
use crate::peers::{self, Remote};
use crate::runs;
use crate::store::{self, Disks, Locked, Store, Vm};
use crate::systemd::{self, State, VmStatus};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Where a VM went, as move_vm returns it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Moved {
    /// It there, as that host's vm_status gives it: a new address and URL.
    #[serde(flatten)]
    pub status: VmStatus,
    /// The host it's on now, as list_hosts names it.
    pub host: String,
    /// How many of the owner's checkpoints went with it.
    pub checkpoints: usize,
    /// The export's length in bytes, which went host to host.
    pub sent: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error(transparent)]
    Runs(#[from] runs::Error),
    #[error("{0}")]
    BadHost(String),
    #[error("{vm} is on {host} already")]
    There { vm: String, host: String },
    #[error("run {run} is going in {vm}: move it once the run is over")]
    RunGoing { vm: String, run: String },
    #[error(
        "something holds {0}'s run lock: a run starting, a restore, a sleep or an \
         export. Move it once that's over"
    )]
    RunStarting(String),
    #[error("{host} has a VM named {vm} already: VM names are one per name across hosts")]
    Taken { vm: String, host: String },
    /// The other host didn't say what it has.
    #[error("{0}")]
    Unasked(String),
    #[error("{vm} is {state}: move it once it's running or stopped")]
    NotNow { vm: String, state: State },
    #[error(
        "{vm} didn't shut down cleanly, so what it wrote since it booted wasn't \
         committed, and it stays here, stopped{kept}. `runner logs {vm}` says why"
    )]
    Unclean {
        vm: String,
        /// Where what it wrote is, if its boot said.
        kept: String,
    },
    #[error("{vm} is here as it was{again}: {why}")]
    Failed {
        vm: String,
        why: String,
        /// How a VM that was up came back, if it was.
        again: String,
    },
    #[error(
        "{vm} is on {host} now, but deleting it here failed, so both hosts have it: \
         {source}. `runner delete {vm}` here takes this one away"
    )]
    NotDeleted {
        vm: String,
        host: String,
        source: store::Error,
    },
    #[error("{vm} is on {host} now, but didn't start there: {why}")]
    NotStarted {
        vm: String,
        host: String,
        why: String,
    },
    #[error("{vm} is on {host} now, but {host} didn't say how it is: {why}")]
    Unseen {
        vm: String,
        host: String,
        why: String,
    },
    #[error("starting the move's runtime: {0}")]
    Runtime(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How long the other host gets to list its VMs, past its dial
/// (peers::HELLO), before the move is refused.
const ASK_WAIT: Duration = Duration::from_secs(20);

/// Moves `name` from this host, which MCP calls `here`, to `target`, a host
/// as ssh takes it, whose MCP runs exec as `user`. Refused before anything
/// stops while a run is going, when `target` is this host or has a VM by
/// that name, or doesn't answer. Blocks until it's done, so call it off an
/// async runtime.
pub fn move_vm(store: &Store, name: &str, target: &str, user: &str, here: &str) -> Result<Moved> {
    peers::check_host(target).map_err(Error::BadHost)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::Runtime)?;
    // Its session ends, and is dropped, within the runtime.
    let _in = rt.enter();
    let to = Remote::new(target, user);
    let (vm, host) = (name.to_string(), to.name.clone());
    if to.is(here) {
        return Err(Error::There { vm, host });
    }
    let held = hold(store, name)?;
    // Before anything is stopped. A VM made there since fails the import,
    // which leaves this one as it was.
    let listed = rt.block_on(ask(&to)).map_err(Error::Unasked)?;
    if has(listed, name).map_err(Error::Unasked)? {
        return Err(Error::Taken { vm, host });
    }
    let Sent { up, locked, sent } = send(store, name, target, &host)?;
    let sent = match sent {
        Err(Unsent {
            why,
            whole: Some(exported),
        }) => rt.block_on(lost(&to, name, why, *exported)),
        sent => sent.map_err(|u| u.why),
    };
    let exported = settle(store, name, &host, up, locked, sent)?;
    drop(held);
    let args = json!({ "name": name });
    let tool = if up { "start_vm" } else { "vm_status" };
    let status = rt.block_on(to.answer(tool, args)).and_then(status_of);
    let status = status.map_err(|why| match up {
        true => Error::NotStarted {
            vm: vm.clone(),
            host: host.clone(),
            why,
        },
        false => Error::Unseen {
            vm: vm.clone(),
            host: host.clone(),
            why,
        },
    })?;
    Ok(Moved {
        status,
        host,
        checkpoints: exported.manifest.checkpoints.len(),
        sent: exported.bytes,
    })
}

// What a move holds of the VM here from the first look to the end: its use,
// so it doesn't idle-sleep, and its run lock, so no run starts.
struct Held {
    _in_use: InUse,
    _no_run: File,
}

fn hold(store: &Store, name: &str) -> Result<Held> {
    store.get(name)?;
    // An idle sleep holds the run lock while it lasts: wait it out.
    let in_use = InUse::take(store, name)?;
    let no_run = runs::hold(store, name).map_err(|e| match e {
        runs::Error::Busy { vm, run } => Error::RunGoing { vm, run },
        runs::Error::Starting(vm) => Error::RunStarting(vm),
        e => e.into(),
    })?;
    Ok(Held {
        _in_use: in_use,
        _no_run: no_run,
    })
}

// The other host's VMs, within ASK_WAIT.
async fn ask(to: &Remote) -> std::result::Result<Value, String> {
    match tokio::time::timeout(ASK_WAIT, to.answer("list_vms", json!({}))).await {
        Ok(answer) => answer,
        Err(_) => Err(format!(
            "{} didn't answer in {} seconds",
            to.name,
            ASK_WAIT.as_secs()
        )),
    }
}

// Whether a list_vms answer names `vm`.
fn has(listed: Value, vm: &str) -> std::result::Result<bool, String> {
    #[derive(Deserialize)]
    struct Listed {
        vms: Vec<Vm>,
    }
    let listed: Listed =
        serde_json::from_value(listed).map_err(|e| format!("its list_vms gave {e}"))?;
    Ok(listed.vms.iter().any(|v| v.name == vm))
}

fn status_of(answer: Value) -> std::result::Result<VmStatus, String> {
    serde_json::from_value(answer).map_err(|e| format!("it gave {e}"))
}

// What send did: whether the VM was up, its lock, still held, and whether
// the import there took it.
struct Sent {
    up: bool,
    locked: Locked,
    sent: std::result::Result<Exported, Unsent>,
}

// Why an import didn't take, and the export, if the whole of it went out
// before ssh failed, so the import may have taken it after all.
struct Unsent {
    why: String,
    whole: Option<Box<Exported>>,
}

// The VM, stopped if it's up, streamed to `target` under its lock, which it
// keeps holding.
fn send(store: &Store, name: &str, target: &str, host: &str) -> Result<Sent> {
    let vm = name.to_string();
    // Before the look, so a stop that commits as it's looked at counts.
    let before = store.get(name)?.generation;
    let state = systemd::status(store, name)?.state;
    // A stopping VM was asked to stop, so it stays down, as a restore has it.
    let up = match state {
        State::Stopped | State::Stopping => false,
        State::Running | State::Sleeping => true,
        state => return Err(Error::NotNow { vm, state }),
    };
    if state != State::Stopped {
        // A sleeping VM is woken to shut down (boot::halt).
        systemd::stop(store, name)?;
        // A clean shutdown commits the disk the boot ran, which is then
        // current; one killed at the stop's timeout, or a crash, leaves it
        // aside, and the current disk is older than what the VM ran.
        if store.get(name)?.generation == before {
            let booted = store.booted(name).ok().flatten();
            let kept = booted.map_or(String::new(), |b| {
                format!(", with that in generation {} for a look", b.generation)
            });
            return Err(Error::Unclean { vm, kept });
        }
    }
    let locked = store.lock(name).map_err(|e| back(store, name, up, e))?;
    let disks = match locked.open_disks(true) {
        Ok(disks) => disks,
        Err(e) => {
            drop(locked);
            return Err(back(store, name, up, e));
        }
    };
    let sent = stream(name, disks, target, host);
    Ok(Sent { up, locked, sent })
}

// Whether the import there took the VM, when the whole export went out but
// ssh failed before it said: the other host has the VM now if it did, since
// an import renames it in last and the name was free. Asked a few times, as
// an import whose connection dropped takes a moment to finish.
async fn lost(
    to: &Remote,
    name: &str,
    why: String,
    exported: Exported,
) -> std::result::Result<Exported, String> {
    let mut said = None;
    for wait in LOST {
        tokio::time::sleep(wait).await;
        match ask(to).await.and_then(|listed| has(listed, name)) {
            Ok(true) => return Ok(exported),
            Ok(false) => said = Some(format!("{} doesn't have it", to.name)),
            Err(e) => said = said.or(Some(e)),
        }
    }
    let host = &to.name;
    Err(match said {
        Some(said) => format!("{why}, and {said}"),
        None => format!(
            "{why}, and {host} didn't say whether it took {name}: if `runner list` \
             shows it there too, delete one"
        ),
    })
}

/// When `lost` asks: at once, then twice more as an import finishes.
const LOST: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(1),
    Duration::from_secs(3),
];

// The VM deleted here, if the import there took it; else, its lock let go,
// left as it was, and started again if it was up.
fn settle(
    store: &Store,
    name: &str,
    host: &str,
    up: bool,
    locked: Locked,
    sent: std::result::Result<Exported, String>,
) -> Result<Exported> {
    match sent {
        Ok(exported) => {
            let source = |source| Error::NotDeleted {
                vm: name.to_string(),
                host: host.to_string(),
                source,
            };
            locked.delete().map_err(source)?;
            Ok(exported)
        }
        Err(why) => {
            drop(locked);
            Err(back(store, name, up, why))
        }
    }
}

// A move that stopped short, once the VM was down: started again if it was
// up, and why.
fn back(store: &Store, name: &str, up: bool, why: impl ToString) -> Error {
    let again = match up {
        false => String::new(),
        true => match systemd::start(store, name) {
            Ok(status) => format!(", and {} again", status.state),
            Err(e) => format!(", but didn't start again ({e})"),
        },
    };
    Error::Failed {
        vm: name.to_string(),
        why: why.to_string(),
        again,
    }
}

// `disks`, as `name`'s export, into `runner import` on `target`, which MCP
// calls `host`, over ssh with this host's own key. What it says on stderr
// is passed on under its name. Err says why it didn't take: what it said
// last, or why the export failed here.
fn stream(
    name: &str,
    disks: Disks,
    target: &str,
    host: &str,
) -> std::result::Result<Exported, Unsent> {
    let unsent = |why| Unsent { why, whole: None };
    let creds: Vec<&str> = disks.creds.iter().map(|c| c.name()).collect();
    let mut cmd = Command::new("ssh");
    cmd.args(peers::SSH)
        .arg(target)
        .args(["runner", "import", name, "-"]);
    // An empty allowlist stays empty, not the default.
    match creds.is_empty() {
        true => cmd.arg("--no-creds"),
        false => cmd.args(["--creds", &creds.join(",")]),
    };
    // Its stdout is import's line, which this host's runner says better;
    // under MCP, stdout here is the protocol.
    let mut ssh = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| unsent(format!("running ssh: {e}")))?;
    let (Some(mut stdin), Some(stderr)) = (ssh.stdin.take(), ssh.stderr.take()) else {
        let _ = ssh.kill();
        let _ = ssh.wait();
        return Err(unsent("ssh came without its pipes".into()));
    };
    let said = relay(stderr, host.to_string());
    let written = export::write(name, disks, &mut stdin);
    // Its end, or what an import there reads as the stream cut short.
    drop(stdin);
    let exited = ssh.wait();
    let last = said.join().ok().flatten();
    let theirs = |code| match &last {
        Some(last) => format!("{host}'s import failed: {last}"),
        None => format!("{host}'s import failed: ssh exited {code}"),
    };
    match (written, exited) {
        (_, Err(e)) => Err(unsent(format!("waiting for ssh: {e}"))),
        (Ok(exported), Ok(code)) if code.success() => Ok(exported),
        // One that hung up first broke the pipe: why is theirs to say.
        (Err(export::Error::Stream { source, .. }), Ok(code))
            if source.kind() == io::ErrorKind::BrokenPipe =>
        {
            Err(unsent(theirs(code)))
        }
        (Err(e), Ok(_)) => Err(unsent(e.to_string())),
        // ssh's own failure, past the whole stream: the import there may
        // have finished. A runner that fails exits 1, or 2 for its usage.
        (Ok(exported), Ok(code)) if code.code() == Some(255) => Err(Unsent {
            why: theirs(code),
            whole: Some(Box::new(exported)),
        }),
        (Ok(_), Ok(code)) => Err(unsent(theirs(code))),
    }
}

// Passes on each line `stderr` gives under `host`'s name, less a runner's
// own `runner: `, and gives back the last that wasn't blank, without its
// full stop, to say why. A stderr that has gone, like a terminal closed,
// stops none of it.
fn relay(
    stderr: impl Read + Send + 'static,
    host: String,
) -> std::thread::JoinHandle<Option<String>> {
    std::thread::spawn(move || {
        let mut last = None;
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let said = line.strip_prefix("runner: ").unwrap_or(&line).trim_end();
            let _ = writeln!(io::stderr(), "runner: {host}: {said}");
            if !said.is_empty() {
                last = Some(said.trim_end_matches('.').to_string());
            }
        }
        last
    })
}
