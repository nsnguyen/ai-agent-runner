// `runner mcp --name runner@kvm-host --peer …`: the first host's MCP
// reaching every host. Three roots on this machine stand in for three hosts:
// this one, kvm-host, and kvm-host-2 and kvm-host-3 behind a fake ssh that
// runs `runner peer` on the peer's own root, with SSH_ORIGINAL_COMMAND set,
// as sshd does for a peer's key line.
mod common;

use common::{FakeVsock, Guest, PEER_SSH, PeerHost as Host, me, script, wait_for};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

const PEERS: [&str; 2] = ["runner@kvm-host-2", "runner@kvm-host-3"];

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
}

impl Session {
    fn send(&mut self, msg: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{msg}").unwrap();
    }

    // Every line on stdout must be protocol, the peers' included.
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

    fn ok(&mut self, id: u64, tool: &str, args: Value) -> Value {
        let got = self.call(id, tool, args);
        assert_eq!(got["isError"], json!(false), "{tool}: {got}");
        got["structuredContent"].clone()
    }

    fn fails(&mut self, id: u64, tool: &str, args: Value) -> String {
        let got = self.call(id, tool, args);
        assert_eq!(got["isError"], json!(true), "{tool}: {got}");
        got["content"][0]["text"].as_str().unwrap().to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The first host as the laptop lists it, and what MCP calls it.
const HERE: &str = "runner@kvm-host";

fn here() -> String {
    "kvm-host".to_string()
}

// This host has base and dev, with 2 GiB free; kvm-host-2 base and web,
// 4 GiB; kvm-host-3 base and old, 6 GiB. web has a guest, and a run that's
// over.
struct Fleet {
    dir: tempfile::TempDir,
    here: Host,
    two: Host,
    three: Host,
    _vsock: FakeVsock,
    _guest: Guest,
}

impl Fleet {
    fn new() -> Fleet {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        script(&bin.join("ssh"), PEER_SSH);
        let fleet = dir.path().join("fleet");
        let here = Host::new(dir.path().join("here"), &["dev"], 2 << 30);
        let two = Host::new(fleet.join("kvm-host-2"), &["web"], 4 << 30);
        let three = Host::new(fleet.join("kvm-host-3"), &["old"], 6 << 30);
        let guest = Guest::spawn(&two.dir.join("guest.sock"));
        let vsock = FakeVsock::serve(&two.root().join("vms/web/vsock.sock"), &guest.sock);
        // A run of web's that exited 0, as `runner drive` leaves it.
        let run = two.root().join("vms/web/runs/1");
        fs::create_dir_all(&run).unwrap();
        let ask = json!({"prompt": "say hi", "dir": "/home/agent/work", "resume": null, "user": "agent", "boots": true, "started": "2026-11-05T00:00:00Z"});
        fs::write(run.join("ask.json"), ask.to_string()).unwrap();
        fs::write(run.join("stream"), "{\"type\":\"assistant\",\"n\":1}\n").unwrap();
        let end = json!({"ended": "2026-11-05T00:01:00Z", "exit_code": 0, "signal": null, "cancelled": false, "outcome": "committed", "error": null});
        fs::write(run.join("end.json"), end.to_string()).unwrap();
        Fleet {
            dir,
            here,
            two,
            three,
            _vsock: vsock,
            _guest: guest,
        }
    }

    fn host(&self, name: &str) -> &Host {
        match name {
            "kvm-host-2" => &self.two,
            "kvm-host-3" => &self.three,
            _ => &self.here,
        }
    }

    fn down(&self, name: &str, down: bool) {
        let mark = self.host(name).dir.join("down");
        if down {
            fs::write(mark, "").unwrap();
        } else {
            fs::remove_file(mark).unwrap();
        }
    }

    // `runner mcp` on this host, with the peers as the laptop passes them.
    fn serve(&self) -> Session {
        let sys = std::env::var("PATH").unwrap();
        let bin = self.dir.path().join("bin");
        let path = format!("{}:{}", bin.display(), self.here.sd.path());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.here.root())
            .env("RUNNER_MEMINFO", self.here.dir.join("meminfo"))
            .env("RUNNER_USER", me())
            .env("PATH", path)
            .env("SYS_PATH", sys)
            .env("FLEET", self.dir.path().join("fleet"))
            .env("RUNNER", env!("CARGO_BIN_EXE_runner"))
            .args(["mcp", "--name", HERE]);
        for p in PEERS {
            cmd.args(["--peer", p]);
        }
        let mut child = cmd
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
        let params =
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
        s.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params}));
        s.reply(1);
        s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn ssh_log(&self) -> Vec<String> {
        let log = fs::read_to_string(self.dir.path().join("fleet/ssh-log")).unwrap_or_default();
        log.lines().map(String::from).collect()
    }
}

// (name, host) for each VM listed.
fn listed(vms: &Value) -> BTreeSet<(String, String)> {
    vms["vms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            let name = v["name"].as_str().unwrap().to_string();
            let host = v["host"].as_str().unwrap_or("NO HOST").to_string();
            (name, host)
        })
        .collect()
}

fn set(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn list_vms_and_list_hosts_cover_every_host() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    let here = here();
    // Nothing is dialed before a call needs a peer.
    s.ok(2, "vm_status", json!({"name": "dev"}));
    s.ok(3, "doctor", json!({}));
    assert_eq!(fleet.ssh_log(), Vec::<String>::new());

    let vms = s.ok(4, "list_vms", json!({}));
    let want = set(&[
        ("base", &here),
        ("dev", &here),
        ("base", "kvm-host-2"),
        ("web", "kvm-host-2"),
        ("base", "kvm-host-3"),
        ("old", "kvm-host-3"),
    ]);
    assert_eq!(listed(&vms), want, "{vms}");
    assert!(vms.get("unreachable").is_none(), "{vms}");

    let hosts = s.ok(5, "list_hosts", json!({}));
    let rows: Vec<(String, u64, u64)> = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let name = h["name"].as_str().unwrap().to_string();
            (
                name,
                h["vms"].as_u64().unwrap(),
                h["mem_available"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (here.clone(), 2, 2 << 30),
            ("kvm-host-2".to_string(), 2, 4 << 30),
            ("kvm-host-3".to_string(), 2, 6 << 30),
        ],
        "{hosts}"
    );

    // One ssh per peer, kept for every call after, which passes on who
    // exec runs as and takes a new host's key on first sight.
    s.ok(6, "list_vms", json!({}));
    s.ok(7, "vm_status", json!({"name": "web"}));
    let log = fleet.ssh_log();
    assert_eq!(log.len(), 2, "{log:?}");
    for peer in PEERS {
        let line = log
            .iter()
            .find(|l| l.contains(&format!(" {peer} ")))
            .unwrap();
        assert!(line.contains("BatchMode=yes"), "{line}");
        assert!(
            line.contains("IdentitiesOnly=yes -i ~/.ssh/id_ed25519"),
            "{line}"
        );
        assert!(line.contains("StrictHostKeyChecking=accept-new"), "{line}");
        let tail = format!(" {peer} runner mcp --user {}", me());
        assert!(line.ends_with(&tail), "{line}");
    }
}

#[test]
fn a_peer_s_vm_or_run_is_reached_on_its_root_only() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();

    let web = s.ok(2, "vm_status", json!({"name": "web"}));
    assert_eq!(
        (&web["name"], &web["state"]),
        (&json!("web"), &json!("stopped"))
    );
    assert!(fleet.two.asked_about("web"));
    assert!(!fleet.here.asked_about("web") && !fleet.three.asked_about("web"));

    // As the first host's RUNNER_USER, whom the fake guest runs as.
    let argv = ["sh", "-c", "echo hi from web; id -un"];
    let out = s.ok(3, "exec", json!({"name": "web", "argv": argv}));
    assert_eq!(
        out["stdout"],
        json!(format!("hi from web\n{}\n", me())),
        "{out}"
    );
    let start = "systemctl start firecracker@web.service".to_string();
    assert!(fleet.two.sd.calls().contains(&start));

    let read = s.ok(4, "run_output", json!({"run": "web.1"}));
    assert_eq!(read["run"]["id"], json!("web.1"), "{read}");
    assert_eq!(read["run"]["state"], json!("exited"), "{read}");
    assert_eq!(
        read["events"],
        json!([{"type": "assistant", "n": 1}]),
        "{read}"
    );
    let runs = s.ok(5, "list_runs", json!({"name": "web"}));
    assert_eq!(runs["runs"][0]["prompt"], json!("say hi"), "{runs}");
    let why = s.fails(6, "cancel_run", json!({"run": "web.1"}));
    assert!(
        why.starts_with("web.1 has already ended: exited 0"),
        "{why}"
    );

    // This host's own VMs stay here.
    s.ok(7, "vm_status", json!({"name": "dev"}));
    assert!(fleet.here.asked_about("dev"));
    assert!(!fleet.two.asked_about("dev") && !fleet.three.asked_about("dev"));

    let gone = s.ok(8, "delete_vm", json!({"name": "web"}));
    assert_eq!(gone["name"], json!("web"), "{gone}");
    assert!(!fleet.two.has("web"));
    assert!(fleet.here.has("dev") && fleet.three.has("old"));
    let vms = s.ok(9, "list_vms", json!({}));
    assert!(!listed(&vms).iter().any(|(v, _)| v == "web"), "{vms}");

    // A VM no host has is named as one host would.
    let why = s.fails(10, "vm_status", json!({"name": "nope"}));
    assert_eq!(why, "no VM named nope");
    let why = s.fails(11, "run_output", json!({"run": "nope.1"}));
    assert_eq!(why, "no VM named nope");
}

#[test]
fn create_vm_places_by_free_memory_and_takes_a_host() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    let here = here();
    let on = |vm: &str| -> Vec<&str> {
        ["here", "kvm-host-2", "kvm-host-3"]
            .into_iter()
            .filter(|h| fleet.host(h).has(vm))
            .collect()
    };

    let made = s.ok(2, "create_vm", json!({"name": "a"}));
    assert_eq!(made["name"], json!("a"), "{made}");
    assert_eq!(on("a"), ["kvm-host-3"], "the most free memory");
    // A fork goes where the VM it forks is.
    s.ok(3, "create_vm", json!({"name": "b", "from": "web"}));
    assert_eq!(on("b"), ["kvm-host-2"]);
    s.ok(4, "create_vm", json!({"name": "c", "host": "kvm-host-2"}));
    assert_eq!(on("c"), ["kvm-host-2"]);
    s.ok(
        5,
        "create_vm",
        json!({"name": "d", "host": "runner@kvm-host-2"}),
    );
    assert_eq!(on("d"), ["kvm-host-2"]);
    s.ok(6, "create_vm", json!({"name": "e", "host": here}));
    assert_eq!(on("e"), ["here"]);
    s.ok(66, "create_vm", json!({"name": "e2", "host": HERE}));
    assert_eq!(on("e2"), ["here"]);
    fleet.here.set_free(8 << 30);
    s.ok(7, "create_vm", json!({"name": "f", "cpus": 4}));
    assert_eq!(on("f"), ["here"]);
    assert_eq!(fleet.here.store().machine("f").unwrap().cpus, 4);
    fleet.here.set_free(1 << 30);
    s.ok(
        8,
        "create_vm",
        json!({"name": "g", "mem": "1G", "idle": "never"}),
    );
    assert_eq!(on("g"), ["kvm-host-3"]);
    assert_eq!(fleet.three.store().machine("g").unwrap().mem_mib, 1024);

    // Names are one per VM across hosts.
    let why = s.fails(9, "create_vm", json!({"name": "web"}));
    assert!(
        why.contains("kvm-host-2 has a VM named web already"),
        "{why}"
    );
    let why = s.fails(
        10,
        "create_vm",
        json!({"name": "dev", "host": "kvm-host-3"}),
    );
    assert!(
        why.contains(&format!("{here} has a VM named dev already")),
        "{why}"
    );
    assert_eq!(on("web"), ["kvm-host-2"]);
    assert_eq!(on("dev"), ["here"]);
    let why = s.fails(11, "create_vm", json!({"name": "h", "host": "nope"}));
    assert!(why.contains("nope") && why.contains("kvm-host-3"), "{why}");
    assert!(on("h").is_empty());
    let why = s.fails(
        12,
        "create_vm",
        json!({"name": "i", "from": "web", "host": "kvm-host-3"}),
    );
    assert!(why.contains("no VM named web"), "{why}");

    let vms = s.ok(13, "list_vms", json!({}));
    let all = listed(&vms);
    for (vm, host) in [
        ("a", "kvm-host-3"),
        ("b", "kvm-host-2"),
        ("e", here.as_str()),
    ] {
        assert!(
            all.contains(&(vm.to_string(), host.to_string())),
            "{vm}: {vms}"
        );
    }
}

#[test]
fn a_peer_whose_ssh_fails_is_named_while_the_rest_answer() {
    let fleet = Fleet::new();
    fleet.down("kvm-host-3", true);
    let mut s = fleet.serve();
    let here = here();

    let vms = s.ok(2, "list_vms", json!({}));
    let want = set(&[
        ("base", &here),
        ("dev", &here),
        ("base", "kvm-host-2"),
        ("web", "kvm-host-2"),
    ]);
    assert_eq!(listed(&vms), want, "{vms}");
    let down = &vms["unreachable"];
    assert_eq!(down.as_array().unwrap().len(), 1, "{vms}");
    assert_eq!(down[0]["host"], json!("kvm-host-3"), "{vms}");
    let why = down[0]["error"].as_str().unwrap();
    assert!(why.contains("Connection timed out"), "{why}");

    let hosts = s.ok(3, "list_hosts", json!({}));
    let names: Vec<&str> = hosts["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [here.as_str(), "kvm-host-2"], "{hosts}");
    assert_eq!(
        hosts["unreachable"][0]["host"],
        json!("kvm-host-3"),
        "{hosts}"
    );

    // A VM only the host that's down could have can't be found, and says why.
    let why = s.fails(4, "vm_status", json!({"name": "old"}));
    assert!(why.contains("kvm-host-3 didn't answer"), "{why}");
    assert!(why.contains("Connection timed out"), "{why}");
    // The rest still answer, and create places on them, never the one down.
    s.ok(5, "vm_status", json!({"name": "web"}));
    s.ok(6, "create_vm", json!({"name": "new"}));
    assert!(fleet.two.has("new"), "4 GiB free beats this host's 2");

    // Back up, it's dialed again.
    fleet.down("kvm-host-3", false);
    let vms = s.ok(7, "list_vms", json!({}));
    assert!(listed(&vms).contains(&("old".to_string(), "kvm-host-3".to_string())));
    assert!(vms.get("unreachable").is_none(), "{vms}");

    // Its connection dropped and down again: its VM, last seen there, is
    // named as there.
    cut(&fleet, "kvm-host-3");
    let why = s.fails(8, "vm_status", json!({"name": "old"}));
    assert!(
        why.contains("old is on kvm-host-3, which didn't answer"),
        "{why}"
    );
    let why = s.fails(9, "create_vm", json!({"name": "old"}));
    assert!(
        why.contains("kvm-host-3 has a VM named old already"),
        "{why}"
    );
    let why = s.fails(10, "create_vm", json!({"name": "z", "from": "old"}));
    let want = "old is on kvm-host-3, which didn't answer: ssh: connect";
    assert!(why.starts_with(want), "{why}");
}

// A VM create_vm makes on a peer is noted there at once, so with that peer
// down before anything lists it again, its name stays taken and the VM is
// named as there.
#[test]
fn a_vm_made_on_a_peer_keeps_its_name_while_that_peer_is_down() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    s.ok(2, "create_vm", json!({"name": "dup", "host": "kvm-host-3"}));
    assert!(fleet.three.has("dup"));
    cut(&fleet, "kvm-host-3");

    let why = s.fails(3, "create_vm", json!({"name": "dup"}));
    assert!(
        why.contains("kvm-host-3 has a VM named dup already"),
        "{why}"
    );
    assert!(!fleet.two.has("dup") && !fleet.here.has("dup"));
    let why = s.fails(4, "vm_status", json!({"name": "dup"}));
    assert!(
        why.contains("dup is on kvm-host-3, which didn't answer"),
        "{why}"
    );
}

// A VM delete_vm deletes on a peer is dropped from what was noted there, so
// with that peer down its name is free and the VM isn't named as there.
#[test]
fn a_vm_deleted_on_a_peer_frees_its_name_while_that_peer_is_down() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    s.ok(2, "delete_vm", json!({"name": "old"}));
    assert!(!fleet.three.has("old"));
    cut(&fleet, "kvm-host-3");

    let why = s.fails(3, "vm_status", json!({"name": "old"}));
    assert!(
        why.starts_with("no host that answered has a VM named old"),
        "{why}"
    );
    s.ok(4, "create_vm", json!({"name": "old"}));
    assert!(fleet.two.has("old"), "4 GiB free beats this host's 2");
}

// move_vm through the first host: web goes from kvm-host-2 to kvm-host-3,
// host to host, and every call after finds it there; dev from here to
// kvm-host-2. Its own host, or one that isn't listed, is refused first.
#[test]
fn move_vm_moves_a_vm_between_any_two_hosts() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    let before = fleet.two.store().get("web").unwrap();
    let moved = s.ok(2, "move_vm", json!({"name": "web", "host": "kvm-host-3"}));
    assert_eq!(moved["name"], json!("web"), "{moved}");
    assert_eq!(moved["host"], json!("kvm-host-3"), "{moved}");
    assert_eq!(moved["state"], json!("stopped"), "{moved}");
    assert_eq!(moved["checkpoints"], json!(0), "{moved}");
    assert!(!fleet.two.has("web") && fleet.three.has("web"));
    assert_eq!(fleet.three.store().get("web").unwrap().bytes, before.bytes);
    // kvm-host-2 dialed kvm-host-3 itself, with its own key.
    let log = fleet.ssh_log();
    let import = log.iter().find(|l| l.contains(" runner import ")).unwrap();
    assert!(
        import.ends_with(" runner@kvm-host-3 runner import web - --creds claude"),
        "{import}"
    );
    s.ok(3, "vm_status", json!({"name": "web"}));
    assert!(fleet.three.asked_about("web"));
    let vms = s.ok(4, "list_vms", json!({}));
    assert!(listed(&vms).contains(&("web".to_string(), "kvm-host-3".to_string())));

    let moved = s.ok(
        5,
        "move_vm",
        json!({"name": "dev", "host": "runner@kvm-host-2"}),
    );
    assert_eq!(moved["host"], json!("kvm-host-2"), "{moved}");
    assert!(!fleet.here.has("dev") && fleet.two.has("dev"));
    // Found there at once, as the move left it, with kvm-host-3 down.
    fleet.down("kvm-host-3", true);
    s.ok(6, "vm_status", json!({"name": "dev"}));
    assert!(fleet.two.asked_about("dev"));
    fleet.down("kvm-host-3", false);

    let why = s.fails(7, "move_vm", json!({"name": "web", "host": "kvm-host-3"}));
    assert_eq!(why, "web is on kvm-host-3 already");
    let why = s.fails(8, "move_vm", json!({"name": "dev", "host": "kvm-host-9"}));
    assert!(why.starts_with("no host named kvm-host-9"), "{why}");
    // Names are one per name across hosts, but one made behind MCP's back
    // still isn't overwritten.
    fleet.here.store().fork("base", "twin").unwrap();
    fleet.three.store().fork("base", "twin").unwrap();
    let why = s.fails(9, "move_vm", json!({"name": "twin", "host": "kvm-host-3"}));
    assert_eq!(
        why,
        "kvm-host-3 has a VM named twin already: VM names are one per name across hosts"
    );
    assert!(fleet.here.has("twin"));
}

// A peer's ssh fails from now on, and the session it had ends.
fn cut(fleet: &Fleet, name: &str) {
    fleet.down(name, true);
    let pid = peer_pid(fleet.host(name));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for("the peer to go", || gone(pid));
}

// A peer that went away while nothing was asked of it, and came back, as
// after a reboot or a dropped tailnet: the next call dials it again.
#[test]
fn a_peer_whose_session_ended_while_idle_is_dialed_again() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    s.ok(2, "vm_status", json!({"name": "web"}));
    let pid = peer_pid(&fleet.two);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_for("the peer to go", || gone(pid));
    let web = s.ok(3, "vm_status", json!({"name": "web"}));
    assert_eq!(web["name"], json!("web"), "{web}");
    let log = fleet.ssh_log();
    let dials = log
        .iter()
        .filter(|l| l.contains(" runner@kvm-host-2 "))
        .count();
    assert_eq!(dials, 2, "{log:?}");
}

// A peer whose runner stops answering while its ssh stays up is named, in
// the time a list waits, rather than holding every list up.
#[test]
fn a_peer_that_stops_answering_is_named_once_its_list_is_overdue() {
    let fleet = Fleet::new();
    let mut s = fleet.serve();
    s.ok(2, "list_vms", json!({}));
    let pid = peer_pid(&fleet.two);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
    let started = std::time::Instant::now();
    let vms = s.ok(3, "list_vms", json!({}));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(25));
    assert_eq!(vms["unreachable"][0]["host"], json!("kvm-host-2"), "{vms}");
    let why = vms["unreachable"][0]["error"].as_str().unwrap();
    assert!(why.starts_with("no answer in"), "{why}");
    assert!(listed(&vms).contains(&("old".to_string(), "kvm-host-3".to_string())));
    // Going again, it answers again.
    let vms = s.ok(4, "list_vms", json!({}));
    assert!(vms.get("unreachable").is_none(), "{vms}");
}

// With a host down whose ssh takes its time to say so: a VM last listed on
// another peer is reached without waiting on it, and calls that all need the
// one that's down wait out one dial between them, not one each.
#[test]
fn a_slow_host_that_s_down_holds_up_only_what_needs_it() {
    let fleet = Fleet::new();
    fs::write(fleet.three.dir.join("slow"), "").unwrap();
    fleet.down("kvm-host-3", true);
    let mut s = fleet.serve();
    s.ok(2, "list_vms", json!({}));
    let started = std::time::Instant::now();
    s.ok(3, "vm_status", json!({"name": "web"}));
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_millis(1500), "{took:?}");
    let started = std::time::Instant::now();
    for id in 4..8 {
        let call = json!({"name": "list_vms", "arguments": {}});
        s.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": call}));
    }
    for id in 4..8 {
        let got = s.reply(id)["result"]["structuredContent"].clone();
        assert_eq!(got["unreachable"][0]["host"], json!("kvm-host-3"), "{got}");
    }
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_millis(3500), "{took:?}");
}

// The process a peer's ssh became: its `runner peer`.
fn peer_pid(host: &Host) -> nix::unistd::Pid {
    let pid = fs::read_to_string(host.dir.join("pid")).unwrap();
    nix::unistd::Pid::from_raw(pid.trim().parse().unwrap())
}

// Whether a killed process has gone, or is only waiting to be reaped: a
// zombie whose other threads have gone too, so its pipes have closed. Its
// first thread shows as a zombie while the rest are still on their way out.
fn gone(pid: nix::unistd::Pid) -> bool {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let line = |key: &str| {
        status
            .lines()
            .find(|l| l.starts_with(key))
            .unwrap_or_default()
            .to_string()
    };
    nix::sys::signal::kill(pid, None).is_err()
        || (line("State:").contains("zombie") && line("Threads:").ends_with("\t1"))
}
