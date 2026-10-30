// `runner boot` and `runner halt`: a VM's two ends, as its systemd unit runs
// them. Boot holds the VM's lock for as long as Firecracker runs, so delete
// and undo refuse a running VM, and halt waits on that same lock.
//
// Each boot is a run: it boots a fresh copy of the current disk, and only a
// clean shutdown, Firecracker exiting 0, makes that copy current.
//
// While the VM runs, boot hands its guest the credentials the VM may have
// whenever runner-guest asks, which it does at each start (src/creds.rs).
use crate::creds::{Cred, Keyring, Source};
use crate::firecracker::{self, Api, Guest};
use crate::net::{self, HOST_RESOLV_CONFS, Tap};
use crate::store::{self, Handed, Store};
use crate::systemd;
use std::fs::{self, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Firecracker binds its socket within milliseconds.
const API_WAIT: Duration = Duration::from_secs(5);
// A guest that asks for its credentials reads them at once.
const HANDOFF_WAIT: Duration = Duration::from_secs(5);

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
/// stdout, which the unit sends to journald. `ip` is iproute2's, for the tap.
pub fn boot(store: &Store, name: &str, bin: &Path, ip: &Path) -> Result<()> {
    let machine = store.machine(name)?;
    if !machine.kernel.is_file() {
        return Err(Error::NoKernel(machine.kernel));
    }
    firecracker::check_version(bin)?;
    let net = store.net(name)?;
    let keyring = credentials(store, name)?;
    let run = store.begin_run(name)?;
    // What a killed boot noted isn't this boot's guest's.
    let handed = run.handed();
    if let Err(e) = handed.clear() {
        run.discard();
        return Err(e.into());
    }
    // Made under the VM's lock, and dropped, which removes it, before the
    // lock goes: halt returns once the lock is free, and systemd may kill
    // this process right after. commit and discard let go of the lock, so
    // the tap is dropped by hand before either.
    let tap = match Tap::add_with(ip, &net) {
        Ok(tap) => tap,
        Err(e) => {
            run.discard();
            return Err(e.into());
        }
    };
    let dns = net::dns(&HOST_RESOLV_CONFS.map(Path::new));
    if dns.is_empty() {
        note(format_args!(
            "{name} boots without DNS: no IPv4 nameserver in {}",
            HOST_RESOLV_CONFS.join(" or ")
        ));
    }
    let ip_arg = net.boot_arg(name, &dns);
    let api_sock = run.api_socket();
    let vsock = run.vsock();
    let creds_sock = run.creds_socket();
    let sockets = [api_sock.as_path(), &vsock, &creds_sock];
    // Firecracker binds two, and won't over a file a crashed one left.
    remove_sockets(&sockets)?;
    // Bound before the guest can boot, so it's there when the guest asks.
    let offer = match Offer::start(name, &creds_sock, keyring, handed.clone()) {
        Ok(offer) => offer,
        Err(e) => {
            let _ = remove_sockets(&sockets);
            drop(tap);
            run.discard();
            return Err(e);
        }
    };
    // Firecracker's stdin is the lock, so the VM stays held while the guest
    // runs even if this process dies. It reads stdin only from a tty or a
    // fifo, so it never touches the file.
    let mut fc = Command::new(bin);
    fc.arg("--api-sock")
        .arg(&api_sock)
        .args(["--id", name])
        .stdin(run.hold()?)
        .env_remove("NOTIFY_SOCKET");
    // The unit gives this process CAP_NET_ADMIN for the tap. Firecracker
    // opens the tap without it, and a guest that broke into Firecracker
    // could use it to undo the host's NAT rules, so it doesn't inherit it.
    // SAFETY: prctl is async-signal-safe, so fine between fork and exec.
    unsafe {
        fc.pre_exec(|| {
            let cleared = libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
            if cleared == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let mut child = fc
        .spawn()
        .map_err(io_err(format!("running {}", bin.display())))?;

    let api = Api::new(&api_sock);
    let guest = Guest {
        kernel: &machine.kernel,
        disk: &run.disk,
        cpus: machine.cpus,
        mem_mib: machine.mem_mib,
        vsock: &vsock,
        net: &net,
        ip_arg: &ip_arg,
    };
    let started = wait_for_api(&mut child, &api_sock)
        .and_then(|()| Ok(api.configure(&guest)?))
        .and_then(|()| Ok(api.start()?));
    if let Err(e) = started {
        let _ = child.kill();
        let _ = child.wait();
        offer.end();
        let _ = remove_sockets(&sockets);
        drop(tap);
        run.discard();
        return Err(e);
    }
    note(format_args!(
        "{name} is running generation {}",
        run.generation
    ));
    // Without systemd there's no one to tell, and with it a failure here
    // shows up as a start that times out.
    if let Err(e) = systemd::notify_ready() {
        note(format_args!("couldn't tell systemd {name} is up: {e}"));
    }

    let status = child.wait().map_err(io_err("waiting for firecracker"));
    offer.end();
    let _ = remove_sockets(&sockets);
    if let Err(e) = handed.clear() {
        note(format_args!("{e}"));
    }
    drop(tap);
    let status = status?;
    if status.success() {
        let vm = run.commit()?;
        note(format_args!(
            "{name} shut down cleanly; generation {} is current",
            vm.generation
        ));
        return Ok(());
    }
    Err(Error::Crashed {
        name: name.to_string(),
        status,
        current: store.get(name)?.generation,
        kept: run.generation,
    })
}

// What the host has of the credentials the VM may have, with a note for
// each it lacks: the VM boots anyway.
fn credentials(store: &Store, name: &str) -> Result<Keyring> {
    let allow = store.creds(name)?;
    let source = Source::from_process();
    let keyring = Keyring::gather(&allow, &source).map_err(io_err("reading credentials"))?;
    let have = keyring.creds();
    for cred in allow.iter().filter(|c| !have.contains(c)) {
        let files: Vec<_> = cred
            .vars()
            .iter()
            .map(|v| format!("/etc/credstore/{v}"))
            .collect();
        note(format_args!(
            "{name} may have {cred}, but this host has none: put it in {} (docs/host.md)",
            files.join(" or ")
        ));
    }
    Ok(keyring)
}

// Hands the guest its keyring each time it asks, for as long as the VM
// runs, and notes which credentials went. Only the runner user can connect,
// and it can read the credentials anyway; Firecracker connects for the guest.
struct Offer {
    sock: PathBuf,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Offer {
    fn start(name: &str, sock: &Path, keyring: Keyring, handed: Handed) -> Result<Offer> {
        let what = || format!("offering credentials on {}", sock.display());
        let listener = UnixListener::bind(sock).map_err(io_err(what()))?;
        fs::set_permissions(sock, Permissions::from_mode(0o600)).map_err(io_err(what()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let name = name.to_string();
        let serve = move || {
            let creds = keyring.creds();
            for conn in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                let sent = conn.and_then(|mut conn| {
                    conn.set_write_timeout(Some(HANDOFF_WAIT))?;
                    keyring.send(&mut conn)
                });
                let sent = sent.map_err(io_err("handing over credentials"));
                match sent.and_then(|()| Ok(handed.record(&creds)?)) {
                    Ok(()) => note(format_args!("{name}'s guest took {}", names(&creds))),
                    Err(e) => note(format_args!("{name}'s guest: {e}")),
                }
            }
        };
        let thread = std::thread::Builder::new()
            .spawn(serve)
            .map_err(io_err("starting a thread"))?;
        Ok(Offer {
            sock: sock.to_path_buf(),
            stop,
            thread,
        })
    }

    // Waits out a handoff in flight, so its note can't land after the boot
    // clears it. A connection of its own wakes the thread.
    fn end(self) {
        self.stop.store(true, Ordering::SeqCst);
        if UnixStream::connect(&self.sock).is_ok() {
            let _ = self.thread.join();
        }
    }
}

fn names(creds: &[Cred]) -> String {
    if creds.is_empty() {
        return "no credentials".to_string();
    }
    let names: Vec<_> = creds.iter().map(|c| c.name()).collect();
    names.join(" and ")
}

/// Asks the guest to shut down, then waits for boot to finish with it. The
/// unit's stop timeout bounds the wait.
pub fn halt(store: &Store, name: &str) -> Result<()> {
    let sock = store.api_socket(name)?;
    match Api::new(&sock).ctrl_alt_del() {
        Ok(()) => note(format_args!("sent Ctrl-Alt-Del to {name}")),
        // Firecracker is gone already; boot may still be committing.
        Err(firecracker::Error::NotListening(_)) => {}
        // The guest may be on its way down anyway, so the wait goes on, and
        // the stop timeout ends it if not.
        Err(e) => note(format_args!("{e}")),
    }
    store.wait_idle(name)?;
    Ok(())
}

// Firecracker makes its stdout non-blocking, and under the unit that's the
// same journal stream as stderr, where eprintln! would panic on EAGAIN. A
// note lost to a stalled journal is better than a boot that dies mid-commit.
fn note(msg: std::fmt::Arguments) {
    let _ = writeln!(io::stderr(), "runner: {msg}");
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
