// The unit file, the states systemd reports, and READY notifications: no
// systemd needed.
use runner::systemd::{self, Error, State};
use std::ffi::OsStr;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::time::Duration;

#[test]
fn the_unit_boots_and_halts_each_vm_as_the_runner_user() {
    let unit = systemd::render_unit(
        Path::new("/usr/local/bin/runner"),
        Path::new("/var/lib/runner"),
        "runner",
    )
    .unwrap();
    let want = "\
# firecracker@.service, written by `runner unit`. Each instance is one VM:
# `systemctl start firecracker@dev` boots dev. Write it again after moving
# the runner binary or its root.
[Unit]
Description=Firecracker microVM %i
# Stopped before its disks' filesystem is unmounted, at host shutdown too.
RequiresMountsFor=/var/lib/runner

[Service]
# Ready once the guest is started; until then the VM is booting. Booting
# copies the disk, which takes minutes for a big one without reflink.
Type=notify
NotifyAccess=main
TimeoutStartSec=600
User=runner
SupplementaryGroups=kvm
Environment=RUNNER_ROOT=/var/lib/runner
ExecStart=/usr/local/bin/runner boot %i
# Ctrl-Alt-Del, then wait for the guest to shut down. Past the timeout,
# systemd kills the VM and its run isn't committed.
ExecStop=/usr/local/bin/runner halt %i
TimeoutStopSec=30
# The serial console.
StandardOutput=journal
StandardError=journal
# For boot to add and remove the VM's tap, and nothing else. Boot clears it
# before running Firecracker, which doesn't need it.
AmbientCapabilities=CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_ADMIN
";
    assert_eq!(unit, want);
}

#[test]
fn the_unit_refuses_what_systemd_would_read_differently() {
    let ok_bin = Path::new("/usr/local/bin/runner");
    let ok_root = Path::new("/var/lib/runner");
    for bad in [
        "/opt/my runner",
        "/opt/100%",
        "/opt/$HOME",
        "relative/runner",
        "/a\nb",
    ] {
        let got = systemd::render_unit(Path::new(bad), ok_root, "runner");
        assert!(matches!(got, Err(Error::BadUnitValue(_))), "{bad:?}");
        let got = systemd::render_unit(ok_bin, Path::new(bad), "runner");
        assert!(matches!(got, Err(Error::BadUnitValue(_))), "{bad:?}");
    }
    for bad in ["", "run ner", "x\nExecStartPre=/bin/evil", "-x", "Runner"] {
        let got = systemd::render_unit(ok_bin, ok_root, bad);
        assert!(matches!(got, Err(Error::BadUnitValue(_))), "{bad:?}");
    }
}

#[test]
fn states_follow_systemds_active_state() {
    for (active, want) in [
        ("inactive", State::Stopped),
        ("failed", State::Stopped),
        ("activating", State::Booting),
        ("active", State::Running),
        ("reloading", State::Running),
        ("deactivating", State::Stopping),
    ] {
        assert_eq!(State::from_active_state(active), Some(want), "{active}");
    }
    assert_eq!(State::from_active_state("exploded"), None);
    assert_eq!(State::Booting.to_string(), "booting");
}

#[test]
fn a_vm_is_one_instance_of_the_template() {
    assert_eq!(systemd::unit_name("dev-2"), "firecracker@dev-2.service");
}

#[test]
fn ready_reaches_a_path_socket_and_an_abstract_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notify");
    let sock = UnixDatagram::bind(&path).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    systemd::notify(path.as_os_str(), "READY=1").unwrap();
    let mut buf = [0; 16];
    let n = sock.recv(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"READY=1");

    let name = format!("runner-test-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let sock = UnixDatagram::bind_addr(&addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    systemd::notify(OsStr::new(&format!("@{name}")), "READY=1").unwrap();
    let n = sock.recv(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"READY=1");
}
