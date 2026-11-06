// A VM's DNS as its boot relays it: each query passed on to the host's
// resolvers, and their answer passed back unchanged. With a list, only its
// names are. These run on localhost, with fake resolvers standing in for the
// host's and 127.0.0.1 for the VM; tests/net.rs's root test serves a VM's own
// gateway, on its tap.
mod common;

use common::resolver::{
    AAAA, Resolver, TXT, answer_to, query, query_for, read_framed, same_port, write_framed,
};
use common::wait_for;
use hickory_proto::rr::Name;
use proptest::prelude::*;
use runner::dns::{self, Address, Hostname, List, Policy, Relay, Verdict};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
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
    let policy = Policy::default();
    let relay = Relay::serve(udp, tcp, GUEST, resolvers.to_vec(), WAIT, policy).unwrap();
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

// A list of these names, and api.anthropic.com, which every list has.
fn list(names: &[&str]) -> List {
    List::new(names.iter().map(|n| n.parse::<Hostname>().unwrap()))
}

// A relay whose list is whatever `list` holds each time it looks, and the
// addresses it has handed on, with the names they were asked for.
struct Listed {
    relay: Relay,
    at: SocketAddr,
    list: Arc<Mutex<Result<Option<List>, String>>>,
    handed: Arc<Mutex<Vec<Address>>>,
    asked: Arc<Mutex<Vec<Name>>>,
}

fn listed(resolvers: &[SocketAddr], first: Option<List>) -> Listed {
    let (udp, tcp) = same_port("127.0.0.1");
    let at = udp.local_addr().unwrap();
    let list = Arc::new(Mutex::new(Ok(first)));
    let handed = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let policy = Policy {
        list: Box::new({
            let list = list.clone();
            move || list.lock().unwrap().clone()
        }),
        hand_on: Arc::new({
            let (handed, asked) = (handed.clone(), asked.clone());
            move |name: &Name, got: &[Address]| {
                asked.lock().unwrap().push(name.clone());
                handed.lock().unwrap().extend_from_slice(got);
            }
        }),
    };
    let relay = Relay::serve(udp, tcp, GUEST, resolvers.to_vec(), WAIT, policy).unwrap();
    Listed {
        relay,
        at,
        list,
        handed,
        asked,
    }
}

impl Listed {
    fn handed(&self) -> Vec<Address> {
        self.handed.lock().unwrap().clone()
    }
}

// An answer's response code, after checking it answers `q`: its ID, marked a
// response, and its question.
fn rcode(q: &[u8], answer: &[u8]) -> u8 {
    assert!(answer.len() >= 12, "{answer:?}");
    assert_eq!(answer[..2], q[..2], "not the query's ID");
    assert_ne!(answer[2] & 0x80, 0, "not a response");
    answer[3] & 0x0f
}

// How many records an answer's answer section holds.
fn answers(answer: &[u8]) -> u16 {
    u16::from_be_bytes([answer[6], answer[7]])
}

const NOERROR: u8 = 0;
const FORMERR: u8 = 1;
const REFUSED: u8 = 5;

// Asks over UDP and over TCP, and returns both answers.
fn both(at: SocketAddr, q: &[u8]) -> [Vec<u8>; 2] {
    let udp = ask_udp("127.0.0.1", at, q, ANSWERED).expect("no answer over UDP");
    let mut conn = connect("127.0.0.1", at, ANSWERED);
    write_framed(&mut conn, q).unwrap();
    [udp, read_framed(&mut conn).unwrap()]
}

#[test]
fn with_no_list_every_name_is_answered_and_nothing_is_handed_on() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], None);
    for (id, name) in [(1, "example.com"), (2, "github.com")] {
        let q = query(id, name);
        assert_eq!(both(vm.at, &q), [answer_to(&q), answer_to(&q)]);
    }
    // Whatever it is, unchanged.
    let q = query_for(3, "example.com", AAAA);
    assert_eq!(both(vm.at, &q), [answer_to(&q), answer_to(&q)]);
    assert_eq!(resolver.heard().len(), 6);
    assert_eq!(vm.handed(), []);
}

#[test]
fn a_listed_name_and_those_under_it_are_answered_and_the_rest_refused_unasked() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], Some(list(&["github.com"])));
    let allowed = [
        "github.com",
        "api.github.com",
        "GitHub.COM",
        "api.anthropic.com",
    ];
    for (id, name) in (10..).zip(allowed) {
        let q = query(id, name);
        assert_eq!(both(vm.at, &q), [answer_to(&q), answer_to(&q)], "{name}");
    }
    let refused = [
        "example.com",
        "evilgithub.com",
        "github.com.evil.com",
        "com",
        "anthropic.com",
    ];
    for (id, name) in (20..).zip(refused) {
        let q = query(id, name);
        for answer in both(vm.at, &q) {
            assert_eq!(rcode(&q, &answer), REFUSED, "{name}");
            assert_eq!(answers(&answer), 0, "{name}");
            // Its question, as asked.
            assert_eq!(answer[12..], q[12..], "{name}");
        }
    }
    // Only the allowed ones, twice each, reached the resolver.
    let heard = resolver.heard();
    assert_eq!(heard.len(), 2 * allowed.len(), "{heard:?}");
    for (id, name) in (10..).zip(allowed) {
        assert_eq!(heard.iter().filter(|h| **h == query(id, name)).count(), 2);
    }
}

#[test]
fn an_allowed_answer_s_addresses_are_handed_on_before_the_guest_gets_it() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], Some(list(&["github.com"])));
    let q = query(1, "github.com");
    assert_eq!(
        ask_udp("127.0.0.1", vm.at, &q, ANSWERED),
        Some(answer_to(&q))
    );
    // Already there as the answer comes: the fake's one A record, its TTL 60.
    let one = Address {
        ip: Ipv4Addr::new(192, 0, 2, 1),
        ttl: Duration::from_secs(60),
    };
    assert_eq!(vm.handed(), [one]);
    // With the name asked, for the rules to check against the list then.
    let asked = vm.asked.lock().unwrap().clone();
    assert_eq!(asked, [Name::from_ascii("github.com.").unwrap()]);
    let mut conn = connect("127.0.0.1", vm.at, ANSWERED);
    write_framed(&mut conn, &q).unwrap();
    assert_eq!(read_framed(&mut conn).unwrap(), answer_to(&q));
    assert_eq!(vm.handed(), [one, one]);
    // A refused name hands on nothing.
    ask_udp("127.0.0.1", vm.at, &query(2, "example.com"), ANSWERED).unwrap();
    assert_eq!(vm.handed().len(), 2);
}

// VMs have no IPv6, so a listed name's AAAA is answered empty, here. Other
// types go on to the resolver.
#[test]
fn a_listed_name_s_aaaa_gets_an_empty_answer_unasked() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], Some(list(&["github.com"])));
    let q = query_for(1, "api.github.com", AAAA);
    for answer in both(vm.at, &q) {
        assert_eq!(rcode(&q, &answer), NOERROR);
        assert_eq!(answers(&answer), 0);
        assert_eq!(answer[12..12 + q.len() - 12], q[12..]);
    }
    assert_eq!(resolver.heard(), Vec::<Vec<u8>>::new());
    let txt = query_for(2, "github.com", TXT);
    assert_eq!(both(vm.at, &txt), [answer_to(&txt), answer_to(&txt)]);
    // An unlisted one's AAAA is refused like the rest.
    let q = query_for(3, "example.com", AAAA);
    assert_eq!(rcode(&q, &both(vm.at, &q)[0]), REFUSED);
    assert_eq!(resolver.heard(), [txt.clone(), txt]);
}

// Data can't leave in what else a query carries, either: a listed VM's boot
// passes on only one plain question, for class IN.
#[test]
fn a_listed_vm_s_query_that_is_not_one_plain_question_is_not_passed_on() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], Some(list(&["github.com"])));
    let plain = query(1, "github.com");
    let edit = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut q = plain.clone();
        f(&mut q);
        q
    };
    // Two questions: github.com, then one under it, pointing back at the first.
    let two = edit(&|q| {
        q[5] = 2;
        q.extend([3, b'a', b'p', b'i', 0xc0, 12, 0, 1, 0, 1]);
    });
    let cases = [
        ("two questions", two, REFUSED),
        ("a response", edit(&|q| q[2] |= 0x80), REFUSED),
        ("an update", edit(&|q| q[2] |= 5 << 3), REFUSED),
        ("class CH", edit(&|q| *q.last_mut().unwrap() = 3), REFUSED),
        (
            "an answer record",
            edit(&|q| {
                q[7] = 1;
                q.extend([
                    0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60, 0, 4, 3, b'h', b'i', b'!',
                ]);
            }),
            REFUSED,
        ),
        (
            "bytes past the question",
            edit(&|q| q.extend(b"leak")),
            FORMERR,
        ),
        (
            "a cut-off question",
            plain[..plain.len() - 3].to_vec(),
            FORMERR,
        ),
    ];
    for (what, q, want) in cases {
        let answer = ask_udp("127.0.0.1", vm.at, &q, ANSWERED);
        let answer = answer.unwrap_or_else(|| panic!("{what}: no answer"));
        assert_eq!(rcode(&q, &answer), want, "{what}");
    }
    assert_eq!(resolver.heard(), Vec::<Vec<u8>>::new());
    // And the plain one still goes.
    assert_eq!(both(vm.at, &plain), [answer_to(&plain), answer_to(&plain)]);
}

// Nor in what rides along with a listed name: what goes on is rebuilt from
// its question, with the guest's ID, its recursion-desired bit, and an OPT
// only if it sent one, saying just its DO bit and a payload size.
#[test]
fn a_listed_vm_s_allowed_query_goes_on_rebuilt_without_what_else_it_carried() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], Some(list(&["github.com"])));
    let plain = query(1, "github.com");
    let secret = b"AWS_SECRET_ACCESS_KEY=xyz";
    let mut q = plain.clone();
    // AD and the reserved Z bit set, and an OPT record, DO and a 4096-byte
    // payload, with an option of its own.
    q[3] |= 0x60;
    q[11] = 1;
    q.extend([0, 0, 41, 0x10, 0, 0, 0, 0x80, 0]);
    let len = u16::try_from(secret.len()).unwrap();
    let option = [&0xfde9u16.to_be_bytes()[..], &len.to_be_bytes(), secret].concat();
    q.extend(u16::try_from(option.len()).unwrap().to_be_bytes());
    q.extend(&option);
    for answer in both(vm.at, &q) {
        assert_eq!(rcode(&q, &answer), NOERROR);
        assert_eq!(answers(&answer), 1);
    }
    let heard = resolver.heard();
    assert_eq!(heard.len(), 2);
    let mut header = plain[..12].to_vec();
    header[11] = 1;
    for h in heard {
        assert!(!h.windows(secret.len()).any(|w| w == secret), "{h:?}");
        assert_eq!(h[..12], header);
        assert_eq!(h[12..plain.len()], plain[12..]);
        assert_eq!(
            h[plain.len()..],
            [0, 0, 41, 0x04, 0xd0, 0, 0, 0x80, 0, 0, 0]
        );
    }
    // One with no OPT goes on as it came.
    assert_eq!(both(vm.at, &plain), [answer_to(&plain), answer_to(&plain)]);
}

// A listed name's answer counts whatever CNAMEs it goes through: a CDN's
// addresses are handed on too.
#[test]
fn a_listed_name_s_addresses_are_handed_on_through_its_cnames() {
    let q = query(1, "github.com");
    let mut a = q.clone();
    a[2] |= 0x80;
    a[7] = 2;
    // github.com is cdn.example.net, whose name is at offset 40, the CNAME's
    // data; and cdn.example.net is 203.0.113.7 for 30s.
    a.extend([0xc0, 12, 0, 5, 0, 1, 0, 0, 1, 0, 0, 17]);
    a.extend([
        3, b'c', b'd', b'n', 7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'n', b'e', b't', 0,
    ]);
    a.extend([0xc0, 40, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 203, 0, 113, 7]);
    assert!(matches!(
        dns::judge(&q, &list(&["github.com"])),
        Verdict::Ask(_)
    ));
    assert_eq!(
        dns::addresses(&a),
        [Address {
            ip: Ipv4Addr::new(203, 0, 113, 7),
            ttl: Duration::from_secs(30)
        }]
    );
}

#[test]
fn a_list_changed_while_the_relay_runs_is_used_within_a_second() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], None);
    let q = query(1, "example.com");
    assert_eq!(
        ask_udp("127.0.0.1", vm.at, &q, ANSWERED),
        Some(answer_to(&q))
    );
    let refused = |at| {
        let answer = ask_udp("127.0.0.1", at, &q, ANSWERED).unwrap();
        rcode(&q, &answer) == REFUSED
    };
    // A second, and a little for this test's own asking.
    let within = Duration::from_millis(1500);
    *vm.list.lock().unwrap() = Ok(Some(list(&["github.com"])));
    let began = Instant::now();
    wait_for("the list to be used", || refused(vm.at));
    assert!(began.elapsed() < within, "{:?}", began.elapsed());
    // Taken away again: every name.
    *vm.list.lock().unwrap() = Ok(None);
    let began = Instant::now();
    wait_for("the list to go", || !refused(vm.at));
    assert!(began.elapsed() < within, "{:?}", began.elapsed());
    // A list that can't be read leaves the last one in place.
    *vm.list.lock().unwrap() = Ok(Some(list(&["github.com"])));
    wait_for("the list again", || refused(vm.at));
    *vm.list.lock().unwrap() = Err("config.json: unreadable".to_string());
    thread::sleep(within);
    assert!(refused(vm.at));
    drop(vm.relay);
}

// The relay reads its list before it serves, so the very first query is
// filtered; one it can't read stops it from starting.
#[test]
fn the_list_is_read_before_the_first_query_and_an_unreadable_one_stops_the_relay() {
    let resolver = Resolver::start(true);
    let vm = listed(&[resolver.at], Some(list(&["github.com"])));
    let q = query(1, "example.com");
    let answer = ask_udp("127.0.0.1", vm.at, &q, ANSWERED).unwrap();
    assert_eq!(rcode(&q, &answer), REFUSED);

    let (udp, tcp) = same_port("127.0.0.1");
    let policy = Policy {
        list: Box::new(|| Err("config.json: unreadable".to_string())),
        ..Policy::default()
    };
    let err = Relay::serve(udp, tcp, GUEST, vec![resolver.at], WAIT, policy)
        .err()
        .expect("served with no list read");
    assert!(err.to_string().contains("unreadable"), "{err}");
}

#[test]
fn what_the_relay_does_with_a_query_is_the_verdict() {
    let github = list(&["github.com"]);
    let q = query(1, "www.github.com");
    assert_eq!(dns::judge(&q, &github), Verdict::Ask(q.clone()));
    let Verdict::Answer(refused) = dns::judge(&query(2, "example.com"), &github) else {
        panic!("example.com asked");
    };
    assert_eq!(rcode(&query(2, "example.com"), &refused), REFUSED);
    // An A record's address and TTL, from an answer.
    let got = dns::addresses(&answer_to(&q));
    assert_eq!(
        got,
        [Address {
            ip: Ipv4Addr::new(192, 0, 2, 1),
            ttl: Duration::from_secs(60)
        }]
    );
    assert_eq!(dns::addresses(b"not an answer"), []);
}

// One listed relay, and its resolver, for every case below.
fn shared_listed_relay() -> SocketAddr {
    static RELAY: OnceLock<Listed> = OnceLock::new();
    RELAY
        .get_or_init(|| {
            let resolver = Resolver::start(true);
            let at = resolver.at;
            std::mem::forget(resolver);
            listed(&[at], Some(list(&["example.com"])))
        })
        .at
}

// A plain query with one byte changed somewhere.
fn flipped() -> impl Strategy<Value = Vec<u8>> {
    let q = query(9, "api.github.com");
    let len = q.len();
    (0..len, any::<u8>()).prop_map(move |(at, byte)| {
        let mut q = q.clone();
        q[at] = byte;
        q
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // No bytes from the guest panic the parser, and whatever it answers
    // itself answers the query's ID.
    #[test]
    fn no_bytes_from_the_guest_panic_the_parser(
        bytes in prop_oneof![proptest::collection::vec(any::<u8>(), 0..600), flipped()],
    ) {
        let github = list(&["github.com"]);
        if let Verdict::Answer(answer) = dns::judge(&bytes, &github) {
            prop_assert!(answer.len() >= 12);
            prop_assert_eq!(&answer[..2], &bytes[..2]);
        }
        let _ = dns::addresses(&bytes);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // Nor stop a listed relay.
    #[test]
    fn no_bytes_from_the_guest_stop_a_listed_relay(
        datagram in proptest::collection::vec(any::<u8>(), 0..600),
        flip in flipped(),
    ) {
        let at = shared_listed_relay();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.send_to(&datagram, at).unwrap();
        socket.send_to(&flip, at).unwrap();
        let mut conn = connect("127.0.0.1", at, ANSWERED);
        let _ = write_framed(&mut conn, &flip);
        drop(conn);
        let q = query(5, "example.com");
        prop_assert_eq!(ask_udp("127.0.0.1", at, &q, ANSWERED), Some(answer_to(&q)));
        let q = query(6, "github.com");
        let answer = ask_udp("127.0.0.1", at, &q, ANSWERED).unwrap();
        prop_assert_eq!(rcode(&q, &answer), REFUSED);
    }
}
