// A VM's DNS, which its boot serves on the VM's gateway, port 53, over UDP
// and TCP, for as long as the boot is up, asleep included. The guest knows
// no other server (Net::boot_arg). Each query goes on to the host's
// resolvers (net::dns), one after another until one answers, and their
// answer comes back unchanged.
//
// Unless the VM has a list of names (Store::allow), which the boot reads
// before it serves and each second after. Then only a name on the list, or
// under one, goes on, as one plain question: everything else gets REFUSED
// from here, so data can't leave in a name either. A listed name's answer
// counts whatever CNAMEs it goes through, and each A record in it is handed
// on (Policy::hand_on) before the guest gets it. AAAA is answered empty
// here: VMs have no IPv6. api.anthropic.com is on every list (ALWAYS).
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
use hickory_proto::op::{Edns, Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{DNSClass, Name, RData, RecordType};
use hickory_proto::serialize::binary::{BinDecodable, BinDecoder};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{self, AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, sockopt};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket,
};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
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
// How often the relay reads the VM's list, as url.rs reads its URL setting.
const LIST_LOOK: Duration = Duration::from_secs(1);
// The UDP payload this relay's own answers say it takes, DNS Flag Day 2020's.
const PAYLOAD: u16 = 1232;
// A DNS message's header: anything shorter isn't one.
const HEADER: usize = 12;
// The most a DNS message can be, over UDP or TCP.
const MAX: usize = 65535;

/// The names on every list, so Claude Code keeps working on a listed VM.
pub const ALWAYS: [&str; 1] = ["api.anthropic.com"];

/// A name a VM's list holds, covering every name under it: a hostname of
/// two labels or more, each of letters, digits and hyphens, kept lowercase
/// and with no trailing dot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hostname(String);

/// A name refused for a list, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{name:?} isn't a hostname: {why}")]
pub struct BadHostname {
    pub name: String,
    pub why: String,
}

impl Hostname {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    // Its labels, last first.
    fn labels(&self) -> impl Iterator<Item = &[u8]> {
        self.0.rsplit('.').map(str::as_bytes)
    }

    // Whether `labels`, last first, are this name's or a name's under it.
    fn covers<'a>(&self, mut labels: impl Iterator<Item = &'a [u8]>) -> bool {
        self.labels()
            .all(|want| labels.next().is_some_and(|l| l.eq_ignore_ascii_case(want)))
    }
}

impl FromStr for Hostname {
    type Err = BadHostname;

    fn from_str(name: &str) -> Result<Hostname, BadHostname> {
        let bad = |why: String| {
            Err(BadHostname {
                name: name.to_string(),
                why,
            })
        };
        let lower = name.to_ascii_lowercase();
        let host = lower.strip_suffix('.').unwrap_or(&lower);
        if host.is_empty() {
            return bad("it's empty".into());
        }
        if host.parse::<IpAddr>().is_ok() {
            return bad("it's an address, and a list holds names".into());
        }
        if host.contains('*') {
            return bad("no wildcard is needed: a name covers every name under it".into());
        }
        if host.len() > 253 {
            return bad("it's longer than DNS's 253 characters".into());
        }
        let labels: Vec<&str> = host.split('.').collect();
        if labels.iter().any(|label| label.is_empty()) {
            return bad("it has an empty label, between two dots or at an end".into());
        }
        for label in &labels {
            if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return bad(
                    "use only letters, digits and hyphens between the dots, and a name's \
                     xn-- form for other scripts"
                        .into(),
                );
            }
            if label.len() > 63 {
                return bad(format!("its label {label:?} is longer than 63 characters"));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return bad(format!("its label {label:?} starts or ends with a hyphen"));
            }
        }
        if labels.len() < 2 {
            return bad("a name of one label, like com, would cover every name under it".into());
        }
        if labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit())) {
            return bad("it ends in a number, like an address".into());
        }
        Ok(Hostname(host.to_string()))
    }
}

impl TryFrom<String> for Hostname {
    type Error = BadHostname;

    fn try_from(name: String) -> Result<Hostname, BadHostname> {
        name.parse()
    }
}

impl From<Hostname> for String {
    fn from(host: Hostname) -> String {
        host.0
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A VM's list as its boot uses it: the names it answers, each with every
/// name under it, and ALWAYS's besides. Sorted, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List(Vec<Hostname>);

impl List {
    pub fn new(names: impl IntoIterator<Item = Hostname>) -> List {
        let always = ALWAYS.iter().map(|name| Hostname(name.to_string()));
        let mut names: Vec<Hostname> = names.into_iter().chain(always).collect();
        names.sort();
        names.dedup();
        List(names)
    }

    pub fn names(&self) -> &[Hostname] {
        &self.0
    }

    /// The name on the list that covers `host`, if one does: `host`, or one
    /// it's under.
    pub fn covering(&self, host: &Hostname) -> Option<&Hostname> {
        self.0.iter().find(|name| name.covers(host.labels()))
    }

    // Whether a query's name is on the list, or under a name on it.
    fn answers(&self, name: &Name) -> bool {
        let labels: Vec<&[u8]> = name.iter().collect();
        self.0
            .iter()
            .any(|host| host.covers(labels.iter().rev().copied()))
    }
}

/// An address an allowed answer gave, and for how long it said to keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub ip: Ipv4Addr,
    pub ttl: Duration,
}

/// Reads the VM's list: None if it has none and every name is answered.
pub type ReadList = Box<dyn Fn() -> Result<Option<List>, String> + Send>;
/// Takes each allowed answer's addresses before the guest gets the answer.
pub type HandOn = Arc<dyn Fn(&[Address]) + Send + Sync>;

/// What a relay answers.
pub struct Policy {
    /// The VM's list, read before the relay serves and each second after. A
    /// read that fails keeps the last list.
    pub list: ReadList,
    /// Where a listed VM's allowed answers' addresses go.
    pub hand_on: HandOn,
}

impl Default for Policy {
    /// Every name, as for a VM with no list, and nothing handed on.
    fn default() -> Policy {
        Policy {
            list: Box::new(|| Ok(None)),
            hand_on: Arc::new(|_| {}),
        }
    }
}

/// What a listed VM's boot does with a query from its guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Passes it on to the host's resolvers.
    Ask,
    /// Answers it from here: REFUSED, an empty answer, or FORMERR.
    Answer(Vec<u8>),
    /// Nothing: it isn't even a DNS header.
    Drop,
}

/// What to do with `query` under `list`. Only one plain question for class
/// IN, for a name on the list or under one, is asked; its AAAA is answered
/// empty, and the rest REFUSED. What doesn't parse whole is FORMERR.
pub fn judge(query: &[u8], list: &List) -> Verdict {
    if query.len() < HEADER {
        return Verdict::Drop;
    }
    let mut decoder = BinDecoder::new(query);
    let msg = match Message::read(&mut decoder) {
        Ok(msg) if decoder.is_empty() => msg,
        _ => return Verdict::Answer(header_only(query, ResponseCode::FormErr)),
    };
    let meta = &msg.metadata;
    let plain = meta.message_type == MessageType::Query
        && meta.op_code == OpCode::Query
        && msg.answers.is_empty()
        && msg.authorities.is_empty()
        && msg.additionals.is_empty()
        && msg.signature.is_none();
    let question = match msg.queries.as_slice() {
        [q] if plain => q,
        _ => return reply(query, &msg, ResponseCode::Refused),
    };
    if question.query_class != DNSClass::IN || !list.answers(&question.name) {
        return reply(query, &msg, ResponseCode::Refused);
    }
    if question.query_type == RecordType::AAAA {
        return reply(query, &msg, ResponseCode::NoError);
    }
    Verdict::Ask
}

/// The A records in an answer, with their TTLs.
pub fn addresses(answer: &[u8]) -> Vec<Address> {
    let Ok(msg) = Message::from_vec(answer) else {
        return Vec::new();
    };
    msg.answers
        .iter()
        .filter(|record| record.dns_class == DNSClass::IN)
        .filter_map(|record| match &record.data {
            RData::A(a) => Some(Address {
                ip: a.0,
                ttl: Duration::from_secs(record.ttl.into()),
            }),
            _ => None,
        })
        .collect()
}

// This relay's own answer to `msg`: its question, as asked, and no records.
fn reply(query: &[u8], msg: &Message, code: ResponseCode) -> Verdict {
    let mut answer = Message::response(msg.metadata.id, msg.metadata.op_code);
    answer.metadata.recursion_desired = msg.metadata.recursion_desired;
    answer.metadata.recursion_available = true;
    answer.metadata.response_code = code;
    answer.queries = msg.queries.clone();
    if msg.edns.is_some() {
        let mut edns = Edns::new();
        edns.set_max_payload(PAYLOAD);
        answer.edns = Some(edns);
    }
    Verdict::Answer(
        answer
            .to_vec()
            .unwrap_or_else(|_| header_only(query, code)),
    )
}

// An answer that's only a header: the query's ID, and `code`.
fn header_only(query: &[u8], code: ResponseCode) -> Vec<u8> {
    let mut answer = vec![0u8; HEADER];
    answer[..2].copy_from_slice(&query[..2]);
    answer[2] = 0x80;
    answer[3] = code.low();
    answer
}

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
    list: RwLock<Option<Arc<List>>>,
    hand_on: HandOn,
}

impl Relay {
    /// Serves `net`'s VM on its gateway's port 53, on its tap, and passes
    /// each query `policy` allows on to `resolvers`, on their port 53.
    pub fn bind(net: &Net, resolvers: &[Ipv4Addr], policy: Policy) -> io::Result<Relay> {
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
            policy,
        )
    }

    /// Serves `guest`, and only it, on these sockets, waiting `wait` on each
    /// resolver in turn. It reads the list first, and doesn't serve if it
    /// can't.
    pub fn serve(
        udp: UdpSocket,
        tcp: TcpListener,
        guest: IpAddr,
        resolvers: Vec<SocketAddr>,
        wait: Duration,
        policy: Policy,
    ) -> io::Result<Relay> {
        let Policy { list: read, hand_on } = policy;
        let first = read().map_err(io::Error::other)?;
        udp.set_read_timeout(Some(LOOK))?;
        tcp.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            guest,
            resolvers,
            wait,
            stop: AtomicBool::new(false),
            busy: AtomicUsize::new(0),
            list: RwLock::new(first.map(Arc::new)),
            hand_on,
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
        let on_list = shared.clone();
        relay.loops.push(
            thread::Builder::new()
                .name("dns-list".into())
                .spawn(move || list_loop(&read, &on_list))?,
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
                if let Some(answer) = answer(&query, &slot.0, over_udp) {
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

// Reads the list each second, until the relay stops. One it can't read
// leaves the last in place, and is noted once.
fn list_loop(read: &ReadList, shared: &Shared) {
    let mut said: Option<String> = None;
    let mut looked = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        let next = LIST_LOOK.saturating_sub(looked.elapsed());
        if !next.is_zero() {
            thread::sleep(next.min(LOOK));
            continue;
        }
        looked = Instant::now();
        match read() {
            Ok(list) => {
                *shared.list.write().unwrap_or_else(PoisonError::into_inner) = list.map(Arc::new);
                said = None;
            }
            Err(why) if said.as_ref() != Some(&why) => {
                note(format_args!("{why}; its DNS keeps the last list it read"));
                said = Some(why);
            }
            Err(_) => {}
        }
    }
}

// The answer to a query from the guest, if any: `ask`'s, if the list lets
// it be asked, after its addresses are handed on.
fn answer(
    query: &[u8],
    shared: &Shared,
    ask: fn(&[u8], &Shared) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let list = shared
        .list
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let Some(list) = list else {
        return ask(query, shared);
    };
    match judge(query, &list) {
        Verdict::Drop => None,
        Verdict::Answer(answer) => Some(answer),
        Verdict::Ask => {
            let answer = ask(query, shared)?;
            let got = addresses(&answer);
            if !got.is_empty() {
                (shared.hand_on)(&got);
            }
            Some(answer)
        }
    }
}

// As boot's notes: under the unit, stderr is the journal stream Firecracker
// made non-blocking, where eprintln! would panic.
fn note(msg: fmt::Arguments) {
    let _ = writeln!(io::stderr(), "runner: {msg}");
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
        let Some(answer) = answer(&query, shared, over_tcp) else {
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
