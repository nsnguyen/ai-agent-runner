// `runner mcp` over real stdin and stdout, as local Claude Code drives it
// through `ssh -T host runner mcp`.
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

fn start(root: &Path) -> Session {
    let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
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
    assert_eq!(names, ["create_vm", "delete_vm", "list_vms", "undo_vm"]);
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
