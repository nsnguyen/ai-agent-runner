// A VM's URL on the tailnet (src/url.rs): the port its slot gives it, the
// rule that it binds only the host's tailnet address and takes connections
// only from the tailnet, its setting, and a connection held through a wake.
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, bind, getsockname, listen, socket,
};
use proptest::prelude::*;
use runner::store::{Config, Store};
use runner::url::{self, Iface, Setting};
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn iface(name: &str, addr: &str) -> Iface {
    Iface {
        name: name.to_string(),
        addr: ip(addr),
    }
}

// A host on a LAN and a tailnet, with VMs on their taps.
fn host() -> Vec<Iface> {
    vec![
        iface("lo", "127.0.0.1"),
        iface("eth0", "192.168.1.20"),
        iface("tailscale0", "100.101.102.103"),
        iface("tailscale0", "fd7a:115c:a1e0::1234"),
        iface("fcac100000", "172.16.0.1"),
    ]
}

fn store_with(names: &[&str]) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    store.import(names[0], &disk).unwrap();
    for name in &names[1..] {
        store.fork(names[0], name).unwrap();
    }
    (dir, store)
}

#[test]
fn a_vm_s_port_comes_from_its_slot() {
    assert_eq!(url::host_port(0), Some(url::FIRST_PORT));
    assert_eq!(url::host_port(5), Some(url::FIRST_PORT + 5));
    // The default range's last slot stays below Linux's ephemeral ports.
    assert!(url::host_port(16383).unwrap() < 32768);
    let last = u32::from(u16::MAX - url::FIRST_PORT);
    assert_eq!(url::host_port(last), Some(u16::MAX));
    assert_eq!(url::host_port(last + 1), None);
}

#[test]
fn each_vm_keeps_a_port_of_its_own() {
    let (_dir, store) = store_with(&["base", "dev", "web"]);
    let port = |name| url::port(&store, name).unwrap();
    let (base, dev, web) = (port("base"), port("dev"), port("web"));
    assert_eq!(base, url::FIRST_PORT);
    assert!(base != dev && dev != web && base != web);
    assert_eq!(dev, url::host_port(store.net("dev").unwrap().slot).unwrap());
    // The same each time it's asked, and after another VM goes.
    store.delete("base").unwrap();
    assert_eq!(port("dev"), dev);
    assert_eq!(port("web"), web);
    // A new VM may take a freed slot, never a live one's.
    store.fork("dev", "new").unwrap();
    assert!(![dev, web].contains(&port("new")));
}

#[test]
fn the_tailnet_address_is_tailscale_s_ipv4_one() {
    assert_eq!(
        url::tailnet_addr(&host()),
        Some(Ipv4Addr::new(100, 101, 102, 103))
    );
    // None without Tailscale up, and none from a CGNAT address elsewhere.
    assert_eq!(url::tailnet_addr(&host()[..2]), None);
    assert_eq!(
        url::tailnet_addr(&[iface("eth1", "100.70.0.5")]),
        None,
        "a carrier's CGNAT address isn't the tailnet"
    );
}

#[test]
fn only_an_address_on_the_tailnet_is_bound() {
    let host = host();
    assert!(url::on_tailnet(ip("100.101.102.103"), &host));
    for addr in [
        "0.0.0.0",
        "127.0.0.1",
        "192.168.1.20",
        "172.16.0.1",
        // Tailscale's range, but not this host's.
        "100.101.102.104",
        "::",
    ] {
        assert!(!url::on_tailnet(ip(addr), &host), "{addr}");
        let refused = url::bind(SocketAddr::new(ip(addr), 0), &host).unwrap_err();
        assert!(
            matches!(refused, url::Error::NotTailnet(a) if a == ip(addr)),
            "{addr}: {refused}"
        );
        assert!(refused.to_string().contains("tailnet"), "{refused}");
    }
    // In the range, but on an interface that isn't Tailscale's.
    let lan = [iface("eth1", "100.70.0.5")];
    assert!(!url::on_tailnet(ip("100.70.0.5"), &lan));
    assert!(url::bind(SocketAddr::new(ip("100.70.0.5"), 0), &lan).is_err());
}

proptest! {
    // Outside 100.64.0.0/10, nothing is the tailnet, whatever the interface.
    #[test]
    fn nothing_outside_tailscale_s_range_is_bound(raw in any::<u32>(), name in "[a-z0-9]{1,15}") {
        let addr = Ipv4Addr::from(raw);
        let in_range = raw >> 22 == (100 << 2) | 1;
        let ifaces = [Iface { name: format!("tailscale{name}"), addr: addr.into() }];
        prop_assert_eq!(url::on_tailnet(addr.into(), &ifaces), in_range);
        if !in_range {
            prop_assert!(url::bind(SocketAddr::new(addr.into(), 0), &ifaces).is_err());
        }
    }
}

#[test]
fn only_the_tailnet_may_connect() {
    // A tailnet peer, and the host itself through its tailnet address.
    assert!(url::peer_allowed(ip("100.80.1.2")));
    assert!(url::peer_allowed(ip("100.101.102.103")));
    // A VM, which reaches the host's own addresses, so it can't reach
    // another VM through its URL; the LAN; loopback.
    for peer in [
        "172.16.0.6",
        "192.168.1.30",
        "127.0.0.1",
        "10.0.0.4",
        "8.8.8.8",
    ] {
        assert!(!url::peer_allowed(ip(peer)), "{peer}");
    }
}

#[test]
fn a_vm_s_url_is_on_at_8080_until_set() {
    let (_dir, store) = store_with(&["base"]);
    let setting = store.url("base").unwrap();
    assert_eq!(setting, Setting::default());
    assert!(!setting.off);
    assert_eq!(setting.guest_port(), url::GUEST_PORT);
    assert_eq!(url::GUEST_PORT, 8080);

    let on_3000 = Setting {
        off: false,
        port: Some(3000),
    };
    store.set_url("base", on_3000).unwrap();
    assert_eq!(store.url("base").unwrap().guest_port(), 3000);
    // Off keeps the port, for when it's on again.
    let off = Setting {
        off: true,
        ..on_3000
    };
    store.set_url("base", off).unwrap();
    assert_eq!(store.url("base").unwrap(), off);
    // A fork keeps its source's setting, on a port of its own.
    store.fork("base", "dev").unwrap();
    assert_eq!(store.url("dev").unwrap(), off);
    // The rest of the config stays.
    store.set_idle("dev", runner::idle::Idle::Never).unwrap();
    assert_eq!(store.url("dev").unwrap(), off);
    // Port 0 isn't one, and nothing changes.
    let zero = Setting {
        off: false,
        port: Some(0),
    };
    assert!(store.set_url("dev", zero).is_err());
    assert_eq!(store.url("dev").unwrap(), off);
    assert!(store.set_url("nope", on_3000).is_err());
    let fresh = Config {
        url: Some(on_3000),
        ..Config::default()
    };
    store.fork_with("base", "fresh", &fresh).unwrap();
    assert_eq!(store.url("fresh").unwrap(), on_3000);
}

#[test]
fn an_off_url_is_none_and_one_with_no_tailnet_says_so() {
    let (_dir, store) = store_with(&["base"]);
    let none: [Iface; 0] = [];
    let err = url::url_on(&store, "base", &none).unwrap_err();
    assert!(err.to_string().contains("Tailscale"), "{err}");
    let shown = url::url_on(&store, "base", &host()).unwrap();
    assert_eq!(
        shown.as_deref(),
        Some(format!("http://100.101.102.103:{}", url::FIRST_PORT).as_str())
    );
    let off = Setting {
        off: true,
        port: None,
    };
    store.set_url("base", off).unwrap();
    assert_eq!(url::url_on(&store, "base", &host()).unwrap(), None);
    assert_eq!(url::url_on(&store, "base", &none).unwrap(), None);
}

// A socket bound to a loopback port but not yet listening: connecting to it is
// refused, as to a VM that's asleep, until `listen` is called on it.
fn unready_guest() -> (std::os::fd::OwnedFd, SocketAddr) {
    let fd = socket(
        AddressFamily::Inet,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .unwrap();
    bind(fd.as_raw_fd(), &SockaddrIn::new(127, 0, 0, 1, 0)).unwrap();
    let at: SockaddrIn = getsockname(fd.as_raw_fd()).unwrap();
    (fd, SocketAddr::new(at.ip().into(), at.port()))
}

// A client connection and the proxy's end of it.
fn client_pair() -> (TcpStream, TcpStream) {
    let front = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
    let (taken, _) = front.accept().unwrap();
    (client, taken)
}

#[test]
fn a_connection_is_held_through_a_wake_then_proxied() {
    let (guest_fd, guest) = unready_guest();
    let (mut client, taken) = client_pair();
    // Sent before the guest can take it: it waits in the proxy.
    client.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();

    let woken = Arc::new(AtomicBool::new(false));
    let (ready, listening) = mpsc::channel();
    let proxy = {
        let woken = woken.clone();
        std::thread::spawn(move || {
            let wake = move || {
                // A wake takes a moment, and only then does the guest listen.
                std::thread::sleep(Duration::from_millis(200));
                listen(&guest_fd, Backlog::new(8).unwrap()).unwrap();
                woken.store(true, Ordering::SeqCst);
                ready.send(guest_fd).unwrap();
                Ok(())
            };
            url::proxy(taken, guest, wake, Duration::from_secs(10))
        })
    };

    // The guest: reads the request, answers, and closes.
    let guest_fd = listening.recv_timeout(Duration::from_secs(5)).unwrap();
    let guest_listener = TcpListener::from(guest_fd);
    let (mut conn, _) = guest_listener.accept().unwrap();
    assert!(woken.load(Ordering::SeqCst));
    let mut req = [0u8; 18];
    conn.read_exact(&mut req).unwrap();
    assert_eq!(&req, b"GET / HTTP/1.0\r\n\r\n");
    conn.write_all(b"HTTP/1.0 200 OK\r\n\r\nhello").unwrap();
    conn.shutdown(Shutdown::Write).unwrap();

    client.shutdown(Shutdown::Write).unwrap();
    let mut got = String::new();
    client.read_to_string(&mut got).unwrap();
    assert_eq!(got, "HTTP/1.0 200 OK\r\n\r\nhello");
    proxy.join().unwrap().unwrap();
}

#[test]
fn a_refused_wake_closes_the_connection_and_says_why() {
    let (_guest_fd, guest) = unready_guest();
    let (mut client, taken) = client_pair();
    let done = url::proxy(
        taken,
        guest,
        || Err("dev is going down".to_string()),
        Duration::from_secs(10),
    );
    let err = done.unwrap_err();
    assert!(err.to_string().contains("dev is going down"), "{err}");
    // Closed, unanswered.
    let mut got = Vec::new();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(client.read_to_end(&mut got).unwrap(), 0);
}

#[test]
fn a_guest_that_never_accepts_closes_the_connection_in_time() {
    // Bound, never listening: every try is refused.
    let (_guest_fd, guest) = unready_guest();
    let (mut client, taken) = client_pair();
    let began = std::time::Instant::now();
    let err = url::proxy(taken, guest, || Ok(()), Duration::from_millis(500)).unwrap_err();
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(err.to_string().contains(&guest.to_string()), "{err}");
    let mut got = Vec::new();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(client.read_to_end(&mut got).unwrap(), 0);
}

#[test]
fn bytes_pass_both_ways_until_each_side_is_done() {
    let guest_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let guest = guest_listener.local_addr().unwrap();
    let (mut client, taken) = client_pair();
    let proxy =
        std::thread::spawn(move || url::proxy(taken, guest, || Ok(()), Duration::from_secs(5)));
    let (mut conn, _) = guest_listener.accept().unwrap();
    // More than a socket buffer each way, at once: an echo.
    let big: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let echo = std::thread::spawn(move || {
        let mut back = conn.try_clone().unwrap();
        std::io::copy(&mut conn, &mut back).unwrap();
        back.shutdown(Shutdown::Write).unwrap();
    });
    let mut reader = client.try_clone().unwrap();
    let sent = big.clone();
    let writer = std::thread::spawn(move || {
        client.write_all(&sent).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
    });
    let mut got = Vec::new();
    reader.read_to_end(&mut got).unwrap();
    writer.join().unwrap();
    echo.join().unwrap();
    assert!(got == big, "echoed {} of {} bytes", got.len(), big.len());
    proxy.join().unwrap().unwrap();
}
