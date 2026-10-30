// `runner claude`: what it decides to create, start and attach, against a
// fake of the VMs. tests/sessions.rs runs its tmux command for real.
mod common;

use common::{FakeSystemd, text};
use runner::claude::{self, Claude, DIR, SESSION, Vms};
use runner::proto::{Exit, Request, WinSize};
use runner::sessions;
use runner::store::{self, Store};
use std::cell::RefCell;
use std::fs;
use std::process::{Command, Stdio};

// VMs by name, each running or not, and whether a running one has a claude
// session. Each call is logged; a call named in `fail` fails.
#[derive(Default)]
struct Fake {
    vms: RefCell<Vec<(String, bool)>>,
    session: bool,
    fail: Option<&'static str>,
    calls: RefCell<Vec<String>>,
    attached: RefCell<Option<Request>>,
}

impl Fake {
    fn with(vm: &str, running: bool) -> Fake {
        let fake = Fake::default();
        fake.vms.borrow_mut().push((vm.to_string(), running));
        fake
    }

    fn call(&self, what: &'static str, call: String) -> claude::Result<()> {
        self.calls.borrow_mut().push(call);
        if self.fail == Some(what) {
            return Err(store::Error::BadConfig(format!("{what} failed")).into());
        }
        Ok(())
    }

    fn running(&self, vm: &str) -> Option<bool> {
        let vms = self.vms.borrow();
        vms.iter().find(|(name, _)| name == vm).map(|(_, on)| *on)
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn attached(&self) -> Request {
        self.attached.borrow().clone().expect("nothing attached")
    }
}

impl Vms for Fake {
    fn exists(&self, vm: &str) -> claude::Result<bool> {
        self.call("exists", format!("exists {vm}"))?;
        Ok(self.running(vm).is_some())
    }

    fn create(&self, vm: &str, from: &str) -> claude::Result<()> {
        self.call("create", format!("create {vm} from {from}"))?;
        self.vms.borrow_mut().push((vm.to_string(), false));
        Ok(())
    }

    fn running(&self, vm: &str) -> claude::Result<bool> {
        self.call("running", format!("running {vm}"))?;
        Ok(self.running(vm).unwrap())
    }

    fn start(&self, vm: &str) -> claude::Result<()> {
        self.call("start", format!("start {vm}"))?;
        assert!(!self.running(vm).unwrap(), "started {vm} twice");
        let mut vms = self.vms.borrow_mut();
        vms.iter_mut().find(|(name, _)| name == vm).unwrap().1 = true;
        Ok(())
    }

    fn has_session(&self, vm: &str, user: &str, session: &str) -> claude::Result<bool> {
        self.call("has_session", format!("has_session {vm} {user} {session}"))?;
        assert!(self.running(vm).unwrap(), "looked in stopped {vm}");
        Ok(self.session)
    }

    fn attach(&self, vm: &str, req: &Request) -> claude::Result<Exit> {
        self.call("attach", format!("attach {vm}"))?;
        assert!(self.running(vm).unwrap(), "attached to stopped {vm}");
        *self.attached.borrow_mut() = Some(req.clone());
        Ok(Exit::Code(0))
    }
}

const SIZE: WinSize = WinSize {
    rows: 40,
    cols: 120,
};

fn ask(vm: &str, args: &[&str]) -> Claude {
    Claude {
        vm: vm.to_string(),
        from: None,
        dir: DIR.to_string(),
        user: "agent".to_string(),
        argv: std::iter::once("claude")
            .chain(args.iter().copied())
            .map(String::from)
            .collect(),
        env: vec![("TERM".to_string(), "xterm-kitty".to_string())],
        size: SIZE,
    }
}

// What it said on stderr, and what it returned.
fn open(fake: &Fake, c: &Claude) -> (String, claude::Result<Exit>) {
    let mut notes = Vec::new();
    let got = claude::open(fake, c, &mut notes);
    (text(&notes), got)
}

// claude, in a new session in `dir`, unless tmux finds the session there.
fn new_session(c: &Claude) -> Request {
    Request {
        argv: sessions::attach_or_run(SESSION, &c.dir, &c.argv),
        env: c.env.clone(),
        cwd: Some(c.dir.clone()),
        user: Some(c.user.clone()),
        tty: Some(SIZE),
    }
}

#[test]
fn a_new_vm_is_made_from_base_then_started_with_claude_in_a_new_session() {
    let fake = Fake::default();
    let c = ask("dev", &[]);
    let (notes, got) = open(&fake, &c);
    assert_eq!(got.unwrap(), Exit::Code(0));
    assert_eq!(
        fake.calls(),
        [
            "exists dev",
            "create dev from base",
            "start dev",
            "attach dev"
        ]
    );
    assert_eq!(fake.attached(), new_session(&c));
    assert_eq!(notes, "runner: made dev from base\n");
}

#[test]
fn from_picks_what_a_new_vm_forks() {
    let fake = Fake::with("golden", false);
    let mut c = ask("dev", &[]);
    c.from = Some("golden".to_string());
    let (notes, got) = open(&fake, &c);
    got.unwrap();
    assert_eq!(fake.calls()[..2], ["exists dev", "create dev from golden"]);
    assert_eq!(notes, "runner: made dev from golden\n");
}

#[test]
fn a_stopped_vm_is_started_not_made() {
    let fake = Fake::with("dev", false);
    let c = ask("dev", &["--continue"]);
    let (notes, got) = open(&fake, &c);
    got.unwrap();
    assert_eq!(
        fake.calls(),
        ["exists dev", "running dev", "start dev", "attach dev"]
    );
    assert_eq!(fake.attached(), new_session(&c));
    assert_eq!(notes, "");
}

#[test]
fn a_running_vm_without_claude_gets_a_new_session_with_the_arguments() {
    let fake = Fake::with("dev", true);
    let mut c = ask("dev", &["--continue", "fix the build"]);
    c.dir = "/home/agent/src".to_string();
    let (notes, got) = open(&fake, &c);
    got.unwrap();
    assert_eq!(
        fake.calls(),
        [
            "exists dev",
            "running dev",
            "has_session dev agent claude",
            "attach dev"
        ]
    );
    let req = fake.attached();
    assert_eq!(req, new_session(&c));
    // The arguments reach claude whole, after the session's own.
    let tail = &req.argv[req.argv.len() - 3..];
    assert_eq!(tail, ["claude", "--continue", "fix the build"]);
    assert!(req.argv.contains(&"/home/agent/src".to_string()), "{req:?}");
    assert_eq!(notes, "");
}

// The same tmux command either way: tmux attaches to a session that's there,
// so two at once still make one claude. The directory isn't needed to attach.
#[test]
fn a_running_claude_is_reattached_never_started_twice() {
    let mut fake = Fake::with("dev", true);
    fake.session = true;
    let c = ask("dev", &[]);
    let (notes, got) = open(&fake, &c);
    got.unwrap();
    assert_eq!(
        fake.calls(),
        [
            "exists dev",
            "running dev",
            "has_session dev agent claude",
            "attach dev"
        ]
    );
    let want = Request {
        cwd: None,
        ..new_session(&c)
    };
    assert_eq!(fake.attached(), want);
    assert_eq!(notes, "");
}

#[test]
fn arguments_for_a_session_already_there_are_dropped_with_a_note() {
    let mut fake = Fake::with("dev", true);
    fake.session = true;
    let (notes, got) = open(&fake, &ask("dev", &["--continue"]));
    got.unwrap();
    assert_eq!(
        notes,
        "runner: dev already has a claude session, so this attaches to it without --continue\n"
    );
}

#[test]
fn from_is_noted_and_unused_when_the_vm_exists() {
    let fake = Fake::with("dev", true);
    let mut c = ask("dev", &[]);
    c.from = Some("golden".to_string());
    let (notes, got) = open(&fake, &c);
    got.unwrap();
    assert!(!fake.calls().iter().any(|c| c.starts_with("create")));
    assert_eq!(
        notes,
        "runner: dev already exists, so it isn't made from golden\n"
    );
}

#[test]
fn a_failed_step_stops_the_rest() {
    let new = Fake::default;
    let stopped = || Fake::with("dev", false);
    let running = || Fake::with("dev", true);
    let looked = ["exists dev", "running dev", "has_session dev agent claude"];
    let cases: [(&str, fn() -> Fake, &[&str]); 6] = [
        ("exists", new, &["exists dev"]),
        ("create", new, &["exists dev", "create dev from base"]),
        ("running", stopped, &["exists dev", "running dev"]),
        (
            "start",
            stopped,
            &["exists dev", "running dev", "start dev"],
        ),
        ("has_session", running, &looked),
        ("attach", running, &[&looked[..], &["attach dev"]].concat()),
    ];
    for (fail, fake, want) in cases {
        let fake = Fake {
            fail: Some(fail),
            ..fake()
        };
        let (_, got) = open(&fake, &ask("dev", &[]));
        let err = got.unwrap_err().to_string();
        assert_eq!(err, format!("{fail} failed"));
        assert_eq!(fake.calls(), want, "{fail}");
    }
}

// The real VMs, through the runner binary: without a terminal there's nothing
// for tmux to attach, so it neither makes the VM nor boots it.
#[test]
fn claude_needs_a_terminal_before_it_makes_or_starts_anything() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("base", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    let out = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .env("PATH", sd.path())
        .args(["claude", "dev", "--", "--continue"])
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    let err = text(&out.stderr);
    assert!(err.contains("needs a terminal"), "{err}");
    assert!(!dir.path().join("vms/dev").exists());
    assert!(sd.calls().is_empty(), "{:?}", sd.calls());
}

#[test]
fn the_host_s_vms_are_the_store_s() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();
    let host = claude::Host(&store);
    assert!(host.exists("base").unwrap());
    assert!(!host.exists("dev").unwrap());
    let bad = host.exists("Not A Name").unwrap_err().to_string();
    assert!(bad.contains("isn't a VM name"), "{bad}");
    host.create("dev", "base").unwrap();
    assert_eq!(store.get("dev").unwrap().bytes, 6);
    // Another runner claude made it first.
    host.create("dev", "base").unwrap();
    let missing = host.create("dev-2", "nope").unwrap_err().to_string();
    assert_eq!(missing, "no VM named nope");
}
