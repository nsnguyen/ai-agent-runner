// Headless runs end to end: `runner run`, `runs`, `output`, `cancel` and
// `result`, with systemctl faked, runner-guest on a Unix socket behind a fake
// of Firecracker's vsock socket, and a fake `claude` that prints stream-json.
mod common;

use common::{
    FakeApi, FakeSystemd, FakeVsock, Guest, debugfs, image, me, script, text, tree, wait_for,
};
use proptest::prelude::*;
use runner::claude::{self, Vms};
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

// A VM named dev on a small ext4 disk, stopped or running as far as
// systemctl says, whose guest is up either way, and a `claude` for it to
// run. A running one has a stand-in for its boot (joined), as runs that join
// it checkpoint it, and a restore of that checkpoint stops it. The fake
// writes what it was given into `ctl`, waits while there's a `ctl/hold`,
// prints an init event (`ctl/init` in its place if there is one), waits for
// `ctl/go`, then prints `ctl/events` more (3 unless set), a result event
// saying `ctl/says` if there is one, and exits with `ctl/exit` (0 unless
// set). A SIGTERM it notes in `ctl/signal` before it dies of it.
struct Vm {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
    ctl: PathBuf,
    work: PathBuf,
    _vsock: FakeVsock,
    _guest: Guest,
    _joined: Option<FakeApi>,
}

fn vm(running: bool) -> Vm {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("etc/machine-id", ""),
        ("home/agent/work/main.rs", "fn main() {}"),
        ("home/agent/work/old.rs", "// unused"),
    ];
    let disk = image(&tree(&dir.path().join("rootfs"), &files));
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
trap 'echo TERM > "$c/signal"; trap - TERM; kill -TERM $$' TERM
printf '%s\n' "$@" > "$c/argv"
pwd > "$c/pwd"
cat > "$c/prompt"
while [ -e "$c/hold" ]; do sleep 0.01; done
if [ -e "$c/init" ]; then
  cat "$c/init"
else
  echo '{{"type":"system","subtype":"init","session_id":"s-1"}}'
fi
while [ ! -e "$c/go" ]; do sleep 0.01; done
n=$(cat "$c/events" 2>/dev/null || echo 3)
i=1
while [ "$i" -le "$n" ]; do
  printf '{{"type":"assistant",'
  [ $((i % 300)) -eq 1 ] && sleep 0.05
  printf '"n":%d}}\n' "$i"
  i=$((i + 1))
done
if [ -e "$c/says" ]; then
  printf '{{"type":"result","result":"%s"}}\n' "$(cat "$c/says")"
fi
echo 'a note on stderr' >&2
exit "$(cat "$c/exit" 2>/dev/null || echo 0)"
"#,
            ctl.display()
        ),
    );
    let joined = running.then(|| joined(&store, dir.path(), &sd));
    Vm {
        dir,
        store,
        sd,
        ctl,
        work,
        _vsock: vsock,
        _guest: guest,
        _joined: joined,
    }
}

// Stands in for `runner boot` of a VM that was running before the run: it
// holds generation 1, a copy of generation 0, answers a pause and a resume,
// and commits at Ctrl-Alt-Del, which `systemctl stop` sends through `runner
// halt`, as the unit's ExecStop does.
fn joined(store: &Store, dir: &Path, sd: &FakeSystemd) -> FakeApi {
    script(
        &sd.bin.join("on-stop"),
        &format!("exec '{}' halt dev\n", env!("CARGO_BIN_EXE_runner")),
    );
    let run = Mutex::new(Some(store.begin_run("dev").unwrap()));
    FakeApi::serve(&dir.join("vms/dev/api.sock"), move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"})
            && let Some(run) = run.lock().unwrap().take()
        {
            run.commit().unwrap();
        }
        (204, None)
    })
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

    fn says(&self, text: &str) {
        fs::write(self.ctl.join("says"), text).unwrap();
    }

    /// `runner result <id>`, parsed.
    fn result(&self, id: &str) -> Value {
        let out = self.ok(&["result", id]);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
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
        // The third column; an error further on can say "running" too.
        wait_for(&format!("{id} to end"), || {
            !self.row(id).contains("Z  running  ")
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
    halted: Arc<Mutex<bool>>,
    _api: FakeApi,
}

fn boot(vm: &Vm) -> Boot {
    let run = Arc::new(Mutex::new(Some(vm.store.begin_run("dev").unwrap())));
    let asked = Arc::new(Mutex::new(None));
    let halted = Arc::new(Mutex::new(false));
    let (seen, state) = (asked.clone(), vm.sd.bin.join("state"));
    let halts = halted.clone();
    let api = FakeApi::serve(&vm.dir.path().join("vms/dev/api.sock"), move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"})
            && let Some(run) = run.lock().unwrap().take()
        {
            *halts.lock().unwrap() = true;
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
    Boot {
        asked,
        halted,
        _api: api,
    }
}

impl Boot {
    fn asked(&self) -> Option<String> {
        self.asked.lock().unwrap().clone()
    }

    /// Whether it took a Ctrl-Alt-Del.
    fn halted(&self) -> bool {
        *self.halted.lock().unwrap()
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
    // Joined, and failed, so it left the VM as it was, and names the
    // checkpoint that rolls it back.
    assert_eq!(
        text(&out.stderr),
        "runner: dev.1 exited 4, not rolled back, checkpoint 2 undoes it\n"
    );
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

// What the runner measured on the disk, read from the generation the run
// booted against the one it booted from, apart from what the agent says.
#[test]
fn a_run_s_result_has_what_it_changed_on_its_disk() {
    let vm = vm(false);
    let _boot = boot(&vm);
    // What the run's guest does to the disk boot runs.
    let disk = vm.dir.path().join("vms/dev/gens/1.ext4");
    let new = vm.dir.path().join("new.rs");
    fs::write(&new, "fn main() { println!(\"hi\") }").unwrap();
    let write = |to: &str| format!("write {} /home/agent/work/{to}", new.display());
    debugfs(&disk, "rm /home/agent/work/old.rs");
    debugfs(&disk, "rm /home/agent/work/main.rs");
    debugfs(&disk, &write("main.rs"));
    debugfs(&disk, &write("new.rs"));
    // And what its boot writes on its own, left out.
    debugfs(&disk, "rm /etc/machine-id");
    debugfs(&disk, &format!("write {} /etc/machine-id", new.display()));

    let id = vm.start("fix main.rs");
    wait_for("claude to start", || vm.started());
    vm.says("I only read main.rs.");
    vm.go(2, 0);
    vm.wait_until_ended(&id);
    let result = vm.result(&id);
    assert_eq!(result["exit_code"], json!(0), "{result}");
    assert_eq!(result["outcome"], json!("committed"));
    assert_eq!(result["from_generation"], json!(0));
    assert_eq!(result["generation"], json!(1));
    assert!(result["duration_ms"].is_u64(), "{result}");
    assert_eq!(
        result["changed"],
        json!({
            "paths": [
                {"path": "/home/agent/work/main.rs", "change": "modified"},
                {"path": "/home/agent/work/new.rs", "change": "added"},
                {"path": "/home/agent/work/old.rs", "change": "deleted"}
            ],
            "more": 0,
            "boot_only": 1
        })
    );
    assert_eq!(
        result["agent_says"],
        json!({"untrusted": true, "text": "I only read main.rs.", "truncated": false})
    );
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
    // The run's disk stays for a look, and its result says which it is.
    let disk = vm.dir.path().join("vms/dev/gens/1.ext4");
    assert!(disk.is_file());
    let result = vm.result(&id);
    assert_eq!(result["exit_code"], json!(3), "{result}");
    assert_eq!(result["outcome"], json!("rolled_back"));
    assert_eq!(result["from_generation"], json!(0));
    assert_eq!(result["generation"], json!(1));
    assert_eq!(
        result["changed"],
        json!({"paths": [], "more": 0, "boot_only": 0})
    );
}

// A run that joins a running VM checkpoints it first, and one that exits 0
// leaves it running on.
#[test]
fn a_joined_run_checkpoints_its_vm_first() {
    let vm = vm(true);
    let id = vm.start("careful now");
    wait_for("claude to start", || vm.started());
    let listed = vm.ok(&["checkpoints", "dev"]);
    let first = listed.lines().next().unwrap();
    assert!(first.starts_with("  2  "), "{listed}");
    assert!(first.contains("  run  "), "{listed}");
    assert!(first.ends_with("  \"before run dev.1\""), "{listed}");
    vm.go(0, 0);
    let row = vm.wait_until_ended(&id);
    assert!(row.contains("  exited 0, joined  "), "{row}");
    let result = vm.result(&id);
    assert_eq!(result["checkpoint"], json!(2), "{result}");
    assert_eq!(result["outcome"], json!(null));
    assert!(
        !vm.sd
            .calls()
            .iter()
            .any(|c| c.starts_with("systemctl stop"))
    );
}

// The systemctl verbs that stop or start the VM, as called so far.
fn stops_and_starts(vm: &Vm) -> Vec<String> {
    vm.sd
        .calls()
        .iter()
        .filter_map(|c| c.split(' ').nth(1).map(String::from))
        .filter(|v| v == "stop" || v == "start")
        .collect()
}

// A joined run that fails leaves the VM running as it was, its sessions and
// all, with what the run wrote on its disk. Its result says it wasn't rolled
// back and names the checkpoint it took, which `restore` rolls back by hand.
#[test]
fn a_joined_run_that_fails_leaves_its_vm_running() {
    let vm = vm(true);
    let id = vm.start("break it");
    wait_for("claude to start", || vm.started());
    let disk = |g: u64| vm.dir.path().join(format!("vms/dev/gens/{g}.ext4"));
    let before = fs::read(disk(1)).unwrap();
    fs::write(disk(1), b"broken").unwrap();
    vm.go(0, 3);
    let row = vm.wait_until_ended(&id);
    assert!(
        row.contains("  exited 3, not rolled back, checkpoint 2 undoes it  "),
        "{row}"
    );
    // Still running on generation 1, what the run wrote and all: nothing
    // committed it.
    assert_eq!(stops_and_starts(&vm), Vec::<String>::new());
    assert_eq!(vm.generation(), 0);
    assert_eq!(fs::read(disk(1)).unwrap(), b"broken");
    let result = vm.result(&id);
    assert_eq!(result["outcome"], json!("not_rolled_back"), "{result}");
    assert_eq!(result["checkpoint"], json!(2));

    // By hand: stopping it commits the broken disk, and the checkpoint's copy
    // goes on top, so `undo` would bring the run's work back.
    vm.ok(&["restore", "dev", "2"]);
    assert_eq!(stops_and_starts(&vm), ["stop", "start"]);
    assert_eq!(vm.generation(), 3);
    assert_eq!(fs::read(disk(3)).unwrap(), before);
}

// So does one that's cancelled.
#[test]
fn a_cancelled_joined_run_leaves_its_vm_running() {
    let vm = vm(true);
    let id = vm.start("never ends");
    wait_for("claude to start", || vm.started());
    let row = vm.ok(&["cancel", &id]);
    assert!(
        row.contains("  cancelled, not rolled back, checkpoint 2 undoes it  "),
        "{row}"
    );
    assert_eq!(stops_and_starts(&vm), Vec::<String>::new());
    assert_eq!(vm.generation(), 0);
    let result = vm.result(&id);
    assert_eq!(result["exit_code"], json!(null), "{result}");
    assert_eq!(result["outcome"], json!("not_rolled_back"));
    assert_eq!(result["checkpoint"], json!(2));
}

// `runner stop` during a joined run ends Claude Code with the guest, and the
// run leaves the VM stopping, as it was asked to be: it neither stops nor
// starts it, and rolls nothing back.
#[test]
fn a_joined_run_whose_vm_is_stopped_under_it_doesn_t_start_it() {
    let vm = vm(true);
    let id = vm.start("break it");
    wait_for("claude to start", || vm.started());
    vm.sd.set_state("deactivating");
    vm.go(0, 143);
    let row = vm.wait_until_ended(&id);
    assert!(
        row.contains("  exited 143, not rolled back, checkpoint 2 undoes it  "),
        "{row}"
    );
    assert_eq!(stops_and_starts(&vm), Vec::<String>::new());
    let result = vm.result(&id);
    assert_eq!(result["outcome"], json!("not_rolled_back"), "{result}");
    assert_eq!(result["checkpoint"], json!(2));
}

// Nothing ran, so there's nothing to roll back, and the VM's sessions live on.
#[test]
fn a_joined_run_whose_claude_never_started_leaves_its_vm_running() {
    let vm = vm(true);
    let missing = vm.dir.path().join("no-such-dir");
    let id = returns_at_once(vm.runner(&["run", "dev", "hi", "--dir", missing.to_str().unwrap()]));
    let id = text(&id.stdout).trim().to_string();
    let row = vm.wait_until_ended(&id);
    assert!(row.contains("  failed, joined: "), "{row}");
    assert!(row.ends_with(r#"  -  "hi""#), "{row}");
    assert!(
        !vm.sd
            .calls()
            .iter()
            .any(|c| c.starts_with("systemctl stop"))
    );
    assert_eq!(vm.result(&id)["checkpoint"], json!(2));
}

// A running VM's disk has nothing from before the run to measure against.
#[test]
fn a_joined_run_s_result_measures_no_disk() {
    let vm = vm(true);
    let id = vm.start("look around");
    wait_for("claude to start", || vm.started());
    vm.says("Done.");
    vm.go(1, 0);
    vm.wait_until_ended(&id);
    let result = vm.result(&id);
    assert_eq!(result["exit_code"], json!(0), "{result}");
    assert_eq!(result["outcome"], json!(null));
    assert_eq!(result["from_generation"], json!(null));
    assert_eq!(result["generation"], json!(null));
    let error = result["changed"]["error"].as_str().unwrap();
    assert!(error.contains("joined dev running"), "{error}");
    assert_eq!(result["agent_says"]["text"], json!("Done."));
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
    let disk = vm.dir.path().join("vms/dev/gens/1.ext4");
    debugfs(&disk, "rm /home/agent/work/old.rs");
    let id = vm.start("never ends");
    wait_for("claude to start", || vm.started());

    let row = vm.ok(&["cancel", &id]);
    assert!(row.contains("  cancelled, rolled back  "), "{row}");
    assert_eq!(boot.asked().as_deref(), Some("run dev.1"));
    assert_eq!(vm.generation(), 0);
    let result = vm.result(&id);
    assert_eq!(result["exit_code"], json!(null), "{result}");
    assert_eq!(result["outcome"], json!("rolled_back"));
    assert_eq!(result["agent_says"]["text"], json!(null));
    // The cancel stopped Claude Code, not the measuring after it.
    assert_eq!(
        result["changed"],
        json!({
            "paths": [{"path": "/home/agent/work/old.rs", "change": "deleted"}],
            "more": 0,
            "boot_only": 0
        })
    );
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

    let result = vm.runner(&["result", &id]).output().unwrap();
    assert!(!result.status.success());
    assert_eq!(
        text(&result.stderr),
        "runner: dev.1 is still running: its result comes once it ends\n"
    );

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
    let result = vm.runner(&["result", &id]).output().unwrap();
    assert!(!result.status.success());
    assert!(
        text(&result.stderr).starts_with("runner: dev.1 has no result: lost: "),
        "{}",
        text(&result.stderr)
    );
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
    assert_eq!(fails(&["result", "dev.9"]), "runner: no run dev.9\n");
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
            assert_eq!(out["result"]["exit_code"], json!(0), "{out}");
            assert_eq!(out["result"]["agent_says"]["untrusted"], json!(true));
            break;
        }
        // Only the reply that reaches the end of an ended run has it.
        assert_eq!(out.get("result"), None, "{out}");
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
    let run = &cancelled["structuredContent"];
    assert_eq!(run["state"], json!("cancelled"), "{cancelled}");
    // Joined, so it's left as it was, with the checkpoint to roll it back.
    assert_eq!(run["outcome"], json!("not_rolled_back"), "{cancelled}");
    assert_eq!(run["checkpoint"], json!(2), "{cancelled}");
    let again = s.call(5, "cancel_run", json!({"run": "dev.1"}));
    assert_eq!(again["isError"], json!(true));
}

const UUID: &str = "0f8b2c4e-1a2b-4c3d-9e8f-123456789abc";

// Claude Code's init event, naming `session`, as a line of its stream.
fn init(session: &str) -> String {
    format!(
        "{}\n",
        json!({"type": "system", "subtype": "init", "cwd": "/w", "session_id": session})
    )
}

// list_runs' runs, newest first.
fn listed(s: &mut Mcp, id: u64) -> Vec<Value> {
    let list = s.call(id, "list_runs", json!({"name": "dev"}));
    let runs = list["structuredContent"]["runs"].as_array();
    runs.unwrap_or_else(|| panic!("{list}")).clone()
}

// Once Claude Code's init event names its session, `runs` shows it between
// how the run stands and its prompt, and MCP's runs carry it as `session`.
// Until then, `-` and nothing.
#[test]
fn a_run_names_its_session_once_claude_code_has_started() {
    let vm = vm(true);
    fs::write(vm.ctl.join("hold"), "").unwrap();
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("hi");
    wait_for("claude to start", || vm.started());
    let mut s = mcp(&vm);
    let row = vm.row(&id);
    assert!(row.ends_with(r#"Z  running  -  "hi""#), "{row}");
    assert_eq!(listed(&mut s, 2)[0].get("session"), None);

    fs::remove_file(vm.ctl.join("hold")).unwrap();
    wait_for("the session to show", || vm.row(&id).contains(UUID));
    let row = vm.row(&id);
    assert!(
        row.ends_with(&format!(r#"Z  running  {UUID}  "hi""#)),
        "{row}"
    );
    assert_eq!(listed(&mut s, 3)[0]["session"], json!(UUID));

    vm.go(1, 0);
    let row = vm.wait_until_ended(&id);
    assert!(
        row.ends_with(&format!(r#"  exited 0, joined  {UUID}  "hi""#)),
        "{row}"
    );
    let args = json!({"run": id, "wait_secs": 0});
    let out = s.call(4, "run_output", args);
    assert_eq!(out["structuredContent"]["run"]["session"], json!(UUID));
}

// The guest wrote the stream, so a session_id that isn't a UUID, as Claude
// Code's are, is left out, and a run with no init event has none.
#[test]
fn a_run_names_no_session_without_a_uuid_in_its_init_event() {
    let cases = [
        ("the fake's own, s-1", None),
        ("no init event", Some(String::new())),
        ("an argument", Some(init("--dangerously-skip-permissions"))),
    ];
    for (what, line) in cases {
        let vm = vm(true);
        if let Some(line) = line {
            fs::write(vm.ctl.join("init"), line).unwrap();
        }
        let id = vm.start("hi");
        vm.go(1, 0);
        let row = vm.wait_until_ended(&id);
        assert!(
            row.ends_with(r#"  exited 0, joined  -  "hi""#),
            "{what}: {row}"
        );
        let mut s = mcp(&vm);
        assert_eq!(listed(&mut s, 2)[0].get("session"), None, "{what}");
    }
}

// The session is only shown, so a stream it can't be read from costs only
// the session, not the run.
#[test]
fn a_stream_that_can_t_be_read_costs_only_the_session() {
    let vm = vm(true);
    let run = vm.dir.path().join("vms/dev/runs/1");
    fs::create_dir_all(run.join("stream")).unwrap();
    let ask = json!({"prompt": "hi", "dir": "/w", "resume": null, "user": "agent", "boots": true, "started": "2026-11-06T15:00:00Z"});
    fs::write(run.join("ask.json"), ask.to_string()).unwrap();
    let end = json!({"ended": "2026-11-06T15:01:00Z", "exit_code": 0, "signal": null, "cancelled": false, "outcome": "committed", "error": null});
    fs::write(run.join("end.json"), end.to_string()).unwrap();
    let row = vm.row("dev.1");
    assert!(row.ends_with(r#"  exited 0, committed  -  "hi""#), "{row}");
}

#[test]
fn the_session_is_found_past_a_torn_or_non_json_first_line() {
    let found = |stream: &str| runs::session(stream.as_bytes()).unwrap();
    let line = init(UUID);
    let uuid = Some(UUID.to_string());
    assert_eq!(found(&line), uuid);
    assert_eq!(found(&format!("{{\"type\":\"assis\n{line}")), uuid);
    assert_eq!(found(&format!("not json\n\n{line}")), uuid);
    // A hook's event can come first.
    let hook = r#"{"type":"system","subtype":"hook_response","session_id":"x"}"#;
    assert_eq!(found(&format!("{hook}\n{line}")), uuid);
    // The last line of a stream needn't end in a newline.
    assert_eq!(found(line.trim_end()), uuid);
    // The first init event is the session's.
    let other = init("11111111-2222-3333-4444-555555555555");
    assert_eq!(found(&format!("{line}{other}")), uuid);
    // Nothing else names it.
    let said = format!(r#"{{"type":"assistant","session_id":"{UUID}"}}"#);
    assert_eq!(found(&said), None);
    assert_eq!(found(""), None);
}

#[test]
fn only_a_uuid_is_taken_for_the_session() {
    let found = |session: Value| {
        let line = json!({"type": "system", "subtype": "init", "session_id": session});
        runs::session(format!("{line}\n").as_bytes()).unwrap()
    };
    assert_eq!(found(json!(UUID)), Some(UUID.to_string()));
    let upper = UUID.to_uppercase();
    assert_eq!(found(json!(upper)), Some(upper));
    for bad in [
        "s-1",
        "",
        "0f8b2c4e-1a2b-4c3d-9e8f-123456789ab",
        "0f8b2c4e-1a2b-4c3d-9e8f-123456789abcd",
        "0f8b2c4e_1a2b-4c3d-9e8f-123456789abc",
        "0f8b2c4e-1a2b-4c3d-9e8f-12345678 abc",
        "0f8b2c4e-1a2b-4c3d-9e8f-123456789abg",
        "0f8b2c4e1a2b4c3d9e8f123456789abc----",
        "--resume",
    ] {
        assert_eq!(found(json!(bad)), None, "{bad:?}");
    }
    assert_eq!(found(json!(7)), None);
    assert_eq!(found(Value::Null), None);
}

#[test]
fn an_init_event_past_the_first_64_kib_isn_t_found() {
    let found = |stream: &str| runs::session(stream.as_bytes()).unwrap();
    let within = runs::SESSION_WITHIN as usize;
    let init = init(UUID);
    // A line of `n` bytes, its newline included, that isn't JSON.
    let pad = |n: usize| format!("{}\n", "x".repeat(n - 1));
    // Ending at 64 KiB, it's found.
    let last = format!("{}{init}", pad(within - init.len()));
    assert_eq!(last.len(), within);
    assert_eq!(found(&last), Some(UUID.to_string()));
    // Its newline past it, too.
    assert_eq!(
        found(&format!("{}{init}", pad(within - init.len() + 1))),
        Some(UUID.to_string())
    );
    // Cut short by the end of the 64 KiB, it isn't, nor past it.
    assert_eq!(
        found(&format!("{}{init}", pad(within - init.len() + 2))),
        None
    );
    assert_eq!(found(&format!("{}{init}", pad(within))), None);
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

proptest! {
    // Whatever comes before it, the init event is found if, and only if, it
    // ends within the stream's first 64 KiB.
    #[test]
    fn the_init_event_is_found_only_within_64_kib(
        // Lines before it: each so many of one byte.
        before in prop::collection::vec((0usize..4000, prop::sample::select(b"x{\"}".to_vec())), 0..40),
        id: u128,
    ) {
        let uuid = format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            id >> 96,
            (id >> 80) & 0xffff,
            (id >> 64) & 0xffff,
            (id >> 48) & 0xffff,
            id & 0xffff_ffff_ffff
        );
        let mut stream = Vec::new();
        for &(len, byte) in &before {
            stream.extend(std::iter::repeat_n(byte, len));
            stream.push(b'\n');
        }
        let line = init(&uuid);
        // Where its JSON ends, short of its newline.
        let ends = stream.len() + line.len() - 1;
        stream.extend(line.as_bytes());
        let found = runs::session(&stream[..]).unwrap();
        let within = ends <= runs::SESSION_WITHIN as usize;
        prop_assert_eq!(found, within.then_some(uuid));
    }
}

// The run as `runner runs dev` lists it is still going: nothing stopped
// Claude Code, and nothing asked it to stop. Then it's let finish, so its
// fake claude doesn't outlive the test.
fn still_going(vm: &Vm, id: &str) {
    std::thread::sleep(Duration::from_millis(300));
    let row = vm.row(id);
    assert!(row.contains("Z  running  "), "{row}");
    assert!(!vm.ctl.join("signal").exists());
    assert!(!vm.dir.path().join("vms/dev/runs/1/takeover").exists());
    let _ = fs::remove_file(vm.ctl.join("hold"));
    vm.go(0, 0);
    vm.wait_until_ended(id);
}

// `runner claude --run` takes a run over: Claude Code gets SIGTERM, as a
// cancel's does, but the VM the run booted stays up on the run's disk, with
// no ask to roll back. So its next clean shutdown commits what the run wrote
// and what the session goes on to write, together, as any boot does.
#[test]
fn a_run_taken_over_leaves_the_vm_it_booted_up_to_commit_later() {
    let vm = vm(false);
    let boot = boot(&vm);
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("carry on");
    wait_for("the session to show", || vm.row(&id).contains(UUID));

    let run = runs::take_over(&vm.store, &id).unwrap();
    assert_eq!(run.state, runs::State::TakenOver);
    assert_eq!(serde_json::to_value(&run).unwrap()["state"], "taken_over");
    assert_eq!(fs::read_to_string(vm.ctl.join("signal")).unwrap(), "TERM\n");
    assert!(!boot.halted(), "the run shut its VM down");
    assert!(!vm.dir.path().join("vms/dev/rollback").exists());
    assert!(!vm.dir.path().join("vms/dev/runs/1/takeover").exists());
    let row = vm.row(&id);
    assert!(row.contains(&format!("Z  taken over  {UUID}  ")), "{row}");
    let result = vm.result(&id);
    assert_eq!(result["exit_code"], json!(null), "{result}");
    assert_eq!(result["outcome"], json!(null));
    assert_eq!(result["generation"], json!(1));
    assert_eq!(
        result["changed"]["error"],
        "taken over: dev runs on, on the run's disk, so what the run changed isn't measured"
    );
    assert_eq!(vm.generation(), 0);

    // Later, as `runner stop` would.
    let api = runner::firecracker::Api::new(vm.dir.path().join("vms/dev/api.sock"));
    api.ctrl_alt_del().unwrap();
    assert!(boot.halted());
    assert_eq!(boot.asked(), None);
    assert_eq!(vm.generation(), 1);
}

// One that joined dev running keeps the checkpoint it took first, in `runs`
// and its result, for a restore by hand; dev runs on as it was.
#[test]
fn a_joined_run_taken_over_keeps_its_checkpoint() {
    let vm = vm(true);
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("look around");
    wait_for("the session to show", || vm.row(&id).contains(UUID));

    let run = runs::take_over(&vm.store, &id).unwrap();
    assert_eq!(run.state, runs::State::TakenOver);
    assert_eq!(run.checkpoint, Some(2));
    assert_eq!(run.outcome, None);
    assert_eq!(fs::read_to_string(vm.ctl.join("signal")).unwrap(), "TERM\n");
    let row = vm.row(&id);
    assert!(
        row.contains(&format!("Z  taken over, checkpoint 2 undoes it  {UUID}  ")),
        "{row}"
    );
    let result = vm.result(&id);
    assert_eq!(result["checkpoint"], json!(2), "{result}");
    assert_eq!(result["outcome"], json!(null));
    assert_eq!(stops_and_starts(&vm), Vec::<String>::new());
    let listed = vm.ok(&["checkpoints", "dev"]);
    let first = listed.lines().next().unwrap();
    assert!(first.ends_with("  \"before run dev.1\""), "{listed}");
}

// A cancel asked for as well wins: the run rolls back as it says.
#[test]
fn a_cancel_asked_for_too_wins_over_a_takeover() {
    let vm = vm(false);
    let boot = boot(&vm);
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("carry on");
    wait_for("the session to show", || vm.row(&id).contains(UUID));
    fs::write(vm.dir.path().join("vms/dev/runs/1/cancel"), "").unwrap();
    let run = runs::take_over(&vm.store, &id).unwrap();
    assert_eq!(run.state, runs::State::Cancelled);
    assert!(vm.row(&id).contains("Z  cancelled, rolled back  "));
    assert!(boot.halted());
    assert_eq!(boot.asked().as_deref(), Some("run dev.1"));
    assert_eq!(vm.generation(), 0);
}

#[test]
fn a_run_that_has_ended_can_t_be_taken_over() {
    let vm = vm(true);
    let id = vm.start("quick");
    vm.go(0, 0);
    vm.wait_until_ended(&id);
    let err = runs::take_over(&vm.store, &id).unwrap_err().to_string();
    assert_eq!(err, "dev.1 has already ended: exited 0, joined");
}

// Without a terminal there's nothing to carry the session on in.
#[test]
fn a_takeover_without_a_terminal_leaves_the_run_going() {
    let vm = vm(true);
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("carry on");
    wait_for("the session to show", || vm.row(&id).contains(UUID));
    let out = vm
        .runner(&["claude", "--run", &id])
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    let err = text(&out.stderr);
    assert!(err.contains("needs a terminal"), "{err}");
    still_going(&vm, &id);
}

// Nor before Claude Code has named its session: there's none to resume.
#[test]
fn a_run_without_a_session_yet_isn_t_taken_over() {
    let vm = vm(true);
    fs::write(vm.ctl.join("hold"), "").unwrap();
    let id = vm.start("hi");
    wait_for("claude to start", || vm.started());
    let t = common::pty(24, 80);
    let mut taking = t.spawn(vm.runner(&["claude", "--run", &id]));
    assert_eq!(taking.wait().unwrap().code(), Some(255), "{}", t.seen());
    t.wait_for("dev.1 has no Claude Code session yet");
    still_going(&vm, &id);
}

// A run that rolled back is taken over from its own disk: a copy goes on top
// of dev's history, dev boots onto it, and the session's record is looked
// for there. The fake guest's home has none, so the error names it.
#[test]
fn a_rolled_back_run_s_takeover_puts_its_disk_back_then_looks_for_its_session() {
    let vm = vm(false);
    let _boot = boot(&vm);
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("break it");
    wait_for("the session to show", || vm.row(&id).contains(UUID));
    vm.go(1, 3);
    let row = vm.wait_until_ended(&id);
    assert!(row.contains("  exited 3, rolled back  "), "{row}");
    assert_eq!(runs::placed(&vm.store, &id).unwrap().generation, Some(1));
    // What the run left on its disk, which dev's own doesn't have.
    let gens = vm.dir.path().join("vms/dev/gens");
    debugfs(&gens.join("1.ext4"), "rm /home/agent/work/old.rs");

    let t = common::pty(24, 200);
    let mut taking = t.spawn(vm.runner(&["claude", "--run", &id]));
    assert_eq!(taking.wait().unwrap().code(), Some(255), "{}", t.seen());
    t.wait_for(
        "runner: put dev.1's disk, generation 1, back on dev as generation 2, over generation 0: \
         `runner restore dev 0` brings that back",
    );
    t.wait_for(&format!(
        "runner: dev.1's session isn't on dev's disk: {} has no \
         ~/.claude/projects/*/{UUID}.jsonl there, so Claude Code never wrote it on the run's \
         disk",
        me()
    ));
    assert_eq!(vm.generation(), 2);
    let disk = |g: u64| fs::read(gens.join(format!("{g}.ext4"))).unwrap();
    assert!(disk(2) == disk(1) && disk(2) != disk(0));
    assert_eq!(
        stops_and_starts(&vm).last().map(String::as_str),
        Some("start")
    );
    let listed = vm.ok(&["checkpoints", "dev"]);
    let first = listed.lines().next().unwrap();
    assert!(first.starts_with("* 2  "), "{listed}");
    assert!(first.ends_with("  \"takeover of dev.1\""), "{listed}");
    assert!(runs::placed(&vm.store, &id).unwrap().taken);

    // Taken again, it goes on from dev's own disk, where the session is now,
    // and puts nothing back a second time.
    vm.sd.set_state("inactive");
    let t = common::pty(24, 200);
    let mut taking = t.spawn(vm.runner(&["claude", "--run", &id]));
    assert_eq!(taking.wait().unwrap().code(), Some(255), "{}", t.seen());
    t.wait_for("there, so a restore, undo or rollback since has taken it");
    assert!(!t.seen().contains("put dev.1's disk"), "{}", t.seen());
    assert_eq!(vm.generation(), 2);
    assert_eq!(vm.store.undo("dev").unwrap().generation, 0);
}

// Putting a rolled-back run's disk back waits for no run, asked before
// whether dev is up, since a run that booted dev has it up, and stopping dev
// would end that run. Once gc has dropped the disk, it says so.
#[test]
fn a_rolled_back_run_s_disk_isn_t_put_back_under_a_run_on_a_dev_that_s_up_or_once_gc_dropped_it() {
    let vm = vm(false);
    let _boot = boot(&vm);
    fs::write(vm.ctl.join("init"), init(UUID)).unwrap();
    let id = vm.start("break it");
    wait_for("the session to show", || vm.row(&id).contains(UUID));
    vm.go(0, 3);
    vm.wait_until_ended(&id);
    // The runner on a terminal: its error, and that it failed.
    let refused = || {
        let t = common::pty(24, 200);
        let mut taking = t.spawn(vm.runner(&["claude", "--run", &id]));
        assert_eq!(taking.wait().unwrap().code(), Some(255), "{}", t.seen());
        wait_for("its error", || t.seen().contains('\n'));
        t.seen()
    };
    vm.sd.set_state("active");
    let starting = runs::hold(&vm.store, "dev").unwrap();
    let seen = refused();
    assert!(
        seen.contains("dev already has a run starting: one run per VM"),
        "{seen}"
    );
    drop(starting);
    let seen = refused();
    assert!(
        seen.contains(
            "dev.1 rolled back, so taking it over puts its disk back on dev, which needs dev \
             stopped and would end its sessions: `runner stop dev` first"
        ),
        "{seen}"
    );
    vm.sd.set_state("inactive");
    let host = claude::Host(&vm.store);
    assert!(host.kept("dev", 1).unwrap());
    vm.store.clone().keeping(0).gc("dev", false).unwrap();
    assert!(!host.kept("dev", 1).unwrap());
    let seen = refused();
    assert!(
        seen.contains(
            "dev.1 rolled back, and its disk, dev's generation 1, is gone: gc dropped it at a \
             later commit, and the session with it"
        ),
        "{seen}"
    );
    assert_eq!(vm.generation(), 0);
    assert!(!runs::placed(&vm.store, &id).unwrap().taken);
}
