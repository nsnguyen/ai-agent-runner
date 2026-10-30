// Detachable sessions end to end: the runner binary with systemctl faked, and
// runner-guest on a Unix socket behind a fake of Firecracker's vsock socket,
// running the real tmux as the test's user. Tests share that user's tmux
// server, so each names its sessions apart and kills them when it's done.
mod common;

use common::{FakeSystemd, FakeVsock, Guest, me, pty, text, wait_for};
use proptest::prelude::*;
use runner::sessions::{self, Session};
use runner::store::Store;
use std::fs;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

// A running VM named dev whose guest is up, as far as the runner can tell.
struct Vm {
    dir: TempDir,
    sd: FakeSystemd,
    _vsock: FakeVsock,
    _guest: Guest,
    made: Mutex<Vec<String>>,
}

fn vm() -> Vm {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state("active");
    let guest = Guest::spawn(&dir.path().join("guest.sock"));
    let vsock = FakeVsock::serve(&dir.path().join("vms/dev/vsock.sock"), &guest.sock);
    Vm {
        dir,
        sd,
        _vsock: vsock,
        _guest: guest,
        made: Mutex::new(Vec::new()),
    }
}

// Sessions a failed test made don't outlive it.
impl Drop for Vm {
    fn drop(&mut self) {
        for name in self.made.lock().unwrap().iter() {
            self.tmux(&["kill-session", "-t", &format!("={name}")]);
        }
    }
}

impl Vm {
    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.dir.path())
            .env("PATH", self.sd.path())
            .env("RUNNER_USER", me())
            .env_remove("TERM")
            .args(args);
        cmd
    }

    // Stdin is a pipe, not a terminal.
    fn run(&self, args: &[&str]) -> Output {
        self.runner(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    // A session name no other test uses.
    fn name(&self, what: &str) -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("t{}-{n}-{what}", std::process::id());
        self.made.lock().unwrap().push(name.clone());
        name
    }

    // tmux in the guest, as the runner's user, the way the runner runs it.
    fn tmux(&self, args: &[&str]) -> Output {
        let mut argv = vec!["exec", "dev", "--", "tmux"];
        argv.extend(args);
        self.run(&argv)
    }

    // Detaches every terminal on the session, as its prefix key and d would.
    fn detach(&self, name: &str) {
        let out = self.tmux(&["detach-client", "-s", &format!("={name}")]);
        assert!(out.status.success(), "{}", text(&out.stderr));
    }

    fn sessions(&self) -> String {
        let out = self.run(&["sessions", "dev"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout)
    }

    // The session's line in `runner sessions`, split at its double spaces.
    fn row(&self, name: &str) -> Option<Vec<String>> {
        self.sessions()
            .lines()
            .map(|l| l.split("  ").map(String::from).collect::<Vec<_>>())
            .find(|cols| cols[0] == name)
    }

    fn wait_for_row(&self, name: &str, command: &str, attached: &str) {
        wait_for(&format!("{name} {command} {attached}"), || {
            self.row(name)
                .is_some_and(|cols| cols[1] == command && cols[3] == attached)
        });
    }

    fn started(&self) -> bool {
        self.sd
            .calls()
            .iter()
            .any(|c| c.starts_with("systemctl start"))
    }
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

#[test]
fn shell_with_a_session_makes_it_and_the_same_name_rejoins_it() {
    let vm = vm();
    let s = vm.name("work");
    let t = pty(24, 80);
    let mut first = t.spawn(vm.runner(&["shell", "dev", "--session", &s]));
    // The arithmetic tells the output from the echo of what was typed.
    t.send("echo one-$((40+2))\r");
    t.wait_for("one-42");
    vm.detach(&s);
    assert_eq!(first.wait().unwrap().code(), Some(0), "{}", t.seen());
    let cols = vm.row(&s).unwrap_or_else(|| panic!("{}", vm.sessions()));
    assert_eq!(cols[3], "detached");

    // One session still, with what ran before on its screen.
    let t = pty(24, 80);
    let mut second = t.spawn(vm.runner(&["shell", "dev", "-s", &s]));
    t.wait_for("one-42");
    vm.wait_for_row(&s, &cols[1], "attached");
    vm.detach(&s);
    assert_eq!(second.wait().unwrap().code(), Some(0), "{}", t.seen());
    let ours = vm.sessions().lines().filter(|l| l.starts_with(&s)).count();
    assert_eq!(ours, 1, "{}", vm.sessions());
}

// What sshd does to the runner when the SSH connection drops: SIGHUP.
#[test]
fn a_dropped_connection_detaches_the_session_and_never_kills_it() {
    let vm = vm();
    let s = vm.name("drop");
    let t = pty(24, 80);
    let mut shell = t.spawn(vm.runner(&["shell", "dev", "--session", &s]));
    t.send("echo before-$((1+1)); sleep 300\r");
    t.wait_for("before-2");
    vm.wait_for_row(&s, "sleep", "attached");
    let pid = nix::unistd::Pid::from_raw(shell.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::SIGHUP).unwrap();
    assert_eq!(shell.wait().unwrap().code(), Some(129), "{}", t.seen());
    // Still there, still running the sleep, with nobody on it.
    vm.wait_for_row(&s, "sleep", "detached");

    let t = pty(24, 80);
    let mut again = t.spawn(vm.runner(&["attach", "dev", &s]));
    t.wait_for("before-2");
    vm.detach(&s);
    assert_eq!(again.wait().unwrap().code(), Some(0), "{}", t.seen());
}

#[test]
fn sessions_lists_the_command_start_time_and_whether_anyone_is_attached() {
    let vm = vm();
    let s = vm.name("list");
    let made = vm.tmux(&["new-session", "-d", "-s", &s, "sleep 300"]);
    assert!(made.status.success(), "{}", text(&made.stderr));
    let cols = vm.row(&s).unwrap_or_else(|| panic!("{}", vm.sessions()));
    assert_eq!(cols.len(), 4, "{cols:?}");
    assert_eq!((cols[1].as_str(), cols[3].as_str()), ("sleep", "detached"));
    let started = chrono::DateTime::parse_from_rfc3339(&cols[2]).unwrap();
    let ago = chrono::Utc::now().signed_duration_since(started);
    assert!(ago.num_seconds().abs() < 60, "{started}");
    assert!(cols[2].ends_with('Z'), "{cols:?}");

    let t = pty(24, 80);
    let mut on = t.spawn(vm.runner(&["attach", "dev", &s]));
    vm.wait_for_row(&s, "sleep", "attached");
    vm.detach(&s);
    assert_eq!(on.wait().unwrap().code(), Some(0), "{}", t.seen());
}

#[test]
fn kill_ends_the_session_and_what_runs_in_it() {
    let vm = vm();
    let s = vm.name("kill");
    vm.tmux(&["new-session", "-d", "-s", &s, "sleep 300"]);
    let out = vm.run(&["kill", "dev", &s]);
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), format!("killed {s}\n"));
    assert!(vm.row(&s).is_none(), "{}", vm.sessions());
    let out = vm.run(&["kill", "dev", &s]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        text(&out.stderr),
        format!("runner: no session {s} in dev\n")
    );
}

// tmux matches a name by its start unless told not to.
#[test]
fn names_match_whole() {
    let vm = vm();
    let longer = vm.name("prefix");
    let s = &longer[..longer.len() - 1];
    vm.tmux(&["new-session", "-d", "-s", &longer, "sleep 300"]);
    let t = pty(24, 80);
    let mut on = t.spawn(vm.runner(&["attach", "dev", s]));
    assert_eq!(on.wait().unwrap().code(), Some(1), "{}", t.seen());
    t.wait_for(&format!("can't find session: {s}"));
    let out = vm.run(&["kill", "dev", s]);
    assert_eq!(code(&out), 1);
    assert!(vm.row(&longer).is_some(), "{}", vm.sessions());
}

#[test]
fn a_stopped_vm_has_no_sessions_and_isn_t_started_to_look() {
    let vm = vm();
    vm.sd.set_state("inactive");
    assert_eq!(vm.sessions(), "no sessions\n");
    let out = vm.run(&["kill", "dev", "work"]);
    assert_eq!(code(&out), 1);
    assert_eq!(text(&out.stderr), "runner: no session work in dev\n");
    let t = pty(24, 80);
    let mut on = t.spawn(vm.runner(&["attach", "dev", "work"]));
    assert_eq!(on.wait().unwrap().code(), Some(255), "{}", t.seen());
    t.wait_for("runner: no session work in dev");
    assert!(!vm.started(), "{:?}", vm.sd.calls());
}

#[test]
fn shell_with_a_session_starts_a_stopped_vm() {
    let vm = vm();
    vm.sd.set_state("inactive");
    let s = vm.name("boot");
    let t = pty(24, 80);
    let mut shell = t.spawn(vm.runner(&["shell", "dev", "--session", &s]));
    vm.wait_for_row(&s, "bash", "attached");
    vm.detach(&s);
    assert_eq!(shell.wait().unwrap().code(), Some(0), "{}", t.seen());
    assert!(vm.started(), "{:?}", vm.sd.calls());
}

// tmux can't attach without one, so neither boots the VM to find that out.
#[test]
fn a_session_needs_a_terminal() {
    let vm = vm();
    vm.sd.set_state("inactive");
    for args in [
        &["shell", "dev", "--session", "work"][..],
        &["attach", "dev", "work"],
    ] {
        let out = vm.run(args);
        assert_eq!(code(&out), 255, "{args:?}");
        let err = text(&out.stderr);
        assert!(err.contains("needs a terminal"), "{args:?}: {err}");
    }
    assert!(vm.sd.calls().is_empty(), "{:?}", vm.sd.calls());
}

#[test]
fn bad_session_names_are_refused_before_anything_runs() {
    let vm = vm();
    let long = "a".repeat(33);
    for bad in ["", "a:b", "a.b", "-x", "has space", "=work", &long] {
        let out = vm.run(&["kill", "dev", bad]);
        assert_eq!(code(&out), 1, "{bad:?}");
        let err = text(&out.stderr);
        assert!(err.contains("isn't a session name"), "{bad:?}: {err}");
        let out = vm.run(&["shell", "dev", "--session", bad]);
        assert_eq!(code(&out), 255, "{bad:?}");
        assert!(
            text(&out.stderr).contains("isn't a session name"),
            "{bad:?}"
        );
    }
    for good in ["work", "Build_2", "a", &"z".repeat(32)] {
        assert!(sessions::check_name(good).is_ok(), "{good:?}");
    }
    assert!(vm.sd.calls().is_empty(), "{:?}", vm.sd.calls());
}

// What `tmux list-sessions -F` prints, and what comes back from it. tmux
// keeps `:` and `.` out of names, so the name can come last.
fn listing(sessions: &[(String, String, i64, u32)]) -> String {
    sessions
        .iter()
        .map(|(name, command, created, clients)| format!("{created}:{clients}:{command}:{name}\n"))
        .collect()
}

#[test]
fn the_list_format_names_the_fields_in_that_order() {
    assert_eq!(
        sessions::LIST_FORMAT,
        "#{session_created}:#{session_attached}:#{pane_current_command}:#{session_name}"
    );
}

proptest! {
    #[test]
    fn a_listing_reads_back(
        rows in prop::collection::vec(
            ("[^:.\n]{1,20}", "[^\n]{0,20}", 0i64..4_000_000_000, 0u32..3),
            0..6,
        )
    ) {
        let got = sessions::parse_list(&listing(&rows));
        let want: Vec<Session> = rows
            .iter()
            .map(|(name, command, created, clients)| Session {
                name: name.clone(),
                command: command.clone(),
                started: chrono::DateTime::from_timestamp(*created, 0).unwrap(),
                attached: *clients > 0,
            })
            .collect();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn a_listing_never_panics(junk in "(?s).{0,200}") {
        sessions::parse_list(&junk);
    }

    // The last `lines` of the pane, never past `cap` bytes, and never part
    // of a line unless one line alone is over the cap.
    #[test]
    fn a_tail_is_the_pane_s_last_lines(
        body in prop::collection::vec("[a-zé ]{0,12}", 0..40),
        blank in 0usize..5,
        lines in 1usize..50,
        cap in 1usize..200,
    ) {
        let pane = body.iter().map(|l| format!("{l}\n")).collect::<String>() + &"\n".repeat(blank);
        let got = sessions::tail(&pane, lines, cap);
        let kept: Vec<&str> = {
            let mut all: Vec<&str> = body.iter().map(String::as_str).collect();
            while all.last().is_some_and(|l| l.is_empty()) {
                all.pop();
            }
            all
        };
        let want: String = kept[kept.len().saturating_sub(lines)..]
            .iter()
            .map(|l| format!("{l}\n"))
            .collect();
        prop_assert!(got.text.len() <= cap, "{:?}", got);
        prop_assert!(want.ends_with(&got.text), "{:?} not the end of {:?}", got.text, want);
        prop_assert_eq!(got.truncated, got.text != want);
        if got.truncated && want.len() > cap {
            let whole_lines = got.text.is_empty() || want[..want.len() - got.text.len()].ends_with('\n');
            let longest = kept.iter().map(|l| l.len() + 1).max().unwrap_or(0);
            prop_assert!(whole_lines || longest > cap, "{:?}", got);
        }
    }
}

#[test]
fn a_tail_drops_the_blank_rows_under_the_cursor() {
    let got = sessions::tail("$ seq 3\n1\n2\n3\n$\n\n\n\n", 3, 1024);
    assert_eq!((got.text.as_str(), got.truncated), ("2\n3\n$\n", false));
    let got = sessions::tail("\n\n\n", 10, 1024);
    assert_eq!((got.text.as_str(), got.truncated), ("", false));
}
