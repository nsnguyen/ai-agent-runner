// A VM's URL on the tailnet, like a Sprite's but private: open
// http://<host's tailnet address>:<port> and the VM's app answers, waking the
// VM if it was asleep. The port is the VM's own, from its slot (net.rs), so it
// stays the same for as long as the VM exists, and goes to port 8080 in the
// VM unless `runner url --port` says otherwise. It's plain TCP, so whatever
// the guest serves on that port works, not only HTTP.
//
// The VM's boot holds the listener (src/boot.rs), so the URL answers while the
// VM is up, awake or asleep, and not while it's stopped. It looks at the VM's
// setting every second, so `runner url <vm> off` and `--port` count at once.
// Each connection holds the VM in use (idle.rs), so it doesn't idle-sleep
// under one; has the boot wake it if it's asleep; waits for the guest to
// accept; then passes bytes both ways until both sides are done.
//
// Tailnet only: it binds only an IPv4 address in Tailscale's 100.64.0.0/10
// that's on one of Tailscale's interfaces, never 0.0.0.0, and takes
// connections only from that range. The host's rules (host/nat.nft) already
// drop what a VM opens to the host's own addresses; the second rule still
// keeps one VM from reaching another through its URL if they're missing.
use crate::idle::InUse;
use crate::store::{self, Store};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{setsockopt, sockopt};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The host port of slot 0; slot n gets this plus n. The default range's
/// 16384 slots stay below Linux's ephemeral ports, from 32768.
pub const FIRST_PORT: u16 = 10000;
/// The port in the VM a URL goes to until `--port` says otherwise.
pub const GUEST_PORT: u16 = 8080;
/// How long a connection waits for the guest to accept it, after the wake.
/// A woken guest was listening when it slept, so it takes it at once; one
/// still booting may take a while to start its app.
pub const ACCEPT_WAIT: Duration = Duration::from_secs(30);

// How often the boot looks at the VM's setting and the host's tailnet address.
const LOOK: Duration = Duration::from_secs(1);
// The most connections taken between looks, so a flood can't hold off a stop.
const BATCH: usize = 64;
// Notes about refused connections come at most this often, so a flood from a
// VM can't fill the journal.
const HUSH: Duration = Duration::from_secs(60);
// A client that goes silent, like a laptop that sleeps with a connection
// open, is probed after a minute and dropped after three unanswered probes,
// and data it leaves unacknowledged as long ends it too. Then its connection
// stops holding the VM awake.
const KEEP_IDLE: u32 = 60;
const KEEP_EVERY: u32 = 10;
const KEEP_PROBES: u32 = 3;
const UNACKED_MS: u32 = 90_000;
// One try at reaching the guest.
const TRY: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_millis(100);
// Tailscale's IPv4 range, 100.64.0.0/10: its first ten bits.
const TAILNET_BITS: u32 = (100 << 2) | 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(
        "{0} isn't this host's tailnet address, so a URL won't bind it: only \
         Tailscale's 100.64.0.0/10 address on its own interface"
    )]
    NotTailnet(IpAddr),
    #[error(
        "this host has no tailnet address: is Tailscale up? A VM's URL binds only \
         Tailscale's 100.x address on its own interface"
    )]
    NoTailnet,
    #[error("slot {0} has no port: URLs take ports {FIRST_PORT} and up, one per slot")]
    NoPort(u32),
    #[error("couldn't wake the VM: {0}")]
    Unwoken(String),
    #[error("nothing in the VM took the connection at {guest} within {wait:?}: {last}")]
    NoAnswer {
        guest: SocketAddr,
        wait: Duration,
        last: io::Error,
    },
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(what: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

/// A VM's URL as its config stores it: on, to port 8080 in the VM, unless
/// set otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    /// Turned off with `runner url <vm> off`. The port stays for when it's
    /// on again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub off: bool,
    /// The port in the VM it goes to, if not 8080.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Setting {
    pub fn guest_port(self) -> u16 {
        self.port.unwrap_or(GUEST_PORT)
    }
}

/// The host port of a slot's URL, or None past 65535.
pub fn host_port(slot: u32) -> Option<u16> {
    u32::from(FIRST_PORT)
        .checked_add(slot)
        .and_then(|port| u16::try_from(port).ok())
}

/// The VM's host port, from its slot.
pub fn port(store: &Store, name: &str) -> Result<u16> {
    let slot = store.net(name)?.slot;
    host_port(slot).ok_or(Error::NoPort(slot))
}

/// An address on one of the host's interfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub addr: IpAddr,
}

/// Every address on the host's interfaces.
pub fn ifaces() -> io::Result<Vec<Iface>> {
    let all = nix::ifaddrs::getifaddrs().map_err(io::Error::from)?;
    Ok(all
        .filter_map(|iface| {
            let addr = iface.address?;
            let addr = match (addr.as_sockaddr_in(), addr.as_sockaddr_in6()) {
                (Some(v4), _) => IpAddr::V4(v4.ip()),
                (_, Some(v6)) => IpAddr::V6(v6.ip()),
                _ => return None,
            };
            Some(Iface {
                name: iface.interface_name,
                addr,
            })
        })
        .collect())
}

fn in_tailnet_range(addr: Ipv4Addr) -> bool {
    u32::from(addr) >> 22 == TAILNET_BITS
}

/// Whether `addr` is this host's own tailnet address: IPv4 in Tailscale's
/// 100.64.0.0/10, on one of Tailscale's interfaces. A carrier's CGNAT address
/// is in that range too, but on another interface.
pub fn on_tailnet(addr: IpAddr, ifaces: &[Iface]) -> bool {
    let IpAddr::V4(v4) = addr else { return false };
    in_tailnet_range(v4)
        && ifaces
            .iter()
            .any(|iface| iface.addr == addr && iface.name.starts_with("tailscale"))
}

/// The host's tailnet address, if Tailscale is up.
pub fn tailnet_addr(ifaces: &[Iface]) -> Option<Ipv4Addr> {
    ifaces.iter().find_map(|iface| match iface.addr {
        IpAddr::V4(v4) if on_tailnet(iface.addr, ifaces) => Some(v4),
        _ => None,
    })
}

/// Listens on `addr`, which has to be the host's tailnet address.
pub fn bind(addr: SocketAddr, ifaces: &[Iface]) -> Result<TcpListener> {
    if !on_tailnet(addr.ip(), ifaces) {
        return Err(Error::NotTailnet(addr.ip()));
    }
    TcpListener::bind(addr).map_err(io_err(format!("listening on {addr}")))
}

/// Whether a connection from `peer` is taken: only from Tailscale's range,
/// which the host itself is in when it connects to its own tailnet address.
/// Not a VM, which would otherwise reach other VMs through their URLs, nor
/// the LAN.
pub fn peer_allowed(peer: IpAddr) -> bool {
    match peer {
        IpAddr::V4(v4) => in_tailnet_range(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(in_tailnet_range),
    }
}

/// The VM's URL, or None if it's off.
pub fn url(store: &Store, name: &str) -> Result<Option<String>> {
    let ifaces = ifaces().map_err(io_err("listing this host's addresses"))?;
    url_on(store, name, &ifaces)
}

/// `url`, on a host with these addresses.
pub fn url_on(store: &Store, name: &str, ifaces: &[Iface]) -> Result<Option<String>> {
    if store.url(name)?.off {
        return Ok(None);
    }
    let port = port(store, name)?;
    let addr = tailnet_addr(ifaces).ok_or(Error::NoTailnet)?;
    Ok(Some(format!("http://{addr}:{port}")))
}

/// One connection to a URL: `wake` has the VM awake, then the guest gets up
/// to `wait` to accept at `guest`, and bytes pass both ways until both sides
/// are done. An error closes the client's connection unanswered.
pub fn proxy(
    client: TcpStream,
    guest: SocketAddr,
    wake: impl FnOnce() -> std::result::Result<(), String>,
    wait: Duration,
) -> Result<()> {
    keep_alive(&client).map_err(io_err("setting keepalive"))?;
    wake().map_err(Error::Unwoken)?;
    let upstream = reach(guest, wait)?;
    pipe(client, upstream)
}

/// Has the kernel find out a client that has gone silent within a minute and
/// a half, by keepalive probes and by data it doesn't acknowledge. The guest's
/// side is left alone: an explicit sleep stops it answering for as long as it
/// sleeps.
pub fn keep_alive(conn: &TcpStream) -> io::Result<()> {
    setsockopt(conn, sockopt::KeepAlive, &true)?;
    setsockopt(conn, sockopt::TcpKeepIdle, &KEEP_IDLE)?;
    setsockopt(conn, sockopt::TcpKeepInterval, &KEEP_EVERY)?;
    setsockopt(conn, sockopt::TcpKeepCount, &KEEP_PROBES)?;
    setsockopt(conn, sockopt::TcpUserTimeout, &UNACKED_MS)?;
    Ok(())
}

// Tries until the guest accepts, or `wait` has passed.
fn reach(guest: SocketAddr, wait: Duration) -> Result<TcpStream> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let try_for = TRY.min(left).max(Duration::from_millis(1));
        match TcpStream::connect_timeout(&guest, try_for) {
            Ok(conn) => return Ok(conn),
            Err(_) if Instant::now() + RETRY < deadline => thread::sleep(RETRY),
            Err(last) => return Err(Error::NoAnswer { guest, wait, last }),
        }
    }
}

// Both ways at once, one of them on a thread of its own. What goes wrong
// partway, like a client that resets, is the two ends' business.
fn pipe(client: TcpStream, guest: TcpStream) -> Result<()> {
    let clone = |s: &TcpStream| s.try_clone().map_err(io_err("cloning a connection"));
    let (from_client, to_guest) = (clone(&client)?, clone(&guest)?);
    let sending = thread::Builder::new()
        .spawn(move || one_way(from_client, to_guest))
        .map_err(io_err("starting a thread"))?;
    one_way(guest, client);
    let _ = sending.join();
    Ok(())
}

// Copies until `from` ends, then says so to `to`. A failure either way ends
// both, so the other direction's copy ends too.
fn one_way(mut from: TcpStream, mut to: TcpStream) {
    match io::copy(&mut from, &mut to) {
        Ok(_) => {
            let _ = to.shutdown(Shutdown::Write);
        }
        Err(_) => {
            let _ = from.shutdown(Shutdown::Both);
            let _ = to.shutdown(Shutdown::Both);
        }
    }
}

/// Has the VM awake, through its boot, or says why not.
pub type Wake = Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>;

/// A VM's URL, served by its boot for as long as it's up. Dropping it stops
/// it within a second, and closes its listener; connections under way carry
/// on until the boot ends.
pub struct Served {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Served {
    /// Serves the VM's URL, to its guest at `guest`. Off, or with no tailnet
    /// address, it listens on nothing, and looks again every second.
    pub fn start(store: &Store, name: &str, guest: Ipv4Addr, wake: Wake) -> io::Result<Served> {
        Served::start_with(store, name, guest, wake, ifaces)
    }

    /// `start`, taking the host's addresses from `ifaces`.
    pub fn start_with(
        store: &Store,
        name: &str,
        guest: Ipv4Addr,
        wake: Wake,
        ifaces: impl Fn() -> io::Result<Vec<Iface>> + Send + 'static,
    ) -> io::Result<Served> {
        let stop = Arc::new(AtomicBool::new(false));
        let serving = Serving {
            store: store.clone(),
            name: name.to_string(),
            guest,
            guest_port: GUEST_PORT,
            wake,
            ifaces: Box::new(ifaces),
            stop: stop.clone(),
            bound: None,
            said: None,
            hush: Hush::default(),
        };
        let thread = thread::Builder::new().spawn(move || serving.run())?;
        Ok(Served {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Serving {
    store: Store,
    name: String,
    guest: Ipv4Addr,
    guest_port: u16,
    wake: Wake,
    ifaces: Box<dyn Fn() -> io::Result<Vec<Iface>> + Send>,
    stop: Arc<AtomicBool>,
    /// The address it listens on, and the listener.
    bound: Option<(SocketAddr, TcpListener)>,
    /// The last trouble noted, so it's noted once, not every second.
    said: Option<String>,
    hush: Hush,
}

impl Serving {
    fn run(mut self) {
        while !self.stop.load(Ordering::SeqCst) {
            self.settle();
            match self.bound.take() {
                Some((at, listener)) => {
                    self.take_all(&listener);
                    self.bound = Some((at, listener));
                }
                None => thread::sleep(LOOK),
            }
        }
    }

    // Listens where the setting and the host's address now say, if that's
    // changed. Trouble that may pass, like Tailscale not up yet, leaves what's
    // bound as it is, and is looked at again.
    fn settle(&mut self) {
        let ifaces = match (self.ifaces)() {
            Ok(ifaces) => ifaces,
            Err(e) => return self.say(format!("can't list this host's addresses: {e}")),
        };
        let wanted = match wanted(&self.store, &self.name, &ifaces) {
            Ok(wanted) => wanted,
            Err(e) => return self.say(e.to_string()),
        };
        let Some((at, guest_port)) = wanted else {
            self.said = None;
            if self.bound.take().is_some() {
                note(format_args!("{}'s URL is off", self.name));
            }
            return;
        };
        self.guest_port = guest_port;
        if self.bound.as_ref().is_some_and(|(bound, _)| *bound == at) {
            return;
        }
        // Its address changed: Tailscale came back with another.
        self.bound = None;
        let listening = bind(at, &ifaces).and_then(|listener| {
            listener
                .set_nonblocking(true)
                .map_err(io_err(format!("listening on {at}")))?;
            Ok(listener)
        });
        match listening {
            Ok(listener) => {
                self.said = None;
                self.bound = Some((at, listener));
                note(format_args!(
                    "{}'s URL is http://{at}, to port {guest_port} in the VM",
                    self.name
                ));
            }
            Err(e) => self.say(e.to_string()),
        }
    }

    fn say(&mut self, why: String) {
        if self.said.as_ref() != Some(&why) {
            note(format_args!("{}'s URL doesn't answer: {why}", self.name));
            self.said = Some(why);
        }
    }

    // Waits up to a second for connections, and takes those that have come,
    // up to a batch, so the stop and the setting are looked at again soon.
    fn take_all(&mut self, listener: &TcpListener) {
        let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        let ms = u16::try_from(LOOK.as_millis()).unwrap_or(u16::MAX);
        if !matches!(poll(&mut fds, PollTimeout::from(ms)), Ok(n) if n > 0) {
            return;
        }
        for _ in 0..BATCH {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            match listener.accept() {
                Ok((conn, peer)) => self.take(conn, peer),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    let name = &self.name;
                    self.hush
                        .note(format_args!("{name}'s URL: taking a connection: {e}"));
                    thread::sleep(RETRY);
                    return;
                }
            }
        }
    }

    // Each connection on a thread of its own, holding the VM in use.
    fn take(&mut self, conn: TcpStream, peer: SocketAddr) {
        let name = self.name.clone();
        if !peer_allowed(peer.ip()) {
            self.hush.note(format_args!(
                "{name}'s URL refused {peer}: only the tailnet may connect"
            ));
            return;
        }
        let guest = SocketAddr::new(self.guest.into(), self.guest_port);
        let (store, wake) = (self.store.clone(), self.wake.clone());
        let serve = move || {
            // Waits out a sleep under way, so the wake finds it asleep.
            let _in_use = match InUse::take(&store, &name) {
                Ok(in_use) => in_use,
                Err(e) => return note(format_args!("{name}'s URL: {e}")),
            };
            let proxied = conn
                .set_nonblocking(false)
                .map_err(io_err("taking a connection"))
                .and_then(|()| proxy(conn, guest, || wake(), ACCEPT_WAIT));
            if let Err(e) = proxied {
                note(format_args!("{name}'s URL, for {peer}: {e}"));
            }
        };
        if let Err(e) = thread::Builder::new().spawn(serve) {
            note(format_args!("{}'s URL: starting a thread: {e}", self.name));
        }
    }
}

// Notes at most one a minute, saying how many were held back since.
#[derive(Default)]
struct Hush {
    last: Option<Instant>,
    held: u32,
}

impl Hush {
    fn note(&mut self, msg: std::fmt::Arguments) {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < HUSH)
        {
            self.held = self.held.saturating_add(1);
            return;
        }
        match self.held {
            0 => note(msg),
            held => note(format_args!("{msg} (and {held} more held back)")),
        }
        self.last = Some(now);
        self.held = 0;
    }
}

// Where the URL should listen now, and the port in the VM it goes to; None
// if it's off.
fn wanted(store: &Store, name: &str, ifaces: &[Iface]) -> Result<Option<(SocketAddr, u16)>> {
    let setting = store.url(name)?;
    if setting.off {
        return Ok(None);
    }
    let port = port(store, name)?;
    let addr = tailnet_addr(ifaces).ok_or(Error::NoTailnet)?;
    Ok(Some((
        SocketAddr::new(addr.into(), port),
        setting.guest_port(),
    )))
}

// As boot's notes: under the unit, stderr is the journal stream Firecracker
// made non-blocking, where eprintln! would panic.
fn note(msg: std::fmt::Arguments) {
    let _ = writeln!(io::stderr(), "runner: {msg}");
}
