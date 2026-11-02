// A VM's network: the /30 its slot picks, its boot argument, its tap, and the
// host's rules for it. The tap tests drive a fake `ip`; the one that makes a
// real tap needs CAP_NET_ADMIN and iproute2, and the one that loads the rules
// needs root and nftables too, so those are ignored by default.
use runner::host;
use runner::net::{DEFAULT_SUBNET, Net, Subnet, Tap, dns};
use runner::store::Error;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
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

#[test]
fn the_boot_argument_sets_address_gateway_hostname_and_dns() {
    let net = DEFAULT_SUBNET.net(1).unwrap();
    let head = "ip=172.16.0.6::172.16.0.5:255.255.255.252:dev:eth0:off";
    assert_eq!(net.boot_arg("dev", &[]), head);
    assert_eq!(
        net.boot_arg("dev", &[ip("8.8.8.8")]),
        format!("{head}:8.8.8.8")
    );
    // The kernel takes two.
    let three = [ip("8.8.8.8"), ip("1.1.1.1"), ip("9.9.9.9")];
    assert_eq!(
        net.boot_arg("dev", &three),
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
    // The ip6 table goes by the taps' names, so every tap has to match it.
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

// Answers each connection's first 4 bytes with `reply`.
fn answer(listener: TcpListener, reply: &'static [u8]) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut req = [0u8; 4];
            if conn.read_exact(&mut req).is_ok() {
                let _ = conn.write_all(reply);
            }
        }
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

// The rules install.sh writes, loaded in a network namespace that stands in
// for the host, with a VM's /30 on a veth to a second namespace that stands
// in for the VM. The host's own namespace and rules are never touched.
// Run as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs root, nftables and iproute2"]
fn a_vm_can_t_reach_the_host_but_hears_back_from_it() {
    let range = subnet("10.254.0.0/16");
    let net = range.net(16001).unwrap();
    let pid = std::process::id();
    let (host_ns, vm_ns) = (format!("runner-host-{pid}"), format!("runner-vm-{pid}"));
    struct Gone(Vec<String>);
    impl Drop for Gone {
        fn drop(&mut self) {
            for ns in &self.0 {
                let _ = Command::new("ip").args(["netns", "del", ns]).status();
            }
        }
    }
    let mut gone = Gone(Vec::new());
    for ns in [&host_ns, &vm_ns] {
        ip_ok(&["netns", "add", ns]);
        gone.0.push(ns.clone());
    }
    let (tap, eth) = (net.tap.as_str(), "eth0");
    ip_ok(&[
        "-n", &host_ns, "link", "add", tap, "type", "veth", "peer", "name", eth, "netns", &vm_ns,
    ]);
    let gateway = format!("{}/30", net.gateway);
    let address = format!("{}/30", net.address);
    ip_ok(&["-n", &host_ns, "addr", "add", &gateway, "dev", tap]);
    ip_ok(&["-n", &host_ns, "link", "set", "dev", tap, "up"]);
    ip_ok(&["-n", &vm_ns, "addr", "add", &address, "dev", eth]);
    ip_ok(&["-n", &vm_ns, "link", "set", "dev", eth, "up"]);

    // The host's sshd, and a service in the VM the host's URL proxy goes to.
    let sshd = inside(&host_ns, || TcpListener::bind("0.0.0.0:22").unwrap());
    answer(sshd, b"host");
    let at = SocketAddr::new(net.address.into(), 8080);
    let service = inside(&vm_ns, move || TcpListener::bind(at).unwrap());
    answer(service, b"vm");
    let host_22 = SocketAddr::new(net.gateway.into(), 22);
    // Without the rules both ways work, so what changes below is the rules'.
    assert_eq!(ask(&vm_ns, host_22).unwrap(), "host");
    assert_eq!(ask(&host_ns, at).unwrap(), "vm");

    let rules = host::nat_rules(&range);
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), &rules).unwrap();
    let out = Command::new("ip")
        .args(["netns", "exec", &host_ns, "nft", "-f"])
        .arg(file.path())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "nft -f: {err}\n{rules}");

    // Dropped, not refused: the VM can't even tell the port is there.
    let got = ask(&vm_ns, host_22);
    assert_eq!(got.as_ref().map_err(|e| e.kind()), Err(ErrorKind::TimedOut));
    // The host opened this one, so the VM's replies come back.
    assert_eq!(ask(&host_ns, at).unwrap(), "vm");
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
