// The guest's calls to its host (src/calls.rs), over Unix sockets in place of
// vsock: the host's side as `runner boot` serves it for its VM, and the
// guest's as `runner-guest checkpoint`, `checkpoints` and `restore` make
// them. A fake of Firecracker's API pauses and resumes, and runner-guest on a
// Unix socket, behind a fake of the VM's vsock socket, takes the checkpoint's
// sync. No KVM needed; tests/kvm.rs has the real thing.
mod common;

use common::{FakeApi, FakeVsock, Guest, Reply, Request, text, wait_for};
use runner::calls::{self, Answer, Call, Calls};
use runner::checkpoint::AGENT_PER_HOUR;
use runner::store::{Run, Store};
use runner::{proto, runs};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const LIVE: &[u8] = b"written while running";

// dev, imported from a disk holding "at import" and running generation 1,
// which its guest has since written LIVE to, and other, a fork of dev that's
// stopped. dev's boot serves its guest's calls, as `runner boot` does.
struct Host {
    dir: TempDir,
    store: Store,
    api: FakeApi,
    vsock: FakeVsock,
    calls: Calls,
    sock: PathBuf,
    _run: Run,
    _guest: Guest,
}

fn host() -> Host {
    host_with(|_| (204, None))
}

// With Firecracker's API answering as `respond` says.
fn host_with(respond: impl Fn(&Request) -> Reply + Send + 'static) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let disk = dir.path().join("rootfs.ext4");
    fs::write(&disk, b"at import").unwrap();
    store.import("dev", &disk).unwrap();
    store.fork("dev", "other").unwrap();
    let guest = Guest::spawn(&dir.path().join("guest.sock"));
    let vsock = FakeVsock::serve(&dir.path().join("vms/dev/vsock.sock"), &guest.sock);
    let run = store.begin_run("dev").unwrap();
    fs::write(&run.disk, LIVE).unwrap();
    let api = FakeApi::serve(&dir.path().join("vms/dev/api.sock"), respond);
    let sock = run.calls_socket();
    let calls = Calls::start(&store, "dev", &sock).unwrap();
    Host {
        dir,
        store,
        api,
        vsock,
        calls,
        sock,
        _run: run,
        _guest: guest,
    }
}

impl Host {
    /// runner-guest, as the agent in the VM runs it.
    fn guest(&self, args: &[&str]) -> Output {
        guest_on(&self.sock, args)
    }

    fn ask(&self, call: &Call) -> Answer {
        let mut conn = UnixStream::connect(&self.sock).unwrap();
        calls::ask(&mut conn, call).unwrap()
    }

    fn ids(&self, vm: &str) -> Vec<u64> {
        let listed = self.store.checkpoints(vm).unwrap();
        listed.iter().map(|c| c.id).collect()
    }

    fn disk(&self, generation: u64) -> Vec<u8> {
        let path = format!("vms/dev/gens/{generation}.ext4");
        fs::read(self.dir.path().join(path)).unwrap()
    }

    /// What was asked of Firecracker's API, by body.
    fn api_bodies(&self) -> Vec<Value> {
        self.api.calls().into_iter().map(|(_, body)| body).collect()
    }

    fn ctrl_alt_dels(&self) -> usize {
        let cad = json!({"action_type": "SendCtrlAltDel"});
        self.api_bodies().iter().filter(|b| **b == cad).count()
    }
}

fn guest_on(sock: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runner-guest"))
        .arg("--host-unix")
        .arg(sock)
        .args(args)
        .output()
        .unwrap()
}

fn refused(answer: &Answer, what: &str) {
    assert!(
        matches!(answer, Answer::Refused(why) if why.contains(what)),
        "wanted a refusal mentioning {what:?}: {answer:?}"
    );
}

#[test]
fn a_guest_s_checkpoint_is_of_its_own_running_vm_and_no_other() {
    let h = host();
    let others = h.store.checkpoints("other").unwrap();
    let out = h.guest(&["checkpoint", "-m", "before the migration"]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let made = &h.store.checkpoints("dev").unwrap()[0];
    let what = (made.id, made.agent, made.auto, made.comment.as_deref());
    assert_eq!(what, (2, true, false, Some("before the migration")));
    // It says what it made as `runner checkpoints` lists it.
    assert_eq!(text(&out.stdout), format!("{}\n", made.row()));
    assert!(made.row().contains("  agent  "), "{}", made.row());
    // The disk the boot runs, synced in the guest, copied while paused.
    assert_eq!(h.disk(2), LIVE);
    assert_eq!(h.vsock.connects(), [format!("CONNECT {}", proto::PORT)]);
    assert_eq!(
        h.api_bodies(),
        [json!({"state": "Paused"}), json!({"state": "Resumed"})]
    );
    // Nothing the guest can say names another VM.
    assert_eq!(h.store.checkpoints("other").unwrap(), others);
}

#[test]
fn a_guest_lists_its_vm_s_checkpoints_as_the_host_does() {
    let h = host();
    let made = h.ask(&Call::Checkpoint { comment: None });
    assert!(matches!(made, Answer::Made(_)), "{made:?}");
    let out = h.guest(&["checkpoints"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let rows: String = h
        .store
        .checkpoints("dev")
        .unwrap()
        .iter()
        .map(|c| c.row() + "\n")
        .collect();
    assert_eq!(text(&out.stdout), rows);
    let on_host = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", h.dir.path())
        .args(["checkpoints", "dev"])
        .output()
        .unwrap();
    assert_eq!(text(&on_host.stdout), rows);
    assert_eq!(h.ids("dev"), [2, 0]);
}

#[test]
fn the_calls_socket_is_the_runner_user_s_alone() {
    let h = host();
    let mode = fs::metadata(&h.sock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        h.sock.file_name().unwrap().to_str().unwrap(),
        format!("vsock.sock_{}", calls::PORT)
    );
}

#[test]
fn an_unknown_call_is_refused_and_does_nothing() {
    let h = host();
    let mut conn = UnixStream::connect(&h.sock).unwrap();
    proto::hello_with(&mut conn, calls::MAGIC, calls::VERSION).unwrap();
    proto::check_hello_with(&mut conn, calls::MAGIC, calls::VERSION).unwrap();
    // A frame of one byte: a call numbered past every one this host knows.
    conn.write_all(&[0, 0, 0, 1, 99]).unwrap();
    let answer: Answer = proto::read_frame(&mut conn).unwrap().unwrap();
    refused(&answer, "doesn't know");

    // Nor is a peer that isn't making a call answered at all, like a
    // runner-guest that dialled the wrong port.
    let mut conn = UnixStream::connect(&h.sock).unwrap();
    proto::hello(&mut conn).unwrap();
    proto::check_hello_with(&mut conn, calls::MAGIC, calls::VERSION).unwrap();
    assert!(!matches!(
        proto::read_frame::<Answer>(&mut conn),
        Ok(Some(_))
    ));

    // A comment longer than any note needs is refused before anything starts.
    let long = "x".repeat(calls::MAX_COMMENT + 1);
    refused(
        &h.ask(&Call::Checkpoint {
            comment: Some(long),
        }),
        "comment",
    );
    assert_eq!(h.ids("dev"), [0]);
    assert!(h.api_bodies().is_empty());
}

#[test]
fn an_agent_s_checkpoints_are_capped_per_hour_and_the_host_s_are_not() {
    let h = host();
    for _ in 0..AGENT_PER_HOUR {
        let made = h.ask(&Call::Checkpoint { comment: None });
        assert!(matches!(made, Answer::Made(_)), "{made:?}");
    }
    let out = h.guest(&["checkpoint", "-m", "one too many"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let err = text(&out.stderr);
    assert!(err.contains(&format!("{AGENT_PER_HOUR} an hour")), "{err}");
    // Refused before the guest was synced or the VM paused again.
    assert_eq!(h.vsock.connects().len(), AGENT_PER_HOUR);
    let agent = |h: &Host| {
        let listed = h.store.checkpoints("dev").unwrap();
        listed.iter().filter(|c| c.agent).count()
    };
    assert_eq!(agent(&h), AGENT_PER_HOUR);

    // A checkpoint the host makes isn't the agent's: it isn't capped, and
    // doesn't make room for another.
    let freezer = h.store.hold_checkpoints("dev").unwrap();
    let host_made = freezer.freeze(Some("from the host")).unwrap();
    drop(freezer);
    assert!(!host_made.agent);
    refused(&h.ask(&Call::Checkpoint { comment: None }), "an hour");
    assert_eq!(agent(&h), AGENT_PER_HOUR);
}

#[test]
fn a_guest_s_restore_is_staged_then_its_vm_rebooted() {
    let h = host();
    let Answer::Made(clean) = h.ask(&Call::Checkpoint {
        comment: Some("clean".into()),
    }) else {
        panic!("no checkpoint")
    };
    let id = clean.id.to_string();
    let out = h.guest(&["restore", &id]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("reboots this VM"), "{err}");
    let said = text(&out.stdout);
    assert!(
        said.contains(&format!("restoring checkpoint {id}")),
        "{said}"
    );
    // Rebooted the way `runner stop` stops it, once the guest has heard.
    wait_for("Ctrl-Alt-Del", || h.ctrl_alt_dels() == 1);

    // One restore at a time.
    refused(&h.ask(&Call::Restore { id: clean.id }), "already");
    let restore = h.calls.close().expect("a staged restore");
    assert_eq!(restore.staged.from, clean.id);
    assert_eq!(h.disk(restore.staged.generation), LIVE);
    // Not current yet: boot puts it on history once the VM is down.
    assert_eq!(h.store.get("dev").unwrap().generation, 0);
    // No run starts on the VM meanwhile, since the reboot would end it.
    assert!(runs::hold(&h.store, "dev").is_err());
    drop(restore);
    assert!(runs::hold(&h.store, "dev").is_ok());
    assert_eq!(h.ctrl_alt_dels(), 1);
}

#[test]
fn a_guest_s_restore_is_refused_while_a_run_is_going() {
    let h = host();
    let run = runs::hold(&h.store, "dev").unwrap();
    let answer = h.ask(&Call::Restore { id: 0 });
    refused(&answer, "a restore would end it");
    // Said to the agent, who can't run the host's commands.
    assert!(!format!("{answer:?}").contains("runner "), "{answer:?}");
    drop(run);
    assert!(h.calls.close().is_none());
    assert_eq!(h.ctrl_alt_dels(), 0);
    assert_eq!(h.ids("dev"), [0]);
}

#[test]
fn restoring_a_missing_checkpoint_is_refused_and_reboots_nothing() {
    let h = host();
    let out = h.guest(&["restore", "99"]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("dev has no checkpoint 99"), "{err}");
    assert!(h.calls.close().is_none());
    assert_eq!(h.ctrl_alt_dels(), 0);
}

#[test]
fn a_restore_asked_once_the_vm_is_going_down_is_refused() {
    let h = host();
    assert!(h.calls.close().is_none());
    refused(&h.ask(&Call::Restore { id: 0 }), "shutting down");
    assert_eq!(h.ctrl_alt_dels(), 0);
    assert_eq!(h.ids("dev"), [0]);
}

#[test]
fn runner_guest_says_when_it_can_t_reach_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let nowhere = dir.path().join("nothing.sock");
    let out = guest_on(&nowhere, &["checkpoint"]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("can't reach the runner host"), "{err}");
    // A restore says what it would do before it asks.
    let out = guest_on(&nowhere, &["restore", "1"]);
    let err = text(&out.stderr);
    let mut lines = err.lines();
    assert!(lines.next().unwrap().contains("reboots this VM"), "{err}");
    assert!(lines.next().unwrap().contains("can't reach"), "{err}");
}

// Firecracker already gone when the restore's Ctrl-Alt-Del goes, as when
// `runner stop` got there first: the VM is down anyway, and boot finishes
// the restore.
#[test]
fn a_restore_whose_vm_is_already_down_is_left_for_boot() {
    let h = host();
    fs::remove_file(h.dir.path().join("vms/dev/api.sock")).unwrap();
    let answer = h.ask(&Call::Restore { id: 0 });
    assert!(matches!(answer, Answer::Restoring { .. }), "{answer:?}");
    // Calls take turns, so this one is answered once the restore's is done.
    assert!(matches!(h.ask(&Call::Checkpoints), Answer::Listed(_)));
    let restore = h.calls.close().expect("the restore was dropped");
    assert!(
        h.dir
            .path()
            .join(format!("vms/dev/gens/{}.ext4", restore.staged.generation))
            .exists()
    );
}

// One Firecracker won't shut down for is dropped: the VM carries on as it was.
#[test]
fn a_restore_firecracker_won_t_reboot_for_is_dropped() {
    let h = host_with(|req| {
        if req.body == json!({"action_type": "SendCtrlAltDel"}) {
            return (400, Some(json!({"fault_message": "no"}).to_string()));
        }
        (204, None)
    });
    let answer = h.ask(&Call::Restore { id: 0 });
    let Answer::Restoring { generation, .. } = answer else {
        panic!("{answer:?}")
    };
    assert!(matches!(h.ask(&Call::Checkpoints), Answer::Listed(_)));
    assert!(h.calls.close().is_none());
    assert!(
        !h.dir
            .path()
            .join(format!("vms/dev/gens/{generation}.ext4"))
            .exists()
    );
    assert_eq!(h.ids("dev"), [0]);
}

// Postcard goes by position, and each VM's runner-guest is on its own disk,
// so a host and the guests of older disks have to keep agreeing: calls and
// answers are only ever added at the end, and what's here keeps its bytes.
#[test]
fn the_wire_keeps_its_shape() {
    let bytes = |v: &dyn erased::Encode| v.encode();
    assert_eq!(
        bytes(&Call::Checkpoint {
            comment: Some("x".into())
        }),
        [0, 1, 1, b'x']
    );
    assert_eq!(bytes(&Call::Checkpoint { comment: None }), [0, 0]);
    assert_eq!(bytes(&Call::Checkpoints), [1]);
    assert_eq!(bytes(&Call::Restore { id: 300 }), [2, 0xac, 0x02]);
    let saved = calls::Saved {
        id: 2,
        time: "2026-11-01T00:00:00Z".parse().unwrap(),
        comment: Some("c".into()),
        auto: false,
        agent: true,
        bytes: 5,
        current: true,
    };
    let mut made = vec![0, 2, 20];
    made.extend(b"2026-11-01T00:00:00Z");
    made.extend([1, 1, b'c', 0, 1, 5, 1]);
    assert_eq!(bytes(&Answer::Made(saved.clone())), made);
    let mut listed = vec![1, 1];
    listed.extend(&made[1..]);
    assert_eq!(bytes(&Answer::Listed(vec![saved])), listed);
    assert_eq!(
        bytes(&Answer::Restoring {
            id: 3,
            generation: 9
        }),
        [2, 3, 9]
    );
    assert_eq!(bytes(&Answer::Refused("no".into())), [3, 2, b'n', b'o']);
}

mod erased {
    pub trait Encode {
        fn encode(&self) -> Vec<u8>;
    }
    impl<T: serde::Serialize> Encode for T {
        fn encode(&self) -> Vec<u8> {
            postcard::to_allocvec(self).unwrap()
        }
    }
}

// More than a frame holds, from a long-lived VM or one huge comment the
// owner gave: the newest that fit, each comment cut to what a call takes.
#[test]
fn a_list_too_long_for_a_frame_keeps_the_newest_that_fit() {
    let h = host();
    let gens = h.dir.path().join("vms/dev/gens");
    let note = |comment: &str| {
        json!({"time": "2026-11-01T00:00:00Z", "comment": comment, "auto": false}).to_string()
    };
    let total = 1200;
    for id in 2..2 + total {
        fs::write(gens.join(format!("{id}.ext4")), b"").unwrap();
        fs::write(gens.join(format!("{id}.json")), note(&"c".repeat(1000))).unwrap();
    }
    let newest = 2 + total;
    fs::write(gens.join(format!("{newest}.ext4")), b"").unwrap();
    let huge = "h".repeat(2 << 20);
    fs::write(gens.join(format!("{newest}.json")), note(&huge)).unwrap();

    let Answer::Listed(listed) = h.ask(&Call::Checkpoints) else {
        panic!("not listed")
    };
    assert_eq!(listed[0].id, newest);
    assert_eq!(
        listed[0].comment.as_ref().unwrap().len(),
        calls::MAX_COMMENT
    );
    assert!(
        listed.len() > 100 && listed.len() < total as usize,
        "{}",
        listed.len()
    );
    let ids: Vec<u64> = listed.iter().map(|c| c.id).collect();
    let want: Vec<u64> = (0..listed.len() as u64).map(|i| newest - i).collect();
    assert_eq!(ids, want);
    let out = h.guest(&["checkpoints"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

// A Ctrl-Alt-Del that may have landed, Firecracker having hung up instead
// of answering: the VM may go down, so the restore waits for boot, which
// drops it at a shutdown that isn't clean.
#[test]
fn a_restore_whose_ctrl_alt_del_may_have_landed_is_kept() {
    let h = host();
    let api = h.dir.path().join("vms/dev/api.sock");
    fs::remove_file(&api).unwrap();
    let mute = std::os::unix::net::UnixListener::bind(&api).unwrap();
    std::thread::spawn(move || {
        for conn in mute.incoming() {
            drop(conn);
        }
    });
    let answer = h.ask(&Call::Restore { id: 0 });
    assert!(matches!(answer, Answer::Restoring { .. }), "{answer:?}");
    assert!(matches!(h.ask(&Call::Checkpoints), Answer::Listed(_)));
    assert!(h.calls.close().is_some(), "the restore was dropped");
}

// A guest that takes its answer but never hangs up, still writing, gets
// its reboot anyway.
#[test]
fn a_guest_that_never_hangs_up_is_rebooted_anyway() {
    let h = host();
    let mut conn = UnixStream::connect(&h.sock).unwrap();
    proto::hello_with(&mut conn, calls::MAGIC, calls::VERSION).unwrap();
    proto::check_hello_with(&mut conn, calls::MAGIC, calls::VERSION).unwrap();
    proto::write_frame(&mut conn, &Call::Restore { id: 0 }).unwrap();
    let answer: Answer = proto::read_frame(&mut conn).unwrap().unwrap();
    assert!(matches!(answer, Answer::Restoring { .. }), "{answer:?}");
    let chatty = std::thread::spawn(move || {
        for _ in 0..100 {
            if conn.write_all(b".").is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    wait_for("Ctrl-Alt-Del", || h.ctrl_alt_dels() == 1);
    drop(chatty);
}
