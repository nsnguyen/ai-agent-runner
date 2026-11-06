// A VM's DNS, which its boot serves on the VM's gateway, port 53, over UDP
// and TCP, for as long as the boot is up, asleep included. The guest knows
// no other server (Net::boot_arg). Each query goes on to the host's
// resolvers (net::dns), one after another until one answers, and their
// answer comes back unchanged. Nothing is filtered yet: this is where an
// allowlist will answer only its names.
//
// The sockets are bound to the VM's tap as well as its gateway, and answer
// only the VM's own address, so no other VM can use them or be sent their
// answers. The host's rules (host/nat.nft) let port 53 in from a tap only to
// the host's end of that same tap. Port 53 needs CAP_NET_BIND_SERVICE, which
// the VM's unit gives boot beside CAP_NET_ADMIN; boot clears both before it
// runs Firecracker.
//
// Each query has a thread of its own, up to IN_FLIGHT at once, so one that
// no resolver answers holds up no other. Past that a datagram is dropped and
// a connection closed, as a busy server would, and the guest asks again.
use crate::net::Net;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{self, AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, sockopt};
use std::ffi::OsString;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket,
};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// DNS's port, on the VM's gateway and on the host's resolvers.
pub const PORT: u16 = 53;
/// How long a query waits on each of the host's resolvers before it tries
/// the next. A guest asks again after 5s, so two of these fit.
pub const WAIT: Duration = Duration::from_secs(2);
// The most queries under way at once, over UDP and TCP together.
const IN_FLIGHT: usize = 64;
// How long a guest's TCP connection may sit between queries.
const IDLE: Duration = Duration::from_secs(10);
// How often the loops look at whether to stop.
const LOOK: Duration = Duration::from_millis(200);
// A DNS message's header: anything shorter isn't one.
const HEADER: usize = 12;
// The most a DNS message can be, over UDP or TCP.
const MAX: usize = 65535;

/// A VM's DNS, served until it's dropped. Dropping it stops it within a
/// fraction of a second; queries under way finish on their own.
pub struct Relay {
    shared: Arc<Shared>,
    loops: Vec<JoinHandle<()>>,
}

struct Shared {
    guest: IpAddr,
    resolvers: Vec<SocketAddr>,
    wait: Duration,
    stop: AtomicBool,
    busy: AtomicUsize,
}

impl Relay {
    /// Serves `net`'s VM on its gateway's port 53, on its tap, and passes
    /// each query on to `resolvers`, on their port 53.
    pub fn bind(net: &Net, resolvers: &[Ipv4Addr]) -> io::Result<Relay> {
        let at = SocketAddrV4::new(net.gateway, PORT);
        let failed = |e: io::Error| io::Error::new(e.kind(), format!("{at} on {}: {e}", net.tap));
        let udp = bound(SockType::Datagram, &net.tap, at).map_err(failed)?;
        let tcp = bound(SockType::Stream, &net.tap, at)
            .and_then(|fd| {
                socket::listen(&fd, Backlog::new(64)?)?;
                Ok(fd)
            })
            .map_err(failed)?;
        let resolvers = resolvers
            .iter()
            .map(|ip| SocketAddr::new((*ip).into(), PORT))
            .collect();
        Relay::serve(
            UdpSocket::from(udp),
            TcpListener::from(tcp),
            net.address.into(),
            resolvers,
            WAIT,
        )
    }

    /// Serves `guest`, and only it, on these sockets, waiting `wait` on each
    /// resolver in turn.
    pub fn serve(
        udp: UdpSocket,
        tcp: TcpListener,
        guest: IpAddr,
        resolvers: Vec<SocketAddr>,
        wait: Duration,
    ) -> io::Result<Relay> {
        udp.set_read_timeout(Some(LOOK))?;
        tcp.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            guest,
            resolvers,
            wait,
            stop: AtomicBool::new(false),
            busy: AtomicUsize::new(0),
        });
        // Dropped, it stops what's started, if the second doesn't start.
        let mut relay = Relay {
            shared: shared.clone(),
            loops: Vec::new(),
        };
        let on_udp = shared.clone();
        relay.loops.push(
            thread::Builder::new()
                .name("dns-udp".into())
                .spawn(move || udp_loop(&udp, &on_udp))?,
        );
        relay.loops.push(
            thread::Builder::new()
                .name("dns-tcp".into())
                .spawn(move || tcp_loop(&tcp, &shared))?,
        );
        Ok(relay)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for handle in self.loops.drain(..) {
            let _ = handle.join();
        }
    }
}

// A socket on `at`, bound to the tap too. Close-on-exec, so Firecracker
// never gets it.
fn bound(kind: SockType, tap: &str, at: SocketAddrV4) -> io::Result<OwnedFd> {
    let fd = socket::socket(AddressFamily::Inet, kind, SockFlag::SOCK_CLOEXEC, None)?;
    socket::setsockopt(&fd, sockopt::BindToDevice, &OsString::from(tap))?;
    if kind == SockType::Stream {
        // A boot again soon after, past connections left in TIME_WAIT.
        socket::setsockopt(&fd, sockopt::ReuseAddr, &true)?;
    }
    socket::bind(fd.as_raw_fd(), &SockaddrIn::from(at))?;
    Ok(fd)
}

// One of the IN_FLIGHT, given back when dropped.
struct Slot(Arc<Shared>);

impl Slot {
    fn take(shared: &Arc<Shared>) -> Option<Slot> {
        let taken = shared
            .busy
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < IN_FLIGHT).then_some(n + 1)
            });
        taken.is_ok().then(|| Slot(shared.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

fn udp_loop(socket: &UdpSocket, shared: &Arc<Shared>) {
    let mut buf = vec![0u8; MAX];
    while !shared.stop.load(Ordering::SeqCst) {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(got) => got,
            Err(e) if quiet(&e) => continue,
            Err(_) => {
                thread::sleep(LOOK);
                continue;
            }
        };
        if from.ip() != shared.guest || n < HEADER {
            continue;
        }
        let Some(slot) = Slot::take(shared) else {
            continue;
        };
        let Ok(reply) = socket.try_clone() else {
            continue;
        };
        let query = buf[..n].to_vec();
        let _ = thread::Builder::new()
            .name("dns-query".into())
            .spawn(move || {
                if let Some(answer) = over_udp(&query, &slot.0) {
                    let _ = reply.send_to(&answer, from);
                }
            });
    }
}

fn tcp_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    let ms = u16::try_from(LOOK.as_millis()).unwrap_or(u16::MAX);
    while !shared.stop.load(Ordering::SeqCst) {
        let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(ms)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(_) => {
                thread::sleep(LOOK);
                continue;
            }
        }
        let (conn, peer) = match listener.accept() {
            Ok(taken) => taken,
            Err(e) if quiet(&e) => continue,
            Err(_) => {
                thread::sleep(LOOK);
                continue;
            }
        };
        // Anyone else's is closed at once.
        if peer.ip() != shared.guest {
            continue;
        }
        let Some(slot) = Slot::take(shared) else {
            continue;
        };
        let _ = thread::Builder::new()
            .name("dns-conn".into())
            .spawn(move || {
                let _ = serve_conn(conn, &slot.0);
            });
    }
}

// Errors that only mean nothing came.
fn quiet(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

// Answers each query on a guest's connection in turn, until it closes, sits
// idle, or sends what isn't a query. A query no resolver answers closes it,
// so the guest asks again.
fn serve_conn(mut conn: TcpStream, shared: &Shared) -> io::Result<()> {
    conn.set_nonblocking(false)?;
    conn.set_read_timeout(Some(IDLE))?;
    conn.set_write_timeout(Some(IDLE))?;
    while !shared.stop.load(Ordering::SeqCst) {
        let query = read_message(&mut conn)?;
        if query.len() < HEADER {
            break;
        }
        let Some(answer) = over_tcp(&query, shared) else {
            break;
        };
        write_message(&mut conn, &answer)?;
    }
    Ok(())
}

// Asks each resolver in turn over UDP, from a socket of the query's own, and
// returns the first answer: the first datagram back with the query's ID.
fn over_udp(query: &[u8], shared: &Shared) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; MAX];
    for &resolver in &shared.resolvers {
        let Ok(socket) = UdpSocket::bind(unspecified(resolver)) else {
            continue;
        };
        if socket
            .connect(resolver)
            .and_then(|()| socket.send(query))
            .is_err()
        {
            continue;
        }
        let deadline = Instant::now() + shared.wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || socket.set_read_timeout(Some(left)).is_err() {
                break;
            }
            match socket.recv(&mut buf) {
                Ok(n) if same_id(query, &buf[..n]) => return Some(buf[..n].to_vec()),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // Timed out, or refused: the next one.
                Err(_) => break,
            }
        }
    }
    None
}

// The same over TCP, on a connection of the query's own.
fn over_tcp(query: &[u8], shared: &Shared) -> Option<Vec<u8>> {
    for resolver in &shared.resolvers {
        let asked = TcpStream::connect_timeout(resolver, shared.wait).and_then(|mut conn| {
            conn.set_read_timeout(Some(shared.wait))?;
            conn.set_write_timeout(Some(shared.wait))?;
            write_message(&mut conn, query)?;
            read_message(&mut conn)
        });
        match asked {
            Ok(answer) if same_id(query, &answer) => return Some(answer),
            _ => {}
        }
    }
    None
}

fn unspecified(to: SocketAddr) -> SocketAddr {
    match to {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    }
}

// Whether `answer` is a DNS message with the query's ID.
fn same_id(query: &[u8], answer: &[u8]) -> bool {
    answer.len() >= HEADER && answer[..2] == query[..2]
}

// A message over TCP: its length in two bytes, then it.
fn read_message(conn: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    conn.read_exact(&mut len)?;
    let mut msg = vec![0u8; usize::from(u16::from_be_bytes(len))];
    conn.read_exact(&mut msg)?;
    Ok(msg)
}

fn write_message(conn: &mut TcpStream, msg: &[u8]) -> io::Result<()> {
    let len = u16::try_from(msg.len()).map_err(|_| io::Error::other("a message too long"))?;
    let mut framed = Vec::with_capacity(2 + msg.len());
    framed.extend(len.to_be_bytes());
    framed.extend(msg);
    conn.write_all(&framed)
}
