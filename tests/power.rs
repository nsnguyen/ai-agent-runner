// The host's power-off once idle (src/power.rs): first the decision, from
// what a look saw, on a clock the test gives; then the busy lock commands
// hold; then `runner host-idle` itself, with fake systemctl and loginctl on
// PATH, whose `systemctl poweroff` only logs; then the timer and service
// host/install.sh installs, as systemd reads them.
mod common;

use common::{FakeSystemd, me, script, text};
use runner::host;
use runner::idle::Idle;
use runner::power::{self, Busy, Clock, Seen, Verdict};
use runner::store::{Slept, Store};
use runner::systemd::State;
use std::fs::{self, File, TryLockError};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

const MIN: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(60 * 60);

fn idle(text: &str) -> Idle {
    text.parse().unwrap()
}

// When the host booted.
fn booted() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn vms(states: &[(&str, State)]) -> Seen {
    Seen {
        vms: states.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
        ..Seen::default()
    }
}

fn stopped() -> Seen {
    vms(&[("base", State::Stopped), ("dev", State::Stopped)])
}

// One look, as runner on the host.
fn decide(clock: &mut Clock, idle: &str, seen: &Seen, now: SystemTime) -> Verdict {
    power::decide(clock, self::idle(idle), "runner", seen, now)
}

#[track_caller]
fn off(verdict: Verdict) -> String {
    match verdict {
        Verdict::Off(why) => why,
        v => panic!("{v:?}, not off"),
    }
}

#[test]
fn every_vm_stopped_or_asleep_for_the_idle_time_powers_off() {
    let seen = vms(&[("base", State::Stopped), ("dev", State::Sleeping)]);
    let mut clock = Clock::new(booted(), None);
    let early = decide(&mut clock, "1h", &seen, booted() + HOUR - MIN);
    assert_eq!(early, Verdict::Wait(MIN));
    let why = off(decide(&mut clock, "1h", &seen, booted() + HOUR));
    assert!(why.contains("1h"), "{why}");
    // Past due too, say after a look the timer missed.
    off(decide(&mut clock, "1h", &seen, booted() + 5 * HOUR));
    // And with no VMs at all.
    let mut clock = Clock::new(booted(), None);
    off(decide(
        &mut clock,
        "90s",
        &Seen::default(),
        booted() + 90 * MIN,
    ));
}

#[test]
fn a_vm_running_booting_or_stopping_keeps_the_host_up_and_starts_the_clock_again() {
    for state in [State::Running, State::Booting, State::Stopping] {
        let mut clock = Clock::new(booted(), None);
        let at = booted() + 3 * HOUR;
        let up = vms(&[("base", State::Stopped), ("dev", state)]);
        let used = decide(&mut clock, "1h", &up, at);
        assert_eq!(used, Verdict::Used(format!("dev is {state}")));
        assert_eq!(clock.since(), at);
        // Stopped since: a whole idle time from the look that saw it.
        let left = decide(&mut clock, "1h", &stopped(), at + MIN);
        assert_eq!(left, Verdict::Wait(HOUR - MIN), "{state}");
        off(decide(&mut clock, "1h", &stopped(), at + HOUR));
    }
}

#[test]
fn a_login_keeps_the_host_up_and_starts_the_clock_again() {
    let mut clock = Clock::new(booted(), None);
    let at = booted() + 3 * HOUR;
    let seen = Seen {
        logins: vec!["runner".into(), "ubuntu".into()],
        ..stopped()
    };
    let used = decide(&mut clock, "1h", &seen, at);
    assert_eq!(used, Verdict::Used("ubuntu is logged in".into()));
    assert_eq!(clock.since(), at);
    let left = decide(&mut clock, "1h", &stopped(), at + 20 * MIN);
    assert_eq!(left, Verdict::Wait(40 * MIN));
}

#[test]
fn runner_s_own_sessions_don_t_count() {
    let mut clock = Clock::new(booted(), None);
    let seen = Seen {
        logins: vec!["runner".into(), "runner".into()],
        ..stopped()
    };
    off(decide(&mut clock, "1h", &seen, booted() + HOUR));
    // They're the sessions of whoever the look runs as.
    let mut clock = Clock::new(booted(), None);
    let theirs = power::decide(&mut clock, idle("1h"), "ubuntu", &seen, booted() + HOUR);
    assert_eq!(theirs, Verdict::Used("runner is logged in".into()));
}

#[test]
fn never_never_powers_off() {
    let mut clock = Clock::new(booted(), None);
    let never = power::decide(
        &mut clock,
        Idle::Never,
        "runner",
        &stopped(),
        booted() + 1000 * HOUR,
    );
    assert_eq!(never, Verdict::Never);
}

#[test]
fn a_host_up_for_less_than_the_idle_time_stays_up() {
    // Last used before this boot, as when aws.sh starts it in the morning:
    // it gets a whole idle time from the boot.
    let mut clock = Clock::new(booted(), Some(booted() - 24 * HOUR));
    assert_eq!(clock.since(), booted());
    let left = decide(&mut clock, "1h", &stopped(), booted() + 59 * MIN);
    assert_eq!(left, Verdict::Wait(MIN));
    // Used since the boot: a whole idle time from then.
    let mut clock = Clock::new(booted(), Some(booted() + 10 * MIN));
    let left = decide(&mut clock, "1h", &stopped(), booted() + HOUR);
    assert_eq!(left, Verdict::Wait(10 * MIN));
    // A clock that went back reads as just used, never as long idle.
    let mut clock = Clock::new(booted(), None);
    let left = decide(&mut clock, "1h", &stopped(), booted() - HOUR);
    assert_eq!(left, Verdict::Wait(HOUR));
}

#[test]
fn a_held_busy_lock_keeps_the_host_up_and_its_clock_starts_when_it_s_let_go() {
    let mut clock = Clock::new(booted(), None);
    let at = booted() + 3 * HOUR;
    let held = Seen {
        busy: true,
        ..stopped()
    };
    let used = decide(&mut clock, "1h", &held, at);
    assert_eq!(used, Verdict::Used("a runner command is at work".into()));
    // Let go 20 minutes on, which the command marks in the lock's mtime.
    let mut clock = Clock::new(booted(), Some(at + 20 * MIN));
    let left = decide(&mut clock, "1h", &stopped(), at + HOUR);
    assert_eq!(left, Verdict::Wait(20 * MIN));
    off(decide(&mut clock, "1h", &stopped(), at + 20 * MIN + HOUR));
}

fn mtime(path: &std::path::Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

#[test]
fn a_command_holds_the_busy_lock_shared_and_touches_it_as_it_lets_go() {
    let dir = tempfile::tempdir().unwrap();
    let lock = power::busy_lock(dir.path());
    let busy = Busy::take(dir.path()).expect("the lock, in a root that's there");
    // Shared: another command takes it too.
    let other = Busy::take(dir.path()).unwrap();
    let look = File::options().write(true).open(&lock).unwrap();
    assert!(matches!(look.try_lock(), Err(TryLockError::WouldBlock)));
    let long_ago = SystemTime::now() - HOUR;
    look.set_modified(long_ago).unwrap();
    drop(other);
    assert!(matches!(look.try_lock(), Err(TryLockError::WouldBlock)));
    drop(busy);
    look.try_lock().unwrap();
    assert!(mtime(&lock) > long_ago + MIN, "{:?}", mtime(&lock));
}

#[test]
fn a_command_goes_on_without_the_lock_in_a_root_that_isn_t_there() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Busy::take(&dir.path().join("nowhere")).is_none());
}

// A host with base and dev, both stopped, nobody logged in, and the
// look's files: its idle time, and /proc/uptime.
struct Host {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
}

fn host(idle: &str, up: Duration) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path().join("root"));
    store.import("base", &disk).unwrap();
    store.fork("base", "dev").unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state("inactive");
    let h = Host { dir, store, sd };
    h.set_idle(idle);
    fs::write(h.uptime(), format!("{}.25 1234.50\n", up.as_secs())).unwrap();
    h
}

impl Host {
    fn setting(&self) -> PathBuf {
        self.dir.path().join("host-idle")
    }

    fn set_idle(&self, idle: &str) {
        fs::write(self.setting(), format!("{idle}\n")).unwrap();
    }

    fn uptime(&self) -> PathBuf {
        self.dir.path().join("uptime")
    }

    fn lock(&self) -> PathBuf {
        power::busy_lock(self.store.root())
    }

    // The last use, as the busy lock's mtime marks it.
    fn used_ago(&self, ago: Duration) {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock());
        file.unwrap().set_modified(SystemTime::now() - ago).unwrap();
    }

    fn look(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", self.store.root())
            .env("PATH", self.sd.path())
            .arg("host-idle")
            .arg("--setting")
            .arg(self.setting())
            .arg("--uptime")
            .arg(self.uptime())
            .output()
            .unwrap()
    }

    #[track_caller]
    fn ok(&self) -> String {
        let out = self.look();
        assert!(
            out.status.success(),
            "{}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn powered_off(&self) -> bool {
        self.sd.calls().iter().any(|c| c == "systemctl poweroff")
    }

    fn asleep(&self, vm: &str) {
        self.sd.set_vm_state(vm, "active");
        let snap = self.store.snapshot(vm).unwrap();
        snap.make_room().unwrap();
        let at = chrono::Utc::now();
        snap.note(&Slept { generation: 1, at }).unwrap();
    }
}

#[test]
fn the_look_powers_off_a_host_idle_for_its_idle_time_and_says_why() {
    let h = host("1h", 3 * HOUR);
    h.used_ago(2 * HOUR);
    h.asleep("dev");
    // The look's own user's sessions, as runner's are on the host.
    h.sd.set_logins(&[&me(), &me()]);
    let said = h.ok();
    assert!(h.powered_off(), "{:?}", h.sd.calls());
    assert!(said.contains("powering off"), "{said}");
    assert!(said.contains("1h"), "{said}");
    // Its reasons came first.
    let calls = h.sd.calls();
    let poweroff = calls
        .iter()
        .position(|c| c == "systemctl poweroff")
        .unwrap();
    assert!(
        calls[..poweroff]
            .iter()
            .any(|c| c.starts_with("loginctl list-sessions")),
        "{calls:?}"
    );
    assert!(
        calls[..poweroff]
            .iter()
            .any(|c| c.contains("firecracker@dev.service")),
        "{calls:?}"
    );
}

#[test]
fn the_look_keeps_a_host_in_use_up_and_starts_its_clock_again() {
    type Use = fn(&Host);
    let in_use: [(&str, Use); 2] = [
        ("a login", |h| h.sd.set_logins(&["ubuntu"])),
        ("a running VM", |h| h.sd.set_vm_state("dev", "active")),
    ];
    for (what, make) in in_use {
        let h = host("1h", 3 * HOUR);
        h.used_ago(2 * HOUR);
        make(&h);
        assert_eq!(h.ok(), "", "{what}");
        assert!(!h.powered_off(), "{what}: {:?}", h.sd.calls());
        let since = SystemTime::now().duration_since(mtime(&h.lock())).unwrap();
        assert!(since < MIN, "{what}: the clock started {since:?} ago");
    }
}

#[test]
fn the_look_waits_out_the_idle_time() {
    // Up for less than the idle time.
    let h = host("1h", 30 * MIN);
    h.used_ago(2 * HOUR);
    assert_eq!(h.ok(), "");
    assert!(!h.powered_off(), "{:?}", h.sd.calls());
    // Used less than the idle time ago.
    let h = host("1h", 3 * HOUR);
    h.used_ago(10 * MIN);
    h.ok();
    assert!(!h.powered_off(), "{:?}", h.sd.calls());
    // A command at work, here.
    let h = host("1h", 3 * HOUR);
    h.used_ago(2 * HOUR);
    let busy = Busy::take(h.store.root()).unwrap();
    h.ok();
    assert!(!h.powered_off(), "{:?}", h.sd.calls());
    drop(busy);
    h.ok();
    assert!(!h.powered_off(), "its clock starts as it lets go");
}

#[test]
fn a_host_set_to_never_or_not_set_looks_at_nothing() {
    let h = host("never", 300 * HOUR);
    h.used_ago(200 * HOUR);
    h.ok();
    assert_eq!(h.sd.calls(), Vec::<String>::new());
    // install.sh's default, before it writes the file.
    fs::remove_file(h.setting()).unwrap();
    h.ok();
    assert_eq!(h.sd.calls(), Vec::<String>::new());
}

#[test]
fn a_bad_setting_or_a_failed_poweroff_fails_the_look_saying_so() {
    let h = host("soon", 3 * HOUR);
    let out = h.look();
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("soon") && err.contains("host-idle"), "{err}");
    assert!(!h.powered_off());

    let h = host("1h", 3 * HOUR);
    h.used_ago(2 * HOUR);
    let d = h.sd.bin.display();
    script(
        &h.sd.bin.join("systemctl"),
        &format!(
            "echo \"systemctl $*\" >> \"{d}/calls\"\ncase \"$1\" in\nshow) echo inactive ;;\npoweroff) echo 'Access denied' >&2; exit 1 ;;\nesac\n"
        ),
    );
    let out = h.look();
    assert!(!out.status.success());
    assert!(
        text(&out.stdout).contains("powering off"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stderr).contains("Access denied"),
        "{}",
        text(&out.stderr)
    );
}

// systemd 255's own reading of the units: no errors, a look a minute after
// boot and every minute after that, as runner, in the unit's root.
#[test]
fn the_timer_and_its_service_pass_systemd_analyze_verify() {
    let dir = tempfile::tempdir().unwrap();
    // verify wants the program there: this build's, where the unit runs it.
    let runner = env!("CARGO_BIN_EXE_runner");
    let service = host::IDLE_SERVICE_UNIT.replace("/usr/local/bin/runner", runner);
    assert_ne!(service, host::IDLE_SERVICE_UNIT);
    let service_at = dir.path().join(host::IDLE_SERVICE);
    let timer_at = dir.path().join(host::IDLE_TIMER);
    fs::write(&service_at, service).unwrap();
    fs::write(&timer_at, host::IDLE_TIMER_UNIT).unwrap();
    let out = Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(&timer_at)
        .arg(&service_at)
        .env("SYSTEMD_LOG_LEVEL", "debug")
        .output()
        .expect("systemd-analyze, from systemd, to be installed");
    let dump = text(&out.stderr) + &text(&out.stdout);
    assert!(out.status.success(), "{dump}");
    for bad in [
        "Unknown key",
        "Unknown section",
        "Failed to parse",
        "is not executable",
    ] {
        assert!(!dump.contains(bad), "{bad}:\n{dump}");
    }
    assert!(
        dump.contains(&format!("Command Line: {runner} host-idle")),
        "{dump}"
    );
    assert!(dump.contains("User: runner"), "{dump}");
    assert!(dump.contains("RUNNER_ROOT=/var/lib/runner"), "{dump}");
    assert!(dump.contains("OnBootSec: 1min"), "{dump}");
    assert!(dump.contains("OnUnitActiveSec: 1min"), "{dump}");
}

// A start that waits for the test to say go, as a slow boot does.
fn slow_start(sd: &FakeSystemd) {
    let d = sd.bin.display();
    script(
        &sd.bin.join("systemctl"),
        &format!(
            r#"echo "systemctl $*" >> "{d}/calls"
case "$1" in
start) touch "{d}/started"; while [ ! -e "{d}/go" ]; do sleep 0.01; done ;;
show) echo active ;;
esac
"#
        ),
    );
}

#[test]
fn a_runner_command_holds_the_busy_lock_while_it_works() {
    let h = host("1h", 3 * HOUR);
    slow_start(&h.sd);
    let mut start = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", h.store.root())
        .env("PATH", h.sd.path())
        .args(["start", "dev"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    common::wait_for("the start", || h.sd.bin.join("started").exists());
    let look = File::options().write(true).open(h.lock()).unwrap();
    assert!(matches!(look.try_lock(), Err(TryLockError::WouldBlock)));
    look.set_modified(SystemTime::now() - HOUR).unwrap();
    fs::write(h.sd.bin.join("go"), "").unwrap();
    assert!(start.wait().unwrap().success());
    look.try_lock().unwrap();
    let since = SystemTime::now().duration_since(mtime(&h.lock())).unwrap();
    assert!(since < MIN, "let go {since:?} ago");
}
