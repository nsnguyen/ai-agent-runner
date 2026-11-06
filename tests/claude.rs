// `runner claude`: what it decides to create, start and attach, against a
// fake of the VMs. tests/sessions.rs runs its tmux command for real.
mod common;

use common::{FakeSystemd, text};
use runner::claude::{self, Claude, DIR, Run, SESSION, TakeOver, Vms};
use runner::proto::{Exit, Request, WinSize};
use runner::result::Outcome;
use runner::runs::State;
use runner::sessions;
use runner::store::{self, Store};
use std::cell::RefCell;
use std::fs;
use std::process::{Command, Stdio};

// VMs by name, each running or not, and whether a running one has a claude
// session. Each call is logged; a call named in `fail` fails. `run` is the
// headless run there is, and `ends` how a takeover leaves it: taken over
// unless set. With `opens`, a claude session opens while the run stops.
// Every generation's disk is kept unless `dropped`, and every session's
// record is on the disk unless `unrecorded`.
#[derive(Default)]
struct Fake {
    vms: RefCell<Vec<(String, bool)>>,
    session: bool,
    opens: RefCell<bool>,
    run: Option<Run>,
    ends: Option<Run>,
    dropped: bool,
    unrecorded: bool,
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
        Ok(self.session
            || *self.opens.borrow() && self.calls().iter().any(|c| c.starts_with("take_over")))
    }

    fn take_over(&self, id: &str) -> claude::Result<Run> {
        self.call("take_over", format!("take_over {id}"))?;
        let taken = Run {
            state: State::TakenOver,
            how: "taken over".to_string(),
            ..self.run.clone().expect("no run")
        };
        Ok(self.ends.clone().unwrap_or(taken))
    }

    fn attach(&self, vm: &str, req: &Request) -> claude::Result<Exit> {
        self.call("attach", format!("attach {vm}"))?;
        assert!(self.running(vm).unwrap(), "attached to stopped {vm}");
        *self.attached.borrow_mut() = Some(req.clone());
        Ok(Exit::Code(0))
    }

    fn run(&self, id: &str) -> claude::Result<Run> {
        self.call("run", format!("run {id}"))?;
        Ok(self.run.clone().expect("no run"))
    }

    fn kept(&self, vm: &str, generation: u64) -> claude::Result<bool> {
        self.call("kept", format!("kept {vm} {generation}"))?;
        Ok(!self.dropped)
    }

    // On generation 2, as the Host's does, refused while the VM is up.
    fn put_back(&self, vm: &str, generation: u64, run: &str) -> claude::Result<(u64, u64)> {
        self.call("put_back", format!("put_back {vm} {generation} {run}"))?;
        if self.running(vm).unwrap() {
            let (id, vm) = (run.to_string(), vm.to_string());
            return Err(claude::Error::Up { id, vm });
        }
        Ok((2, 5))
    }

    fn mark_taken(&self, id: &str, onto: Option<u64>) -> claude::Result<()> {
        let onto = onto.map(|g| format!(" onto {g}")).unwrap_or_default();
        self.call("mark_taken", format!("mark_taken {id}{onto}"))
    }

    fn has_record(&self, vm: &str, user: &str, session: &str) -> claude::Result<bool> {
        self.call("has_record", format!("has_record {vm} {user} {session}"))?;
        assert!(self.running(vm).unwrap(), "looked in stopped {vm}");
        Ok(!self.unrecorded)
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
    let looked = ["exists dev", "running dev", "has_session dev agent claude"];
    let attached = [&looked[..], &["attach dev"]].concat();
    // dev: None if it's new, else whether it's running.
    for (fail, dev, want) in [
        ("exists", None, &["exists dev"][..]),
        ("create", None, &["exists dev", "create dev from base"]),
        ("running", Some(false), &["exists dev", "running dev"]),
        (
            "start",
            Some(false),
            &["exists dev", "running dev", "start dev"],
        ),
        ("has_session", Some(true), &looked),
        ("attach", Some(true), &attached),
    ] {
        let mut fake = dev.map_or_else(Fake::default, |on| Fake::with("dev", on));
        fake.fail = Some(fail);
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

const UUID: &str = "0f8b2c4e-1a2b-4c3d-9e8f-123456789abc";

// dev.1, going, with its session named, in a directory of its own. It
// booted dev.
fn going() -> Run {
    Run {
        id: "dev.1".to_string(),
        vm: "dev".to_string(),
        state: State::Running,
        how: "running".to_string(),
        session: Some(UUID.to_string()),
        dir: "/home/agent/src".to_string(),
        user: "me".to_string(),
        boots: true,
        outcome: None,
        generation: None,
        taken: false,
    }
}

// dev.1 once it's over: it exited 0, and dev's shutdown committed its disk,
// generation 3.
fn committed() -> Run {
    Run {
        state: State::Exited,
        how: "exited 0, committed".to_string(),
        outcome: Some(Outcome::Committed),
        generation: Some(3),
        ..going()
    }
}

// Cancelled, so its disk, generation 3, rolled back.
fn rolled_back() -> Run {
    Run {
        state: State::Cancelled,
        how: "cancelled, rolled back".to_string(),
        outcome: Some(Outcome::RolledBack),
        ..committed()
    }
}

// What a takeover attaches: `claude --resume` and the arguments after `--`,
// in dev's claude session, in the run's directory, as its user.
fn resumed(t: &TakeOver) -> Request {
    let argv = ["claude", "--resume", UUID]
        .into_iter()
        .chain(t.args.iter().map(String::as_str))
        .map(String::from)
        .collect::<Vec<_>>();
    Request {
        argv: sessions::attach_or_run(SESSION, "/home/agent/src", &argv),
        env: t.env.clone(),
        cwd: Some("/home/agent/src".to_string()),
        user: Some("me".to_string()),
        tty: Some(SIZE),
    }
}

fn take(args: &[&str]) -> TakeOver {
    TakeOver {
        run: "dev.1".to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: vec![("TERM".to_string(), "xterm-kitty".to_string())],
        size: SIZE,
    }
}

fn take_over(fake: &Fake, t: &TakeOver) -> (String, claude::Result<Exit>) {
    let mut notes = Vec::new();
    let got = claude::take_over(fake, t, &mut notes);
    (text(&notes), got)
}

// `runner claude --run dev.1`: once the run's claude -p has stopped, its
// session goes on in dev's claude session, in the run's directory, as its
// user, with the arguments after `--`.
#[test]
fn a_run_taken_over_resumes_its_session_where_it_ran() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(going());
    let t = take(&["--model", "opus"]);
    let (notes, got) = take_over(&fake, &t);
    assert_eq!(got.unwrap(), Exit::Code(0));
    assert_eq!(
        fake.calls(),
        [
            "run dev.1",
            "has_session dev me claude",
            "take_over dev.1",
            "has_session dev me claude",
            "attach dev"
        ]
    );
    let argv = ["claude", "--resume", UUID, "--model", "opus"].map(String::from);
    let want = Request {
        argv: sessions::attach_or_run(SESSION, "/home/agent/src", &argv),
        env: t.env.clone(),
        cwd: Some("/home/agent/src".to_string()),
        user: Some("me".to_string()),
        tty: Some(SIZE),
    };
    assert_eq!(fake.attached(), want);
    assert_eq!(fake.attached(), resumed(&t));
    assert_eq!(
        notes,
        "runner: stopping dev.1's claude -p\n\
         runner: dev.1 is taken over: its session goes on in dev's claude session, \
         which `runner claude dev` reattaches\n"
    );
}

// Each is checked before anything stops, so a refused takeover leaves the
// run going.
#[test]
fn a_takeover_is_refused_before_the_run_is_stopped() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(Run {
        session: None,
        ..going()
    });
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev.1 has no Claude Code session yet, so there's none to resume: \
         `runner runs dev` shows it once Claude Code has named it"
    );
    assert_eq!(fake.calls(), ["run dev.1"]);

    let mut fake = Fake::with("dev", true);
    fake.run = Some(going());
    fake.session = true;
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev already has a claude session, which the run's would replace: \
         `runner kill dev claude` ends it"
    );
    assert_eq!(fake.calls(), ["run dev.1", "has_session dev me claude"]);
}

// Claude Code can exit on its own while the takeover waits on it: then the
// run ends as it would have, and nothing is opened.
#[test]
fn a_run_whose_claude_code_ends_first_isn_t_resumed() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(going());
    fake.ends = Some(Run {
        state: State::Exited,
        how: "exited 0, committed".to_string(),
        ..going()
    });
    let (notes, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev.1 ended before the takeover stopped it, so nothing was opened: \
         exited 0, committed. `runner claude --run dev.1` again picks it up from there"
    );
    assert_eq!(
        fake.calls(),
        ["run dev.1", "has_session dev me claude", "take_over dev.1"]
    );
    assert_eq!(notes, "runner: stopping dev.1's claude -p\n");

    // Or it's still stopping when the takeover gives up waiting.
    fake.ends = Some(going());
    fake.calls.borrow_mut().clear();
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        format!(
            "dev.1 is still stopping: once `runner runs dev` says it's taken over, \
             `runner claude dev --dir /home/agent/src --user me -- --resume {UUID}` carries \
             its session on"
        )
    );
    assert!(!fake.calls().contains(&"attach dev".to_string()));
}

// Stopping the run can take a while: a claude session opened meanwhile isn't
// attached to as if it were the run's.
#[test]
fn a_claude_session_opened_while_the_run_stops_isn_t_taken_for_the_run_s() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(going());
    *fake.opens.borrow_mut() = true;
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        format!(
            "dev.1 is taken over, but dev got a claude session while it stopped: \
             `runner kill dev claude` ends that, then `runner claude dev --dir /home/agent/src \
             --user me -- --resume {UUID}` carries the run's session on"
        )
    );
    assert_eq!(
        fake.calls(),
        [
            "run dev.1",
            "has_session dev me claude",
            "take_over dev.1",
            "has_session dev me claude"
        ]
    );
}

// A takeover names no VM, nor where or what a new one forks: the run says.
#[test]
fn run_takes_no_vm_from_or_dir() {
    for args in [
        &["claude", "dev", "--run", "dev.1"][..],
        &["claude", "--run", "dev.1", "--from", "base"],
        &["claude", "--run", "dev.1", "--dir", "/tmp"],
        &["claude"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", dir.path())
            .args(args)
            .stdin(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
}

// A run that's over goes on from the disk it ended on. One that committed
// ended on dev's own: a stopped dev is started for it, as `runner claude
// dev` would, then the session resumes as a going run's does.
#[test]
fn a_committed_run_s_takeover_starts_a_stopped_dev_then_resumes_where_it_ran() {
    let mut fake = Fake::with("dev", false);
    fake.run = Some(committed());
    let t = take(&["--model", "opus"]);
    let (notes, got) = take_over(&fake, &t);
    assert_eq!(got.unwrap(), Exit::Code(0));
    assert_eq!(
        fake.calls(),
        [
            "run dev.1",
            "running dev",
            "start dev",
            format!("has_record dev me {UUID}").as_str(),
            "mark_taken dev.1",
            "attach dev"
        ]
    );
    assert_eq!(fake.attached(), resumed(&t));
    assert_eq!(
        notes,
        "runner: dev.1's session goes on in dev's claude session, which `runner claude dev` \
         reattaches\n"
    );
}

// One that joined dev left it as it was, so its session is on dev's own disk
// too. #160's refusal holds: a claude session in dev would be replaced.
#[test]
fn a_joined_run_s_takeover_resumes_on_dev_as_it_runs() {
    let joined = Run {
        state: State::Failed,
        how: "failed, not rolled back, checkpoint 2 undoes it".to_string(),
        boots: false,
        outcome: Some(Outcome::NotRolledBack),
        generation: None,
        ..going()
    };
    let mut fake = Fake::with("dev", true);
    fake.run = Some(joined.clone());
    let t = take(&[]);
    let (_, got) = take_over(&fake, &t);
    assert_eq!(got.unwrap(), Exit::Code(0));
    assert_eq!(
        fake.calls(),
        [
            "run dev.1",
            "running dev",
            "has_session dev me claude",
            format!("has_record dev me {UUID}").as_str(),
            "mark_taken dev.1",
            "attach dev"
        ]
    );
    assert_eq!(fake.attached(), resumed(&t));

    let mut fake = Fake::with("dev", true);
    fake.run = Some(joined);
    fake.session = true;
    let (_, got) = take_over(&fake, &t);
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev already has a claude session, which the run's would replace: \
         `runner kill dev claude` ends it"
    );
    assert_eq!(
        fake.calls(),
        ["run dev.1", "running dev", "has_session dev me claude"]
    );
}

// One that rolled back ended on a disk of its own, kept in dev's gens/: a
// copy goes on top of dev's history first, as a restore's does, and dev
// boots onto it.
#[test]
fn a_rolled_back_run_s_takeover_puts_its_disk_back_first() {
    let mut fake = Fake::with("dev", false);
    fake.run = Some(rolled_back());
    let t = take(&[]);
    let (notes, got) = take_over(&fake, &t);
    assert_eq!(got.unwrap(), Exit::Code(0));
    assert_eq!(
        fake.calls(),
        [
            "run dev.1",
            "kept dev 3",
            "put_back dev 3 dev.1",
            "mark_taken dev.1 onto 5",
            "start dev",
            format!("has_record dev me {UUID}").as_str(),
            "attach dev"
        ]
    );
    assert_eq!(fake.attached(), resumed(&t));
    assert_eq!(
        notes,
        "runner: put dev.1's disk, generation 3, back on dev as generation 5, over generation \
         2: `runner restore dev 2` brings that back\n\
         runner: dev.1's session goes on in dev's claude session, which `runner claude dev` \
         reattaches\n"
    );
}

// Putting it back needs dev stopped, which would end its sessions: that's
// the owner's call. Nor is there anything to put back once gc dropped it.
#[test]
fn a_rolled_back_run_s_disk_isn_t_put_back_on_a_dev_that_s_up_or_once_gc_dropped_it() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(rolled_back());
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev.1 rolled back, so taking it over puts its disk back on dev, which needs dev \
         stopped and would end its sessions: `runner stop dev` first"
    );
    assert_eq!(
        fake.calls(),
        ["run dev.1", "kept dev 3", "put_back dev 3 dev.1"]
    );

    let mut fake = Fake::with("dev", true);
    fake.run = Some(rolled_back());
    fake.dropped = true;
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev.1 rolled back, and its disk, dev's generation 3, is gone: gc dropped it at a \
         later commit, and the session with it"
    );
    assert_eq!(fake.calls(), ["run dev.1", "kept dev 3"]);
}

// Each before anything starts or changes.
#[test]
fn a_run_that_s_over_without_a_session_or_a_known_disk_is_refused() {
    for (run, want) in [
        (
            Run {
                state: State::Lost,
                how: "lost: the process driving it died: /r/log may say why".to_string(),
                outcome: None,
                ..going()
            },
            "dev.1 is over, but what became of its disk isn't known, so neither is its \
             session: lost: the process driving it died: /r/log may say why",
        ),
        (
            Run {
                state: State::Failed,
                how: "failed, rolled back: no directory /home/agent/src".to_string(),
                session: None,
                ..rolled_back()
            },
            "dev.1 ended before Claude Code named a session, so there's none to resume: \
             failed, rolled back: no directory /home/agent/src",
        ),
        (
            Run {
                generation: None,
                ..rolled_back()
            },
            "dev.1 is over, but what became of its disk isn't known, so neither is its \
             session: cancelled, rolled back",
        ),
    ] {
        let mut fake = Fake::with("dev", false);
        fake.run = Some(run);
        let (_, got) = take_over(&fake, &take(&[]));
        assert_eq!(got.unwrap_err().to_string(), want);
        assert_eq!(fake.calls(), ["run dev.1"], "{want}");
    }
}

// A restore, undo or later rollback can take the session's record from dev's
// disk: then the runner says so, in place of claude's own "No conversation
// found".
#[test]
fn a_session_whose_record_isn_t_on_the_disk_is_named() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(committed());
    fake.unrecorded = true;
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        format!(
            "dev.1's session isn't on dev's disk: me has no ~/.claude/projects/*/{UUID}.jsonl \
             there, so a restore, undo or rollback since has taken it"
        )
    );
    assert!(!fake.calls().contains(&"attach dev".to_string()));
    assert!(!fake.calls().contains(&"mark_taken dev.1".to_string()));

    // On a disk just put back, it was never written there, and the disk
    // from before is a restore away.
    let mut fake = Fake::with("dev", false);
    fake.run = Some(rolled_back());
    fake.unrecorded = true;
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        format!(
            "dev.1's session isn't on dev's disk: me has no ~/.claude/projects/*/{UUID}.jsonl \
             there, so Claude Code never wrote it on the run's disk, which dev runs on now: \
             `runner restore dev 2` brings back the disk from before"
        )
    );
}

// A run taken over already, while it went or since it ended, has its session
// on dev's own disk, or in dev's claude session if that's there: then the
// takeover points at `runner claude dev`, which reattaches. Its disk isn't
// put back again.
#[test]
fn a_run_taken_over_already_reattaches_or_goes_on_from_dev_s_own_disk() {
    let mut fake = Fake::with("dev", true);
    fake.run = Some(Run {
        state: State::TakenOver,
        how: "taken over".to_string(),
        ..going()
    });
    fake.session = true;
    let (_, got) = take_over(&fake, &take(&[]));
    assert_eq!(
        got.unwrap_err().to_string(),
        "dev.1 is taken over already: its session goes on in dev's claude session, which \
         `runner claude dev` reattaches"
    );
    assert_eq!(
        fake.calls(),
        ["run dev.1", "running dev", "has_session dev me claude"]
    );

    let mut fake = Fake::with("dev", false);
    fake.run = Some(Run {
        taken: true,
        ..rolled_back()
    });
    let t = take(&[]);
    let (_, got) = take_over(&fake, &t);
    assert_eq!(got.unwrap(), Exit::Code(0));
    assert_eq!(
        fake.calls(),
        [
            "run dev.1",
            "running dev",
            "start dev",
            format!("has_record dev me {UUID}").as_str(),
            "attach dev"
        ]
    );
    assert_eq!(fake.attached(), resumed(&t));
}

// The record is looked for as the run's user, under its home.
#[test]
fn the_record_is_any_project_s_session_file_in_the_user_s_home() {
    let home = tempfile::tempdir().unwrap();
    let look = |session: &str| {
        Command::new("sh")
            .args(["-c", claude::RECORD, "sh", session])
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .status()
            .unwrap()
            .code()
    };
    assert_eq!(look(UUID), Some(1));
    let project = home.path().join(".claude/projects/-home-agent-src");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("4c1e0b2a-1a2b-4c3d-9e8f-123456789abc.jsonl"),
        "{}\n",
    )
    .unwrap();
    assert_eq!(look(UUID), Some(1));
    fs::write(project.join(format!("{UUID}.jsonl")), "{}\n").unwrap();
    assert_eq!(look(UUID), Some(0));
}
