// `runner boot` and `runner halt`: a VM's two ends, as its systemd unit runs
// them. Boot holds the VM's lock for as long as Firecracker runs, so delete
// and undo refuse a running VM, and halt waits on that same lock.
//
// Each boot is a run: it boots a fresh copy of the current disk, and only a
// clean shutdown, Firecracker exiting 0, makes that copy current.
use crate::firecracker::{self, Api, Guest};
use crate::store::{self, Store};
use crate::systemd;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

// Firecracker binds its socket within milliseconds.
const API_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Firecracker(#[from] firecracker::Error),
    #[error("no kernel at {}: put one there, or create the VM with --kernel", .0.display())]
    NoKernel(PathBuf),
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
    #[error("firecracker exited before its API came up ({0})")]
    EarlyExit(ExitStatus),
    #[error("firecracker's API didn't come up within {}s", API_WAIT.as_secs())]
    NoApi,
    #[error(
        "firecracker exited with {status}: {name} stays at generation {current}, \
         and generation {kept} is kept for a look"
    )]
    Crashed {
        name: String,
        status: ExitStatus,
        current: u64,
        kept: u64,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(what: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

/// Boots the VM and returns when Firecracker exits. Its console goes to
/// stdout, which the unit sends to journald.
pub fn boot(store: &Store, name: &str, bin: &Path) -> Result<()> {
    let machine = store.machine(name)?;
    if !machine.kernel.is_file() {
        return Err(Error::NoKernel(machine.kernel));
    }
    firecracker::check_version(bin)?;
    let run = store.begin_run(name)?;
    let api_sock = run.api_socket();
    let vsock = run.vsock();
    // Firecracker binds both, and won't over a file a crashed one left.
    remove_sockets(&[&api_sock, &vsock])?;
    let mut child = Command::new(bin)
        .arg("--api-sock")
        .arg(&api_sock)
        .args(["--id", name])
        .stdin(Stdio::null())
        .env_remove("NOTIFY_SOCKET")
        .spawn()
        .map_err(io_err(format!("running {}", bin.display())))?;

    let api = Api::new(&api_sock);
    let guest = Guest {
        kernel: &machine.kernel,
        disk: &run.disk,
        cpus: machine.cpus,
        mem_mib: machine.mem_mib,
        vsock: &vsock,
    };
    let started = wait_for_api(&mut child, &api_sock)
        .and_then(|()| Ok(api.configure(&guest)?))
        .and_then(|()| Ok(api.start()?));
    if let Err(e) = started {
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_sockets(&[&api_sock, &vsock]);
        run.discard();
        return Err(e);
    }
    eprintln!("runner: {name} is running generation {}", run.generation);
    // Without systemd there's no one to tell, and with it a failure here
    // shows up as a start that times out.
    if let Err(e) = systemd::notify_ready() {
        eprintln!("runner: couldn't tell systemd {name} is up: {e}");
    }

    let status = child.wait().map_err(io_err("waiting for firecracker"));
    let _ = remove_sockets(&[&api_sock, &vsock]);
    let status = status?;
    if status.success() {
        let vm = run.commit()?;
        eprintln!(
            "runner: {name} shut down cleanly; generation {} is current",
            vm.generation
        );
        return Ok(());
    }
    Err(Error::Crashed {
        name: name.to_string(),
        status,
        current: store.get(name)?.generation,
        kept: run.generation,
    })
}

/// Asks the guest to shut down, then waits for boot to finish with it. The
/// unit's stop timeout bounds the wait.
pub fn halt(store: &Store, name: &str) -> Result<()> {
    let sock = store.api_socket(name)?;
    match Api::new(&sock).ctrl_alt_del() {
        Ok(()) => eprintln!("runner: sent Ctrl-Alt-Del to {name}"),
        // Firecracker is gone already; boot may still be committing.
        Err(firecracker::Error::NotListening(_)) => {}
        Err(e) => return Err(e.into()),
    }
    store.wait_idle(name)?;
    Ok(())
}

fn wait_for_api(child: &mut Child, sock: &Path) -> Result<()> {
    let deadline = Instant::now() + API_WAIT;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(io_err("waiting for firecracker"))?
        {
            return Err(Error::EarlyExit(status));
        }
        if UnixStream::connect(sock).is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(Error::NoApi);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn remove_sockets(socks: &[&Path]) -> Result<()> {
    for sock in socks {
        match std::fs::remove_file(sock) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(io_err(format!("removing {}", sock.display()))(e));
            }
            _ => {}
        }
    }
    Ok(())
}
