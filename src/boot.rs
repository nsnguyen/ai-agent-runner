// `runner boot` and `runner halt`: a VM's two ends, as its systemd unit runs
// them. Boot holds the VM's lock for as long as Firecracker runs, so delete
// and undo refuse a running VM, and halt waits on that same lock.
//
// Each boot is a run: it boots a fresh copy of the current disk, and only a
// clean shutdown, Firecracker exiting 0, makes that copy current, unless a
// headless run that hasn't ended cleanly asks to roll back (Store::ask_rollback).
//
// While the VM runs, boot hands its guest the credentials the VM may have
// whenever runner-guest asks, which it does at each start (src/creds.rs), and
// serves the guest's own calls: checkpoint, list and restore (src/calls.rs).
// A restore the guest asked for is finished at the clean shutdown that
// follows, and ends the boot with RESTART, for the unit to boot it again.
//
// Boot also puts the VM to sleep and wakes it (src/sleep.rs): it stops
// Firecracker once a snapshot is written, and stays up itself, still holding
// the lock, until it starts a fresh one to load the snapshot. So one boot can
// run several Firecrackers in turn, and only the last one's exit ends it.
// halt goes through boot too, so a sleeping VM is woken to shut down cleanly,
// and nothing puts the VM to sleep once it's shutting down.
//
// And it puts the VM to sleep once it has gone unused for its idle time: every
// few seconds it looks at whether anything is using it (src/idle.rs).
//
// At a host shutdown, a sleeping VM's boot parks it and ends without
// committing (src/sleep.rs). The next boot of a parked VM boots no fresh copy:
// it loads the snapshot onto the disk it was parked on, and sets the guest's
// clock, as a wake does. One whose load fails boots its last committed disk;
// one whose Firecracker doesn't come up fails, still parked.
//
// It serves the VM's URL on the tailnet, too (src/url.rs), for as long as it's
// up: a connection to a sleeping VM has the loop wake it, as a `runner wake`
// does. And the VM's DNS, on its gateway (src/dns.rs), for as long too.
use crate::calls::{Calls, Restore};
use crate::creds::{Cred, Keyring, Source};
use crate::dns;
use crate::exec;
use crate::firecracker::{self, Api, Guest};
use crate::idle::{self, Idle, Look, Quiet, Timer, Uses};
use crate::net::{self, HOST_RESOLV_CONFS, Net, Tap};
use crate::proto::{self, Request};
use crate::runs;
use crate::sessions;
use crate::sleep::{self, Ask, Asks, Reply};
use crate::store::{self, Handed, Run, Slept, Store};
use crate::systemd;
use crate::url;
use chrono::Utc;
use nix::errno::Errno;
use nix::sys::wait::{Id, WaitPidFlag, waitid};
use nix::unistd::Pid;
use std::fs::{self, File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Firecracker binds its socket within milliseconds.
const API_WAIT: Duration = Duration::from_secs(5);
// A guest that asks for its credentials reads them at once.
const HANDOFF_WAIT: Duration = Duration::from_secs(5);
// A guest just woken was listening when it slept, so it answers at once.
const CLOCK_WAIT: Duration = Duration::from_secs(5);
// How often boot looks at whether its VM is in use, at most: a shorter idle
// time is looked at as often as it is.
const LOOK: Duration = Duration::from_secs(10);
// A guest that's up takes a connection at once. Every command on the VM waits
// while boot asks, so it doesn't wait long.
const ASK_WAIT: Duration = Duration::from_secs(2);

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
    #[error(
        "{name} has a list of names, and only its boot's DNS holds it to them, but boot \
         can't serve its DNS: {why}"
    )]
    NoDns { name: String, why: String },
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
    #[error(
        "{name} couldn't wake ({why}), so its snapshot is dropped: it stays at \
         generation {current}, and generation {kept} is kept for a look"
    )]
    Unwoken {
        name: String,
        why: String,
        current: u64,
        kept: u64,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// The exit status of a boot that ended to restore a checkpoint the guest
/// asked for: the unit's RestartForceExitStatus, so systemd boots the VM
/// again, onto the restored disk, unless it's being stopped.
pub const RESTART: u8 = 75;

/// How a boot that went well ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The VM is down, and stays down.
    Down,
    /// Down, with a restore the guest asked for made current: boot it again.
    Restart,
    /// Parked at a host shutdown, asleep, for its next boot to resume.
    Parked,
}

fn io_err(what: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

/// Boots the VM and returns when Firecracker exits. Its console goes to
/// stdout, which the unit sends to journald. `ip` is iproute2's, for the tap.
pub fn boot(store: &Store, name: &str, bin: &Path, ip: &Path) -> Result<Ended> {
    let machine = store.machine(name)?;
    if !machine.kernel.is_file() {
        return Err(Error::NoKernel(machine.kernel));
    }
    firecracker::check_version(bin)?;
    let net = store.net(name)?;
    let keyring = credentials(store, name)?;
    let mut run = store.begin_run(name)?;
    // What a killed boot noted isn't this boot's guest's, and a snapshot one
    // left asleep is of a disk no boot can run again. A parked VM's are its
    // own: its guest holds those credentials, and the snapshot is to resume.
    let handed = run.handed();
    if run.parked().is_none()
        && let Err(e) = handed.clear().and_then(|()| run.snapshot().clear())
    {
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
    // On the tap's address, so after it, and dropped just before it.
    let (relay, ip_arg) = match serve_dns(store, name, &net, run.parked().is_some()) {
        Ok(served) => served,
        Err(e) => {
            drop(tap);
            run.discard();
            return Err(e);
        }
    };
    let api_sock = run.api_socket();
    let vsock = run.vsock();
    let creds_sock = run.creds_socket();
    let calls_sock = run.calls_socket();
    let boot_sock = run.boot_socket();
    let sockets = [
        api_sock.as_path(),
        &vsock,
        &creds_sock,
        &calls_sock,
        &boot_sock,
    ];
    // Firecracker binds two, and won't over a file a crashed one left.
    remove_sockets(&sockets)?;
    // Bound before the guest can boot, so they're there when the guest asks.
    // An ask that comes while the VM boots waits for it to be up.
    let (events, inbox) = mpsc::channel();
    let uses = match Uses::open(store, name) {
        Ok(uses) => uses,
        Err(e) => {
            drop(relay);
            drop(tap);
            run.discard();
            return Err(e.into());
        }
    };
    let served = Offer::start(name, &creds_sock, keyring, handed.clone()).and_then(|offer| {
        let calls = Calls::start(store, name, &calls_sock)
            .map_err(io_err(format!("serving calls on {}", calls_sock.display())))?;
        let asks = Asks::serve(&boot_sock, asker(events.clone(), name))
            .map_err(io_err(format!("taking asks on {}", boot_sock.display())))?;
        let url = url::Served::start(store, name, net.address, waker(events.clone(), name))
            .map_err(io_err("serving the VM's URL"))?;
        Ok((offer, calls, asks, url))
    });
    let (offer, calls, asks, url) = match served {
        Ok(served) => served,
        Err(e) => {
            let _ = remove_sockets(&sockets);
            drop(relay);
            drop(tap);
            run.discard();
            return Err(e);
        }
    };
    let api = Api::new(&api_sock);
    // A parked VM resumes where it slept, on the disk, tap and sockets it
    // slept with.
    let mut resumed = None;
    if run.parked().is_some() {
        let began = Instant::now();
        match load(bin, &api, &api_sock, &vsock, name, &run, || run.resuming()) {
            Ok(child) => resumed = Some((child, began)),
            Err(Unwoken::Asleep(why) | Unwoken::Lost(why)) => {
                let kept = run.generation;
                // Still noted, so Firecracker never came up to load it: it
                // can resume once it does.
                let unparked = if run.snapshot().parked() {
                    let why = format!("{name} didn't resume, and stays parked: {why}");
                    Err(io_err("resuming")(io::Error::other(why)))
                } else {
                    run.unpark().map_err(Error::from)
                };
                if let Err(e) = unparked {
                    drop(url);
                    drop(asks);
                    drop(calls);
                    drop(offer);
                    let _ = remove_sockets(&sockets);
                    drop(relay);
                    drop(tap);
                    run.discard();
                    return Err(e);
                }
                // A cold boot, whose guest asks for its credentials again.
                if let Err(e) = handed.clear() {
                    note(format_args!("{e}"));
                }
                note(format_args!(
                    "{name} couldn't resume ({why}), so its snapshot is dropped: it boots \
                     generation {}, its last committed disk, and generation {kept}, where \
                     it was parked, is kept for a look",
                    run.from
                ));
            }
        }
    }
    let (mut child, resumed) = match resumed {
        Some((child, began)) => (child, Some(began)),
        None => (firecracker(bin, &api_sock, name, run.hold()?)?, None),
    };

    let guest = Guest {
        kernel: &machine.kernel,
        disk: &run.disk,
        cpus: machine.cpus,
        mem_mib: machine.mem_mib,
        vsock: &vsock,
        net: &net,
        ip_arg: &ip_arg,
    };
    let mut up = Up {
        store,
        name,
        run: &run,
        bin,
        api,
        api_sock: &api_sock,
        vsock: &vsock,
        inbox,
        events,
        fc: None,
        epoch: 0,
        halting: false,
        dropped: None,
        parked: false,
        uses,
        timer: Timer::new(Instant::now()),
    };
    let started = match resumed {
        Some(_) => up.watch(&child),
        None => wait_for_api(&mut child, &api_sock)
            .and_then(|()| Ok(up.api.configure(&guest)?))
            .and_then(|()| Ok(up.api.start()?))
            .and_then(|()| up.watch(&child)),
    };
    if let Err(e) = started {
        let _ = child.kill();
        let _ = child.wait();
        drop(up);
        drop(url);
        drop(asks);
        drop(calls);
        drop(offer);
        let _ = remove_sockets(&sockets);
        drop(relay);
        drop(tap);
        run.discard();
        return Err(e);
    }
    up.fc = Some(child);
    match resumed {
        Some(began) => {
            // Credentials and all. Unnoted since the load, it's no longer
            // asleep, so what's left goes at the next boot if not now.
            if let Err(e) = run.snapshot().clear() {
                note(format_args!("{name} resumed, but its snapshot stays: {e}"));
            }
            let took = began.elapsed();
            if let Err(e) = set_clock(&vsock) {
                note(format_args!(
                    "{name}'s clock is behind by as long as it was parked: setting it \
                     failed: {e}"
                ));
            }
            note(format_args!(
                "{name} resumed generation {} where it was parked, in {took:.1?}",
                run.generation
            ));
        }
        None => note(format_args!(
            "{name} is running generation {}",
            run.generation
        )),
    }
    // Without systemd there's no one to tell, and with it a failure here
    // shows up as a start that times out.
    if let Err(e) = systemd::notify_ready() {
        note(format_args!("couldn't tell systemd {name} is up: {e}"));
    }

    let over = up.serve();
    drop(up);
    let restore = calls.close();
    drop(url);
    drop(asks);
    drop(calls);
    // Before the clear, so a handoff in flight can't note after it.
    drop(offer);
    let _ = remove_sockets(&sockets);
    // A parked VM's guest still holds what it was handed.
    if !matches!(over, Over::Parked)
        && let Err(e) = handed.clear()
    {
        note(format_args!("{e}"));
    }
    drop(relay);
    drop(tap);
    let status = match over {
        Over::Exited(Ok(status)) => status,
        Over::Exited(Err(e)) => {
            drop_restore(store, name, restore, "boot lost track of firecracker");
            return Err(io_err("waiting for firecracker")(e));
        }
        Over::Dropped(why) => {
            drop_restore(store, name, restore, "its snapshot was dropped");
            return Err(Error::Unwoken {
                name: name.to_string(),
                why,
                current: store.get(name)?.generation,
                kept: run.generation,
            });
        }
        // Uncommitted: its disk stays as its snapshot left it.
        Over::Parked => {
            drop_restore(store, name, restore, "it was parked");
            note(format_args!(
                "{name} is parked: generation {} and its memory stay, for its next \
                 boot to resume",
                run.generation
            ));
            return Ok(Ended::Parked);
        }
    };
    // A clean shutdown under a run that hadn't ended cleanly (src/runs.rs).
    if status.success()
        && let Some(by) = run.take_rollback()?
    {
        note(format_args!(
            "{name} shut down cleanly, but {by} asked to roll back: generation {} \
             stays current, and generation {} is kept for a look",
            run.from, run.generation
        ));
        drop_restore(store, name, restore, &format!("{by} asked to roll back"));
        return Ok(Ended::Down);
    }
    if status.success() {
        let from = run.generation;
        let vm = run.commit_then(restore.as_ref().map(|r| &r.staged))?;
        let Some(restore) = restore else {
            note(format_args!(
                "{name} shut down cleanly; generation {} is current",
                vm.generation
            ));
            return Ok(Ended::Down);
        };
        note(format_args!(
            "{name} shut down cleanly to restore checkpoint {}: generation {from} is \
             committed, generation {} is current, and the unit boots it again unless \
             it's being stopped",
            restore.staged.from, vm.generation
        ));
        return Ok(Ended::Restart);
    }
    drop_restore(
        store,
        name,
        restore,
        &format!("firecracker exited with {status}"),
    );
    Err(Error::Crashed {
        name: name.to_string(),
        status,
        current: store.get(name)?.generation,
        kept: run.generation,
    })
}

// The VM's DNS, served on its gateway, and the boot argument that points
// the guest there. A boot that can't serve it, with a resolver already on
// port 53 of every address, say, gives the guest the host's resolvers, which
// the host's rules let it reach if they're off the host. A parked VM that
// resumes keeps the server it booted with, likely its gateway. A VM with a
// list of names doesn't boot at all then: only its boot holds it to them.
fn serve_dns(
    store: &Store,
    name: &str,
    net: &Net,
    parked: bool,
) -> Result<(Option<dns::Relay>, String)> {
    let listed = store.allowed(name)?.is_some();
    let no_dns = |why: String| {
        Err(Error::NoDns {
            name: name.to_string(),
            why,
        })
    };
    let resolvers = net::dns(&HOST_RESOLV_CONFS.map(Path::new));
    if resolvers.is_empty() {
        let why = format!("no IPv4 nameserver in {}", HOST_RESOLV_CONFS.join(" or "));
        if listed {
            return no_dns(why);
        }
        note(format_args!("{name} boots without DNS: {why}"));
        return Ok((None, net.boot_arg_with(name, &[])));
    }
    match dns::Relay::bind(net, &resolvers, dns_policy(store, name)) {
        Ok(relay) => Ok((Some(relay), net.boot_arg(name))),
        Err(e) if listed => no_dns(e.to_string()),
        Err(e) if parked => {
            note(format_args!(
                "boot can't serve {name}'s DNS on {e}: resumed, its guest gets no answer \
                 at its gateway until it boots again"
            ));
            Ok((None, net.boot_arg_with(name, &resolvers)))
        }
        Err(e) => {
            note(format_args!(
                "{name}'s DNS goes straight to the host's resolvers: boot can't serve it on \
                 {e}. A list of names set on it now counts only from its next boot"
            ));
            Ok((None, net.boot_arg_with(name, &resolvers)))
        }
    }
}

// The VM's list, as the store has it each time the relay looks, and where
// its allowed answers' addresses go. Nothing takes them yet: until the
// host's rules hold a listed VM to them, it reaches any address it has.
fn dns_policy(store: &Store, name: &str) -> dns::Policy {
    let (store, name) = (store.clone(), name.to_string());
    dns::Policy {
        list: Box::new(move || {
            store
                .allowed(&name)
                .map_err(|e| format!("{name}'s list of names: {e}"))
        }),
        hand_on: Arc::new(|_| {}),
    }
}

// Firecracker for this boot's VM, on its API socket. Its stdin is the lock,
// so the VM stays held while the guest runs even if boot dies. It reads stdin
// only from a tty or a fifo, so it never touches the file.
fn firecracker(bin: &Path, api_sock: &Path, name: &str, lock: File) -> Result<Child> {
    let mut fc = Command::new(bin);
    fc.arg("--api-sock")
        .arg(api_sock)
        .args(["--id", name])
        .stdin(lock)
        .env_remove("NOTIFY_SOCKET")
        // Nor does it get a credential: it's what a guest would break into
        // first, and the host's may include ones this VM isn't given.
        .env_remove("CREDENTIALS_DIRECTORY");
    for var in Cred::ALL.iter().flat_map(|c| c.vars()) {
        fc.env_remove(var);
    }
    // The unit gives this process CAP_NET_ADMIN for the tap, and
    // CAP_NET_BIND_SERVICE for the VM's DNS. Firecracker opens the tap
    // without either, and a guest that broke into Firecracker could use the
    // first to undo the host's NAT rules, so it inherits neither.
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
    fc.spawn()
        .map_err(io_err(format!("running {}", bin.display())))
}

// What boot's loop hears: a Firecracker gone, or an ask on its socket.
enum Event {
    /// The Firecracker of this epoch has exited. It's left for the loop to
    /// reap, so its pid can't be reused before then.
    Exited(u32),
    Asked(Ask, mpsc::Sender<Reply>),
}

// How the loop ended.
enum Over {
    /// The VM's Firecracker exited.
    Exited(io::Result<ExitStatus>),
    /// It was asleep and couldn't wake, so its snapshot is gone.
    Dropped(String),
    /// It was asleep, and is parked: its snapshot stays.
    Parked,
}

// Why a wake didn't happen.
enum Unwoken {
    /// Nothing was loaded: the snapshot is as it was, to wake from later.
    Asleep(String),
    /// A load that may have run the guest, which then wrote to its disk, so
    /// the snapshot is of a moment before the disk: it can't be woken again.
    Lost(String),
}

// What the boot's socket is asked goes to its loop, which answers once it's
// done, one ask at a time.
fn asker(events: mpsc::Sender<Event>, name: &str) -> impl Fn(Ask) -> Reply + Send + 'static {
    let name = name.to_string();
    move |ask| {
        let (reply, answer) = mpsc::channel();
        if events.send(Event::Asked(ask, reply)).is_err() {
            return Reply::Refused(format!("{name} is going down"));
        }
        let down = |_| Reply::Refused(format!("{name} went down first"));
        answer.recv().unwrap_or_else(down)
    }
}

// What the URL asks to have the VM awake: a wake, through the loop.
fn waker(events: mpsc::Sender<Event>, name: &str) -> url::Wake {
    let ask = asker(events, name);
    Arc::new(move || match ask(Ask::Wake) {
        Reply::Done => Ok(()),
        Reply::Refused(why) => Err(why),
    })
}

// The VM from its start to its shutdown, awake or asleep.
struct Up<'a> {
    store: &'a Store,
    name: &'a str,
    run: &'a Run,
    bin: &'a Path,
    api: Api,
    api_sock: &'a Path,
    vsock: &'a Path,
    inbox: mpsc::Receiver<Event>,
    events: mpsc::Sender<Event>,
    /// Firecracker, unless the VM is asleep.
    fc: Option<Child>,
    /// Counts Firecrackers, so the exit of one stopped for a sleep isn't
    /// taken for the next one's.
    epoch: u32,
    /// A halt has begun: no more sleeps.
    halting: bool,
    /// Why a snapshot that couldn't wake was dropped, which ends the boot.
    dropped: Option<String>,
    /// Parked at a host shutdown, which ends the boot.
    parked: bool,
    /// The VM's use lock, which commands using it hold (src/idle.rs).
    uses: Uses,
    /// Since the VM was last seen in use, or last woke.
    timer: Timer,
}

impl Up<'_> {
    // Until Firecracker exits on its own, or a snapshot that can't wake is
    // dropped.
    fn serve(&mut self) -> Over {
        // The first at once: it finds the VM just booted, and says when next.
        let mut look_at = Instant::now();
        loop {
            let event = match self
                .inbox
                .recv_timeout(look_at.saturating_duration_since(Instant::now()))
            {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                // The loop holds a sender itself, so this never happens.
                Err(RecvTimeoutError::Disconnected) => {
                    return Over::Exited(Err(io::ErrorKind::BrokenPipe.into()));
                }
            };
            if Instant::now() >= look_at {
                look_at = Instant::now() + self.look();
            }
            let Some(event) = event else { continue };
            match event {
                Event::Exited(epoch) if epoch == self.epoch => {
                    if let Some(mut fc) = self.fc.take() {
                        return Over::Exited(fc.wait());
                    }
                }
                // One this boot stopped for a sleep, and reaped already.
                Event::Exited(_) => {}
                Event::Asked(ask, reply) => {
                    let done = match ask {
                        Ask::Sleep => self.sleep(),
                        Ask::Wake => match self.wake() {
                            // A whole idle time from now: it was woken to be used.
                            Ok(()) => {
                                self.timer.used(Instant::now());
                                Ok(())
                            }
                            Err(Unwoken::Asleep(e)) => Err(format!(
                                "{} couldn't wake, and sleeps on: {e}. `runner stop {}` \
                                 drops its snapshot and rolls its disk back",
                                self.name, self.name
                            )),
                            Err(Unwoken::Lost(why)) => Err(self.lose(why)),
                        },
                        Ask::Halt => self.halt(),
                        Ask::Park => self.park(),
                    };
                    let _ = reply.send(done.map_or_else(Reply::Refused, |()| Reply::Done));
                    if let Some(why) = self.dropped.take() {
                        return Over::Dropped(why);
                    }
                    if self.parked {
                        return Over::Parked;
                    }
                }
            }
        }
    }

    // Tells the loop when this Firecracker exits, as Exited of this epoch.
    fn watch(&self, child: &Child) -> Result<()> {
        let pid = Pid::from_raw(child.id() as libc::pid_t);
        let (epoch, events) = (self.epoch, self.events.clone());
        let wait = move || {
            let exited = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT;
            while let Err(Errno::EINTR) = waitid(Id::Pid(pid), exited) {}
            let _ = events.send(Event::Exited(epoch));
        };
        std::thread::Builder::new()
            .spawn(wait)
            .map(drop)
            .map_err(io_err("starting a thread"))
    }

    // Puts the VM to sleep if nothing has used it for its idle time, which
    // it reads each time, so a change counts at once. Returns how long until
    // the next look.
    fn look(&mut self) -> Duration {
        let name = self.name;
        let idle = match self.store.idle(name) {
            Ok(idle) => idle,
            Err(e) => {
                note(format_args!("{name} stays awake: {e}"));
                return LOOK;
            }
        };
        // A config edited by hand to 0 doesn't spin.
        let every = idle
            .after()
            .map_or(LOOK, |after| after.clamp(Duration::from_secs(1), LOOK));
        // Asleep, or going down: nothing to look at.
        if self.fc.is_none() || self.halting {
            return every;
        }
        let now = Instant::now();
        let mut probe = Probe {
            uses: &mut self.uses,
            store: self.store,
            name,
            vsock: self.vsock,
            quiet: None,
        };
        let look = idle::look(&mut self.timer, idle, now, &mut probe);
        // Held through the sleep, so no command starts on the VM meanwhile.
        let _quiet = probe.quiet.take();
        if look == Look::Sleep {
            self.idle_sleep(idle);
        }
        every
    }

    fn idle_sleep(&mut self, idle: Idle) {
        let name = self.name;
        note(format_args!(
            "{name} went unused for {idle}: putting it to sleep"
        ));
        if let Err(why) = self.sleep() {
            // Tried again after another idle time, not at every look.
            self.timer.used(Instant::now());
            note(format_args!("{name} stays awake: {why}"));
        }
    }

    fn sleep(&mut self) -> std::result::Result<(), String> {
        let name = self.name;
        if self.fc.is_none() {
            return Ok(());
        }
        if self.halting {
            return Err(format!("{name} is shutting down"));
        }
        // Firecracker's docs: a snapshot taken while the kernel boots may
        // not resume. runner-guest asks for its credentials once it's up.
        match self.store.handed(name) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(format!(
                    "{name}'s guest isn't up yet: runner-guest hasn't asked for its \
                     credentials. Sleep it once it has"
                ));
            }
            Err(e) => return Err(e.to_string()),
        }
        // A run streams over vsock, which a snapshot cuts.
        let _no_run = runs::hold(self.store, name).map_err(|e| match e {
            runs::Error::Busy { run, .. } => format!(
                "run {run} is going in {name}, and sleeping would cut it off: \
                 sleep it once the run is over"
            ),
            runs::Error::Starting(_) => format!(
                "a run is starting in {name}, or a restore its agent asked for is \
                 under way: sleep it once that's over"
            ),
            e => e.to_string(),
        })?;
        // A checkpoint pauses and resumes the VM too: they take turns.
        let _freezer = self
            .store
            .hold_checkpoints(name)
            .map_err(|e| e.to_string())?;
        let snap = self.run.snapshot();
        snap.make_room().map_err(|e| e.to_string())?;
        // Noted first: from here status says sleeping, so a wake asked
        // meanwhile waits behind this sleep, rather than looking for a guest
        // that's going.
        let slept = Slept {
            generation: self.run.generation,
            at: Utc::now(),
            parked: false,
        };
        snap.note(&slept).map_err(|e| e.to_string())?;
        let began = Instant::now();
        let taken = self
            .api
            .pause()
            .and_then(|()| self.api.snapshot(&snap.vmstate(), &snap.mem()));
        if let Err(e) = taken {
            let _ = snap.clear();
            // A refusal can come after the pause took, as a timeout can.
            return Err(match self.api.resume() {
                Ok(()) => format!("{name} didn't sleep, and carries on: {e}"),
                Err(still) => format!(
                    "{name} didn't sleep ({e}), and is still paused: {still}. \
                     `runner stop {name}` stops it"
                ),
            });
        }
        // Paused, and all of it written: nothing's lost with Firecracker.
        if let Some(mut fc) = self.fc.take() {
            let _ = fc.kill();
            let _ = fc.wait();
        }
        // Firecracker leaves them, and the next one won't bind over them.
        let _ = remove_sockets(&[self.api_sock, self.vsock]);
        note(format_args!(
            "{name} is asleep: generation {} and its memory are in {}, written in {:.1?}",
            self.run.generation,
            snap.mem()
                .parent()
                .map_or_else(String::new, |d| d.display().to_string()),
            began.elapsed()
        ));
        Ok(())
    }

    // Loads the snapshot into a fresh Firecracker on the same sockets, then
    // sets the guest's clock. The snapshot stays until the load has worked.
    fn wake(&mut self) -> std::result::Result<(), Unwoken> {
        let name = self.name;
        if self.fc.is_some() {
            return Ok(());
        }
        let asleep = |e: &dyn std::fmt::Display| Unwoken::Asleep(e.to_string());
        let snap = self.run.snapshot();
        // Only this boot's own: one of the disk it runs.
        match snap.slept() {
            Ok(Some(slept)) if slept.generation != self.run.generation => {
                return Err(Unwoken::Asleep(format!(
                    "its snapshot runs on generation {}, not this boot's {}",
                    slept.generation, self.run.generation
                )));
            }
            Err(e) => return Err(asleep(&e)),
            _ => {}
        }
        // Firecracker may have been upgraded while the VM slept.
        firecracker::check_version(self.bin).map_err(|e| asleep(&e))?;
        let began = Instant::now();
        let mut child = load(
            self.bin,
            &self.api,
            self.api_sock,
            self.vsock,
            name,
            self.run,
            || Ok(()),
        )?;
        self.epoch += 1;
        if let Err(e) = self.watch(&child) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = remove_sockets(&[self.api_sock, self.vsock]);
            return Err(Unwoken::Lost(e.to_string()));
        }
        self.fc = Some(child);
        // Its memory file stays open in Firecracker, which reads pages from
        // it, until it exits.
        if let Err(e) = snap.clear() {
            note(format_args!("{name} is awake, but its snapshot stays: {e}"));
        }
        let took = began.elapsed();
        if let Err(e) = set_clock(self.vsock) {
            note(format_args!(
                "{name}'s clock is behind by as long as it slept: setting it failed: {e}"
            ));
        }
        note(format_args!("{name} woke in {took:.1?}"));
        Ok(())
    }

    // At a host shutdown: a sleeping VM is parked, which ends the boot. An
    // awake one isn't: its unit's stop, which comes next, shuts it down
    // cleanly, waking it first if it has gone to sleep since.
    fn park(&mut self) -> std::result::Result<(), String> {
        let name = self.name;
        if self.fc.is_some() {
            return Err(format!(
                "{name} is awake: it isn't parked, and shuts down cleanly as its unit stops"
            ));
        }
        self.run
            .park()
            .map_err(|e| format!("{name} isn't parked, and wakes to shut down: {e}"))?;
        self.parked = true;
        Ok(())
    }

    // Drops a snapshot that can't wake, and with it the boot, as after a kill.
    fn lose(&mut self, why: String) -> String {
        let name = self.name;
        let _ = self.run.snapshot().clear();
        let lost = format!(
            "{name} couldn't wake, so its snapshot is dropped and its disk rolls back: {why}"
        );
        self.dropped = Some(why);
        lost
    }

    // Wakes a sleeping VM, which can only shut down cleanly awake, then asks
    // it to shut down. If it can't wake, its snapshot goes, and so does the
    // boot, as after a kill.
    fn halt(&mut self) -> std::result::Result<(), String> {
        let name = self.name;
        self.halting = true;
        if self.fc.is_none()
            && let Err(Unwoken::Asleep(why) | Unwoken::Lost(why)) = self.wake()
        {
            return Err(self.lose(why));
        }
        // A checkpoint under way finishes first (checkpoint.rs). A paused
        // guest can't take Ctrl-Alt-Del, and one whose checkpoint was killed
        // partway is still paused, so it's resumed; one that isn't says so,
        // unheard.
        drop(
            self.store
                .hold_checkpoints(name)
                .map_err(|e| e.to_string())?,
        );
        let _ = self.api.resume();
        match self.api.ctrl_alt_del() {
            Ok(()) => {
                note(format_args!("sent Ctrl-Alt-Del to {name}"));
                Ok(())
            }
            // Firecracker is gone already; boot may still be committing.
            Err(firecracker::Error::NotListening(_)) => Ok(()),
            // The guest may be on its way down anyway, so halt waits on,
            // and the stop timeout ends it if not.
            Err(e) => Err(e.to_string()),
        }
    }
}

// A fresh Firecracker on the boot's sockets, with the VM's snapshot loaded
// into it and running: what a wake starts, and a parked VM's boot. `first`
// runs once Firecracker is up, just before the load. On a failure it's gone,
// and so are its sockets.
fn load(
    bin: &Path,
    api: &Api,
    api_sock: &Path,
    vsock: &Path,
    name: &str,
    run: &Run,
    first: impl FnOnce() -> store::Result<()>,
) -> std::result::Result<Child, Unwoken> {
    let asleep = |e: &dyn std::fmt::Display| Unwoken::Asleep(e.to_string());
    let snap = run.snapshot();
    remove_sockets(&[api_sock, vsock]).map_err(|e| asleep(&e))?;
    let lock = run.hold().map_err(|e| asleep(&e))?;
    let mut child = firecracker(bin, api_sock, name, lock).map_err(|e| asleep(&e))?;
    let loaded = wait_for_api(&mut child, api_sock)
        .map_err(|e| asleep(&e))
        .and_then(|()| first().map_err(|e| asleep(&e)))
        .and_then(|()| load_into(api, &snap));
    if let Err(e) = loaded {
        let _ = child.kill();
        let _ = child.wait();
        let _ = remove_sockets(&[api_sock, vsock]);
        return Err(e);
    }
    Ok(child)
}

// A refused load means Firecracker never ran the guest; no answer, that it
// may have.
fn load_into(api: &Api, snap: &store::Snapshot) -> std::result::Result<(), Unwoken> {
    match api.load(&snap.vmstate(), &snap.mem()) {
        Ok(()) => Ok(()),
        Err(e @ (firecracker::Error::Api { .. } | firecracker::Error::NotListening(_))) => {
            Err(Unwoken::Asleep(e.to_string()))
        }
        Err(e) => Err(Unwoken::Lost(e.to_string())),
    }
}

// What a look asks about this boot's VM (src/idle.rs).
struct Probe<'a> {
    uses: &'a mut Uses,
    store: &'a Store,
    name: &'a str,
    vsock: &'a Path,
    /// The use lock, from a look that found it quiet.
    quiet: Option<Quiet>,
}

impl idle::Probe for Probe<'_> {
    fn used(&mut self) -> bool {
        match self.uses.quiet() {
            Ok(quiet) => {
                let used = quiet.is_none();
                self.quiet = quiet;
                used
            }
            Err(e) => {
                note(format_args!("{} stays awake: {e}", self.name));
                true
            }
        }
    }

    fn running(&mut self) -> bool {
        match runs::hold(self.store, self.name) {
            Ok(_lock) => false,
            Err(runs::Error::Busy { .. } | runs::Error::Starting(_)) => true,
            Err(e) => {
                note(format_args!("{} stays awake: {e}", self.name));
                true
            }
        }
    }

    // A guest that can't say is taken to have none, and the sleep decides:
    // it's refused for a guest that isn't up.
    fn attached(&mut self) -> bool {
        let asked = exec::connect(self.vsock, proto::PORT, ASK_WAIT)
            .map_err(|e| sessions::Error::from(exec::Error::from(e)))
            .and_then(sessions::attached);
        asked.unwrap_or_else(|e| {
            note(format_args!(
                "couldn't ask {}'s guest whether a terminal is on a session, so none \
                 counts: {e}",
                self.name
            ));
            false
        })
    }
}

// The guest's clock stood still while it slept. The host sends it its own
// time, which runner-guest sets as root with `date`, there in every image.
fn set_clock(vsock: &Path) -> std::result::Result<(), String> {
    let conn = exec::connect(vsock, proto::PORT, CLOCK_WAIT).map_err(|e| e.to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let at = format!("@{}.{:09}", now.as_secs(), now.subsec_nanos());
    let req = Request {
        argv: ["date", "-u", "-s", &at].map(String::from).to_vec(),
        env: Vec::new(),
        cwd: None,
        user: None,
        tty: None,
    };
    let done = exec::capture(conn, &req, b"", CLOCK_WAIT, 4096).map_err(|e| e.to_string())?;
    match done.exit_code {
        Some(0) => Ok(()),
        code => Err(format!("date exited {code:?}: {}", done.stderr.trim())),
    }
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
// Dropping it stops it and removes its socket, whichever way boot returns.
struct Offer {
    sock: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Offer {
    fn start(name: &str, sock: &Path, keyring: Keyring, handed: Handed) -> Result<Offer> {
        let what = || format!("offering credentials on {}", sock.display());
        let listener = UnixListener::bind(sock).map_err(io_err(what()))?;
        let mut offer = Offer {
            sock: sock.to_path_buf(),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        fs::set_permissions(sock, Permissions::from_mode(0o600)).map_err(io_err(what()))?;
        let stopped = offer.stop.clone();
        let name = name.to_string();
        let serve = move || {
            let creds = keyring.creds();
            for conn in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                // Written, not yet read: the guest's own log says if it
                // couldn't take them.
                let sent = conn.and_then(|mut conn| {
                    conn.set_write_timeout(Some(HANDOFF_WAIT))?;
                    keyring.send(&mut conn)
                });
                let sent = sent.map_err(io_err("handing over credentials"));
                match sent.and_then(|()| Ok(handed.record(&creds)?)) {
                    Ok(()) => note(format_args!("handed {name}'s guest {}", names(&creds))),
                    Err(e) => {
                        note(format_args!("{name}'s guest: {e}"));
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        };
        let thread = std::thread::Builder::new()
            .spawn(serve)
            .map_err(io_err("starting a thread"))?;
        offer.thread = Some(thread);
        Ok(offer)
    }
}

impl Drop for Offer {
    // Waits out a handoff in flight, so its note can't land after the boot
    // clears it. A connection of its own wakes the thread.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take()
            && UnixStream::connect(&self.sock).is_ok()
        {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.sock);
    }
}

// A restore the guest asked for, which a VM that didn't shut down cleanly
// doesn't get: the copy goes, and its VM stays as it would have.
fn drop_restore(store: &Store, name: &str, restore: Option<Restore>, why: &str) {
    let Some(restore) = restore else { return };
    let from = restore.staged.from;
    let generation = restore.staged.generation;
    note(format_args!(
        "{name}'s restore of checkpoint {from} is dropped: {why}"
    ));
    if let Err(e) = store.drop_restore(name, restore.staged) {
        note(format_args!(
            "couldn't remove {name}'s generation {generation}: {e}"
        ));
    }
}

fn names(creds: &[Cred]) -> String {
    if creds.is_empty() {
        return "no credentials".to_string();
    }
    let names: Vec<_> = creds.iter().map(|c| c.name()).collect();
    names.join(" and ")
}

/// Asks the VM's boot to shut it down, waking it first if it's asleep, then
/// waits for boot to finish with it. The unit's stop timeout bounds the wait.
pub fn halt(store: &Store, name: &str) -> Result<()> {
    match sleep::ask(store, name, Ask::Halt) {
        Ok(()) => {}
        // Ctrl-Alt-Del failed, or a VM that couldn't wake was dropped: the
        // guest may be on its way down anyway, so the wait goes on.
        Err(sleep::Error::Refused(why)) => note(format_args!("{why}")),
        // Nothing booted, or a boot from before it took asks.
        Err(sleep::Error::NoBoot(_)) => by_api(store, name)?,
        Err(e) => {
            note(format_args!("{e}"));
            by_api(store, name)?;
        }
    }
    store.wait_idle(name)?;
    Ok(())
}

// Ctrl-Alt-Del straight to Firecracker, for a boot that can't be asked.
fn by_api(store: &Store, name: &str) -> Result<()> {
    let sock = store.api_socket(name)?;
    let api = Api::new(&sock);
    // A checkpoint under way finishes first (checkpoint.rs). A paused guest
    // can't take Ctrl-Alt-Del, and one whose checkpoint was killed partway
    // is still paused, so it's resumed; one that isn't says so, unheard.
    drop(store.hold_checkpoints(name)?);
    let _ = api.resume();
    match api.ctrl_alt_del() {
        Ok(()) => note(format_args!("sent Ctrl-Alt-Del to {name}")),
        // Firecracker is gone already; boot may still be committing.
        Err(firecracker::Error::NotListening(_)) => {}
        // The guest may be on its way down anyway, so the wait goes on, and
        // the stop timeout ends it if not.
        Err(e) => note(format_args!("{e}")),
    }
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
