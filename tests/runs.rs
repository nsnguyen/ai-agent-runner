// Headless runs end to end: `runner run`, `runs`, `output` and `cancel`, with
// systemctl faked, runner-guest on a Unix socket behind a fake of
// Firecracker's vsock socket, and a fake `claude` that prints stream-json.
mod common;

use common::{FakeApi, FakeSystemd, FakeVsock, Guest, me, script, text, wait_for};
use proptest::prelude::*;
use runner::runs::{self, Line};
use runner::store::Store;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;

// A VM named dev, stopped or running as far as systemctl says, whose guest
// is up either way, and a `claude` for it to run. The fake writes what it
// was given into `ctl`, prints an init event, waits for `ctl/go`, then
// prints `ctl/events` more (3 unless set) and exits with `ctl/exit` (0
// unless set).
struct Vm {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
    ctl: PathBuf,
    work: PathBuf,
    _vsock: FakeVsock,
    _guest: Guest,
}

fn vm(running: bool) -> Vm {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state(if running { "active" } else { "inactive" });
    let guest = Guest::spawn(&dir.path().join("guest.sock"));
    let vsock = FakeVsock::serve(&dir.path().join("vms/dev/vsock.sock"), &guest.sock);
    let ctl = dir.path().join("ctl");
    let work = dir.path().join("work");
    fs::create_dir_all(&ctl).unwrap();
    fs::create_dir_all(&work).unwrap();
    // Each event after the init one comes in two writes, now and then with
    // a pause between, so the stream often ends partway through a line.
    script(
        &ctl.join("claude"),
        &format!(
            r#"c='{}'
printf '%s\n' "$@" > "$c/argv"
pwd > "$c/pwd"
cat > "$c/prompt"
echo '{{"type":"system","subtype":"init","session_id":"s-1"}}'
while [ ! -e "$c/go" ]; do sleep 0.01; done
n=$(cat "$c/events" 2>/dev/null || echo 3)
i=1
while [ "$i" -le "$n" ]; do
  printf '{{"type":"assistant",'
  [ $((i % 300)) -eq 1 ] && sleep 0.05
  printf '"n":%d}}\n' "$i"
  i=$((i + 1))
done
echo 'a note on stderr' >&2
exit "$(cat "$c/exit" 2>/dev/null || echo 0)"
"#,
            ctl.display()
        ),
    );
    Vm {
        dir,
        store,
        sd,
        ctl,
        work,
        _vsock: vsock,
        _guest: guest,
    }
}

impl Vm {
    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.dir.path())
            .env("PATH", self.sd.path())
            .env("RUNNER_USER", me())
            .env("RUNNER_CLAUDE", self.ctl.join("claude"))
            .args(args);
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.runner(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    /// `runner run dev <prompt>` in the work directory: the run's id.
    fn start(&self, prompt: &str) -> String {
        let id = returns_at_once(self.runner(&["run", "dev", prompt, "--dir", self.work_dir()]));
        assert!(id.status.success(), "{}", text(&id.stderr));
        text(&id.stdout).trim().to_string()
    }

    fn work_dir(&self) -> &str {
        self.work.to_str().unwrap()
    }

    fn go(&self, events: u32, exit: i32) {
        fs::write(self.ctl.join("events"), events.to_string()).unwrap();
        fs::write(self.ctl.join("exit"), exit.to_string()).unwrap();
        fs::write(self.ctl.join("go"), "").unwrap();
    }

    fn started(&self) -> bool {
        self.ctl.join("prompt").exists()
    }

    /// The run as `runner runs dev` lists it.
    fn row(&self, id: &str) -> String {
        let rows = self.ok(&["runs", "dev"]);
        let row = rows.lines().find(|r| r.starts_with(&format!("{id}  ")));
        row.unwrap_or_else(|| panic!("no {id} in {rows}"))
            .to_string()
    }

    fn wait_until_ended(&self, id: &str) -> String {
        wait_for(&format!("{id} to end"), || {
            !self.row(id).contains("running")
        });
        self.row(id)
    }

    fn generation(&self) -> u64 {
        self.store.get("dev").unwrap().generation
    }
}

// The command that starts a run returns, its output closed, while the run
// is still going: nothing it leaves running holds the caller's stdout or
// stderr, which would keep `ssh host runner run` from returning.
fn returns_at_once(mut cmd: Command) -> Output {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output().unwrap()));
    rx.recv_timeout(Duration::from_secs(10))
        .expect("runner run didn't return while its run went on")
}

// Stands in for `runner boot` under the unit: it holds the run boot would
// (Store::begin_run), and at Ctrl-Alt-Del it commits that run unless a run
// asked to roll back, as boot does, and the unit goes inactive.
struct Boot {
    asked: Arc<Mutex<Option<String>>>,
    _api: FakeApi,
}

fn boot(vm: &Vm) -> Boot {
    let run = Arc::new(Mutex::new(Some(vm.store.begin_run("dev").unwrap())));
    let asked = Arc::new(Mutex::new(None));
    let (seen, state) = (asked.clone(), vm.sd.bin.join("state"));
    let api = FakeApi::serve(&vm.dir.path().join("vms/dev/api.sock"), move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"})
            && let Some(run) = run.lock().unwrap().take()
        {
            match run.take_rollback().unwrap() {
                Some(by) => *seen.lock().unwrap() = Some(by),
                None => {
                    run.commit().unwrap();
                }
            }
            fs::write(&state, "inactive\n").unwrap();
        }
        (204, None)
    });
    Boot { asked, _api: api }
}

impl Boot {
    fn asked(&self) -> Option<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[test]
fn run_returns_its_id_before_the_run_ends() {
    let vm = vm(true);
    let id = vm.start("fix the tests\nand say how");
    assert_eq!(id, "dev.1");
    assert!(
        vm.row("dev.1").contains("  running  "),
        "{}",
        vm.row("dev.1")
    );
    wait_for("claude to start", || vm.started());

    vm.go(2, 0);
    let row = vm.wait_until_ended("dev.1");
    // Joined running, so it neither committed nor rolled back.
    assert!(row.contains("  exited 0, joined  "), "{row}");
    assert!(row.ends_with(r#""fix the tests\nand say how""#), "{row}");
    // The prompt comes on stdin, in the directory asked for.
    let read = |f: &str| fs::read_to_string(vm.ctl.join(f)).unwrap();
    assert_eq!(read("prompt"), "fix the tests\nand say how");
    assert_eq!(read("pwd").trim(), vm.work_dir());
    let argv: Vec<String> = read("argv").lines().map(String::from).collect();
    assert_eq!(
        argv,
        [
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--dangerously-skip-permissions"
        ]
    );
    let out = vm.ok(&["output", "dev.1"]);
    assert_eq!(
        out,
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}\n\
         {\"type\":\"assistant\",\"n\":1}\n{\"type\":\"assistant\",\"n\":2}\n"
    );
    // A run on a running VM leaves it as it was.
    assert!(
        !vm.sd
            .calls()
            .iter()
            .any(|c| c.starts_with("systemctl start"))
    );
    assert_eq!(vm.generation(), 0);
}

#[test]
fn resume_and_a_prompt_starting_with_a_dash_reach_claude() {
    let vm = vm(true);
    let started = returns_at_once(vm.runner(&[
        "run",
        "dev",
        "--resume",
        "s-1",
        "--dir",
        vm.work_dir(),
        "-n",
    ]));
    assert!(started.status.success(), "{}", text(&started.stderr));
    vm.go(0, 0);
    vm.wait_until_ended("dev.1");
    let argv = fs::read_to_string(vm.ctl.join("argv")).unwrap();
    assert!(argv.ends_with("--resume\ns-1\n"), "{argv}");
    // A prompt can start with a dash.
    assert_eq!(fs::read_to_string(vm.ctl.join("prompt")).unwrap(), "-n");
}

// Read in pieces while it's written, each from where the last left off,
// the stream comes back whole: every line once, in order, and none torn.
#[test]
fn output_read_by_offset_has_no_gaps_or_repeats() {
    let vm = vm(true);
    let id = vm.start("count");
    wait_for("claude to start", || vm.started());
    vm.go(3000, 0);
    let mut got = Vec::new();
    let mut reads = 0;
    loop {
        let ended = !vm.row(&id).contains("running");
        let from = got.len().to_string();
        let out = vm
            .runner(&["output", &id, "--from", &from])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        if !ended {
            assert!(out.stdout.is_empty() || out.stdout.ends_with(b"\n"));
        }
        got.extend(out.stdout);
        reads += 1;
        if ended {
            break;
        }
    }
    let stream = fs::read(vm.dir.path().join("vms/dev/runs/1/stream")).unwrap();
    assert_eq!(text(&got), text(&stream));
    let lines: Vec<Value> = text(&got)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3001, "after {reads} reads");
    for (i, line) in lines[1..].iter().enumerate() {
        assert_eq!(line["n"], json!(i + 1));
    }
}

#[test]
fn follow_prints_until_the_run_ends_and_says_how() {
    let vm = vm(true);
    let id = vm.start("go on");
    let follow = vm
        .runner(&["output", &id, "--follow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for("claude to start", || vm.started());
    vm.go(2, 4);
    let out = follow.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).lines().count(), 3);
    assert_eq!(text(&out.stderr), "runner: dev.1 exited 4, joined\n");
}

#[test]
fn exit_0_commits_the_generation_a_run_boots() {
    let vm = vm(false);
    let boot = boot(&vm);
    let id = vm.start("make a file");
    wait_for("claude to start", || vm.started());
    assert!(
        vm.sd
            .calls()
            .contains(&"systemctl start firecracker@dev.service".to_string())
    );

    vm.go(1, 0);
    let row = vm.wait_until_ended(&id);
    assert!(row.contains("  exited 0, committed  "), "{row}");
    assert_eq!(boot.asked(), None);
    assert_eq!(vm.generation(), 1);
}

#[test]
fn a_non_zero_exit_rolls_back_what_the_run_booted() {
    let vm = vm(false);
    let boot = boot(&vm);
    let id = vm.start("break it");
    wait_for("claude to start", || vm.started());
    vm.go(1, 3);
    let row = vm.wait_until_ended(&id);
    assert!(row.contains("  exited 3, rolled back  "), "{row}");
    assert_eq!(boot.asked().as_deref(), Some("run dev.1"));
    assert_eq!(vm.generation(), 0);
    // The run's disk stays for a look.
    assert!(vm.dir.path().join("vms/dev/gens/1.ext4").is_file());
}

// As `runner stop`, a host reboot, or the agent's own `sudo reboot` would.
#[test]
fn a_vm_shut_down_under_a_run_rolls_back() {
    let vm = vm(false);
    let boot = boot(&vm);
    let id = vm.start("interrupted");
    wait_for("claude to start", || vm.started());
    let api = runner::firecracker::Api::new(vm.dir.path().join("vms/dev/api.sock"));
    api.ctrl_alt_del().unwrap();
    assert_eq!(boot.asked().as_deref(), Some("run dev.1"));

    vm.go(0, 0);
    let row = vm.wait_until_ended(&id);
    assert!(row.contains("  exited 0, rolled back: "), "{row}");
    assert_eq!(vm.generation(), 0);
}

#[test]
fn cancel_ends_the_run_and_rolls_back_what_it_booted() {
    let vm = vm(false);
    let boot = boot(&vm);
    let id = vm.start("never ends");
    wait_for("claude to start", || vm.started());

    let row = vm.ok(&["cancel", &id]);
    assert!(row.contains("  cancelled, rolled back  "), "{row}");
    assert_eq!(boot.asked().as_deref(), Some("run dev.1"));
    assert_eq!(vm.generation(), 0);
    // It's over, so there's nothing to cancel.
    let again = vm.runner(&["cancel", &id]).output().unwrap();
    assert!(!again.status.success());
    assert_eq!(
        text(&again.stderr),
        "runner: dev.1 has already ended: cancelled, rolled back\n"
    );
}

#[test]
fn a_second_run_on_a_busy_vm_is_refused() {
    let vm = vm(true);
    let id = vm.start("first");
    wait_for("claude to start", || vm.started());

    let second = vm.runner(&["run", "dev", "second"]).output().unwrap();
    assert!(!second.status.success());
    assert_eq!(
        text(&second.stderr),
        "runner: dev already has a run going, dev.1: one run per VM; `runner cancel dev.1` ends it\n"
    );
    assert_eq!(vm.ok(&["runs", "dev"]).lines().count(), 1);

    vm.go(0, 0);
    vm.wait_until_ended(&id);
    fs::remove_file(vm.ctl.join("go")).unwrap();
    assert_eq!(vm.start("third"), "dev.2");
    let rows = vm.ok(&["runs", "dev"]);
    let ids: Vec<&str> = rows
        .lines()
        .map(|r| r.split("  ").next().unwrap())
        .collect();
    assert_eq!(ids, ["dev.2", "dev.1"], "newest first");
}

#[test]
fn a_run_whose_driver_died_is_lost() {
    let vm = vm(true);
    let id = vm.start("doomed");
    wait_for("claude to start", || vm.started());
    let pid = driver(vm.dir.path(), "1").expect("no process drives dev.1");
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for("dev.1 to be lost", || vm.row(&id).contains("  lost: "));
    let log = vm.dir.path().join("vms/dev/runs/1/log");
    let row = vm.row(&id);
    assert!(
        row.contains(&format!(
            "the process driving it died: {} may say why",
            log.display()
        )),
        "{row}"
    );
    // Its lock went with it.
    vm.go(0, 0);
    assert_eq!(vm.start("next"), "dev.2");
}

#[test]
fn the_stderr_of_a_run_that_ended_is_kept() {
    let vm = vm(true);
    let id = vm.start("speak up");
    vm.go(0, 1);
    vm.wait_until_ended(&id);
    let stderr = fs::read_to_string(vm.dir.path().join("vms/dev/runs/1/stderr")).unwrap();
    assert_eq!(stderr, "a note on stderr\n");
}

#[test]
fn unknown_runs_and_bad_ids_are_named() {
    let vm = vm(true);
    let fails = |args: &[&str]| {
        let out = vm.runner(args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        text(&out.stderr)
    };
    assert_eq!(fails(&["output", "dev.9"]), "runner: no run dev.9\n");
    assert_eq!(fails(&["cancel", "nope.1"]), "runner: no VM named nope\n");
    assert_eq!(
        fails(&["output", "dev"]),
        "runner: \"dev\" isn't a run id: use the one `runner run` printed, like dev.1\n"
    );
    assert_eq!(vm.ok(&["runs", "dev"]), "no runs yet\n");
}

#[test]
fn reading_past_the_end_of_a_stream_is_refused() {
    let vm = vm(true);
    let id = vm.start("short");
    vm.go(0, 0);
    vm.wait_until_ended(&id);
    let out = vm
        .runner(&["output", &id, "--from", "9999"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("dev.1's stream has only 54 bytes"),
        "{}",
        text(&out.stderr)
    );
}

// The process driving run `n` under `root`: `runner drive dev <n>`.
fn driver(root: &Path, n: &str) -> Option<nix::unistd::Pid> {
    let want = ["--root", root.to_str().unwrap(), "drive", "dev", n].map(String::from);
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let mut cmdline = Vec::new();
        let read = fs::File::open(entry.path().join("cmdline"))
            .and_then(|mut f| f.read_to_end(&mut cmdline));
        if read.is_err() {
            continue;
        }
        let args: Vec<String> = cmdline.split(|&b| b == 0).map(text).collect();
        if args.windows(want.len()).any(|w| w == want) {
            return Some(nix::unistd::Pid::from_raw(pid));
        }
    }
    None
}

#[test]
fn ids_are_the_vm_and_a_number() {
    assert_eq!(runs::id("dev", 3), "dev.3");
    assert_eq!(runs::parse_id("dev.3").unwrap(), ("dev".to_string(), 3));
    assert_eq!(runs::parse_id("a-b.10").unwrap(), ("a-b".to_string(), 10));
    for bad in [
        "dev", "dev.", ".3", "dev.x", "dev.0", "dev.-1", "dev.3.4", "Dev.1", "dev.+3",
    ] {
        assert!(runs::parse_id(bad).is_err(), "{bad}");
    }
}

// What reading a stream sees: lines as `Line`s, the bytes they took, and
// where the next read starts.
fn lines_of(path: &Path, from: u64, cap: usize, ended: bool) -> (Vec<Line>, u64) {
    let got = runs::lines(path, from, cap, ended).unwrap();
    (got.lines, got.next)
}

#[test]
fn a_line_longer_than_the_cap_comes_alone_and_cut() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream");
    fs::write(&path, "ab\nccccccccc\nd\nee").unwrap();
    let whole = |s: &str| Line::Whole(s.as_bytes().to_vec());
    assert_eq!(lines_of(&path, 0, 4, false), (vec![whole("ab\n")], 3));
    assert_eq!(lines_of(&path, 3, 4, false), (vec![Line::Cut(10)], 13));
    // Running, the last line isn't done; ended, it is.
    assert_eq!(lines_of(&path, 13, 4, false), (vec![whole("d\n")], 15));
    assert_eq!(lines_of(&path, 15, 4, false), (vec![], 15));
    assert_eq!(lines_of(&path, 15, 4, true), (vec![whole("ee")], 17));
    assert_eq!(
        lines_of(&path, 13, 4, true),
        (vec![whole("d\n"), whole("ee")], 17)
    );
}

proptest! {
    // However the writes and reads interleave, reading on from each read's
    // `next` sees every byte once, in order, and only whole lines until the
    // run has ended. A cut line comes alone and is one line longer than the
    // cap; its bytes are taken from what was written.
    #[test]
    fn reads_from_each_next_see_the_stream_whole(
        writes in prop::collection::vec(prop::collection::vec(prop_oneof![Just(b'\n'), Just(b'x')], 0..40), 0..20),
        cap in 1usize..64,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream");
        let mut file = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let (mut got, mut next, mut written) = (Vec::new(), 0, Vec::new());
        let read = |next: &mut u64, got: &mut Vec<u8>, written: &[u8], ended| loop {
            let r = runs::lines(&path, *next, cap, ended).unwrap();
            for line in &r.lines {
                match line {
                    Line::Whole(b) => got.extend(b),
                    Line::Cut(n) => {
                        let cut = &written[got.len()..got.len() + *n as usize];
                        assert_eq!(r.lines.len(), 1);
                        assert!(cut.len() > cap, "{cut:?} fits in {cap}");
                        assert!(!cut[..cut.len() - 1].contains(&b'\n'));
                        assert!(ended || cut.ends_with(b"\n"));
                        got.extend(cut);
                    }
                }
            }
            if r.next == *next { return; }
            *next = r.next;
        };
        for w in &writes {
            file.write_all(w).unwrap();
            written.extend(w);
            read(&mut next, &mut got, &written, false);
            prop_assert!(got.is_empty() || got.ends_with(b"\n"));
            prop_assert!(written.starts_with(&got));
        }
        read(&mut next, &mut got, &written, true);
        prop_assert_eq!(got, written);
    }
}

// `runner mcp`: run_agent hands back the run at once, run_output reads it by
// cursor, list_runs and cancel_run.
#[test]
fn mcp_runs_an_agent_and_reads_it_by_cursor() {
    let vm = vm(true);
    let mut s = mcp(&vm);
    let started = s.call(
        2,
        "run_agent",
        json!({"name": "dev", "prompt": "hi", "dir": vm.work_dir()}),
    );
    let run = &started["structuredContent"];
    assert_eq!(run["id"], json!("dev.1"), "{started}");
    assert_eq!(run["state"], json!("running"));
    assert_eq!(run["boots"], json!(false));
    wait_for("claude to start", || vm.started());
    vm.go(40, 0);

    let mut events = Vec::new();
    let mut cursor = 0;
    for id in 3.. {
        let args = json!({"run": "dev.1", "cursor": cursor, "wait_secs": 5});
        let out = s.call(id, "run_output", args)["structuredContent"].clone();
        events.extend(out["events"].as_array().unwrap().iter().cloned());
        cursor = out["cursor"].as_u64().unwrap();
        if out["run"]["state"] != json!("running") && out["more"] == json!(false) {
            assert_eq!(out["run"]["exit_code"], json!(0));
            assert_eq!(out["run"]["stderr"], json!("a note on stderr\n"));
            break;
        }
        assert!(id < 200, "run_output never reached the end");
    }
    assert_eq!(events.len(), 41);
    assert_eq!(events[0]["session_id"], json!("s-1"));
    assert_eq!(events[40], json!({"type": "assistant", "n": 40}));

    let list = s.call(300, "list_runs", json!({"name": "dev"}));
    let runs = list["structuredContent"]["runs"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], json!("exited"));
}

#[test]
fn mcp_cancels_a_run() {
    let vm = vm(true);
    let mut s = mcp(&vm);
    s.call(
        2,
        "run_agent",
        json!({"name": "dev", "prompt": "hi", "dir": vm.work_dir()}),
    );
    wait_for("claude to start", || vm.started());
    // Caught up and still going, so it waits, then says so.
    let out = s.call(
        3,
        "run_output",
        json!({"run": "dev.1", "cursor": 54, "wait_secs": 0}),
    );
    let out = &out["structuredContent"];
    assert_eq!(out["events"], json!([]));
    assert_eq!(out["cursor"], json!(54));
    let cancelled = s.call(4, "cancel_run", json!({"run": "dev.1"}));
    assert_eq!(
        cancelled["structuredContent"]["state"],
        json!("cancelled"),
        "{cancelled}"
    );
    let again = s.call(5, "cancel_run", json!({"run": "dev.1"}));
    assert_eq!(again["isError"], json!(true));
}

struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}

impl Mcp {
    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
    }

    fn reply(&mut self, id: u64) -> Value {
        for line in self.lines.by_ref() {
            let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if msg["id"] == json!(id) {
                return msg;
            }
        }
        panic!("no reply to {id}");
    }

    fn call(&mut self, id: u64, tool: &str, args: Value) -> Value {
        let params = json!({"name": tool, "arguments": args});
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        self.reply(id)["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mcp(vm: &Vm) -> Mcp {
    use std::io::BufRead;
    let mut child = vm
        .runner(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut s = Mcp {
        child,
        stdin,
        lines,
    };
    let client = json!({"name": "test", "version": "0"});
    let params = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
    s.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params}));
    s.reply(1);
    s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    s
}
