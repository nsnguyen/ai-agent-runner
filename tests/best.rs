// Best of N: `runner run dev --n 3` forks dev into dev-1 to dev-3 and starts
// the prompt in each as an ordinary run. systemctl is faked, and every VM has
// stand-ins of its own: runner-guest on a Unix socket behind a fake of
// Firecracker's vsock socket, and Firecracker's API, which marks the unit
// inactive at Ctrl-Alt-Del as a shutdown does. A fork's are linked into its
// directory as systemctl starts it, since it's made only then. The fake
// `claude` notes what it got under the name of the VM it runs in, which it
// reads from its guest's command line, then waits for `ctl/go`.
mod common;

use common::wait_for;
use common::{FakeApi, FakeSystemd, FakeVsock, Guest, debugfs, image, me, script, text, tree};
use runner::best;
use runner::creds::Cred;
use runner::store::{self, Checkpoint, Store};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;

const GIB: u64 = 1 << 30;

struct Host {
    dir: TempDir,
    store: Store,
    sd: FakeSystemd,
    ctl: PathBuf,
    work: PathBuf,
    _fakes: Vec<Fake>,
}

// One VM's stand-ins. At Ctrl-Alt-Del, its API commits the run of a boot
// given one, as a clean shutdown does, and the unit goes inactive.
struct Fake {
    _guest: Guest,
    _vsock: FakeVsock,
    _api: FakeApi,
}

fn fake(dir: &Path, sd: &FakeSystemd, vm: &str, run: Option<store::Run>) -> Fake {
    let fakes = dir.join("fakes");
    fs::create_dir_all(&fakes).unwrap();
    let guest = Guest::spawn(&fakes.join(format!("{vm}.guest")));
    let vsock = FakeVsock::serve(&fakes.join(format!("{vm}.vsock")), &guest.sock);
    let state = sd.bin.join(format!("state.firecracker@{vm}.service"));
    let run = Mutex::new(run);
    let api = FakeApi::serve(&fakes.join(format!("{vm}.api")), move |req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            if let Some(run) = run.lock().unwrap().take() {
                run.commit().unwrap();
            }
            fs::write(&state, "inactive\n").unwrap();
        }
        (204, None)
    });
    Fake {
        _guest: guest,
        _vsock: vsock,
        _api: api,
    }
}

// dev on a small ext4 disk, stopped or running as far as systemctl says. A
// running one has a stand-in for its boot, which holds generation 1, a copy
// of generation 0, as a boot does, so the checkpoint a batch takes copies
// that. The host has 64 GiB free unless a test says otherwise.
fn host(running: bool) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("etc/machine-id", ""),
        ("home/agent/work/main.rs", "fn main() {}"),
    ];
    let disk = image(&tree(&dir.path().join("rootfs"), &files));
    let store = Store::new(dir.path());
    store.import("dev", &disk).unwrap();
    let sd = FakeSystemd::new(dir.path());
    sd.set_state("inactive");
    sd.set_vm_state("dev", if running { "active" } else { "inactive" });
    let names = ["dev-1", "dev-2", "dev-3", "dev-4"];
    names.iter().for_each(|vm| sd.set_vm_state(vm, "inactive"));
    let boot = running.then(|| store.begin_run("dev").unwrap());
    let mut fakes = vec![fake(dir.path(), &sd, "dev", boot)];
    fakes.extend(names.iter().map(|vm| fake(dir.path(), &sd, vm, None)));
    let (d, bin) = (dir.path().display(), sd.bin.display());
    let link = r#"vm=${2#firecracker@}; vm=${vm%.service}
ln -sf "$f/$vm.vsock" "$v/$vm/vsock.sock"
ln -sf "$f/$vm.api" "$v/$vm/api.sock""#;
    script(
        &sd.bin.join("on-start"),
        &format!("f='{d}/fakes'; v='{d}/vms'\n{link}\necho active > '{bin}/state.'\"$2\"\n"),
    );
    let ln = |what: &str| {
        let to = dir.path().join(format!("fakes/dev.{what}"));
        std::os::unix::fs::symlink(to, dir.path().join(format!("vms/dev/{what}.sock"))).unwrap();
    };
    ln("vsock");
    ln("api");
    let ctl = dir.path().join("ctl");
    let work = dir.path().join("work");
    fs::create_dir_all(&ctl).unwrap();
    fs::create_dir_all(&work).unwrap();
    script(
        &ctl.join("claude"),
        &format!(
            r#"c='{}'
vm=$(tr '\0' '\n' < /proc/$PPID/cmdline | tail -n 1)
vm=$(basename "$vm" .guest)
mkdir -p "$c/$vm"
printf '%s\n' "$@" > "$c/$vm/argv"
pwd > "$c/$vm/pwd"
cat > "$c/$vm/prompt"
: > "$c/$vm/started"
echo '{{"type":"system","subtype":"init","session_id":"s-'"$vm"'"}}'
while [ ! -e "$c/go" ] && [ -d "$c" ]; do sleep 0.01; done
exit "$(cat "$c/exit" 2>/dev/null || echo 0)"
"#,
            ctl.display()
        ),
    );
    let h = Host {
        dir,
        store,
        sd,
        ctl,
        work,
        _fakes: fakes,
    };
    h.free(64 * GIB);
    h
}

impl Host {
    fn runner(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.env("RUNNER_ROOT", self.dir.path())
            .env("PATH", self.sd.path())
            .env("RUNNER_USER", me())
            .env("RUNNER_CLAUDE", self.ctl.join("claude"))
            .env("RUNNER_MEMINFO", self.meminfo())
            .args(args);
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.runner(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    // What `args` printed on stderr, which it fails with.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.runner(args).output().unwrap();
        assert!(
            !out.status.success(),
            "{args:?} worked: {}",
            text(&out.stdout)
        );
        text(&out.stderr)
    }

    /// `runner run dev --n <n> <prompt>` in the work directory: the ids it
    /// printed, one a line.
    fn best_of(&self, n: u32, prompt: &str) -> Vec<String> {
        let n = n.to_string();
        let args = ["run", "dev", "--n", &n, prompt, "--dir", self.work_dir()];
        let out = returns_at_once(self.runner(&args));
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout).lines().map(String::from).collect()
    }

    fn work_dir(&self) -> &str {
        self.work.to_str().unwrap()
    }

    fn meminfo(&self) -> PathBuf {
        self.dir.path().join("meminfo")
    }

    // MemAvailable, in bytes.
    fn free(&self, bytes: u64) {
        let text = format!(
            "MemTotal: {} kB\nMemAvailable: {} kB\n",
            128 * GIB / 1024,
            bytes / 1024
        );
        fs::write(self.meminfo(), text).unwrap();
    }

    // What the fake claude in `vm` was given, once it has started.
    fn got(&self, vm: &str, what: &str) -> String {
        let started = self.ctl.join(vm).join("started");
        wait_for(&format!("claude to start in {vm}"), || started.exists());
        fs::read_to_string(self.ctl.join(vm).join(what)).unwrap()
    }

    fn go(&self) {
        fs::write(self.ctl.join("go"), "").unwrap();
    }

    /// The run as `runner runs <its vm>` lists it, once it has ended.
    fn ended(&self, id: &str) -> String {
        let vm = id.rsplit_once('.').unwrap().0;
        let row = || {
            let rows = self.ok(&["runs", vm]);
            let row = rows.lines().find(|r| r.starts_with(&format!("{id}  ")));
            row.unwrap_or_else(|| panic!("no {id} in {rows}"))
                .to_string()
        };
        // The third column; an error further on can say "running" too.
        wait_for(&format!("{id} to end"), || !row().contains("Z  running  "));
        row()
    }

    fn vms(&self) -> Vec<String> {
        let vms = self.store.list().unwrap();
        vms.into_iter().map(|vm| vm.name).collect()
    }

    fn history(&self) -> Vec<Checkpoint> {
        self.store.checkpoints("dev").unwrap()
    }

    fn disk(&self, vm: &str, generation: u64) -> Vec<u8> {
        fs::read(self.store.disk(vm, generation).unwrap()).unwrap()
    }
}

// The command that starts the runs returns, its output closed, while they
// go on: nothing it leaves running holds the caller's stdout or stderr.
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
        .expect("the command returned while its runs went on")
}

// A file on an ext4 disk, as debugfs reads it.
fn cat(disk: &Path, path: &str) -> String {
    let out = Command::new("debugfs")
        .args(["-R", &format!("cat {path}")])
        .arg(disk)
        .output()
        .unwrap();
    text(&out.stdout)
}

const ARGV: &str = "-p\n--verbose\n--output-format\nstream-json\n--dangerously-skip-permissions\n";

#[test]
fn best_of_three_on_a_stopped_vm_runs_the_prompt_in_three_new_forks() {
    let h = host(false);
    h.store
        .set_creds("dev", &[Cred::Claude, Cred::Github])
        .unwrap();
    let before = h.history();

    let ids = h.best_of(3, "fix the tests\nall of them");
    assert_eq!(ids, ["dev-1.1", "dev-2.1", "dev-3.1"]);
    assert_eq!(h.vms(), ["dev", "dev-1", "dev-2", "dev-3"]);
    let current = h.disk("dev", 0);
    for vm in ["dev-1", "dev-2", "dev-3"] {
        assert!(h.disk(vm, 0) == current, "{vm}'s disk isn't dev's");
        // The default credentials, so N agents can't all push with dev's.
        assert_eq!(h.store.creds(vm).unwrap(), [Cred::Claude], "{vm}");
        assert_eq!(h.got(vm, "prompt"), "fix the tests\nall of them", "{vm}");
        assert_eq!(h.got(vm, "pwd").trim(), h.work_dir(), "{vm}");
        assert_eq!(h.got(vm, "argv"), ARGV, "{vm}");
        // Each fork was new and stopped, so its run boots it.
        let start = format!("systemctl start firecracker@{vm}.service");
        assert!(h.sd.calls().contains(&start), "{vm}");
    }
    // dev's record of them, for `tries` and `keep`.
    let batch = best::tries(&h.store, "dev").unwrap().expect("a record");
    assert_eq!(batch.prompt, "fix the tests\nall of them");
    assert_eq!(batch.generation, 0);
    assert_eq!(batch.checkpoint, None);
    let tries: Vec<(&str, Option<&str>)> = (batch.tries.iter())
        .map(|t| (t.vm.as_str(), t.run.as_deref()))
        .collect();
    assert_eq!(
        tries,
        [
            ("dev-1", Some("dev-1.1")),
            ("dev-2", Some("dev-2.1")),
            ("dev-3", Some("dev-3.1"))
        ]
    );
    // Each is an ordinary run.
    assert!(h.ok(&["runs", "dev-2"]).starts_with("dev-2.1  "));
    let cancelled = h.ok(&["cancel", "dev-3.1"]);
    assert!(
        cancelled.contains("  cancelled, rolled back  "),
        "{cancelled}"
    );

    h.go();
    for id in ["dev-1.1", "dev-2.1"] {
        let row = h.ended(id);
        assert!(row.contains("  exited 0, "), "{row}");
        let result = h.ok(&["result", id]);
        assert!(result.contains("\"exit_code\": 0"), "{result}");
    }
    // dev isn't touched.
    assert_eq!(h.ok(&["runs", "dev"]), "no runs yet\n");
    assert_eq!(h.history(), before);
    assert_eq!(h.store.creds("dev").unwrap(), [Cred::Claude, Cred::Github]);
    let dev_starts = "systemctl start firecracker@dev.service".to_string();
    assert!(!h.sd.calls().contains(&dev_starts));
}

#[test]
fn best_of_two_on_a_running_vm_forks_a_checkpoint_of_what_it_runs() {
    let h = host(true);
    // What dev wrote since it booted, on the generation it runs.
    let note = h.dir.path().join("note.txt");
    fs::write(&note, "since boot").unwrap();
    let running = h.store.disk("dev", 1).unwrap();
    debugfs(
        &running,
        &format!("write {} /home/agent/work/note.txt", note.display()),
    );

    let ids = h.best_of(2, "p");
    assert_eq!(ids, ["dev-1.1", "dev-2.1"]);
    let runs: Vec<Checkpoint> = h.history().into_iter().filter(|c| c.run).collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].comment.as_deref(), Some("before best of 2"));
    let made = h.disk("dev", runs[0].id);
    for vm in ["dev-1", "dev-2"] {
        assert!(h.disk(vm, 0) == made, "{vm}'s disk isn't the checkpoint");
        let disk = h.store.disk(vm, 0).unwrap();
        assert_eq!(cat(&disk, "/home/agent/work/note.txt"), "since boot");
        assert_eq!(h.got(vm, "prompt"), "p");
    }
    let batch = best::tries(&h.store, "dev").unwrap().unwrap();
    assert_eq!(batch.generation, runs[0].id);
    assert_eq!(batch.checkpoint, Some(runs[0].id));
    // dev runs on as it was.
    assert!(
        h.sd.calls()
            .iter()
            .all(|c| !c.contains("firecracker@dev.service") || c.contains(" show "))
    );

    h.go();
    ids.iter().for_each(|id| {
        h.ended(id);
    });
}

#[test]
fn a_batch_of_the_wrong_size_or_too_big_for_memory_is_refused() {
    let h = host(false);
    let before = h.history();
    for n in ["1", "5", "0"] {
        let err = h.refused(&["run", "dev", "--n", n, "p"]);
        assert!(err.contains("2 to 4"), "{n}: {err}");
    }
    // Room for two of dev's 2 GiB, with 1 GiB to spare.
    h.free(5 * GIB + 100 * (1 << 20));
    let err = h.refused(&["run", "dev", "--n", "3", "p"]);
    assert!(err.contains("--n 2"), "{err}");
    h.free(4 * GIB);
    let err = h.refused(&["run", "dev", "--n", "2", "p"]);
    assert!(err.contains("not even two"), "{err}");
    assert_eq!(h.vms(), ["dev"]);
    assert_eq!(h.history(), before);
    assert_eq!(best::tries(&h.store, "dev").unwrap(), None);
}

#[test]
fn a_try_s_name_that_s_taken_is_refused_and_named() {
    let h = host(false);
    h.store.fork("dev", "dev-2").unwrap();
    let err = h.refused(&["run", "dev", "--n", "3", "p"]);
    assert!(err.contains("dev-2"), "{err}");
    assert_eq!(h.vms(), ["dev", "dev-2"]);
    assert_eq!(best::tries(&h.store, "dev").unwrap(), None);
    assert!(!h.sd.calls().iter().any(|c| c.contains("systemctl start")));
}

#[test]
fn a_batch_is_refused_while_dev_has_a_run_going() {
    let h = host(false);
    let before = h.history();
    let run = returns_at_once(h.runner(&["run", "dev", "x", "--dir", h.work_dir()]));
    assert!(run.status.success(), "{}", text(&run.stderr));
    h.got("dev", "started");

    let err = h.refused(&["run", "dev", "--n", "2", "p"]);
    assert!(err.contains("already has a run going, dev.1"), "{err}");
    assert_eq!(h.vms(), ["dev"]);
    assert_eq!(h.history(), before);
    h.go();
    h.ended("dev.1");
}

#[test]
fn one_batch_at_a_time() {
    let h = host(false);
    h.go();
    let ids = h.best_of(2, "p");
    ids.iter().for_each(|id| {
        h.ended(id);
    });
    // Over, but not kept or dropped.
    let err = h.refused(&["run", "dev", "--n", "2", "p"]);
    assert!(err.contains("dev-1 and dev-2"), "{err}");
    assert_eq!(h.vms(), ["dev", "dev-1", "dev-2"]);
    // Gone by hand, they're dropped.
    h.ok(&["delete", "dev-1"]);
    h.ok(&["delete", "dev-2"]);
    assert_eq!(h.best_of(2, "again"), ["dev-1.1", "dev-2.1"]);
    let batch = best::tries(&h.store, "dev").unwrap().unwrap();
    assert_eq!(batch.prompt, "again");
}

#[test]
fn mcp_runs_best_of() {
    let h = host(false);
    let mut s = mcp(&h);
    let args = json!({"name": "dev", "prompt": "hi", "n": 3, "dir": h.work_dir()});
    let started = s.call(2, "run_best_of", args);
    let runs = started["structuredContent"]["runs"]
        .as_array()
        .unwrap()
        .clone();
    let ids: Vec<&str> = runs.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["dev-1.1", "dev-2.1", "dev-3.1"], "{started}");
    for run in &runs {
        assert_eq!(run["state"], json!("running"));
        assert_eq!(run["boots"], json!(true));
    }
    let bad = s.call(
        3,
        "run_best_of",
        json!({"name": "dev", "prompt": "hi", "n": 5}),
    );
    assert_eq!(bad["isError"], json!(true), "{bad}");
    h.go();
    ids.iter().for_each(|id| {
        h.ended(id);
    });
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

fn mcp(h: &Host) -> Mcp {
    use std::io::BufRead;
    let mut child = h
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
