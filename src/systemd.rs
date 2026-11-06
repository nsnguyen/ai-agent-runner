// Each VM is an instance of one template unit, firecracker@<vm>.service,
// whose main process is `runner boot <vm>`. start, stop and status are
// systemctl on it, logs is journalctl. systemctl's output is captured, never
// inherited: under `runner mcp`, stdout is protocol. A VM that sleeps keeps
// its unit active, since its boot stays up (src/sleep.rs): status tells it
// apart by its snapshot. One parked at a host shutdown sleeps with its unit
// stopped, until a start or wake starts it, and its boot resumes it.
use crate::boot::RESTART;
use crate::creds::Cred;
use crate::store::{Store, Vm};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::net::Ipv4Addr;
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
    #[error(transparent)]
    Sleep(#[from] crate::sleep::Error),
    #[error("{0}")]
    NotNow(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Stopped,
    /// Copying the disk and configuring Firecracker; the guest hasn't started.
    Booting,
    Running,
    /// Shutting down after Ctrl-Alt-Del.
    Stopping,
    /// Its memory in a snapshot and its Firecracker stopped, while its boot
    /// waits to wake it.
    Sleeping,
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
            State::Sleeping => "sleeping",
        })
    }
}

// Deserialize too, for another host's, which a move gets (moves.rs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VmStatus {
    #[serde(flatten)]
    pub vm: Vm,
    /// Where the host reaches it, the same for as long as it exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<Ipv4Addr>,
    pub state: State,
    /// The credentials its guest took this boot, by name; absent until it has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creds: Option<Vec<Cred>>,
    /// Its URL on the tailnet, which answers while it's up and wakes it from
    /// sleep (url.rs); absent while it's off, or the host isn't on the tailnet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

pub fn unit_name(vm: &str) -> String {
    format!("firecracker@{vm}.service")
}

/// Boots the VM, or wakes it if it's sleeping, and returns once its guest is
/// started.
pub fn start(store: &Store, name: &str) -> Result<VmStatus> {
    store.get(name)?;
    systemctl(name, &["start", &unit_name(name)])?;
    let status = status(store, name)?;
    if !awake(store, name, status.state)? {
        return Ok(status);
    }
    self::status(store, name)
}

/// Returns once the VM is down, cleanly or, past the timeout, killed. A
/// sleeping VM is woken to shut down (boot::halt), and a parked one resumed.
pub fn stop(store: &Store, name: &str) -> Result<VmStatus> {
    store.get(name)?;
    if parked(store, name)? {
        systemctl(name, &["start", &unit_name(name)])?;
    }
    systemctl(name, &["stop", &unit_name(name)])?;
    status(store, name)
}

/// Puts a running VM to sleep (src/sleep.rs). One asleep already stays so.
pub fn sleep(store: &Store, name: &str) -> Result<VmStatus> {
    match status(store, name)?.state {
        State::Running => crate::sleep::sleep(store, name)?,
        State::Sleeping => {}
        state => {
            return Err(Error::NotNow(format!(
                "{name} is {state}: only a running VM sleeps"
            )));
        }
    }
    status(store, name)
}

/// Wakes a sleeping VM, or starts a parked one, which resumes it. One awake
/// already stays so.
pub fn wake(store: &Store, name: &str) -> Result<VmStatus> {
    if parked(store, name)? {
        return start(store, name);
    }
    match status(store, name)?.state {
        state @ (State::Sleeping | State::Running) => {
            awake(store, name, state)?;
        }
        State::Stopped => {
            return Err(Error::NotNow(format!(
                "{name} is stopped, not sleeping: `runner start {name}` boots it"
            )));
        }
        state => return Err(Error::NotNow(format!("{name} is {state}, not sleeping"))),
    }
    status(store, name)
}

// Asks the boot of a VM that's up to have it awake: done at once if it is,
// and after a sleep under way if one is, so a VM that looked to be running
// while it went to sleep is woken too. A boot from before sleep, which takes
// no asks, can't have put it to sleep. True if the boot was asked.
fn awake(store: &Store, name: &str, state: State) -> Result<bool> {
    if !matches!(state, State::Running | State::Sleeping) {
        return Ok(false);
    }
    match crate::sleep::wake(store, name) {
        Ok(()) => Ok(true),
        Err(crate::sleep::Error::NoBoot(_)) if state == State::Running => Ok(false),
        Err(e) => Err(e.into()),
    }
}

// Parked at a host shutdown (src/sleep.rs): asleep, with its unit stopped.
fn parked(store: &Store, name: &str) -> Result<bool> {
    Ok(store.parked(name)? && unit_state(name)? == State::Stopped)
}

// What systemd says of the VM's unit.
fn unit_state(name: &str) -> Result<State> {
    let unit = unit_name(name);
    let active = systemctl(name, &["show", "--property=ActiveState", "--value", &unit])?;
    let active = active.trim();
    State::from_active_state(active).ok_or_else(|| Error::UnknownState {
        unit,
        state: active.to_string(),
    })
}

pub fn status(store: &Store, name: &str) -> Result<VmStatus> {
    let vm = store.get(name)?;
    let mut state = unit_state(name)?;
    // Only while its boot is up: one killed asleep leaves its snapshot. One
    // parked has no boot up, and sleeps on.
    match state {
        State::Running if store.sleeping(name)?.is_some() => state = State::Sleeping,
        State::Stopped if store.parked(name)? => state = State::Sleeping,
        _ => {}
    }
    // A start or stop that worked shouldn't be reported as failed for want
    // of an address.
    let address = store.net(name).ok().map(|net| net.address);
    let creds = match state {
        State::Stopped => None,
        _ => store.handed(name).ok().flatten(),
    };
    let url = crate::url::url(store, name).ok().flatten();
    Ok(VmStatus {
        vm,
        address,
        state,
        creds,
        url,
    })
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
    let load: String = Cred::ALL
        .iter()
        .flat_map(|c| c.vars())
        .map(|var| format!("LoadCredential={var}\n"))
        .collect();
    Ok(format!(
        "\
# firecracker@.service, as `runner unit` writes it and host/install.sh
# installs it. Each instance is one VM: `systemctl start firecracker@dev`
# boots dev. Write it again after moving the runner binary or its root.
[Unit]
Description=Firecracker microVM %i
# After the VMs' NAT, when both start at once.
After=network.target runner-nat.service
# Stopped before its disks' filesystem is unmounted, at host shutdown too.
RequiresMountsFor={root}
# And after runner-park.service, which parks it first if it's asleep.
Before=runner-park.service

[Service]
# Ready once the guest is started; until then the VM is booting. Booting
# copies the disk, which takes minutes for a big one without reflink.
Type=notify
NotifyAccess=main
TimeoutStartSec=600
User={user}
SupplementaryGroups=kvm
Environment=RUNNER_ROOT={root}
# The credentials a VM may get, each from /etc/credstore/<name> on the host.
# One that isn't there is skipped. Boot hands a VM's to its guest.
{load}# %i, not %I: %I would unescape a VM name's dashes into slashes.
ExecStart={runner} boot %i
# Ctrl-Alt-Del, then wait for the guest to shut down. Past the timeout,
# systemd kills the VM and its run isn't committed.
ExecStop={runner} halt %i
TimeoutStopSec={STOP_TIMEOUT_SECS}
# A guest's own restore (runner-guest restore) ends its boot with {RESTART}, once
# the restored disk is current: boot the VM again, onto that disk, unless
# it's being stopped.
SuccessExitStatus={RESTART}
RestartForceExitStatus={RESTART}
# The serial console.
StandardOutput=journal
StandardError=journal
# For boot to add and remove the VM's tap, and nothing else. Boot clears it
# before running Firecracker, which doesn't need it, and no setuid program
# gets it back.
AmbientCapabilities=CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_ADMIN
NoNewPrivileges=yes
"
    ))
}

/// The unit that parks each sleeping VM at a host shutdown, before the VMs'
/// units stop, for `runner unit --park` to print: as render_unit's.
pub fn render_park_unit(runner: &Path, root: &Path, user: &str) -> Result<String> {
    let runner = plain_path(runner)?;
    let root = plain_path(root)?;
    check_user(user)?;
    Ok(format!(
        "\
# runner-park.service, as `runner unit --park` writes it and host/install.sh
# installs it. At a host shutdown, it parks each sleeping VM before the VMs'
# units stop (src/sleep.rs): the VM's boot ends, leaving its snapshot and
# disk, and its next start resumes it. Write it again after moving the
# runner binary or its root.
[Unit]
Description=runner: sleeping VMs sleep on through a host shutdown
RequiresMountsFor={root}

[Service]
# Up from the host's start, with nothing to run: its stop is what it's for.
# Each firecracker@ unit is ordered Before= this one, so this one stops first.
Type=oneshot
RemainAfterExit=yes
User={user}
Environment=RUNNER_ROOT={root}
ExecStop={runner} park --all
# Each park syncs its VM's memory file, which takes a while for a big VM.
TimeoutStopSec=300

[Install]
WantedBy=multi-user.target
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
