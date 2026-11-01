// Sleep when idle (src/idle.rs): the timer and what resets it on a fake
// clock, the setting, and the use lock commands hold while they use a VM.
// tests/boot.rs has a boot putting an idle VM to sleep, and tests/kvm.rs the
// real thing.
use proptest::prelude::*;
use runner::idle::{self, Idle, InUse, Look, Probe, Timer, Uses};
use runner::store::{Config, Store};
use std::fs;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MIN: Duration = Duration::from_secs(60);

// What a look sees, set by the test, and what it asked.
#[derive(Default)]
struct Seen {
    used: bool,
    running: bool,
    attached: bool,
    asked: Vec<&'static str>,
}

impl Probe for Seen {
    fn used(&mut self) -> bool {
        self.asked.push("used");
        self.used
    }
    fn running(&mut self) -> bool {
        self.asked.push("running");
        self.running
    }
    fn attached(&mut self) -> bool {
        self.asked.push("attached");
        self.attached
    }
}

// The clock is the test's: every look says when it is.
fn at(t0: Instant, d: Duration) -> Instant {
    t0 + d
}

fn ten() -> Idle {
    Idle::After(10 * 60)
}

#[test]
fn the_timer_fires_once_nothing_has_used_the_vm_for_its_idle_time() {
    let t0 = Instant::now();
    let mut timer = Timer::new(t0);
    let mut quiet = Seen::default();
    assert_eq!(
        idle::look(&mut timer, ten(), at(t0, 9 * MIN), &mut quiet),
        Look::Wait
    );
    assert_eq!(
        idle::look(&mut timer, ten(), at(t0, 10 * MIN), &mut quiet),
        Look::Sleep
    );
}

// Each kind of use, seen at minute 9, starts the 10 minutes again.
#[test]
fn each_kind_of_use_resets_the_timer() {
    type Set = fn(&mut Seen);
    let kinds: [(&str, Set); 3] = [
        ("a command holding or touching the use lock", |s| {
            s.used = true
        }),
        ("a headless run", |s| s.running = true),
        ("a terminal on a tmux session", |s| s.attached = true),
    ];
    for (kind, set) in kinds {
        let t0 = Instant::now();
        let mut timer = Timer::new(t0);
        let mut busy = Seen::default();
        set(&mut busy);
        // Attached is only asked once the timer is due, so the use comes then.
        let when = if busy.attached { 10 * MIN } else { 9 * MIN };
        let look = idle::look(&mut timer, ten(), at(t0, when), &mut busy);
        assert_eq!(look, Look::Busy, "{kind}");
        let mut quiet = Seen::default();
        let look = idle::look(&mut timer, ten(), at(t0, when + 9 * MIN), &mut quiet);
        assert_eq!(look, Look::Wait, "{kind}: the timer didn't start again");
        let look = idle::look(&mut timer, ten(), at(t0, when + 10 * MIN), &mut quiet);
        assert_eq!(look, Look::Sleep, "{kind}: the timer didn't fire after it");
    }
}

#[test]
fn never_never_sleeps() {
    let t0 = Instant::now();
    let mut timer = Timer::new(t0);
    let mut quiet = Seen::default();
    for hours in [0, 1, 24, 24 * 365] {
        let now = at(t0, Duration::from_secs(hours * 3600));
        let look = idle::look(&mut timer, Idle::Never, now, &mut quiet);
        assert_eq!(look, Look::Wait, "{hours}h");
    }
    // Nothing to ask either.
    assert!(quiet.asked.is_empty(), "{:?}", quiet.asked);
}

// The guest is asked about terminals only when nothing cheaper was in use and
// the time is up, so a busy VM's guest isn't asked at every look.
#[test]
fn the_guest_is_asked_only_when_the_timer_is_due() {
    let t0 = Instant::now();
    let mut timer = Timer::new(t0);
    let mut quiet = Seen::default();
    idle::look(&mut timer, ten(), at(t0, MIN), &mut quiet);
    assert_eq!(quiet.asked, ["used", "running"]);
    let mut used = Seen {
        used: true,
        ..Seen::default()
    };
    idle::look(&mut timer, ten(), at(t0, 20 * MIN), &mut used);
    assert_eq!(used.asked, ["used"]);
    let mut quiet = Seen::default();
    idle::look(&mut timer, ten(), at(t0, 30 * MIN), &mut quiet);
    assert_eq!(quiet.asked, ["used", "running", "attached"]);
}

// A clock that went back, or a look from before the last use, never fires.
#[test]
fn a_look_from_before_the_last_use_waits() {
    let t0 = Instant::now();
    let mut timer = Timer::new(at(t0, 20 * MIN));
    let mut quiet = Seen::default();
    let look = idle::look(&mut timer, ten(), at(t0, 15 * MIN), &mut quiet);
    assert_eq!(look, Look::Wait);
}

#[test]
fn the_idle_time_reads_as_minutes_or_with_a_unit() {
    let read = |s: &str| s.parse::<Idle>();
    assert_eq!(read("10"), Ok(Idle::After(600)));
    assert_eq!(read("10m"), Ok(Idle::After(600)));
    assert_eq!(read("90s"), Ok(Idle::After(90)));
    assert_eq!(read("2h"), Ok(Idle::After(7200)));
    assert_eq!(read("never"), Ok(Idle::Never));
    for bad in [
        "",
        "0",
        "0s",
        "m",
        "-1",
        "1.5",
        "10ms",
        "10 m",
        "1d",
        "Never",
        "4294967296",
    ] {
        let err = read(bad).unwrap_err();
        assert!(err.contains("minutes"), "{bad:?}: {err}");
    }
    assert_eq!(Idle::default(), Idle::After(600));
    assert_eq!(Idle::After(600).to_string(), "10m");
    assert_eq!(Idle::After(7200).to_string(), "2h");
    assert_eq!(Idle::After(90).to_string(), "90s");
    assert_eq!(Idle::Never.to_string(), "never");
    assert_eq!(Idle::After(90).after(), Some(Duration::from_secs(90)));
    assert_eq!(Idle::Never.after(), None);
}

proptest! {
    #[test]
    fn the_idle_time_reads_back_as_it_prints(secs in 1u32..) {
        let idle = Idle::After(secs);
        prop_assert_eq!(idle.to_string().parse::<Idle>(), Ok(idle));
    }

    #[test]
    fn anything_else_is_refused_not_panicked_on(text in "\\PC{0,12}") {
        let _ = text.parse::<Idle>();
    }
}

fn store_with_dev() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    store.import("dev", &disk).unwrap();
    (dir, store)
}

#[test]
fn a_vm_s_idle_time_is_set_per_vm_and_forks_keep_it() {
    let (_dir, store) = store_with_dev();
    assert_eq!(store.idle("dev").unwrap(), Idle::default());
    let quick = Config {
        idle: Some(Idle::After(60)),
        ..Config::default()
    };
    store.fork_with("dev", "quick", &quick).unwrap();
    assert_eq!(store.idle("quick").unwrap(), Idle::After(60));
    assert_eq!(store.idle("dev").unwrap(), Idle::default());
    store.fork("quick", "child").unwrap();
    assert_eq!(store.idle("child").unwrap(), Idle::After(60));

    store.set_idle("dev", Idle::Never).unwrap();
    assert_eq!(store.idle("dev").unwrap(), Idle::Never);
    // The rest of its config stays.
    let sized = Config {
        cpus: Some(4),
        ..Config::default()
    };
    store.fork_with("dev", "big", &sized).unwrap();
    store.set_idle("big", Idle::After(120)).unwrap();
    assert_eq!(store.machine("big").unwrap().cpus, 4);
    assert_eq!(store.idle("big").unwrap(), Idle::After(120));
    assert!(store.set_idle("nope", Idle::Never).is_err());
}

// The boot's view of the use lock: held, touched since its last look, or
// quiet, which it holds through a sleep.
#[test]
fn a_command_s_use_is_seen_while_it_lasts_and_once_after() {
    let (_dir, store) = store_with_dev();
    let mut uses = Uses::open(&store, "dev").unwrap();
    // Nothing yet.
    assert!(uses.quiet().unwrap().is_some());

    let used = InUse::take(&store, "dev").unwrap();
    assert!(uses.quiet().unwrap().is_none(), "held, and seen quiet");
    drop(used);
    // Let go since the last look, so it was used.
    assert!(
        uses.quiet().unwrap().is_none(),
        "used since, and seen quiet"
    );
    assert!(uses.quiet().unwrap().is_some(), "quiet, and seen used");

    // Even a use that came and went between two looks.
    drop(InUse::take(&store, "dev").unwrap());
    assert!(uses.quiet().unwrap().is_none());
    assert!(uses.quiet().unwrap().is_some());
}

// While the boot holds the lock quiet, as it does through a sleep, a command
// waits, and then sees the VM as the sleep left it.
#[test]
fn a_command_waits_for_a_sleep_under_way() {
    let (_dir, store) = store_with_dev();
    let mut uses = Uses::open(&store, "dev").unwrap();
    let quiet = uses.quiet().unwrap().expect("not quiet");
    let (took, taken) = mpsc::channel();
    let store2 = store.clone();
    let waiting = std::thread::spawn(move || {
        let used = InUse::take(&store2, "dev").unwrap();
        took.send(()).unwrap();
        used
    });
    assert!(
        taken.recv_timeout(Duration::from_millis(300)).is_err(),
        "a command took the VM while it went to sleep"
    );
    drop(quiet);
    taken.recv_timeout(Duration::from_secs(5)).unwrap();
    let used = waiting.join().unwrap();
    assert!(uses.quiet().unwrap().is_none());
    drop(used);
}

// Two commands at once both use the VM.
#[test]
fn uses_share_the_lock() {
    let (_dir, store) = store_with_dev();
    let a = InUse::take(&store, "dev").unwrap();
    let b = InUse::take(&store, "dev").unwrap();
    let mut uses = Uses::open(&store, "dev").unwrap();
    drop(a);
    assert!(uses.quiet().unwrap().is_none());
    drop(b);
    assert!(uses.quiet().unwrap().is_none());
    assert!(uses.quiet().unwrap().is_some());
}

#[test]
fn a_missing_vm_has_no_use_to_take() {
    let (_dir, store) = store_with_dev();
    assert!(InUse::take(&store, "nope").is_err());
}
