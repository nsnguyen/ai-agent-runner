// `runner mcp` over real stdin and stdout, as local Claude Code drives it
// through `ssh -T host runner mcp`.
mod common;

use common::{FakeSystemd, FakeVsock, Guest, StandIn, me};
use runner::exec::OUTPUT_CAP;
use runner::store::Store;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
}

impl Session {
    fn send(&mut self, msg: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{msg}").unwrap();
    }

    // What happens when Claude Code quits or the SSH connection drops.
    fn hang_up(mut self) -> ExitStatus {
        drop(self.stdin.take());
        self.child.wait().unwrap()
    }

    // Every line on stdout must be protocol; anything else breaks the client.
    fn reply(&mut self, id: u64) -> Value {
        for line in self.lines.by_ref() {
            let line = line.unwrap();
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("not JSON-RPC on stdout ({e}): {line}"));
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

// A test that fails partway still reaps its server.
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// A VM's allocated bytes, which the filesystem decides.
fn used(root: &Path, vm: &str) -> u64 {
    fs::read_dir(root.join("vms").join(vm).join("gens"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().blocks() * 512)
        .sum()
}

fn start(root: &std::path::Path) -> Session {
    start_with_path(root, &std::env::var("PATH").unwrap())
}

fn start_with_path(root: &std::path::Path, path: &str) -> Session {
    start_with_env(root, &[("PATH", path)])
}

fn start_with_env(root: &std::path::Path, env: &[(&str, &str)]) -> Session {
    let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .envs(env.iter().copied())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut s = Session {
        child,
        stdin,
        lines,
    };
    let client = json!({"name": "test", "version": "0"});
    let params = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
    s.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params}));
    let init = s.reply(1)["result"].clone();
    assert!(init["capabilities"]["tools"].is_object());
    assert_eq!(init["serverInfo"]["name"], json!("runner"));
    s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    s
}

#[test]
fn lists_its_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = start(dir.path());
    s.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = s.reply(2)["result"]["tools"].clone();
    let mut names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "cancel_run",
            "checkpoint_vm",
            "create_vm",
            "delete_vm",
            "doctor",
            "exec",
            "kill_session",
            "list_checkpoints",
            "list_runs",
            "list_sessions",
            "list_vms",
            "read_session",
            "restore_vm",
            "run_agent",
            "run_output",
            "sleep_vm",
            "start_vm",
            "stop_vm",
            "undo_vm",
            "vm_status",
            "wake_vm"
        ]
    );
}

#[test]
fn create_then_list() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("base", &disk).unwrap();

    let mut s = start(dir.path());
    let made = s.call(2, "create_vm", json!({"name": "dev"}));
    assert_eq!(made["isError"], json!(false), "{made}");
    let dev = json!({"name": "dev", "generation": 0, "bytes": 6, "used": used(dir.path(), "dev")});
    assert_eq!(made["structuredContent"], dev);

    let listed = s.call(3, "list_vms", json!({}));
    let base =
        json!({"name": "base", "generation": 0, "bytes": 6, "used": used(dir.path(), "base")});
    assert_eq!(listed["structuredContent"], json!({"vms": [base, dev]}));

    let gone = s.call(4, "delete_vm", json!({"name": "dev"}));
    assert_eq!(gone["isError"], json!(false), "{gone}");
    assert_eq!(gone["structuredContent"], dev);
    let listed = s.call(5, "list_vms", json!({}));
    assert_eq!(listed["structuredContent"], json!({"vms": [base]}));
}

#[test]
fn a_bad_call_is_a_tool_error_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = start(dir.path());
    let got = s.call(2, "undo_vm", json!({"name": "../etc"}));
    assert_eq!(got["isError"], json!(true), "{got}");
    assert!(
        got["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("isn't a VM name")
    );
    // Still serving after the error, and exits cleanly when the client goes.
    let listed = s.call(3, "list_vms", json!({}));
    assert_eq!(listed["structuredContent"], json!({"vms": []}));
    assert!(s.hang_up().success());
}

#[test]
fn start_status_and_stop_through_systemd() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("base", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());

    let mut s = start_with_path(dir.path(), &sd.path());
    let address = Store::new(dir.path()).net("base").unwrap().address;
    let used = used(dir.path(), "base");
    let base = |state: &str| json!({"name": "base", "generation": 0, "bytes": 6, "used": used, "address": address, "state": state});
    let got = s.call(2, "vm_status", json!({"name": "base"}));
    assert_eq!(got["structuredContent"], base("stopped"), "{got}");
    let got = s.call(3, "start_vm", json!({"name": "base"}));
    assert_eq!(got["structuredContent"], base("running"), "{got}");
    let got = s.call(4, "stop_vm", json!({"name": "base"}));
    assert_eq!(got["structuredContent"], base("stopped"), "{got}");
    assert!(
        sd.calls()
            .contains(&"systemctl start firecracker@base.service".to_string())
    );

    let got = s.call(5, "start_vm", json!({"name": "nope"}));
    assert_eq!(got["isError"], json!(true), "{got}");
    sd.fail_with("Job for firecracker@base.service failed.");
    let got = s.call(6, "start_vm", json!({"name": "base"}));
    assert_eq!(got["isError"], json!(true), "{got}");
    let why = got["content"][0]["text"].as_str().unwrap();
    assert!(
        why.contains("Job for firecracker@base.service failed."),
        "{why}"
    );
}

// Through the VM's boot (src/sleep.rs), which a stand-in plays here.
#[test]
fn sleep_and_wake_through_the_vm_s_boot() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state("active");
    let boot = StandIn::serve(&store, "dev");
    let mut s = start_with_path(dir.path(), &sd.path());
    let got = s.call(2, "sleep_vm", json!({"name": "dev"}));
    assert_eq!(
        got["structuredContent"]["state"],
        json!("sleeping"),
        "{got}"
    );
    let got = s.call(3, "vm_status", json!({"name": "dev"}));
    assert_eq!(
        got["structuredContent"]["state"],
        json!("sleeping"),
        "{got}"
    );
    let got = s.call(4, "wake_vm", json!({"name": "dev"}));
    assert_eq!(got["structuredContent"]["state"], json!("running"), "{got}");
    assert_eq!(
        boot.asked(),
        [runner::sleep::Ask::Sleep, runner::sleep::Ask::Wake]
    );
    sd.set_state("inactive");
    let got = s.call(5, "wake_vm", json!({"name": "dev"}));
    assert_eq!(got["isError"], json!(true), "{got}");
}

#[test]
fn create_vm_takes_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();

    let mut s = start(dir.path());
    let made = s.call(
        2,
        "create_vm",
        json!({"name": "big", "cpus": 4, "mem": "8G"}),
    );
    assert_eq!(made["isError"], json!(false), "{made}");
    let machine = store.machine("big").unwrap();
    assert_eq!((machine.cpus, machine.mem_mib), (4, 8192));
    let bad = s.call(3, "create_vm", json!({"name": "huge", "cpus": 64}));
    assert_eq!(bad["isError"], json!(true), "{bad}");
}

#[test]
fn doctor_reports_every_check() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = start(dir.path());
    let got = s.call(2, "doctor", json!({}));
    assert_eq!(got["isError"], json!(false), "{got}");
    let report = &got["structuredContent"];
    let names: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    let all = [
        "kvm",
        "firecracker",
        "reflink",
        "kernel",
        "base",
        "nat",
        "unit",
    ];
    assert_eq!(names, all, "{report}");
    // No base VM in an empty root, so at least that one fails.
    assert_eq!(report["checks"][4]["status"], json!("fail"), "{report}");
    assert_eq!(report["ok"], json!(false), "{report}");
}

#[test]
fn checkpoint_list_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("dev", &disk).unwrap();

    let mut s = start(dir.path());
    let made = s.call(
        2,
        "checkpoint_vm",
        json!({"name": "dev", "comment": "clean"}),
    );
    assert_eq!(made["isError"], json!(false), "{made}");
    let mut c = made["structuredContent"].clone();
    let time = c.as_object_mut().unwrap().remove("time").unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(time.as_str().unwrap()).is_ok());
    let want = json!({"id": 1, "comment": "clean", "auto": false, "agent": false, "bytes": 6, "current": false});
    assert_eq!(c, want);

    let listed = s.call(3, "list_checkpoints", json!({"name": "dev"}));
    let listed: Vec<_> = listed["structuredContent"]["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].clone(), c["auto"].clone(), c["current"].clone()))
        .collect();
    assert_eq!(
        listed,
        [
            (json!(1), json!(false), json!(false)),
            (json!(0), json!(true), json!(true))
        ]
    );

    let back = s.call(4, "restore_vm", json!({"name": "dev", "id": 1}));
    assert_eq!(back["isError"], json!(false), "{back}");
    let dev = json!({"name": "dev", "generation": 2, "bytes": 6, "used": used(dir.path(), "dev")});
    assert_eq!(back["structuredContent"], dev);

    let bad = s.call(5, "restore_vm", json!({"name": "dev", "id": 9}));
    assert_eq!(bad["isError"], json!(true), "{bad}");
    let why = bad["content"][0]["text"].as_str().unwrap();
    assert!(why.contains("dev has no checkpoint 9"), "{why}");
}

// A VM named dev with a guest behind its vsock socket: runner-guest on a
// Unix socket, run as the test's user.
struct GuestVm {
    dir: tempfile::TempDir,
    sd: FakeSystemd,
    _vsock: FakeVsock,
    _guest: Guest,
}

fn guest_vm() -> GuestVm {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    let guest = Guest::spawn(&dir.path().join("guest.sock"));
    let vsock = FakeVsock::serve(&dir.path().join("vms/dev/vsock.sock"), &guest.sock);
    GuestVm {
        dir,
        sd,
        _vsock: vsock,
        _guest: guest,
    }
}

impl GuestVm {
    fn serve(&self) -> Session {
        let path = self.sd.path();
        start_with_env(self.dir.path(), &[("PATH", &path), ("RUNNER_USER", &me())])
    }
}

#[test]
fn exec_runs_a_command_in_a_vm_it_starts_first() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let argv = ["sh", "-c", "echo out; echo err >&2; exit 3"];
    let got = s.call(2, "exec", json!({"name": "dev", "argv": argv}));
    assert_eq!(got["isError"], json!(false), "{got}");
    let want = json!({
        "exit_code": 3, "signal": null, "timed_out": false,
        "stdout": "out\n", "stdout_truncated": false,
        "stderr": "err\n", "stderr_truncated": false,
    });
    assert_eq!(got["structuredContent"], want);
    assert!(
        vm.sd
            .calls()
            .contains(&"systemctl start firecracker@dev.service".to_string()),
        "{:?}",
        vm.sd.calls()
    );
}

#[test]
fn exec_takes_stdin_and_caps_output() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let got = s.call(
        2,
        "exec",
        json!({"name": "dev", "argv": ["cat"], "stdin": "héllo"}),
    );
    assert_eq!(got["structuredContent"]["stdout"], json!("héllo"), "{got}");
    let script = format!("head -c {} /dev/zero | tr '\\0' a", OUTPUT_CAP + 1);
    let got = s.call(
        3,
        "exec",
        json!({"name": "dev", "argv": ["sh", "-c", script]}),
    );
    let out = &got["structuredContent"];
    assert_eq!(out["stdout"].as_str().unwrap().len(), OUTPUT_CAP, "{got}");
    assert_eq!(out["stdout_truncated"], json!(true));
    assert_eq!(out["exit_code"], json!(0));
}

#[test]
fn exec_kills_a_command_past_its_timeout() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let start = std::time::Instant::now();
    let args = json!({"name": "dev", "argv": ["sleep", "30"], "timeout_secs": 1});
    let got = s.call(2, "exec", args);
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let out = &got["structuredContent"];
    assert_eq!(out["timed_out"], json!(true), "{got}");
    assert_eq!(
        (&out["exit_code"], &out["signal"]),
        (&json!(null), &json!(9))
    );
}

#[test]
fn exec_errors_are_tool_errors() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let why = |got: &Value| got["content"][0]["text"].as_str().unwrap().to_string();
    let got = s.call(2, "exec", json!({"name": "nope", "argv": ["true"]}));
    assert_eq!(got["isError"], json!(true), "{got}");
    assert_eq!(why(&got), "no VM named nope");
    let got = s.call(3, "exec", json!({"name": "dev", "argv": []}));
    assert_eq!(got["isError"], json!(true), "{got}");
    assert!(why(&got).contains("argv"), "{got}");
    // Neither of those started the VM.
    assert!(vm.sd.calls().is_empty(), "{:?}", vm.sd.calls());
    let got = s.call(
        4,
        "exec",
        json!({"name": "dev", "argv": ["no-such-command"]}),
    );
    assert_eq!(got["isError"], json!(true), "{got}");
    assert!(why(&got).contains("couldn't run no-such-command"), "{got}");
}

// Sessions share the test user's tmux server with other tests, so this one's
// has a name of its own and goes when the test does.
struct Tmux<'a> {
    vm: &'a GuestVm,
    name: String,
}

impl Drop for Tmux<'_> {
    fn drop(&mut self) {
        let target = format!("={}", self.name);
        let mut s = self.vm.serve();
        s.call(
            99,
            "exec",
            json!({"name": "dev", "argv": ["tmux", "kill-session", "-t", target]}),
        );
    }
}

fn sessions_of(s: &mut Session, id: u64) -> Vec<Value> {
    let got = s.call(id, "list_sessions", json!({"name": "dev"}));
    assert_eq!(got["isError"], json!(false), "{got}");
    got["structuredContent"]["sessions"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn sessions_are_listed_read_and_killed() {
    let vm = guest_vm();
    vm.sd.set_state("active");
    let mut s = vm.serve();
    let name = format!("mcp-{}", std::process::id());
    let _tmux = Tmux {
        vm: &vm,
        name: name.clone(),
    };
    // 300 lines, more than the pane's 24 rows, then a wait.
    let script = "seq 300; exec sleep 300";
    let argv = json!([
        "tmux",
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        "80",
        "-y",
        "24",
        script
    ]);
    let got = s.call(2, "exec", json!({"name": "dev", "argv": argv}));
    assert_eq!(got["structuredContent"]["exit_code"], json!(0), "{got}");

    let mut id = 3;
    let ours = loop {
        id += 1;
        let all = sessions_of(&mut s, id);
        let ours = all.into_iter().find(|v| v["name"] == json!(name)).unwrap();
        if ours["command"] == json!("sleep") || id > 200 {
            break ours;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(ours["command"], json!("sleep"), "{ours}");
    assert_eq!(ours["attached"], json!(false), "{ours}");
    assert!(ours["started"].as_str().unwrap().ends_with('Z'), "{ours}");

    let read = |s: &mut Session, id: u64, args: Value| {
        let got = s.call(id, "read_session", args);
        assert_eq!(got["isError"], json!(false), "{got}");
        got["structuredContent"].clone()
    };
    // The pane's command is the sleep a moment before tmux has drawn seq's end.
    let last3 = json!({"name": "dev", "session": name, "lines": 3});
    let want = json!({"text": "298\n299\n300\n", "truncated": false});
    let mut got = read(&mut s, 300, last3.clone());
    for id in 400..500 {
        if got == want {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        got = read(&mut s, id, last3.clone());
    }
    assert_eq!(got, want);
    // 100 unless told, reaching back past the screen into tmux's history.
    let got = read(&mut s, 301, json!({"name": "dev", "session": name}));
    let want: String = (201..=300).map(|n| format!("{n}\n")).collect();
    assert_eq!(got["text"], json!(want));

    let got = s.call(302, "kill_session", json!({"name": "dev", "session": name}));
    assert_eq!(got["structuredContent"], json!({"killed": name}), "{got}");
    let left = sessions_of(&mut s, 303);
    assert!(left.iter().all(|v| v["name"] != json!(name)), "{left:?}");
}

#[test]
fn session_errors_are_tool_errors() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let why = |got: &Value| got["content"][0]["text"].as_str().unwrap().to_string();
    // Stopped: no sessions, and nothing boots to look.
    assert_eq!(sessions_of(&mut s, 2), Vec::<Value>::new());
    let got = s.call(3, "read_session", json!({"name": "dev", "session": "work"}));
    assert_eq!(got["isError"], json!(true), "{got}");
    assert_eq!(why(&got), "no session work in dev");
    let got = s.call(4, "kill_session", json!({"name": "dev", "session": "work"}));
    assert_eq!(why(&got), "no session work in dev");
    assert!(
        vm.sd
            .calls()
            .iter()
            .all(|c| !c.starts_with("systemctl start"))
    );

    vm.sd.set_state("active");
    let missing = format!("mcp-none-{}", std::process::id());
    let got = s.call(
        5,
        "kill_session",
        json!({"name": "dev", "session": missing}),
    );
    assert_eq!(why(&got), format!("no session {missing} in dev"));
    let got = s.call(6, "read_session", json!({"name": "dev", "session": "a:b"}));
    assert!(why(&got).contains("isn't a session name"), "{got}");
    for lines in [0, 2001] {
        let args = json!({"name": "dev", "session": "work", "lines": lines});
        let got = s.call(7, "read_session", args);
        assert_eq!(got["isError"], json!(true), "{got}");
        assert!(why(&got).contains("lines"), "{got}");
    }
    let got = s.call(8, "list_sessions", json!({"name": "nope"}));
    assert_eq!(why(&got), "no VM named nope");
}
