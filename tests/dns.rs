// A VM's DNS as its boot relays it: each query passed on to the host's
// resolvers, and their answer passed back unchanged. These run on localhost,
// with fake resolvers standing in for the host's and 127.0.0.1 for the VM;
// tests/net.rs's root test serves a VM's own gateway, on its tap.
mod common;

use common::resolver::{Resolver, answer_to, query, read_framed, same_port, write_framed};
use common::wait_for;
use proptest::prelude::*;
use runner::dns::Relay;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const GUEST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
// How long the relay waits on each resolver, short so the tests are quick.
const WAIT: Duration = Duration::from_millis(300);
// How long a test waits for an answer that should come.
const ANSWERED: Duration = Duration::from_secs(5);

// A relay for the guest at 127.0.0.1, to these resolvers, and where it
// listens: the same port over UDP and TCP, as on a VM's gateway.
fn relay(resolvers: &[SocketAddr]) -> (Relay, SocketAddr) {
    let (udp, tcp) = same_port("127.0.0.1");
    let at = udp.local_addr().unwrap();
    let relay = Relay::serve(udp, tcp, GUEST, resolvers.to_vec(), WAIT).unwrap();
    (relay, at)
}

// Asks `relay` over UDP from `from`, and returns what comes back within
// `within`, if anything.
fn ask_udp(from: &str, relay: SocketAddr, q: &[u8], within: Duration) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind((from, 0)).unwrap();
    socket.send_to(q, relay).unwrap();
    socket.set_read_timeout(Some(within)).unwrap();
    let mut buf = [0u8; 65536];
    match socket.recv_from(&mut buf) {
        Ok((n, _)) => Some(buf[..n].to_vec()),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => None,
        Err(e) => panic!("asking {relay}: {e}"),
    }
}

// A TCP connection to `relay` from `from`, that gives up reading after
// `within`.
fn connect(from: &str, relay: SocketAddr, within: Duration) -> TcpStream {
    let conn = match from {
        "127.0.0.1" => TcpStream::connect_timeout(&relay, ANSWERED).unwrap(),
        _ => connect_from(from, relay),
    };
    conn.set_read_timeout(Some(within)).unwrap();
    conn
}

// std can't bind a TCP client before connecting, so this goes through nix.
fn connect_from(from: &str, relay: SocketAddr) -> TcpStream {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, SockaddrIn, bind, connect, socket};
    use std::os::fd::AsRawFd;
    let fd = socket(
        AddressFamily::Inet,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .unwrap();
    let local: SocketAddr = format!("{from}:0").parse().unwrap();
    let SocketAddr::V4(local) = local else {
        unreachable!()
    };
    bind(fd.as_raw_fd(), &SockaddrIn::from(local)).unwrap();
    let SocketAddr::V4(remote) = relay else {
        unreachable!()
    };
    connect(fd.as_raw_fd(), &SockaddrIn::from(remote)).unwrap();
    TcpStream::from(fd)
}

#[test]
fn a_udp_query_reaches_the_resolver_and_its_answer_comes_back_unchanged() {
    let resolver = Resolver::start(true);
    let (_relay, at) = relay(&[resolver.at]);
    let q = query(0x1234, "example.com");
    let got = ask_udp("127.0.0.1", at, &q, ANSWERED);
    assert_eq!(got, Some(answer_to(&q)));
    assert_eq!(resolver.heard(), [q]);
}

#[test]
fn a_tcp_query_reaches_the_resolver_and_its_answer_comes_back_unchanged() {
    let resolver = Resolver::start(true);
    let (_relay, at) = relay(&[resolver.at]);
    let mut conn = connect("127.0.0.1", at, ANSWERED);
    // Two on one connection, as a resolver may send.
    let (q1, q2) = (query(1, "example.com"), query(2, "github.com"));
    for q in [&q1, &q2] {
        write_framed(&mut conn, q).unwrap();
        assert_eq!(read_framed(&mut conn).unwrap(), answer_to(q));
    }
    assert_eq!(resolver.heard(), [q1, q2]);
}

// The relay tries each resolver in turn, a while each, as a stub resolver
// would, since the guest knows only the one: its gateway.
#[test]
fn a_resolver_that_never_answers_is_passed_over() {
    let (silent, live) = (Resolver::start(false), Resolver::start(true));
    let (_relay, at) = relay(&[silent.at, live.at]);
    let q = query(7, "example.com");
    assert_eq!(ask_udp("127.0.0.1", at, &q, ANSWERED), Some(answer_to(&q)));
    let mut conn = connect("127.0.0.1", at, ANSWERED);
    write_framed(&mut conn, &q).unwrap();
    assert_eq!(read_framed(&mut conn).unwrap(), answer_to(&q));
    // It heard both, though maybe only just.
    wait_for("the silent resolver to hear both", || {
        silent.heard() == [q.clone(), q.clone()]
    });
    assert_eq!(live.heard(), [q.clone(), q]);
}

// A query no resolver answers holds up neither the queries after it nor the
// relay's end.
#[test]
fn a_resolver_that_never_answers_doesn_t_hang_the_relay() {
    let (silent, live) = (Resolver::start(false), Resolver::start(true));
    let (relay, at) = relay(&[silent.at, live.at]);
    // Each waits out the silent one, at the same time, not in turn: in turn,
    // 16 would take 16 waits.
    let began = Instant::now();
    let asks: Vec<_> = (0..16)
        .map(|id| {
            thread::spawn(move || {
                (
                    id,
                    ask_udp("127.0.0.1", at, &query(id, "a.example"), ANSWERED),
                )
            })
        })
        .collect();
    for ask in asks {
        let (id, got) = ask.join().unwrap();
        assert_eq!(got, Some(answer_to(&query(id, "a.example"))));
    }
    assert!(began.elapsed() < WAIT * 8, "{:?}", began.elapsed());
    drop(relay);

    // With only the silent one, nothing comes back, over either.
    let (relay, at) = self::relay(&[silent.at]);
    let q = query(9, "example.com");
    assert_eq!(ask_udp("127.0.0.1", at, &q, WAIT * 3), None);
    let mut conn = connect("127.0.0.1", at, ANSWERED);
    write_framed(&mut conn, &q).unwrap();
    let mut rest = Vec::new();
    // Closed with nothing said, so the guest tries again.
    assert_eq!(conn.read_to_end(&mut rest).unwrap(), 0);
    // A guest's TCP connection that's under way doesn't hold the end up.
    let mut idle = connect("127.0.0.1", at, WAIT);
    write_framed(&mut idle, &q).unwrap();
    let began = Instant::now();
    drop(relay);
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    // And it's gone: nothing listens there now.
    assert_eq!(ask_udp("127.0.0.1", at, &q, WAIT * 2), None);
    assert!(TcpStream::connect_timeout(&at, WAIT).is_err());
}

// Only the VM's own address is answered: nothing from another reaches the
// resolver, nor is answered.
#[test]
fn the_relay_answers_only_its_guest() {
    let resolver = Resolver::start(true);
    let (_relay, at) = relay(&[resolver.at]);
    let q = query(3, "example.com");
    assert_eq!(ask_udp("127.0.0.2", at, &q, WAIT * 2), None);
    let mut conn = connect("127.0.0.2", at, WAIT * 2);
    // The relay may close it before the query is even written.
    let _ = write_framed(&mut conn, &q);
    let mut rest = Vec::new();
    assert!(matches!(conn.read_to_end(&mut rest), Ok(0) | Err(_)));
    assert!(rest.is_empty());
    assert!(resolver.heard().is_empty(), "{:?}", resolver.heard());
    // The guest still is.
    assert_eq!(ask_udp("127.0.0.1", at, &q, ANSWERED), Some(answer_to(&q)));
}

// What isn't even a DNS header isn't passed on.
#[test]
fn a_datagram_shorter_than_a_header_is_dropped() {
    let resolver = Resolver::start(true);
    let (_relay, at) = relay(&[resolver.at]);
    assert_eq!(ask_udp("127.0.0.1", at, b"short", WAIT * 2), None);
    let q = query(4, "example.com");
    assert_eq!(ask_udp("127.0.0.1", at, &q, ANSWERED), Some(answer_to(&q)));
    assert_eq!(resolver.heard(), [q]);
}

// One relay, and its resolver, for every case below.
fn shared_relay() -> SocketAddr {
    static RELAY: OnceLock<(Relay, SocketAddr)> = OnceLock::new();
    RELAY
        .get_or_init(|| {
            let resolver = Resolver::start(true);
            let at = resolver.at;
            // Kept running for the rest of the tests.
            std::mem::forget(resolver);
            relay(&[at])
        })
        .1
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // Whatever bytes the guest sends, over UDP or TCP, the relay goes on
    // answering.
    #[test]
    fn no_bytes_from_the_guest_stop_the_relay(
        datagram in proptest::collection::vec(any::<u8>(), 0..600),
        stream in proptest::collection::vec(any::<u8>(), 0..600),
    ) {
        let at = shared_relay();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.send_to(&datagram, at).unwrap();
        let mut conn = connect("127.0.0.1", at, ANSWERED);
        let _ = conn.write_all(&stream);
        drop(conn);
        let q = query(5, "example.com");
        prop_assert_eq!(ask_udp("127.0.0.1", at, &q, ANSWERED), Some(answer_to(&q)));
        let mut conn = connect("127.0.0.1", at, ANSWERED);
        write_framed(&mut conn, &q).unwrap();
        prop_assert_eq!(read_framed(&mut conn).unwrap(), answer_to(&q));
    }
}
