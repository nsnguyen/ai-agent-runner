// A VM's network: the /30 its slot picks, its boot argument, its tap, and the
// host's rules for it, its DNS included. The tap tests drive a fake `ip`; the one that makes a
// real tap needs CAP_NET_ADMIN and iproute2, and the one that loads the rules
// needs root and nftables too, so those are ignored by default.
mod common;

use common::resolver::{Resolver, answer_to, query, read_framed, write_framed};
use runner::dns::{Policy, Relay};
use runner::host;
use runner::net::{DEFAULT_SUBNET, Net, Subnet, Tap, dns};
use runner::store::Error;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

fn subnet(s: &str) -> Subnet {
    s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn ip(s: &str) -> Ipv4Addr {
    s.parse().unwrap()
}

#[test]
fn the_default_subnet_is_172_16_slash_16() {
    assert_eq!(DEFAULT_SUBNET, subnet("172.16.0.0/16"));
    assert_eq!(DEFAULT_SUBNET.to_string(), "172.16.0.0/16");
    assert_eq!(DEFAULT_SUBNET.slots(), 16384);
}

#[test]
fn a_subnet_must_be_aligned_and_hold_a_slash_30() {
    for good in ["10.0.0.0/8", "192.168.100.0/24", "10.1.2.4/30", "0.0.0.0/0"] {
        assert_eq!(subnet(good).to_string(), good);
    }
    for bad in [
        "",
        "172.16.0.0",
        "172.16.0.1/16",
        "10.0.0.0/31",
        "10.0.0.0/32",
        "10.0.0.0/33",
        "10.0.0/24",
        "10.0.0.0/-1",
        "10.0.0.0/ 24",
        "nope/16",
    ] {
        assert!(bad.parse::<Subnet>().is_err(), "{bad:?} parsed");
    }
    let why = "172.16.0.1/16".parse::<Subnet>().unwrap_err();
    assert!(why.contains("172.16.0.0/16"), "{why}");
}

#[test]
fn each_slot_is_a_slash_30_with_its_own_tap() {
    let s = DEFAULT_SUBNET;
    let want = |slot, tap: &str, gateway, address, mac: &str| Net {
        slot,
        tap: tap.to_string(),
        gateway: ip(gateway),
        address: ip(address),
        mac: mac.to_string(),
    };
    assert_eq!(
        s.net(0),
        Some(want(
            0,
            "fcac100000",
            "172.16.0.1",
            "172.16.0.2",
            "06:00:ac:10:00:02"
        ))
    );
    assert_eq!(
        s.net(1),
        Some(want(
            1,
            "fcac100004",
            "172.16.0.5",
            "172.16.0.6",
            "06:00:ac:10:00:06"
        ))
    );
    assert_eq!(
        s.net(64),
        Some(want(
            64,
            "fcac100100",
            "172.16.1.1",
            "172.16.1.2",
            "06:00:ac:10:01:02"
        ))
    );
    assert_eq!(
        s.net(16383),
        Some(want(
            16383,
            "fcac10fffc",
            "172.16.255.253",
            "172.16.255.254",
            "06:00:ac:10:ff:fe"
        ))
    );
    assert_eq!(s.net(16384), None);
    assert_eq!(subnet("10.0.0.0/30").net(1), None);
    // The tap is named for its /30, so the same slot in another subnet, as a
    // test root might use, is another tap.
    assert_eq!(subnet("10.254.0.0/16").net(1).unwrap().tap, "fc0afe0004");
}

// The guest's DNS is its boot, on its gateway (src/dns.rs).
#[test]
fn the_boot_argument_sets_address_gateway_hostname_and_the_gateway_as_dns() {
    let net = DEFAULT_SUBNET.net(1).unwrap();
    assert_eq!(
        net.boot_arg("dev"),
        "ip=172.16.0.6::172.16.0.5:255.255.255.252:dev:eth0:off:172.16.0.5"
    );
}

// A boot that can't serve DNS gives the guest the host's resolvers instead.
#[test]
fn the_boot_argument_can_name_other_dns_servers() {
    let net = DEFAULT_SUBNET.net(1).unwrap();
    let head = "ip=172.16.0.6::172.16.0.5:255.255.255.252:dev:eth0:off";
    assert_eq!(net.boot_arg_with("dev", &[]), head);
    assert_eq!(
        net.boot_arg_with("dev", &[ip("8.8.8.8")]),
        format!("{head}:8.8.8.8")
    );
    // The kernel takes two.
    let three = [ip("8.8.8.8"), ip("1.1.1.1"), ip("9.9.9.9")];
    assert_eq!(
        net.boot_arg_with("dev", &three),
        format!("{head}:8.8.8.8:1.1.1.1")
    );
}

#[test]
fn dns_comes_from_the_first_resolv_conf_a_guest_can_use() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, text: &str| {
        let path = dir.path().join(name);
        fs::write(&path, text).unwrap();
        path
    };
    let stub = file(
        "stub",
        "# systemd-resolved\nnameserver 127.0.0.53\noptions edns0 trust-ad\n",
    );
    let upstream = file(
        "upstream",
        "nameserver ::1\nnameserver 2001:4860:4860::8888\n\
         nameserver 192.168.1.1 # router\n\tnameserver   1.1.1.1\n\
         ; nameserver 9.9.9.9\nsearch lan\nnameserver 0.0.0.0\n",
    );
    let missing = dir.path().join("missing");
    assert_eq!(
        dns(&[&missing, &stub, &upstream]),
        [ip("192.168.1.1"), ip("1.1.1.1")]
    );
    assert_eq!(dns(&[&upstream, &stub]), [ip("192.168.1.1"), ip("1.1.1.1")]);
    assert_eq!(dns(&[&stub, &missing]), Vec::<Ipv4Addr>::new());
}

// Writing a script and running it from several threads at once can fail with
// ETXTBSY, so the fake-`ip` tests take turns.
static FAKE_IP: Mutex<()> = Mutex::new(());

// An `ip` that logs its arguments and fails the step named in `fail`.
fn fake_ip(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("log");
    let script = dir.join("ip");
    let body = format!(
        "#!/bin/sh\necho \"$*\" >> '{log}'\n\
         if [ -f '{fail}' ] && [ \"$1 $2\" = \"$(cat '{fail}')\" ]; then\n\
         echo \"RTNETLINK answers: Operation not permitted\" >&2; exit 2\nfi\n",
        log = log.display(),
        fail = dir.join("fail").display(),
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn lines(log: &Path) -> Vec<String> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.lines().map(str::to_string).collect()
}

#[test]
fn a_tap_replaces_a_stale_one_comes_up_and_goes_when_dropped() {
    let _turn = FAKE_IP.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (ip_cmd, log) = fake_ip(dir.path());
    let net = DEFAULT_SUBNET.net(1).unwrap();
    let tap = Tap::add_with(&ip_cmd, &net).unwrap();
    assert_eq!(tap.name(), "fcac100004");
    // `tuntap del`, not `link del`: it won't remove a tap something holds
    // open, or anything that isn't a tap.
    assert_eq!(
        lines(&log),
        [
            "tuntap del dev fcac100004 mode tap",
            "tuntap add dev fcac100004 mode tap",
            "addr add 172.16.0.5/30 dev fcac100004",
            "link set dev fcac100004 up",
        ]
    );
    drop(tap);
    assert_eq!(lines(&log).last().unwrap(), "link del dev fcac100004");
    assert_eq!(lines(&log).len(), 5);
}

#[test]
fn a_tap_that_fails_partway_is_removed_and_says_why() {
    let _turn = FAKE_IP.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (ip_cmd, log) = fake_ip(dir.path());
    fs::write(dir.path().join("fail"), "addr add").unwrap();
    let net = DEFAULT_SUBNET.net(0).unwrap();
    let err = Tap::add_with(&ip_cmd, &net).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, Error::Command { .. }), "{err:?}");
    assert!(
        msg.contains("addr add 172.16.0.1/30 dev fcac100000"),
        "{msg}"
    );
    assert!(msg.contains("Operation not permitted"), "{msg}");
    assert_eq!(lines(&log).last().unwrap(), "link del dev fcac100000");

    // No tap made at all: nothing to remove.
    fs::write(dir.path().join("fail"), "tuntap add").unwrap();
    fs::remove_file(&log).unwrap();
    Tap::add_with(&ip_cmd, &net).unwrap_err();
    assert_eq!(
        lines(&log),
        [
            "tuntap del dev fcac100000 mode tap",
            "tuntap add dev fcac100000 mode tap"
        ]
    );
}

#[test]
fn a_missing_ip_is_an_error_not_a_panic() {
    let _turn = FAKE_IP.lock().unwrap();
    let net = DEFAULT_SUBNET.net(0).unwrap();
    let err = Tap::add_with(Path::new("/nonexistent/ip"), &net).unwrap_err();
    assert!(err.to_string().contains("/nonexistent/ip"), "{err}");
}

// Run on a host, or here as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs CAP_NET_ADMIN and iproute2"]
fn a_real_tap_comes_up_with_the_gateway_address_and_goes() {
    // A subnet no real VM uses, so a tap no real VM has.
    let net = subnet("10.254.0.0/16").net(16000).unwrap();
    let sys = Path::new("/sys/class/net").join(&net.tap);
    let tap = Tap::add_with(Path::new("ip"), &net).unwrap();
    assert!(sys.exists());
    let shown = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", &net.tap])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(shown.contains("10.254.250.1/30"), "{shown}");
    // A boot killed before it could clean up leaves its tap; the next replaces it.
    std::mem::forget(tap);
    let tap = Tap::add_with(Path::new("ip"), &net).unwrap();
    drop(tap);
    assert!(!sys.exists());
}

#[test]
fn the_rules_name_the_range_everywhere_install_sh_would() {
    let rules = host::nat_rules(&subnet("10.20.0.0/16"));
    assert!(!rules.contains('@'), "{rules}");
    assert!(
        rules.contains("ip saddr 10.20.0.0/16 ip daddr != 10.20.0.0/16 masquerade"),
        "{rules}"
    );
    // What a tap brings in from outside the range goes first.
    assert!(
        rules.contains("iifname \"fc*\" ip saddr != 10.20.0.0/16 drop"),
        "{rules}"
    );
    // The filters go by the taps' names, so every tap has to match them.
    assert!(rules.contains("iifname \"fc*\" drop"), "{rules}");
    for slot in [0, 1, 16383] {
        assert!(DEFAULT_SUBNET.net(slot).unwrap().tap.starts_with("fc"));
    }
}

// `ip` with these arguments, which has to work.
fn ip_ok(args: &[&str]) {
    let out = Command::new("ip").args(args).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ip {args:?}: {err}");
}

// Runs `f` on a thread inside the network namespace `ns`. A socket made
// there stays in it, whichever thread uses it after.
fn inside<T: Send + 'static>(ns: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let path = format!("/run/netns/{ns}");
    std::thread::spawn(move || {
        let ns = fs::File::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        nix::sched::setns(&ns, nix::sched::CloneFlags::CLONE_NEWNET).unwrap();
        f()
    })
    .join()
    .unwrap()
}

// Answers each connection's first 4 bytes with what `reply` makes of it.
fn serve(listener: TcpListener, reply: impl Fn(&TcpStream) -> String + Send + 'static) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut req = [0u8; 4];
            if conn.read_exact(&mut req).is_ok() {
                let _ = conn.write_all(reply(&conn).as_bytes());
            }
        }
    });
}

fn answer(listener: TcpListener, reply: &'static str) {
    serve(listener, |_| reply.to_string());
}

// Answers with the address the connection came from.
fn answer_with_source(listener: TcpListener) {
    serve(listener, |conn| match conn.peer_addr() {
        Ok(from) => from.ip().to_string(),
        Err(e) => e.to_string(),
    });
}

// Connects to `to` from inside `ns`, says ping and reads the reply.
fn ask(ns: &str, to: SocketAddr) -> std::io::Result<String> {
    inside(ns, move || {
        let mut conn = TcpStream::connect_timeout(&to, Duration::from_secs(2))?;
        conn.set_read_timeout(Some(Duration::from_secs(2)))?;
        conn.write_all(b"ping")?;
        let mut reply = String::new();
        conn.read_to_string(&mut reply)?;
        Ok(reply)
    })
}

// Sends a datagram from inside `ns` to `to`, and says whether `inbox`, the
// socket listening there, got it within a second.
fn datagram(ns: &str, to: SocketAddr, inbox: &UdpSocket) -> bool {
    datagram_from(ns, "0.0.0.0:0".parse().unwrap(), to, inbox)
}

// The same, sent from `from`, an address of `ns`'s own.
fn datagram_from(ns: &str, from: SocketAddr, to: SocketAddr, inbox: &UdpSocket) -> bool {
    static SENT: AtomicU32 = AtomicU32::new(0);
    let token = format!("probe {}", SENT.fetch_add(1, Ordering::Relaxed));
    let sent = token.clone();
    inside(ns, move || {
        let socket = UdpSocket::bind(from).unwrap();
        socket.send_to(sent.as_bytes(), to).unwrap();
    });
    inbox
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut got = [0u8; 64];
    // An earlier probe's datagram may come late.
    while let Ok(n) = inbox.recv(&mut got) {
        if got[..n] == *token.as_bytes() {
            return true;
        }
    }
    false
}

// Network namespaces for one test, deleted when this drops. The host's own
// namespace and rules are never touched.
#[derive(Default)]
struct Spaces(Vec<String>);

impl Spaces {
    fn add(&mut self, role: &str) -> String {
        let ns = format!("runner-{role}-{}", std::process::id());
        ip_ok(&["netns", "add", &ns]);
        self.0.push(ns.clone());
        ns
    }
}

impl Drop for Spaces {
    fn drop(&mut self) {
        for ns in &self.0 {
            let _ = Command::new("ip").args(["netns", "del", ns]).status();
        }
    }
}

// `ip -n <ns>` with these arguments, split at spaces, which has to work.
fn ip_in(ns: &str, args: &str) {
    let args: Vec<&str> = ["-n", ns].into_iter().chain(args.split(' ')).collect();
    ip_ok(&args);
}

// A VM's tap, but a veth: the host's end, named like the tap and holding the
// gateway's address, in `host`, and the VM's eth0 in `vm`, with `address` on
// it and the gateway as its way out.
fn plug(host: &str, vm: &str, net: &Net, address: &str) {
    let (tap, gateway) = (&net.tap, net.gateway);
    ip_in(
        host,
        &format!("link add {tap} type veth peer name eth0 netns {vm}"),
    );
    ip_in(host, &format!("addr add {gateway}/30 dev {tap}"));
    ip_in(host, &format!("link set dev {tap} up"));
    ip_in(vm, &format!("addr add {address} dev eth0"));
    ip_in(vm, "link set dev eth0 up");
    ip_in(
        vm,
        &format!("route add default via {gateway} dev eth0 onlink"),
    );
}

// Loads the rules install.sh writes for `range` in `ns`.
fn load_rules(ns: &str, range: &Subnet) {
    let rules = host::nat_rules(range);
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), &rules).unwrap();
    let out = Command::new("ip")
        .args(["netns", "exec", ns, "nft", "-f"])
        .arg(file.path())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "nft -f: {err}\n{rules}");
}

// Asks `to` for example.com over UDP from inside `ns`, and returns the
// answer that came within two seconds, if any.
fn ask_dns_udp(ns: &str, to: SocketAddr) -> Option<Vec<u8>> {
    inside(ns, move || {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket.send_to(&query(1, "example.com"), to).unwrap();
        let mut buf = [0u8; 512];
        socket.recv(&mut buf).ok().map(|n| buf[..n].to_vec())
    })
}

// The same over TCP.
fn ask_dns_tcp(ns: &str, to: SocketAddr) -> std::io::Result<Vec<u8>> {
    inside(ns, move || {
        let mut conn = TcpStream::connect_timeout(&to, Duration::from_secs(2))?;
        conn.set_read_timeout(Some(Duration::from_secs(2)))?;
        write_framed(&mut conn, &query(2, "example.com"))?;
        read_framed(&mut conn)
    })
}

#[track_caller]
fn assert_dropped(got: std::io::Result<String>, to: SocketAddr) {
    let got = got.map_err(|e| e.kind());
    assert_eq!(got, Err(ErrorKind::TimedOut), "{to}");
}

// The rules install.sh writes, loaded in a network namespace that stands in
// for the host, with a VM's /30 on a veth to a second namespace that stands
// in for the VM. The host's own namespace and rules are never touched.
// Run as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs root, nftables and iproute2"]
fn a_vm_can_t_reach_the_host_but_hears_back_from_it() {
    let range = subnet("10.254.0.0/16");
    let net = range.net(16001).unwrap();
    let mut spaces = Spaces::default();
    let (host_ns, vm_ns) = (spaces.add("host"), spaces.add("vm"));
    plug(&host_ns, &vm_ns, &net, &format!("{}/30", net.address));

    // The host's sshd, and a service in the VM the host's URL proxy goes to.
    let sshd = inside(&host_ns, || TcpListener::bind("0.0.0.0:22").unwrap());
    answer(sshd, "host");
    let at = SocketAddr::new(net.address.into(), 8080);
    let service = inside(&vm_ns, move || TcpListener::bind(at).unwrap());
    answer(service, "vm");
    let host_22 = SocketAddr::new(net.gateway.into(), 22);
    // Without the rules both ways work, so what changes below is the rules'.
    assert_eq!(ask(&vm_ns, host_22).unwrap(), "host");
    assert_eq!(ask(&host_ns, at).unwrap(), "vm");

    load_rules(&host_ns, &range);
    // Dropped, not refused: the VM can't even tell the port is there.
    assert_dropped(ask(&vm_ns, host_22), host_22);
    // The host opened this one, so the VM's replies come back.
    assert_eq!(ask(&host_ns, at).unwrap(), "vm");
}

// A VM's DNS is its own boot's, on its gateway: the rules let port 53 in from
// a tap only to the host's end of that same tap. So no VM can use another's
// boot, nor a resolver on another of the host's addresses (#122). A second
// boot's sockets are bound to its own tap, so it refuses the VM even without
// the rules; with them, nothing comes back at all.
// Run as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs root, nftables and iproute2"]
fn a_vm_s_dns_is_its_own_boot_and_no_other() {
    let range = subnet("10.254.0.0/16");
    let (own, other) = (range.net(16004).unwrap(), range.net(16005).unwrap());
    let mut spaces = Spaces::default();
    let host = spaces.add("dns-host");
    let (vm, neighbor) = (spaces.add("dns-vm"), spaces.add("dns-neighbor"));
    plug(&host, &vm, &own, &format!("{}/30", own.address));
    plug(&host, &neighbor, &other, &format!("{}/30", other.address));
    // The host's resolver, on loopback, and another address of the host's
    // with a resolver of its own on it, like a dnsmasq on its LAN address.
    ip_in(&host, "link set dev lo up");
    ip_in(&host, "addr add 192.168.77.1/32 dev lo");
    let resolver = |at: &'static str| {
        inside(&host, move || {
            let udp = UdpSocket::bind(at).unwrap();
            Resolver::on(udp, TcpListener::bind(at).unwrap(), true)
        })
    };
    let upstream = resolver("127.0.0.1:53");
    let _lan = resolver("192.168.77.1:53");
    // Each VM's boot, passing queries on to the host's resolver.
    let (own_net, other_net) = (own.clone(), other.clone());
    let _boots = inside(&host, move || {
        let resolvers = [Ipv4Addr::LOCALHOST];
        let own = Relay::bind(&own_net, &resolvers, Policy::default()).unwrap();
        (
            own,
            Relay::bind(&other_net, &resolvers, Policy::default()).unwrap(),
        )
    });
    let at = |address: Ipv4Addr| SocketAddr::new(address.into(), 53);
    let (own_boot, other_boot) = (at(own.gateway), at(other.gateway));
    let lan = at(ip("192.168.77.1"));
    let (udp_answer, tcp_answer) = (
        answer_to(&query(1, "example.com")),
        answer_to(&query(2, "example.com")),
    );

    // Without the rules, the VM's own boot answers it, the LAN's resolver
    // too, and the second boot refuses it.
    assert_eq!(ask_dns_udp(&vm, own_boot), Some(udp_answer.clone()));
    assert_eq!(ask_dns_tcp(&vm, own_boot).unwrap(), tcp_answer);
    assert_eq!(ask_dns_udp(&vm, lan), Some(udp_answer.clone()));
    assert_eq!(ask_dns_tcp(&vm, lan).unwrap(), tcp_answer);
    assert_eq!(ask_dns_udp(&vm, other_boot), None);
    let refused = ask_dns_tcp(&vm, other_boot).map_err(|e| e.kind());
    assert_eq!(refused, Err(ErrorKind::ConnectionRefused));
    // The second VM's own boot answers it, though.
    assert_eq!(ask_dns_udp(&neighbor, other_boot), Some(udp_answer.clone()));

    load_rules(&host, &range);
    assert_eq!(ask_dns_udp(&vm, own_boot), Some(udp_answer.clone()));
    assert_eq!(ask_dns_tcp(&vm, own_boot).unwrap(), tcp_answer);
    assert_eq!(ask_dns_udp(&neighbor, other_boot), Some(udp_answer));
    // Dropped: nothing even refuses.
    for to in [other_boot, lan] {
        assert_eq!(ask_dns_udp(&vm, to), None, "{to}");
        let got = ask_dns_tcp(&vm, to).map_err(|e| e.kind());
        assert_eq!(got, Err(ErrorKind::TimedOut), "{to}");
    }
    // The answers were the host's resolver's, passed on.
    assert!(upstream.heard().contains(&query(1, "example.com")));
    assert!(upstream.heard().contains(&query(2, "example.com")));
}

// A VM whose boot can't serve its DNS, under a resolver the host runs on
// every address, say, doesn't reach that resolver at its gateway either: the
// rules let port 53 in only to a socket bound to the tap's own address, as
// the boot's are (#122).
// Run as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs root, nftables and iproute2"]
fn a_vm_whose_boot_can_t_serve_its_dns_doesn_t_reach_the_host_s() {
    let range = subnet("10.254.0.0/16");
    let net = range.net(16006).unwrap();
    let mut spaces = Spaces::default();
    let (host, vm) = (spaces.add("dnsless-host"), spaces.add("dnsless-vm"));
    plug(&host, &vm, &net, &format!("{}/30", net.address));
    let _dnsmasq = inside(&host, || {
        let udp = UdpSocket::bind("0.0.0.0:53").unwrap();
        Resolver::on(udp, TcpListener::bind("0.0.0.0:53").unwrap(), true)
    });
    // So the boot can't bind port 53 on the gateway.
    let own = net.clone();
    let bound = inside(&host, move || {
        Relay::bind(&own, &[Ipv4Addr::LOCALHOST], Policy::default())
            .map(drop)
            .map_err(|e| e.kind())
    });
    assert_eq!(bound, Err(ErrorKind::AddrInUse));
    let gateway = SocketAddr::new(net.gateway.into(), 53);
    let answer = answer_to(&query(1, "example.com"));
    assert_eq!(ask_dns_udp(&vm, gateway), Some(answer));

    load_rules(&host, &range);
    assert_eq!(ask_dns_udp(&vm, gateway), None);
    let got = ask_dns_tcp(&vm, gateway).map_err(|e| e.kind());
    assert_eq!(got, Err(ErrorKind::TimedOut));
}

// A VM that gives itself an address outside the VMs' range gets nothing past
// the rules, over TCP or UDP: not to the host, at its tap's address or its
// LAN one, not through it to the LAN or the internet, and not as a forged
// reply to what the host opened. Without the rules all of it gets there, and
// the host routes the replies back, so it's the rules that stop it. A VM at
// its own address still reaches the internet, masqueraded as the host, and
// the LAN's resolver, but nothing else there.
// Run as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs root, nftables and iproute2"]
fn a_vm_that_forges_its_source_gets_nothing_past_the_rules() {
    let range = subnet("10.254.0.0/16");
    let (own, other) = (range.net(16002).unwrap(), range.net(16003).unwrap());
    let mut spaces = Spaces::default();
    let host = spaces.add("forge-host");
    let (vm, forger) = (spaces.add("forge-vm"), spaces.add("forge-forger"));
    let lan = spaces.add("forge-lan");
    plug(&host, &vm, &own, &format!("{}/30", own.address));
    plug(&host, &forger, &other, "192.0.2.66/32");
    ip_in(&host, &format!("route add 192.0.2.66/32 dev {}", other.tap));
    // The host's LAN, with a resolver on it, and the internet beyond: a web
    // server, and the resolver the host itself uses.
    ip_in(
        &host,
        &format!("link add lan0 type veth peer name lan0 netns {lan}"),
    );
    ip_in(&host, "addr add 192.168.77.1/24 dev lan0");
    ip_in(&host, "link set dev lan0 up");
    ip_in(&host, "route add default via 192.168.77.2");
    ip_in(&lan, "addr add 192.168.77.2/24 dev lan0");
    ip_in(&lan, "addr add 203.0.113.1/32 dev lo");
    ip_in(&lan, "addr add 198.51.100.53/32 dev lo");
    ip_in(&lan, "link set dev lan0 up");
    ip_in(&lan, "link set dev lo up");
    ip_in(&lan, "route add default via 192.168.77.1");
    // The forger takes that resolver's address too, to answer the host's
    // lookups.
    ip_in(&forger, "addr add 198.51.100.53/32 dev eth0");
    // The host forwards, and doesn't check that a source routes back the way
    // it came, as a host with a strict reverse-path filter would: the rules
    // have to hold without it.
    let tap = other.tap.clone();
    inside(&host, move || {
        fs::write("/proc/sys/net/ipv4/ip_forward", "1").unwrap();
        for conf in ["all", &tap] {
            let path = format!("/proc/sys/net/ipv4/conf/{conf}/rp_filter");
            fs::write(path, "0").unwrap();
        }
    });

    // The host's sshd and resolver, and beyond it, servers that say who they
    // see: the LAN's sshd and resolver, and a web server.
    answer(
        inside(&host, || TcpListener::bind("0.0.0.0:22").unwrap()),
        "host",
    );
    let host_inbox = inside(&host, || UdpSocket::bind("0.0.0.0:53").unwrap());
    for port in [22, 53, 80] {
        let listener = inside(&lan, move || TcpListener::bind(("0.0.0.0", port)).unwrap());
        answer_with_source(listener);
    }
    let lan_inbox = inside(&lan, || UdpSocket::bind("0.0.0.0:53").unwrap());
    let to = |address: &str, port| SocketAddr::new(ip(address).into(), port);
    let host_tcp = [
        SocketAddr::new(other.gateway.into(), 22),
        to("192.168.77.1", 22),
    ];
    let beyond_tcp = [to("192.168.77.2", 53), to("203.0.113.1", 80)];
    let udp = [
        (SocketAddr::new(other.gateway.into(), 53), &host_inbox),
        (to("192.168.77.1", 53), &host_inbox),
        (to("192.168.77.2", 53), &lan_inbox),
        (to("203.0.113.1", 53), &lan_inbox),
    ];
    // The host looks something up, and the forger answers as the resolver.
    let lookup = inside(&host, || UdpSocket::bind("192.168.77.1:0").unwrap());
    let upstream = to("198.51.100.53", 53);
    let forged_answer = || {
        lookup.send_to(b"query", upstream).unwrap();
        let to = lookup.local_addr().unwrap();
        datagram_from(&forger, upstream, to, &lookup)
    };

    // Without the rules, every forged packet gets there, and back.
    for at in host_tcp {
        assert_eq!(ask(&forger, at).unwrap(), "host", "{at}");
    }
    for at in beyond_tcp {
        assert_eq!(ask(&forger, at).unwrap(), "192.0.2.66", "{at}");
    }
    for (at, inbox) in udp {
        assert!(datagram(&forger, at, inbox), "{at}");
    }
    assert!(forged_answer());

    load_rules(&host, &range);
    for at in host_tcp.into_iter().chain(beyond_tcp) {
        assert_dropped(ask(&forger, at), at);
    }
    for (at, inbox) in udp {
        assert!(!datagram(&forger, at, inbox), "{at}");
    }
    // To the host, it looks like a reply to its own lookup, which the rules
    // let in from the VMs' range.
    assert!(!forged_answer());
    let web = to("203.0.113.1", 80);
    assert_eq!(ask(&vm, web).unwrap(), "192.168.77.1");
    let resolver = to("192.168.77.2", 53);
    assert_eq!(ask(&vm, resolver).unwrap(), "192.168.77.1");
    assert!(datagram(&vm, resolver, &lan_inbox));
    let sshd = to("192.168.77.2", 22);
    assert_dropped(ask(&vm, sshd), sshd);
}

mod props {
    use super::*;
    use proptest::prelude::*;

    fn any_subnet() -> impl Strategy<Value = Subnet> {
        (any::<u32>(), 0u8..=30).prop_map(|(bits, prefix)| {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            subnet(&format!("{}/{prefix}", Ipv4Addr::from(bits & mask)))
        })
    }

    proptest! {
        #[test]
        fn parse_never_panics(s in "[0-9./ a-z-]{0,24}") {
            let _ = s.parse::<Subnet>();
        }

        #[test]
        fn a_subnet_reads_back_as_written(s in any_subnet()) {
            prop_assert_eq!(s.to_string().parse::<Subnet>(), Ok(s));
        }

        // Every slot's addresses sit in the subnet, in a /30 of their own, and
        // its tap's name fits Linux's 15 characters.
        #[test]
        fn every_slot_is_its_own_slash_30_inside_the_subnet(
            s in any_subnet(),
            pick in any::<u32>(),
        ) {
            let slot = pick % s.slots();
            let net = s.net(slot).unwrap();
            let (gw, addr) = (u32::from(net.gateway), u32::from(net.address));
            let base = u32::from(s.to_string().split('/').next().unwrap().parse::<Ipv4Addr>().unwrap());
            prop_assert_eq!(gw & 3, 1);
            prop_assert_eq!(addr, gw + 1);
            prop_assert_eq!((gw - base) / 4, slot);
            prop_assert_eq!(net.tap.len(), 10, "{}", net.tap);
            prop_assert_eq!(s.net(s.slots()), None);
        }
    }
}
