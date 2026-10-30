// Each VM is an instance of one template unit, firecracker@<vm>.service,
// whose main process is `runner boot <vm>`. start, stop and status are
// systemctl on it, logs is journalctl. systemctl's output is captured, never
// inherited: under `runner mcp`, stdout is protocol.
use crate::store::{Store, Vm};
use rmcp::schemars;
use serde::Serialize;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

/// How long a VM gets to shut down after Ctrl-Alt-Del before systemd kills it.
pub const STOP_TIMEOUT_SECS: u32 = 30;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error("{command} failed: {stderr}\n`runner logs {vm}` says more")]
    Failed {
        command: String,
        vm: String,
        stderr: String,
    },
    #[error("running {prog}: {source}")]
    Run {
        prog: &'static str,
        source: io::Error,
    },
    #[error("systemd says {unit} is {state:?}, which runner doesn't know")]
    UnknownState { unit: String, state: String },
    #[error("{0}")]
    BadUnitValue(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Stopped,
    /// Copying the disk and configuring Firecracker; the guest hasn't started.
    Booting,
    Running,
    /// Shutting down after Ctrl-Alt-Del.
    Stopping,
}

impl State {
    /// From the unit's ActiveState. `runner boot` tells systemd it's ready once
    /// the guest is started, which is what ends `activating`.
    pub fn from_active_state(active: &str) -> Option<State> {
        Some(match active {
            "inactive" | "failed" | "maintenance" => State::Stopped,
            "activating" => State::Booting,
            "active" | "reloading" | "refreshing" => State::Running,
            "deactivating" => State::Stopping,
            _ => return None,
        })
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            State::Stopped => "stopped",
            State::Booting => "booting",
            State::Running => "running",
            State::Stopping => "stopping",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct VmStatus {
    #[serde(flatten)]
    pub vm: Vm,
    pub state: State,
}

pub fn unit_name(vm: &str) -> String {
    format!("firecracker@{vm}.service")
}

/// Boots the VM and returns once its guest is started.
pub fn start(store: &Store, name: &str) -> Result<VmStatus> {
    store.get(name)?;
    systemctl(name, &["start", &unit_name(name)])?;
    status(store, name)
}

/// Returns once the VM is down, cleanly or, past the timeout, killed.
pub fn stop(store: &Store, name: &str) -> Result<VmStatus> {
    store.get(name)?;
    systemctl(name, &["stop", &unit_name(name)])?;
    status(store, name)
}

pub fn status(store: &Store, name: &str) -> Result<VmStatus> {
    let vm = store.get(name)?;
    let unit = unit_name(name);
    let active = systemctl(name, &["show", "--property=ActiveState", "--value", &unit])?;
    let active = active.trim();
    let state = State::from_active_state(active).ok_or_else(|| Error::UnknownState {
        unit,
        state: active.to_string(),
    })?;
    Ok(VmStatus { vm, state })
}

/// The VM's journal: its serial console and the runner's own lines, on this
/// terminal. CLI only.
pub fn logs(store: &Store, name: &str, follow: bool, lines: Option<u32>) -> Result<ExitStatus> {
    store.get(name)?;
    let mut cmd = Command::new("journalctl");
    cmd.arg(format!("--unit={}", unit_name(name)))
        .arg("--no-pager");
    if let Some(n) = lines {
        cmd.arg(format!("--lines={n}"));
    }
    if follow {
        cmd.arg("--follow");
    }
    cmd.status().map_err(|source| Error::Run {
        prog: "journalctl",
        source,
    })
}

// stdin is closed, so systemctl fails rather than asking for a password;
// the host's polkit rule is what lets the runner user start its units.
fn systemctl(vm: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Run {
            prog: "systemctl",
            source,
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(Error::Failed {
        command: format!("systemctl {}", args.join(" ")),
        vm: vm.to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// The template unit, for `runner unit` to print. `runner` is this binary,
/// `root` is RUNNER_ROOT, and `user` owns the root: the unit writes there.
pub fn render_unit(runner: &Path, root: &Path, user: &str) -> Result<String> {
    let runner = plain_path(runner)?;
    let root = plain_path(root)?;
    check_user(user)?;
    Ok(format!(
        "\
# firecracker@.service, written by `runner unit`. Each instance is one VM:
# `systemctl start firecracker@dev` boots dev. Write it again after moving
# the runner binary or its root.
[Unit]
Description=Firecracker microVM %i
# Stopped before its disks' filesystem is unmounted, at host shutdown too.
RequiresMountsFor={root}

[Service]
# Ready once the guest is started; until then the VM is booting. Booting
# copies the disk, which takes minutes for a big one without reflink.
Type=notify
NotifyAccess=main
TimeoutStartSec=600
User={user}
SupplementaryGroups=kvm
Environment=RUNNER_ROOT={root}
ExecStart={runner} boot %i
# Ctrl-Alt-Del, then wait for the guest to shut down. Past the timeout,
# systemd kills the VM and its run isn't committed.
ExecStop={runner} halt %i
TimeoutStopSec={STOP_TIMEOUT_SECS}
# The serial console.
StandardOutput=journal
StandardError=journal
"
    ))
}

// systemd reads %, $, quotes, backslashes and spaces in these lines, so
// rather than escape them, take only paths that need none.
fn plain_path(path: &Path) -> Result<&str> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"/._+-".contains(&b);
    path.to_str()
        .filter(|s| path.is_absolute() && s.bytes().all(plain))
        .ok_or_else(|| {
            Error::BadUnitValue(format!(
                "{:?} can't go in a unit file as is: use an absolute path of letters, digits and /._+-",
                path
            ))
        })
}

// useradd's default rule for names.
fn check_user(user: &str) -> Result<()> {
    let mut bytes = user.bytes();
    let first = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_');
    let rest = bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b));
    if first && rest && user.len() <= 32 {
        Ok(())
    } else {
        Err(Error::BadUnitValue(format!("{user:?} isn't a user name")))
    }
}

/// Tells systemd the service is up, when it's run as a Type=notify unit.
pub fn notify_ready() -> io::Result<()> {
    match std::env::var_os("NOTIFY_SOCKET") {
        Some(socket) => notify(&socket, "READY=1"),
        None => Ok(()),
    }
}

/// sd_notify, without libsystemd: one datagram to NOTIFY_SOCKET, which is a
/// path, or an abstract name when it starts with @.
pub fn notify(socket: &OsStr, state: &str) -> io::Result<()> {
    let sock = UnixDatagram::unbound()?;
    match socket.as_bytes().strip_prefix(b"@") {
        Some(name) => sock.send_to_addr(state.as_bytes(), &SocketAddr::from_abstract_name(name)?),
        None => sock.send_to(state.as_bytes(), socket),
    }?;
    Ok(())
}
