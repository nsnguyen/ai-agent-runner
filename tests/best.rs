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
use runner::changes::{Change, Changes, Kind};
use runner::creds::Cred;
use runner::result::{AgentSays, Changed, Outcome, RunResult};
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
    // Side by side.
    let table = h.ok(&["tries", "dev"]);
    assert!(row(&table, "dev-1.1").contains("  running  "), "{table}");
    assert!(row(&table, "dev-2.1").contains("  running  "), "{table}");
    let third = row(&table, "dev-3.1");
    assert!(third.contains("  cancelled, rolled back  "), "{table}");

    h.go();
    let table = h.ok(&["tries", "dev", "--wait"]);
    for id in ["dev-1.1", "dev-2.1"] {
        assert!(row(&table, id).contains("  exited 0, "), "{table}");
    }
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
    assert!(err.contains("a VM named dev-2 already exists"), "{err}");
    assert!(err.contains("dev-1 to dev-3"), "{err}");
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
    let (before, batch) = (h.history(), best::tries(&h.store, "dev").unwrap());
    let err = h.refused(&["run", "dev", "--n", "2", "p"]);
    assert!(err.contains("dev-1 and dev-2"), "{err}");
    assert_eq!(h.vms(), ["dev", "dev-1", "dev-2"]);
    assert_eq!(h.history(), before);
    assert_eq!(best::tries(&h.store, "dev").unwrap(), batch);
    // Gone by hand, they're dropped.
    h.ok(&["delete", "dev-1"]);
    h.ok(&["delete", "dev-2"]);
    assert_eq!(h.best_of(2, "again"), ["dev-1.1", "dev-2.1"]);
    let batch = best::tries(&h.store, "dev").unwrap().unwrap();
    assert_eq!(batch.prompt, "again");
}

// A try whose run can't start goes, with the ones after it, and the runs
// that started go on.
#[test]
fn tries_whose_runs_didn_t_start_are_deleted() {
    let h = host(false);
    // Its unit's state is one systemctl never says.
    h.sd.set_vm_state("dev-2", "bogus");
    let args = ["run", "dev", "--n", "3", "p", "--dir", h.work_dir()];
    let err = h.refused(&args);
    assert!(err.contains("dev-2's run didn't start"), "{err}");
    assert!(err.contains("dev-1.1 started"), "{err}");
    assert_eq!(h.vms(), ["dev", "dev-1"]);
    let batch = best::tries(&h.store, "dev").unwrap().unwrap();
    let tries: Vec<&str> = batch.tries.iter().map(|t| t.vm.as_str()).collect();
    assert_eq!(tries, ["dev-1"]);
    assert_eq!(h.got("dev-1", "prompt"), "p");
    h.go();
    h.ended("dev-1.1");
}

// A run as `runner drive` leaves it: what was asked, and once it's over, what
// the runner measured, then how it ended.
fn record_run(h: &Host, vm: &str, started: &str, end: Option<(Value, &RunResult)>) {
    let run = h.store.runs(vm).unwrap().join("1");
    fs::create_dir_all(&run).unwrap();
    let ask = json!({"prompt": "fix the tests", "dir": "/home/agent/work", "resume": null, "user": "agent", "boots": true, "started": started});
    fs::write(run.join("ask.json"), ask.to_string()).unwrap();
    fs::write(run.join("stream"), "").unwrap();
    if let Some((end, result)) = end {
        let result = serde_json::to_string(result).unwrap();
        fs::write(run.join("result.json"), result).unwrap();
        fs::write(run.join("end.json"), end.to_string()).unwrap();
    }
}

fn end(ended: &str, code: i32, outcome: &str) -> Value {
    json!({"ended": ended, "exit_code": code, "signal": null, "cancelled": false, "outcome": outcome, "error": null})
}

fn result(code: i32, ms: u64, outcome: Outcome, paths: &[&str], says: &str) -> RunResult {
    let paths = (paths.iter())
        .map(|p| Change {
            path: p.to_string(),
            change: Kind::Modified,
            contains: None,
        })
        .collect();
    RunResult {
        exit_code: Some(code),
        duration_ms: ms,
        outcome: Some(outcome),
        from_generation: Some(0),
        generation: Some(1),
        checkpoint: None,
        changed: Changed::Measured(Changes {
            paths,
            more: 0,
            boot_only: 4,
        }),
        agent_says: AgentSays::new(Some(says.to_string())),
    }
}

const FIXED: &str = "Fixed the flaky test.\nIt waited on the clock; now it doesn't.";

// dev's batch of three as best::start and `runner drive` leave it: dev-1.1
// exited 0 and committed two paths, dev-2.1 exited 1 and rolled back one, and
// dev-3.1, started two hours ago, goes on while the lock this returns is held.
fn three_tries(h: &Host) -> fs::File {
    for vm in ["dev-1", "dev-2", "dev-3"] {
        h.store.fork("dev", vm).unwrap();
    }
    let batch = best::Batch {
        prompt: "fix the tests".to_string(),
        dir: "/home/agent/work".to_string(),
        resume: None,
        started: "2026-11-06T09:00:00Z".parse().unwrap(),
        generation: 0,
        checkpoint: None,
        tries: (1..=3)
            .map(|i| best::Try {
                vm: format!("dev-{i}"),
                run: Some(format!("dev-{i}.1")),
            })
            .collect(),
    };
    let record = serde_json::to_string(&batch).unwrap();
    fs::write(h.store.tries("dev").unwrap(), record).unwrap();
    let paths = ["/home/agent/work/main.rs", "/home/agent/work/clock.rs"];
    let committed = result(0, 134_000, Outcome::Committed, &paths, FIXED);
    let said = "I couldn't make it pass: the fixture is missing.\nSorry.";
    let rolled_back = result(1, 63_000, Outcome::RolledBack, &paths[..1], said);
    let t0 = "2026-11-06T09:00:00Z";
    let committed_end = end("2026-11-06T09:02:14Z", 0, "committed");
    record_run(h, "dev-1", t0, Some((committed_end, &committed)));
    let rolled_back_end = end("2026-11-06T09:01:03Z", 1, "rolled_back");
    record_run(h, "dev-2", t0, Some((rolled_back_end, &rolled_back)));
    let ago = chrono::Utc::now() - chrono::Duration::seconds(2 * 3600 + 30);
    let ago = ago.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    record_run(h, "dev-3", &ago, None);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(h.store.runs("dev-3").unwrap().join("lock"))
        .unwrap();
    lock.lock().unwrap();
    lock
}

// dev-3.1 exits 0 and commits, as `runner drive` ends it.
fn finish_third(h: &Host, lock: fs::File) {
    let done = result(0, 7_300_000, Outcome::Committed, &[], "Done.");
    let run = h.store.runs("dev-3").unwrap().join("1");
    let result = serde_json::to_string(&done).unwrap();
    fs::write(run.join("result.json"), result).unwrap();
    let ended = end("2026-11-06T11:01:40Z", 0, "committed");
    fs::write(run.join("end.json"), ended.to_string()).unwrap();
    drop(lock);
}

// The row a table has for `id`.
fn row<'a>(table: &'a str, id: &str) -> &'a str {
    let row = table.lines().find(|l| l.starts_with(&format!("{id} ")));
    row.unwrap_or_else(|| panic!("no {id} in\n{table}"))
}

#[test]
fn tries_shows_a_row_per_try_side_by_side() {
    let h = host(false);
    let _lock = three_tries(&h);
    let table = h.ok(&["tries", "dev"]);
    let want = [
        "RUN      STATE                  EXIT  TOOK   CHANGED  AGENT SAYS (UNTRUSTED)",
        "dev-1.1  exited 0, committed    0     2m14s  2        \"Fixed the flaky test.\"",
        "dev-2.1  exited 1, rolled back  1     1m03s  1        \"I couldn't make it pass: the fixture is missing.\"",
        "dev-3.1  running                -     2h00m  -        -",
    ];
    assert_eq!(table, want.join("\n") + "\n");
}

#[test]
fn tries_json_has_each_try_s_run_and_full_result() {
    let h = host(false);
    let _lock = three_tries(&h);
    let out: Value = serde_json::from_str(&h.ok(&["tries", "dev", "--json"])).unwrap();
    assert_eq!(out["vm"], json!("dev"));
    assert_eq!(out["prompt"], json!("fix the tests"));
    assert_eq!(out["generation"], json!(0));
    let tries = out["tries"].as_array().unwrap();
    let ids: Vec<&str> = tries.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["dev-1.1", "dev-2.1", "dev-3.1"]);
    let one = &tries[0];
    assert_eq!(one["vm"], json!("dev-1"));
    assert_eq!(one["gone"], json!(false));
    assert_eq!(one["run"]["state"], json!("exited"));
    assert_eq!(one["run"]["outcome"], json!("committed"));
    assert_eq!(one["result"]["exit_code"], json!(0));
    let paths = &one["result"]["changed"]["paths"];
    let paths: Vec<&str> = (paths.as_array().unwrap().iter())
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        ["/home/agent/work/main.rs", "/home/agent/work/clock.rs"]
    );
    // The whole of what the agent says, kept apart.
    let says = &one["result"]["agent_says"];
    assert_eq!(says["untrusted"], json!(true));
    assert_eq!(says["text"], json!(FIXED));
    assert_eq!(tries[1]["result"]["outcome"], json!("rolled_back"));
    // Still going, so no result yet.
    assert_eq!(tries[2]["run"]["state"], json!("running"));
    assert!(tries[2].get("result").is_none(), "{}", tries[2]);
}

#[test]
fn tries_wait_returns_once_every_try_has_ended() {
    let h = host(false);
    let lock = three_tries(&h);
    let mut waiting = h
        .runner(&["tries", "dev", "--wait"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        waiting.try_wait().unwrap().is_none(),
        "returned with dev-3.1 running"
    );
    finish_third(&h, lock);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(waiting.wait_with_output().unwrap()));
    let out = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("--wait returned once dev-3.1 ended");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let table = text(&out.stdout);
    let third = row(&table, "dev-3.1");
    assert!(third.contains("  exited 0, committed  "), "{table}");
    assert!(third.ends_with("  \"Done.\""), "{table}");
    // With nothing going, it returns at once.
    let out = returns_at_once(h.runner(&["tries", "dev", "--wait", "--json"]));
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn a_try_deleted_by_hand_shows_as_gone() {
    let h = host(false);
    let _lock = three_tries(&h);
    h.ok(&["delete", "dev-2"]);
    let table = h.ok(&["tries", "dev"]);
    assert!(row(&table, "dev-2.1").contains("  gone  "), "{table}");
    assert!(
        row(&table, "dev-1.1").contains("  exited 0, committed  "),
        "{table}"
    );
    assert!(row(&table, "dev-3.1").contains("  running  "), "{table}");
    let out: Value = serde_json::from_str(&h.ok(&["tries", "dev", "--json"])).unwrap();
    let gone = &out["tries"][1];
    assert_eq!(gone["id"], json!("dev-2.1"));
    assert_eq!(gone["gone"], json!(true));
    assert!(gone.get("run").is_none(), "{gone}");
    assert_eq!(out["tries"][0]["gone"], json!(false));
}

#[test]
fn tries_on_a_vm_with_none_says_so() {
    let h = host(false);
    assert_eq!(h.ok(&["tries", "dev"]), "no tries yet\n");
    let out: Value = serde_json::from_str(&h.ok(&["tries", "dev", "--json"])).unwrap();
    assert_eq!(out, json!({"vm": "dev", "tries": []}));
    let err = h.refused(&["tries", "nope"]);
    assert!(err.contains("no VM named nope"), "{err}");
}

#[test]
fn mcp_lists_tries() {
    let h = host(false);
    let _lock = three_tries(&h);
    let mut s = mcp(&h);
    let got = s.call(2, "list_tries", json!({"name": "dev"}));
    let tries = got["structuredContent"]["tries"]
        .as_array()
        .unwrap()
        .clone();
    let ids: Vec<&str> = tries.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["dev-1.1", "dev-2.1", "dev-3.1"], "{got}");
    let states: Vec<&str> = (tries.iter())
        .map(|t| t["run"]["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["exited", "exited", "running"]);
    let codes: Vec<Value> = (tries.iter())
        .map(|t| t["result"]["exit_code"].clone())
        .collect();
    assert_eq!(codes, [json!(0), json!(1), Value::Null]);
    let paths = tries[0]["result"]["changed"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    assert_eq!(tries[0]["result"]["agent_says"]["untrusted"], json!(true));
    // A VM with no batch has no tries.
    let none = s.call(3, "list_tries", json!({"name": "dev-1"}));
    assert_eq!(none["structuredContent"]["tries"], json!([]), "{none}");
    let bad = s.call(4, "list_tries", json!({"name": "nope"}));
    assert_eq!(bad["isError"], json!(true), "{bad}");
}

#[test]
fn mcp_runs_best_of() {
    let h = host(false);
    let mut s = mcp(&h);
    let args = json!({"name": "dev", "prompt": "hi", "n": 3, "dir": h.work_dir(), "resume": "s-9"});
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
        let vm = run["vm"].as_str().unwrap();
        // --resume reaches every try.
        assert_eq!(h.got(vm, "argv"), format!("{ARGV}--resume\ns-9\n"), "{vm}");
        assert_eq!(h.got(vm, "prompt"), "hi");
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
