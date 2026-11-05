// `runner move <vm> <host>`: a VM, with its disks, my checkpoints, its
// settings and its allowlist, from this host to another, its export
// streamed into `runner import` there over ssh. Two roots on this machine
// stand in for the two hosts: this one, and kvm-host-2 behind common's fake
// ssh, which runs `runner peer` on its root as sshd does for a peer's key
// line. systemctl is faked on each.
mod common;

use chrono::Utc;
use common::{PEER_SSH, PeerHost, script, text};
use runner::creds::Cred;
use runner::store::{Checkpoint, Config, Maker, Store};
use runner::url::Setting;
use serde_json::json;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const TARGET: &str = "runner@kvm-host-2";
const MIB: u64 = 1 << 20;

struct Pair {
    dir: tempfile::TempDir,
    here: PeerHost,
    there: PeerHost,
}

impl Pair {
    fn new() -> Pair {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        script(&bin.join("ssh"), PEER_SSH);
        let here = PeerHost::new(dir.path().join("here"), &[], 2 << 30);
        let there = PeerHost::new(dir.path().join("fleet/kvm-host-2"), &[], 4 << 30);
        Pair { dir, here, there }
    }

    // `runner move <args>` on this host, as the runner user runs it.
    fn mv(&self, args: &[&str]) -> Output {
        let sys = std::env::var("PATH").unwrap();
        let bin = self.dir.path().join("bin");
        let path = format!("{}:{}", bin.display(), self.here.sd.path());
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", self.here.root())
            .env("PATH", path)
            .env("SYS_PATH", sys)
            .env("FLEET", self.dir.path().join("fleet"))
            .env("RUNNER", env!("CARGO_BIN_EXE_runner"))
            .arg("move")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn moved(&self, args: &[&str]) -> (String, String) {
        let out = self.mv(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        (text(&out.stdout), text(&out.stderr))
    }

    fn refused(&self, args: &[&str]) -> String {
        let out = self.mv(args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}: {}",
            text(&out.stdout)
        );
        text(&out.stderr)
    }

    fn ssh_log(&self) -> Vec<String> {
        let log = fs::read_to_string(self.dir.path().join("fleet/ssh-log")).unwrap_or_default();
        log.lines().map(String::from).collect()
    }
}

// A disk's bytes, holes as zeros: small enough to hold.
fn bytes(path: &PathBuf) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// dev, forked from a 64 MiB sparse disk with its own sizes, idle time, URL
// and allowlist, and two checkpoints of mine, each of a different disk,
// then a current disk unlike either. An agent's checkpoint, which an export
// leaves out, too.
fn dev(store: &Store, dir: &std::path::Path) -> (Checkpoint, Checkpoint) {
    let src = dir.join("img.ext4");
    let file = File::create(&src).unwrap();
    file.set_len(64 * MIB).unwrap();
    file.write_all_at(&[7; 4096], 0).unwrap();
    file.write_all_at(&[9; 4096], 40 * MIB).unwrap();
    store.import("img", &src).unwrap();
    let config = Config {
        cpus: Some(4),
        mem_mib: Some(4096),
        kernel: None,
        idle: Some("30m".parse().unwrap()),
        url: None,
    };
    store.fork_with("img", "dev", &config).unwrap();
    let url = Setting {
        off: false,
        port: Some(3000),
    };
    store.set_url("dev", url).unwrap();
    store
        .set_creds("dev", &[Cred::Github, Cred::Claude])
        .unwrap();
    let first = store.checkpoint("dev", Some("first")).unwrap();
    rewrite(store, 1);
    let second = store.checkpoint("dev", Some("tests\npass")).unwrap();
    rewrite(store, 2);
    store
        .checkpoint_by("dev", Some("the agent's"), Maker::Agent)
        .unwrap();
    (first, second)
}

// A run on dev that rewrites its first 4 KiB, committed.
fn rewrite(store: &Store, seed: u8) {
    let run = store.begin_run("dev").unwrap();
    let file = fs::OpenOptions::new().write(true).open(&run.disk).unwrap();
    file.write_all_at(&[seed; 4096], 0).unwrap();
    run.commit().unwrap();
}

fn current(store: &Store, vm: &str) -> PathBuf {
    store.disk(vm, store.get(vm).unwrap().generation).unwrap()
}

// My checkpoints, as `runner checkpoints` lists them.
fn mine(store: &Store, vm: &str) -> Vec<(u64, chrono::DateTime<Utc>, Option<String>)> {
    let all = store.checkpoints(vm).unwrap();
    let own = all.into_iter().filter(|c| !c.auto && !c.agent && !c.run);
    let mut got: Vec<_> = own.map(|c| (c.id, c.time, c.comment)).collect();
    got.sort();
    got
}

#[test]
fn a_stopped_vm_arrives_whole_and_is_gone_from_here() {
    let pair = Pair::new();
    let here = pair.here.store();
    let (first, second) = dev(&here, &pair.here.dir);
    let disk = bytes(&current(&here, "dev"));
    let one = bytes(&here.disk("dev", first.id).unwrap());
    let two = bytes(&here.disk("dev", second.id).unwrap());
    let config = here.config("dev").unwrap();
    let kept = mine(&here, "dev");

    let (out, err) = pair.moved(&["dev", TARGET]);
    assert!(!pair.here.has("dev"), "dev is still here");
    let there = pair.there.store();
    assert_eq!(bytes(&current(&there, "dev")), disk);
    assert_eq!(bytes(&there.disk("dev", first.id).unwrap()), one);
    assert_eq!(bytes(&there.disk("dev", second.id).unwrap()), two);
    assert_eq!(mine(&there, "dev"), kept);
    assert_eq!(there.config("dev").unwrap(), config);
    assert_eq!(there.creds("dev").unwrap(), [Cred::Claude, Cred::Github]);
    assert_eq!(pair.there.staged(), Vec::<String>::new());

    let vm = there.get("dev").unwrap();
    let line = format!(
        "dev  gen {}  {} bytes  stopped  on kvm-host-2\n",
        vm.generation, vm.bytes
    );
    assert_eq!(out, line, "{err}");
    assert!(
        err.contains("moved dev to kvm-host-2: its disk and 2 checkpoints"),
        "{err}"
    );
    // Stopped, it stays so.
    assert!(
        !pair
            .here
            .sd
            .calls()
            .iter()
            .any(|c| c.starts_with("systemctl stop"))
    );
    assert!(
        !pair
            .there
            .sd
            .calls()
            .iter()
            .any(|c| c.starts_with("systemctl start"))
    );

    // With this host's own key, to that host's `runner peer`.
    let import = pair.ssh_log();
    let import = import
        .iter()
        .find(|l| l.contains(" runner import "))
        .expect("an import over ssh");
    assert!(import.contains("BatchMode=yes"), "{import}");
    assert!(
        import.contains("IdentitiesOnly=yes -i ~/.ssh/id_ed25519"),
        "{import}"
    );
    assert!(
        import.ends_with(&format!(
            " {TARGET} runner import dev - --creds claude,github"
        )),
        "{import}"
    );
}

#[test]
fn a_vm_with_no_credentials_arrives_with_none() {
    let pair = Pair::new();
    let here = pair.here.store();
    here.fork("base", "bare").unwrap();
    here.set_creds("bare", &[]).unwrap();
    pair.moved(&["bare", TARGET]);
    assert_eq!(pair.there.store().creds("bare").unwrap(), []);
}

#[test]
fn a_running_vm_is_stopped_moved_and_started_there() {
    let pair = Pair::new();
    let here = pair.here.store();
    dev(&here, &pair.here.dir);
    pair.here.sd.set_state("active");

    let (out, err) = pair.moved(&["dev", TARGET]);
    assert!(!pair.here.has("dev"));
    assert!(pair.there.has("dev"));
    let stop = "systemctl stop firecracker@dev.service".to_string();
    assert!(pair.here.sd.calls().contains(&stop), "{err}");
    let start = "systemctl start firecracker@dev.service".to_string();
    assert!(pair.there.sd.calls().contains(&start), "{err}");
    assert!(out.starts_with("dev  gen "), "{out}");
    assert!(out.ends_with("  running  on kvm-host-2\n"), "{out}");
}

// The stream cut off a few blocks in: import there refuses it and leaves
// nothing, and dev is here as it was, up again.
#[test]
fn a_target_that_fails_mid_stream_leaves_the_vm_here_as_it_was() {
    for up in [false, true] {
        let pair = Pair::new();
        let here = pair.here.store();
        dev(&here, &pair.here.dir);
        if up {
            pair.here.sd.set_state("active");
        }
        let before = here.get("dev").unwrap();
        let kept = mine(&here, "dev");
        let disk = bytes(&current(&here, "dev"));
        fs::write(pair.there.dir.join("cut"), "20000").unwrap();

        let err = pair.refused(&["dev", TARGET]);
        assert!(err.contains("kvm-host-2"), "{err}");
        assert!(err.contains("dev is here as it was"), "{err}");
        assert!(!pair.there.has("dev"));
        assert_eq!(pair.there.staged(), Vec::<String>::new());
        assert_eq!(here.get("dev").unwrap().generation, before.generation);
        assert_eq!(mine(&here, "dev"), kept);
        assert_eq!(bytes(&current(&here, "dev")), disk);
        let calls = pair.here.sd.calls();
        let start = "systemctl start firecracker@dev.service".to_string();
        assert_eq!(calls.contains(&start), up, "{calls:?}");
        assert!(!pair.there.sd.calls().iter().any(|c| c.contains("start")));
    }
}

// The whole stream went and the import there took it, but ssh lost its
// answer: the move asks that host, finds dev there, and finishes.
#[test]
fn an_import_whose_answer_was_lost_is_asked_after() {
    let pair = Pair::new();
    let here = pair.here.store();
    dev(&here, &pair.here.dir);
    pair.here.sd.set_state("active");
    fs::write(pair.there.dir.join("cut"), "100000000000").unwrap();
    let (out, err) = pair.moved(&["dev", TARGET]);
    assert!(err.contains("Connection to kvm-host-2 closed"), "{err}");
    assert!(!pair.here.has("dev"));
    assert!(pair.there.has("dev"));
    assert!(out.ends_with("  running  on kvm-host-2\n"), "{out}");
}

// The whole stream went, but nothing there took it: dev is here as it was.
#[test]
fn a_stream_that_went_whole_but_wasn_t_taken_leaves_the_vm_here() {
    let pair = Pair::new();
    let here = pair.here.store();
    dev(&here, &pair.here.dir);
    pair.here.sd.set_state("active");
    fs::write(pair.there.dir.join("swallow"), "").unwrap();
    let err = pair.refused(&["dev", TARGET]);
    assert!(
        err.contains("dev is here as it was, and running again"),
        "{err}"
    );
    assert!(err.contains("kvm-host-2 doesn't have it"), "{err}");
    assert!(pair.here.has("dev"));
    assert!(!pair.there.has("dev"));
}

#[test]
fn a_run_going_refuses_it_before_anything_is_stopped_or_sent() {
    let pair = Pair::new();
    let here = pair.here.store();
    here.fork("base", "dev").unwrap();
    pair.here.sd.set_state("active");
    // What `runner drive` holds for as long as its run goes on.
    let runs = here.runs("dev").unwrap();
    fs::create_dir_all(runs.join("1")).unwrap();
    let lock = File::create(runs.join("lock")).unwrap();
    lock.lock().unwrap();
    let ask = json!({
        "prompt": "fix it",
        "dir": "/home/agent/work",
        "resume": null,
        "user": "agent",
        "boots": false,
        "started": Utc::now(),
    });
    fs::write(runs.join("1/ask.json"), ask.to_string()).unwrap();

    let err = pair.refused(&["dev", TARGET]);
    assert!(err.contains("run dev.1 is going in dev"), "{err}");
    assert!(pair.here.has("dev"));
    assert_eq!(pair.ssh_log(), Vec::<String>::new());
    assert!(!pair.here.sd.calls().iter().any(|c| c.contains("stop")));
}

// A host that has a dev, this host itself, or one that doesn't answer is
// refused before dev is stopped.
#[test]
fn a_target_that_has_the_vm_or_is_this_host_is_refused_first() {
    let pair = Pair::new();
    pair.here.store().fork("base", "dev").unwrap();
    pair.here.sd.set_state("active");
    pair.there.store().fork("base", "dev").unwrap();
    let err = pair.refused(&["dev", TARGET]);
    assert!(
        err.contains("kvm-host-2 has a VM named dev already"),
        "{err}"
    );

    let me = format!("runner@{}", runner::hosts::hostname());
    let err = pair.refused(&["dev", &me]);
    assert!(
        err.contains(&format!("dev is on {} already", runner::hosts::hostname())),
        "{err}"
    );

    pair.there.store().delete("dev").unwrap();
    fs::write(pair.there.dir.join("down"), "").unwrap();
    let err = pair.refused(&["dev", TARGET]);
    assert!(err.contains("kvm-host-2 didn't answer"), "{err}");
    assert!(err.contains("Connection timed out"), "{err}");

    assert!(pair.here.has("dev"));
    assert!(!pair.here.sd.calls().iter().any(|c| c.contains("stop")));
    assert!(!pair.there.has("dev"));
}

#[test]
fn a_vm_that_isn_t_here_is_refused() {
    let pair = Pair::new();
    let err = pair.refused(&["nope", TARGET]);
    assert!(err.contains("no VM named nope"), "{err}");
    assert_eq!(pair.ssh_log(), Vec::<String>::new());
}
