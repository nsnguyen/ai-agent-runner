// The unit file, the states systemd reports, and READY notifications: no
// systemd needed.
use runner::creds::Cred;
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
# firecracker@.service, as `runner unit` writes it and host/install.sh
# installs it. Each instance is one VM: `systemctl start firecracker@dev`
# boots dev. Write it again after moving the runner binary or its root.
[Unit]
Description=Firecracker microVM %i
# After the VMs' NAT, when both start at once.
After=network.target runner-nat.service
# Stopped before its disks' filesystem is unmounted, at host shutdown too.
RequiresMountsFor=/var/lib/runner
# And after runner-park.service, which parks it first if it's asleep.
Before=runner-park.service

[Service]
# Ready once the guest is started; until then the VM is booting. Booting
# copies the disk, which takes minutes for a big one without reflink.
Type=notify
NotifyAccess=main
TimeoutStartSec=600
User=runner
SupplementaryGroups=kvm
Environment=RUNNER_ROOT=/var/lib/runner
# The credentials a VM may get, each from /etc/credstore/<name> on the host.
# One that isn't there is skipped. Boot hands a VM's to its guest.
LoadCredential=CLAUDE_CODE_OAUTH_TOKEN
LoadCredential=ANTHROPIC_API_KEY
LoadCredential=GH_TOKEN
# %i, not %I: %I would unescape a VM name's dashes into slashes.
ExecStart=/usr/local/bin/runner boot %i
# Ctrl-Alt-Del, then wait for the guest to shut down. Past the timeout,
# systemd kills the VM and its run isn't committed.
ExecStop=/usr/local/bin/runner halt %i
TimeoutStopSec=30
# A guest's own restore (runner-guest restore) ends its boot with 75, once
# the restored disk is current: boot the VM again, onto that disk, unless
# it's being stopped.
SuccessExitStatus=75
RestartForceExitStatus=75
# The serial console.
StandardOutput=journal
StandardError=journal
# For boot to add and remove the VM's tap, and nothing else. Boot clears it
# before running Firecracker, which doesn't need it, and no setuid program
# gets it back.
AmbientCapabilities=CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_ADMIN
NoNewPrivileges=yes
";
    assert_eq!(unit, want);
    let restart = format!("\nRestartForceExitStatus={}\n", runner::boot::RESTART);
    assert!(unit.contains(&restart), "{unit}");
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

// The unit host/install.sh installs, which `runner doctor` checks against, is
// `runner unit`'s for the install's binary, root and user. A hand-kept copy
// once lost ExecStop: every `runner stop` killed its VM, and no run was
// committed.
#[test]
fn the_installed_unit_is_runner_units() {
    let rendered = systemd::render_unit(
        Path::new("/usr/local/bin/runner"),
        Path::new("/var/lib/runner"),
        "runner",
    )
    .unwrap();
    assert_eq!(runner::host::UNIT, rendered);
}

#[test]
fn the_park_unit_parks_every_sleeping_vm_as_the_runner_user() {
    let unit = systemd::render_park_unit(
        Path::new("/usr/local/bin/runner"),
        Path::new("/var/lib/runner"),
        "runner",
    )
    .unwrap();
    let want = "\
# runner-park.service, as `runner unit --park` writes it and host/install.sh
# installs it. At a host shutdown, it parks each sleeping VM before the VMs'
# units stop (src/sleep.rs): the VM's boot ends, leaving its snapshot and
# disk, and its next start resumes it. Write it again after moving the
# runner binary or its root.
[Unit]
Description=runner: sleeping VMs sleep on through a host shutdown
RequiresMountsFor=/var/lib/runner

[Service]
# Up from the host's start, with nothing to run: its stop is what it's for.
# Each firecracker@ unit is ordered Before= this one, so this one stops first.
Type=oneshot
RemainAfterExit=yes
User=runner
Environment=RUNNER_ROOT=/var/lib/runner
ExecStop=/usr/local/bin/runner park --all
# Each park syncs its VM's memory file, which takes a while for a big VM.
TimeoutStopSec=300

[Install]
WantedBy=multi-user.target
";
    assert_eq!(unit, want);
    assert!(matches!(
        systemd::render_park_unit(Path::new("/opt/my runner"), Path::new("/r"), "runner"),
        Err(Error::BadUnitValue(_))
    ));
}

#[test]
fn the_installed_park_unit_is_runner_units() {
    let rendered = systemd::render_park_unit(
        Path::new("/usr/local/bin/runner"),
        Path::new("/var/lib/runner"),
        "runner",
    )
    .unwrap();
    assert_eq!(runner::host::PARK_UNIT, rendered);
}

// systemd 255's own reading of both units together, as `systemd-analyze
// verify` dumps it: at a shutdown, which stops units in the reverse of their
// start order, the park unit stops before each VM's.
#[test]
fn systemd_stops_the_park_unit_before_each_vm() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Path::new(env!("CARGO_BIN_EXE_runner"));
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let uid = nix::unistd::geteuid();
    let user = nix::unistd::User::from_uid(uid).unwrap().unwrap().name;
    let vm = systemd::render_unit(runner, &root, &user).unwrap();
    std::fs::write(dir.path().join("firecracker@.service"), vm).unwrap();
    let park = systemd::render_park_unit(runner, &root, &user).unwrap();
    let park_path = dir.path().join("runner-park.service");
    std::fs::write(&park_path, park).unwrap();
    let out = std::process::Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(&park_path)
        .arg(dir.path().join("firecracker@dev.service"))
        .env("SYSTEMD_LOG_LEVEL", "debug")
        .output()
        .expect("systemd-analyze, from systemd, to be installed");
    let dump =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{dump}");
    // One unit's lines in the dump.
    let section = |unit: &str| -> Vec<String> {
        let head = format!("-> Unit {unit}:");
        let mut lines = dump.lines().skip_while(|l| l.trim() != head);
        let first = lines.next();
        assert!(first.is_some(), "no {unit} in:\n{dump}");
        let body = lines.take_while(|l| !l.trim().starts_with("-> Unit "));
        body.map(|l| l.trim().to_string()).collect()
    };
    let park = section("runner-park.service");
    assert!(
        park.iter()
            .any(|l| l.starts_with("After: firecracker@dev.service")),
        "{park:#?}"
    );
    assert!(
        park.iter()
            .any(|l| l.starts_with("Before: shutdown.target"))
    );
    let stop = format!("Command Line: {} park --all", runner.display());
    assert!(park.contains(&stop), "{park:#?}");
    let dev = section("firecracker@dev.service");
    assert!(
        dev.iter()
            .any(|l| l.starts_with("Before: runner-park.service")),
        "{dev:#?}"
    );
}

// Every variable a credential can arrive as, in the terse form: a missing
// one is skipped rather than failing the boot.
#[test]
fn the_unit_loads_every_credential() {
    for var in Cred::ALL.iter().flat_map(|c| c.vars()) {
        let line = format!("\nLoadCredential={var}\n");
        assert!(runner::host::UNIT.contains(&line), "no {line:?}");
    }
}
