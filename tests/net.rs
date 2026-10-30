// A VM's network: the /30 its slot picks, its boot argument, and its tap. The
// tap tests drive a fake `ip`; the one that makes a real tap needs
// CAP_NET_ADMIN and iproute2, so it's ignored by default.
use runner::net::{DEFAULT_SUBNET, Net, Subnet, Tap, dns};
use runner::store::Error;
use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
            "fctap0",
            "172.16.0.1",
            "172.16.0.2",
            "06:00:ac:10:00:02"
        ))
    );
    assert_eq!(
        s.net(1),
        Some(want(
            1,
            "fctap1",
            "172.16.0.5",
            "172.16.0.6",
            "06:00:ac:10:00:06"
        ))
    );
    assert_eq!(
        s.net(64),
        Some(want(
            64,
            "fctap64",
            "172.16.1.1",
            "172.16.1.2",
            "06:00:ac:10:01:02"
        ))
    );
    assert_eq!(
        s.net(16383),
        Some(want(
            16383,
            "fctap16383",
            "172.16.255.253",
            "172.16.255.254",
            "06:00:ac:10:ff:fe"
        ))
    );
    assert_eq!(s.net(16384), None);
    assert_eq!(subnet("10.0.0.0/30").net(1), None);
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
    assert_eq!(tap.name(), "fctap1");
    assert_eq!(
        lines(&log),
        [
            "link del dev fctap1",
            "tuntap add dev fctap1 mode tap",
            "addr add 172.16.0.5/30 dev fctap1",
            "link set dev fctap1 up",
        ]
    );
    drop(tap);
    assert_eq!(lines(&log).last().unwrap(), "link del dev fctap1");
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
    assert!(msg.contains("addr add 172.16.0.1/30 dev fctap0"), "{msg}");
    assert!(msg.contains("Operation not permitted"), "{msg}");
    assert_eq!(lines(&log).last().unwrap(), "link del dev fctap0");

    // No tap made at all: nothing to remove.
    fs::write(dir.path().join("fail"), "tuntap add").unwrap();
    fs::remove_file(&log).unwrap();
    Tap::add_with(&ip_cmd, &net).unwrap_err();
    assert_eq!(
        lines(&log),
        ["link del dev fctap0", "tuntap add dev fctap0 mode tap"]
    );
}

#[test]
fn a_missing_ip_is_an_error_not_a_panic() {
    let net = DEFAULT_SUBNET.net(0).unwrap();
    let err = Tap::add_with(Path::new("/nonexistent/ip"), &net).unwrap_err();
    assert!(err.to_string().contains("/nonexistent/ip"), "{err}");
}

// Run on a host, or here as root: cargo test --test net -- --ignored
#[test]
#[ignore = "needs CAP_NET_ADMIN and iproute2"]
fn a_real_tap_comes_up_with_the_gateway_address_and_goes() {
    // A subnet and slot no real VM uses.
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
            prop_assert!(net.tap.len() <= 15, "{}", net.tap);
            prop_assert_eq!(s.net(s.slots()), None);
        }
    }
}
