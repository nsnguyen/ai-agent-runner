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
            "add_service",
            "cancel_run",
            "checkpoint_vm",
            "create_vm",
            "delete_vm",
            "doctor",
            "exec",
            "keep_try",
            "kill_session",
            "list_checkpoints",
            "list_hosts",
            "list_runs",
            "list_services",
            "list_sessions",
            "list_tries",
            "list_vms",
            "move_vm",
            "read_session",
            "rebase_vm",
            "remove_service",
            "resize_vm",
            "restore_vm",
            "run_agent",
            "run_best_of",
            "run_output",
            "sleep_vm",
            "start_vm",
            "stop_vm",
            "undo_vm",
            "vm_allowlist",
            "vm_status",
            "wake_vm"
        ]
    );
    // It says how many of runs' and the agent's checkpoints gc keeps.
    let listing = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "list_checkpoints")
        .unwrap();
    let said = listing["description"].as_str().unwrap();
    let keep = format!("newest {}", runner::store::KEEP_CHECKPOINTS);
    assert!(said.contains(&keep), "{said}");
}

// A peer's own MCP has no peers, and takes move_vm's host as ssh takes it:
// never as one of ssh's options, which would run a command.
#[test]
fn move_vm_refuses_a_host_ssh_would_take_for_an_option() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();
    store.fork("base", "dev").unwrap();
    let ran = dir.path().join("ran");
    let host = format!("-oProxyCommand=touch {}", ran.display());

    let mut s = start(dir.path());
    let got = s.call(2, "move_vm", json!({"name": "dev", "host": host}));
    assert_eq!(got["isError"], json!(true), "{got}");
    let why = got["content"][0]["text"].as_str().unwrap();
    assert!(why.contains("isn't a host"), "{why}");
    assert!(!ran.exists(), "ssh ran its option");
    assert!(store.get("dev").is_ok());
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
    let dev = json!({"name": "dev", "generation": 0, "bytes": 6, "used": used(dir.path(), "dev"), "cpus": 2, "mem_mib": 2048});
    assert_eq!(made["structuredContent"], dev);

    // Each VM with its host, this one, by the kernel's name for it.
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let here = |vm: &Value| {
        let mut vm = vm.clone();
        vm["host"] = json!(host.trim());
        vm
    };
    let listed = s.call(3, "list_vms", json!({}));
    let base = json!({"name": "base", "generation": 0, "bytes": 6, "used": used(dir.path(), "base"), "cpus": 2, "mem_mib": 2048});
    assert_eq!(
        listed["structuredContent"],
        json!({"vms": [here(&base), here(&dev)]})
    );

    let gone = s.call(4, "delete_vm", json!({"name": "dev"}));
    assert_eq!(gone["isError"], json!(false), "{gone}");
    assert_eq!(gone["structuredContent"], dev);
    let listed = s.call(5, "list_vms", json!({}));
    assert_eq!(listed["structuredContent"], json!({"vms": [here(&base)]}));
    // Without peers, only this host is a host.
    let made = s.call(6, "create_vm", json!({"name": "x", "host": host.trim()}));
    assert_eq!(made["isError"], json!(false), "{made}");
    let bad = s.call(7, "create_vm", json!({"name": "y", "host": "kvm-host-2"}));
    let why = bad["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        why,
        format!("no host named kvm-host-2: the hosts are {}", host.trim())
    );
}

// A local Claude Code reads VMs' output, so it's what an agent would talk
// into widening a list: MCP reads the list, and only `runner net` sets it.
#[test]
fn vm_allowlist_reads_the_list_and_no_tool_changes_it() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();
    store.fork("base", "dev").unwrap();

    let mut s = start(dir.path());
    let open = s.call(2, "vm_allowlist", json!({"name": "dev"}));
    assert_eq!(open["isError"], json!(false), "{open}");
    assert_eq!(
        open["structuredContent"],
        json!({"name": "dev", "open": true, "names": []})
    );
    let hosts = ["registry.npmjs.org", "github.com"].map(|n| n.parse().unwrap());
    store.allow("dev", &hosts).unwrap();
    let listed = s.call(3, "vm_allowlist", json!({"name": "dev"}));
    assert_eq!(
        listed["structuredContent"],
        json!({
            "name": "dev",
            "open": false,
            "names": ["api.anthropic.com", "github.com", "registry.npmjs.org"]
        })
    );
    let missing = s.call(4, "vm_allowlist", json!({"name": "nope"}));
    assert_eq!(missing["isError"], json!(true), "{missing}");

    // It's marked read-only, and takes only the VM's name. No tool takes
    // names to allow or deny, nor is named for changing a list.
    s.send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}));
    let tools = s.reply(5)["result"]["tools"].clone();
    let tools = tools.as_array().unwrap();
    let tool = tools.iter().find(|t| t["name"] == "vm_allowlist").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], json!(true), "{tool}");
    let takes = |t: &Value| -> Vec<String> {
        let props = t["inputSchema"]["properties"].as_object();
        props.map_or(Vec::new(), |p| p.keys().cloned().collect())
    };
    assert_eq!(takes(tool), ["name"]);
    for t in tools {
        let name = t["name"].as_str().unwrap();
        for word in ["allow", "deny", "open", "net"] {
            assert!(
                name == "vm_allowlist" || !name.contains(word),
                "{name} looks like it changes a list"
            );
        }
        for arg in takes(t) {
            let arg = arg.as_str();
            assert!(
                !["allow", "deny", "names", "hosts", "hostnames", "open"].contains(&arg),
                "{name} takes {arg}"
            );
        }
    }
    // The list is as it was.
    assert_eq!(store.allowed("dev").unwrap().unwrap().names().len(), 3);
}

#[test]
fn rebase_vm_says_what_it_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let base = common::tree(&dir.path().join("base"), &[("etc/os-release", "2\n")]);
    store.import("base", &common::image(&base)).unwrap();
    let dev = common::tree(
        &dir.path().join("dev"),
        &[
            ("etc/systemd/system/runner-svc-web.service", "[Service]\n"),
            ("home/agent/work/a\nb", "skipped"),
        ],
    );
    store.import("dev", &common::image(&dev)).unwrap();

    let mut s = start(dir.path());
    let done = s.call(2, "rebase_vm", json!({"name": "dev"}));
    assert_eq!(done["isError"], json!(false), "{done}");
    let vm = json!({"name": "dev", "generation": 1, "bytes": 16 << 20, "used": used(dir.path(), "dev"), "cpus": 2, "mem_mib": 2048});
    assert_eq!(
        done["structuredContent"],
        json!({
            "vm": vm,
            "base_generation": 0,
            "kept": ["/home", "/root", "/etc/machine-id"],
            "services": ["web"],
            "skipped": [{"path": "/home/agent/work/a\nb", "why": "name"}],
            "more": 0,
            "own_claude": null,
        })
    );

    let held = store.lock("dev").unwrap();
    let busy = s.call(3, "rebase_vm", json!({"name": "dev"}));
    assert_eq!(busy["isError"], json!(true), "{busy}");
    let why = busy["content"][0]["text"].as_str().unwrap();
    assert!(why.contains("stop it first: runner stop dev"), "{why}");
    drop(held);
    assert_eq!(store.get("dev").unwrap().generation, 1);
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
    let base = |state: &str| json!({"name": "base", "generation": 0, "bytes": 6, "used": used, "cpus": 2, "mem_mib": 2048, "address": address, "state": state});
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

// Each call holds the host up while it works, and the session between calls
// doesn't, so a `runner mcp` left open on the laptop lets the host power off
// (src/power.rs).
#[test]
fn a_tool_call_holds_the_busy_lock_and_the_session_doesn_t() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    Store::new(dir.path()).import("base", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    let d = sd.bin.display();
    common::script(
        &sd.bin.join("systemctl"),
        &format!(
            r#"case "$1" in
start) touch "{d}/started"; while [ ! -e "{d}/go" ]; do sleep 0.01; done ;;
show) echo active ;;
esac
"#
        ),
    );
    let lock = runner::power::busy_lock(dir.path());
    let free = || {
        let file = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock);
        file.unwrap().try_lock().is_ok()
    };

    let mut s = start_with_path(dir.path(), &sd.path());
    let got = s.call(2, "list_vms", json!({}));
    assert_ne!(got["isError"], json!(true), "{got}");
    assert!(free(), "held between calls");
    let params = json!({"name": "start_vm", "arguments": {"name": "base"}});
    s.send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": params}));
    common::wait_for("the start", || sd.bin.join("started").exists());
    assert!(!free(), "free while start_vm works");
    fs::write(sd.bin.join("go"), "").unwrap();
    let got = s.reply(3)["result"].clone();
    assert_eq!(got["structuredContent"]["state"], json!("running"), "{got}");
    assert!(free(), "held after start_vm");
}

// With no peers, this host only: its VMs, the ones up, and its room, as
// `runner hosts` counts them. tests/mcp_hosts.rs has the rest.
#[test]
fn list_hosts_gives_this_host_s_row() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();
    store.fork("base", "dev").unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_vm_state("dev", "active");
    // 8 GiB, less the 1 GiB reserve and dev's 2 GiB and 64 MiB.
    let meminfo = dir.path().join("meminfo");
    fs::write(&meminfo, "MemTotal: 8388608 kB\nMemAvailable: 1 kB\n").unwrap();

    let path = sd.path();
    let reserve = dir.path().join("reserve");
    let mut s = start_with_env(
        dir.path(),
        &[
            ("PATH", &path),
            ("RUNNER_MEMINFO", meminfo.to_str().unwrap()),
            ("RUNNER_RESERVE", reserve.to_str().unwrap()),
        ],
    );
    let got = s.call(2, "list_hosts", json!({}));
    assert_eq!(got["isError"], json!(false), "{got}");
    let name = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let free = (8192u64 - 1024 - 2048 - 64) << 20;
    assert_eq!(
        got["structuredContent"],
        json!({"hosts": [{"name": name.trim(), "vms": 2, "up": 1, "mem_available": free}]})
    );

    fs::write(&meminfo, "MemAvailable: 8000000 kB\n").unwrap();
    let got = s.call(3, "list_hosts", json!({}));
    assert_eq!(got["isError"], json!(true), "{got}");
    let why = got["content"][0]["text"].as_str().unwrap();
    assert!(why.contains("MemTotal"), "{why}");
}

// A start on a full host is refused as the CLI's is, with nothing started:
// 4 GiB, less the 1 GiB reserve and dev's 2 GiB and 64 MiB, leaves 0.9G,
// short of web's 2 GiB and 64 MiB.
#[test]
fn start_vm_on_a_full_host_says_what_the_cli_says() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();
    store.fork("base", "dev").unwrap();
    store.fork("base", "web").unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_vm_state("dev", "active");
    sd.set_vm_state("web", "inactive");
    let meminfo = dir.path().join("meminfo");
    fs::write(&meminfo, "MemTotal: 4194304 kB\n").unwrap();
    let path = sd.path();
    let reserve = dir.path().join("reserve");
    let env = [
        ("PATH", path.as_str()),
        ("RUNNER_MEMINFO", meminfo.to_str().unwrap()),
        ("RUNNER_RESERVE", reserve.to_str().unwrap()),
    ];
    let cli = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", dir.path())
        .envs(env)
        .args(["start", "web"])
        .output()
        .unwrap();
    assert!(!cli.status.success());
    let said = String::from_utf8_lossy(&cli.stderr);
    let said = said.trim().strip_prefix("runner: ").unwrap();
    assert!(said.starts_with("web needs 2.1G; "), "{said}");
    assert!(
        said.ends_with(" has 0.9G free: stop or sleep a VM, or move web"),
        "{said}"
    );

    let mut s = start_with_env(dir.path(), &env);
    let got = s.call(2, "start_vm", json!({"name": "web"}));
    assert_eq!(got["isError"], json!(true), "{got}");
    assert_eq!(got["content"][0]["text"], json!(said), "{got}");
    assert!(
        !sd.calls().iter().any(|c| c.starts_with("systemctl start")),
        "{:?}",
        sd.calls()
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

// A 16 MiB ext4 disk grown to 32 MiB, sizes set, refused while up, and a
// fork grown as it's made.
#[test]
fn resize_vm_grows_a_disk_and_sets_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let tree = common::tree(&dir.path().join("dev"), &[("home/agent/a", "kept\n")]);
    store.import("dev", &common::image(&tree)).unwrap();
    let sd = FakeSystemd::new(dir.path());
    let mut s = start_with_path(dir.path(), &sd.path());

    let args = json!({"name": "dev", "disk": "32M", "mem": "4G", "cpus": 4});
    let done = s.call(2, "resize_vm", args);
    assert_eq!(done["isError"], json!(false), "{done}");
    let used = used(dir.path(), "dev");
    let vm = json!({"name": "dev", "generation": 1, "bytes": 32 << 20, "used": used, "cpus": 4, "mem_mib": 4096});
    assert_eq!(done["structuredContent"], vm);
    let machine = store.machine("dev").unwrap();
    assert_eq!((machine.cpus, machine.mem_mib), (4, 4096));
    let undone = s.call(3, "undo_vm", json!({"name": "dev"}));
    assert_eq!(
        undone["structuredContent"]["bytes"],
        json!(16 << 20),
        "{undone}"
    );

    let bad = s.call(4, "resize_vm", json!({"name": "dev", "disk": "big"}));
    assert_eq!(bad["isError"], json!(true), "{bad}");
    let why = bad["content"][0]["text"].as_str().unwrap();
    assert!(why.contains("isn't a disk size"), "{why}");
    let none = s.call(5, "resize_vm", json!({"name": "dev"}));
    assert_eq!(none["isError"], json!(true), "{none}");
    sd.set_state("active");
    let up = s.call(6, "resize_vm", json!({"name": "dev", "cpus": 2}));
    assert_eq!(up["isError"], json!(true), "{up}");
    let why = up["content"][0]["text"].as_str().unwrap();
    assert!(why.contains("dev is running"), "{why}");
    assert_eq!(store.machine("dev").unwrap().cpus, 4);

    // A fork grown as it's made, its source as it was.
    let history = fs::read_to_string(dir.path().join("vms/dev/history")).unwrap();
    let args = json!({"name": "app", "from": "dev", "disk": "64M"});
    let made = s.call(7, "create_vm", args);
    assert_eq!(made["isError"], json!(false), "{made}");
    assert_eq!(
        made["structuredContent"]["bytes"],
        json!(64 << 20),
        "{made}"
    );
    let after = fs::read_to_string(dir.path().join("vms/dev/history")).unwrap();
    assert_eq!(after, history);
    let small = s.call(
        8,
        "create_vm",
        json!({"name": "x", "from": "dev", "disk": "8M"}),
    );
    assert_eq!(small["isError"], json!(true), "{small}");
}

#[test]
fn create_vm_takes_an_idle_time() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"rootfs").unwrap();
    let store = Store::new(dir.path());
    store.import("base", &disk).unwrap();

    let mut s = start(dir.path());
    let made = s.call(2, "create_vm", json!({"name": "dev", "idle": "never"}));
    assert_eq!(made["isError"], json!(false), "{made}");
    assert_eq!(store.idle("dev").unwrap(), runner::idle::Idle::Never);
    let bad = s.call(3, "create_vm", json!({"name": "x", "idle": "soon"}));
    assert_eq!(bad["isError"], json!(true), "{bad}");
    assert!(bad.to_string().contains("isn't an idle time"), "{bad}");
    assert!(store.get("x").is_err());
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
        "e2fsprogs",
        "reflink",
        "kernel",
        "base",
        "nat",
        "unit",
        "key",
        "host-idle",
        "room",
    ];
    assert_eq!(names, all, "{report}");
    // No base VM in an empty root, so at least that one fails.
    assert_eq!(report["checks"][5]["status"], json!("fail"), "{report}");
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
    let want = json!({"id": 1, "comment": "clean", "auto": false, "agent": false, "run": false, "bytes": 6, "current": false});
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
    let dev = json!({"name": "dev", "generation": 2, "bytes": 6, "used": used(dir.path(), "dev"), "cpus": 2, "mem_mib": 2048});
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
    let half = "a".repeat(OUTPUT_CAP / 2);
    let want = format!("{half}\n[runner cut 1 byte here]\n{half}");
    assert_eq!(out["stdout"], json!(want), "{got}");
    assert_eq!(out["stdout_truncated"], json!(true));
    assert_eq!(out["exit_code"], json!(0));
}

// Lines numbered from 1, each 16 bytes, so every slice is told apart.
fn numbered(bytes: usize) -> String {
    let lines: String = (1..=bytes / 16)
        .map(|n| format!("line {n:>10}\n"))
        .collect();
    assert_eq!(lines.len(), bytes);
    lines
}

#[test]
fn exec_keeps_the_first_and_last_16_kib_of_each_stream() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let out = numbered(100 * 1024);
    let both = ["sh", "-c", "tee /dev/stderr"];
    let got = s.call(
        2,
        "exec",
        json!({"name": "dev", "argv": both, "stdin": out}),
    );
    let want = format!(
        "{}\n[runner cut {} bytes here]\n{}",
        &out[..16 * 1024],
        68 * 1024,
        &out[out.len() - 16 * 1024..],
    );
    let got = &got["structuredContent"];
    assert_eq!(got["stdout"], json!(want), "{got}");
    assert_eq!(got["stderr"], json!(want));
    assert_eq!(got["stdout_truncated"], json!(true));
    assert_eq!(got["stderr_truncated"], json!(true));
    assert_eq!(got["exit_code"], json!(0));
    // 32 KiB or less comes back whole.
    for len in [32 * 1024, 32 * 1024 - 16, 16] {
        let out = numbered(len);
        let got = s.call(
            3,
            "exec",
            json!({"name": "dev", "argv": both, "stdin": out}),
        );
        let got = &got["structuredContent"];
        assert_eq!(got["stdout"], json!(out), "{len}");
        assert_eq!(got["stderr"], json!(out), "{len}");
        assert_eq!(got["stdout_truncated"], json!(false), "{len}");
        assert_eq!(got["stderr_truncated"], json!(false), "{len}");
    }
}

#[test]
fn exec_s_description_says_it_keeps_both_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = start(dir.path());
    s.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = s.reply(2)["result"]["tools"].clone();
    let exec = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "exec")
        .unwrap();
    let said = exec["description"].as_str().unwrap();
    assert!(said.contains("first 16 KiB and the last 16 KiB"), "{said}");
    assert!(said.contains("only through `runner creds`"), "{said}");
}

#[test]
fn exec_sets_env() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let argv = ["sh", "-c", "echo \"$FOO|$X\""];
    let env = json!({"FOO": "bar baz", "X": "a=b"});
    let got = s.call(2, "exec", json!({"name": "dev", "argv": argv, "env": env}));
    assert_eq!(
        got["structuredContent"]["stdout"],
        json!("bar baz|a=b\n"),
        "{got}"
    );
    // A name a variable can't have is a tool error.
    for bad in ["", "A=B"] {
        let got = s.call(
            3,
            "exec",
            json!({"name": "dev", "argv": ["true"], "env": {bad: "x"}}),
        );
        assert_eq!(got["isError"], json!(true), "{got}");
        let why = got["content"][0]["text"].as_str().unwrap();
        assert!(why.contains("isn't a variable name"), "{why}");
    }
}

// Refused before the VM is looked at, so nothing runs and nothing boots.
#[test]
fn exec_refuses_a_credential_variable() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let ran = vm.dir.path().join("ran");
    let argv = ["touch", ran.to_str().unwrap()];
    for (id, var) in [
        (2, "GH_TOKEN"),
        (3, "CLAUDE_CODE_OAUTH_TOKEN"),
        (4, "ANTHROPIC_API_KEY"),
    ] {
        let env = json!({"FOO": "1", var: "x"});
        let got = s.call(id, "exec", json!({"name": "dev", "argv": argv, "env": env}));
        assert_eq!(got["isError"], json!(true), "{got}");
        let why = got["content"][0]["text"].as_str().unwrap();
        let want =
            format!("{var} is a credential: credentials reach a VM only through `runner creds`");
        assert_eq!(why, want);
        assert!(!ran.exists(), "{var} ran the command");
    }
    assert!(vm.sd.calls().is_empty(), "{:?}", vm.sd.calls());
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

// The service tools' happy path needs the guest's systemd: tests/services.rs
// runs their scripts, and tests/kvm.rs the real thing.
#[test]
fn service_errors_are_tool_errors() {
    let vm = guest_vm();
    let mut s = vm.serve();
    let why = |got: &Value| got["content"][0]["text"].as_str().unwrap().to_string();
    let add = |name: &str, service: &str, argv: Value| json!({"name": name, "service": service, "argv": argv});
    let got = s.call(2, "add_service", add("dev", "a b", json!(["serve"])));
    assert_eq!(got["isError"], json!(true), "{got}");
    assert!(why(&got).contains("isn't a service name"), "{got}");
    let got = s.call(3, "add_service", add("dev", "web", json!([])));
    assert_eq!(why(&got), "no command: give the program and its arguments");
    let args = json!({"name": "dev", "service": "web", "argv": ["serve"], "dir": "work"});
    let got = s.call(4, "add_service", args);
    assert!(why(&got).contains("absolute"), "{got}");
    let got = s.call(5, "remove_service", json!({"name": "dev", "service": "-x"}));
    assert!(why(&got).contains("isn't a service name"), "{got}");
    // None of that touched the VM.
    assert!(vm.sd.calls().is_empty(), "{:?}", vm.sd.calls());
    let got = s.call(6, "list_services", json!({"name": "nope"}));
    assert_eq!(why(&got), "no VM named nope");
    let got = s.call(7, "add_service", add("nope", "web", json!(["serve"])));
    assert_eq!(why(&got), "no VM named nope");
}
