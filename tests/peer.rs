// `runner peer`, the forced command of another host's key in this host's
// ~runner/.ssh/authorized_keys: sshd runs it whatever that host asked for,
// and leaves what it asked for in SSH_ORIGINAL_COMMAND. Only `runner mcp`
// and `runner import <vm> -` run; anything else is refused before anything
// runs. No ssh here: the tests set SSH_ORIGINAL_COMMAND as sshd would.
mod common;

use common::{script, text};
use proptest::prelude::*;
use runner::creds::Cred;
use runner::peers;
use runner::store::Store;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Output, Stdio};

// What runner peer says when it refuses.
const ONLY: &str = "runs only `runner mcp` and `runner import <vm> -`";

// Programs a refused command would run if anything ran it, each leaving a
// mark when it does. systemctl answers a VM's state, stopped, unmarked: a
// read only, which import --replace asks first; starting one leaves a mark.
const TRAPS: [&str; 6] = ["bash", "sh", "id", "systemctl", "ssh", "runner"];
const SYSTEMCTL: &str = "[ \"$1\" != show ] || { echo inactive; exit 0; }\n";

struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    // With a VM x already, so a command naming it would find it.
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for trap in TRAPS {
            let first = if trap == "systemctl" { SYSTEMCTL } else { "" };
            script(
                &bin.join(trap),
                &format!("{first}touch \"$MARKS/{trap}\"\n"),
            );
        }
        fs::create_dir(dir.path().join("marks")).unwrap();
        let host = Host { dir };
        let disk = host.dir.path().join("x.ext4");
        fs::write(&disk, b"x's disk").unwrap();
        host.store().import("x", &disk).unwrap();
        host
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    fn store(&self) -> Store {
        Store::new(self.root())
    }

    fn vms(&self) -> Vec<String> {
        self.store()
            .list()
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect()
    }

    fn marks(&self) -> Vec<String> {
        let marks = fs::read_dir(self.dir.path().join("marks")).unwrap();
        marks
            .map(|m| m.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    // `runner peer` as sshd starts it for a peer's key, with the traps first
    // on PATH.
    fn peer(&self, original: Option<&OsStr>) -> Command {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_runner"));
        cmd.arg("peer")
            .env("RUNNER_ROOT", self.root())
            .env("PATH", path)
            .env("MARKS", self.dir.path().join("marks"))
            .env_remove(peers::ORIGINAL);
        if let Some(original) = original {
            cmd.env(peers::ORIGINAL, original);
        }
        cmd
    }

    fn run(&self, original: &str, stdin: &[u8]) -> Output {
        let mut child = self
            .peer(Some(OsStr::new(original)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take().unwrap();
        // A refused command reads none of it, so the pipe may close first.
        let _ = pipe.write_all(stdin);
        drop(pipe);
        child.wait_with_output().unwrap()
    }
}

#[track_caller]
fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "{:?}: {}",
        out.status,
        text(&out.stderr)
    );
}

#[test]
fn import_of_a_stream_on_stdin_runs() {
    let host = Host::new();
    let out = host.run("runner import y -", b"y's disk");
    ok(&out);
    assert_eq!(text(&out.stdout), "y  gen 0  8 bytes\n");
    let y = host.store().get("y").unwrap();
    let disk = host.store().disk("y", y.generation).unwrap();
    assert_eq!(fs::read(disk).unwrap(), b"y's disk");

    // With import's own flags.
    let out = host.run("runner import z - --creds claude,github", b"z's disk");
    ok(&out);
    assert_eq!(
        host.store().creds("z").unwrap(),
        [Cred::Claude, Cred::Github]
    );
    let out = host.run("runner import --creds=github w -", b"w's disk");
    ok(&out);
    assert_eq!(host.store().creds("w").unwrap(), [Cred::Github]);
    let out = host.run("runner import --replace x -", b"x's new disk");
    ok(&out);
    assert_eq!(text(&out.stdout), "x  gen 1  12 bytes\n");
    assert!(host.marks().is_empty(), "{:?}", host.marks());
}

#[test]
fn mcp_runs_on_stdin_and_stdout() {
    let host = Host::new();
    for original in [
        "runner mcp",
        "runner mcp --user root",
        "runner mcp --user=agent",
    ] {
        let mut child = host
            .peer(Some(OsStr::new(original)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let send = |stdin: &mut ChildStdin, msg: Value| writeln!(stdin, "{msg}").unwrap();
        let client = json!({"name": "test", "version": "0"});
        let params =
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params});
        send(&mut stdin, init);
        let init: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], json!("runner"));
        send(
            &mut stdin,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        let call = json!({"name": "list_vms", "arguments": {}});
        send(
            &mut stdin,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": call}),
        );
        let listed: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        let vms = &listed["result"]["structuredContent"]["vms"];
        assert_eq!(vms[0]["name"], json!("x"), "{original}: {listed}");
        drop(stdin);
        assert!(child.wait().unwrap().success(), "{original}");
    }
    assert!(host.marks().is_empty(), "{:?}", host.marks());
}

#[test]
fn anything_else_is_refused_without_running_it() {
    let cases = [
        "",
        " ",
        "bash",
        "sh -c id",
        "id",
        "runner",
        "runner exec x -- id",
        "runner shell x",
        "runner list",
        "runner delete x",
        "runner peer",
        "runner mcp; id",
        "runner mcp;id",
        "runner mcp && id",
        "runner mcp || id",
        "runner mcp | id",
        "runner mcp & id",
        "runner mcp\nid",
        "runner mcp\rid",
        "runner mcp\tid",
        "runner mcp $(id)",
        "runner mcp `id`",
        "runner mcp > /tmp/x",
        "runner mcp --root /tmp",
        "runner mcp --help",
        "runner mcp --user",
        "runner mcp --user=",
        "runner mcp --user --help",
        "runner mcp extra",
        // A peer's own MCP forwards nothing, so it takes no peers.
        "runner mcp --peer runner@kvm-host-2",
        "runner mcp --peer=runner@kvm-host-2",
        "runner mcp --user agent --peer runner@kvm-host-2",
        "/usr/local/bin/runner mcp",
        "RUNNER_ROOT=/tmp runner mcp",
        "runner --root /tmp import y -",
        "runner --keep 0 import y -",
        "runner import --keep 0 y -",
        "runner import --keep=0 y -",
        "runner import y - --root=/tmp",
        "runner import y",
        "runner import -",
        "runner import - -",
        "runner import y /etc/passwd",
        "runner import y ~/disk",
        "runner import y - extra",
        "runner import y -- -",
        "runner import y - --creds",
        "runner import y - --creds --replace",
        "runner import y - --creds=",
        "runner import y -; id",
        "runner import y - <(id)",
        "runner import y '-'",
        "runner import \"y\" -",
        "runner import y\\ -",
        "runner import y -h -",
        "runner import y - --help",
        "runner import y - --version",
        "runner Import y -",
        "runner MCP",
    ];
    let host = Host::new();
    for original in cases {
        let out = host.run(original, b"a disk");
        assert_eq!(out.status.code(), Some(1), "{original:?}");
        assert_eq!(text(&out.stdout), "", "{original:?}");
        let err = text(&out.stderr);
        assert!(err.contains(ONLY), "{original:?}: {err}");
        assert!(host.marks().is_empty(), "{original:?}: {:?}", host.marks());
        assert_eq!(host.vms(), ["x"], "{original:?}");
    }
    // An interactive login sets no SSH_ORIGINAL_COMMAND, and one that isn't
    // UTF-8 is refused too.
    for original in [None, Some(OsStr::from_bytes(b"runner mcp \xff"))] {
        let out = host.peer(original).stdin(Stdio::null()).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{original:?}");
        assert!(text(&out.stderr).contains(ONLY), "{original:?}");
        assert!(host.marks().is_empty(), "{original:?}");
    }
}

// What clap refuses of what vet lets through, like a credential runner
// doesn't know, is refused as clap refuses it, still running nothing.
#[test]
fn clap_still_parses_what_runs() {
    let host = Host::new();
    let out = host.run("runner import y - --creds nope", b"a disk");
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("nope"), "{}", text(&out.stderr));
    let out = host.run("runner import --replace y - --creds claude", b"a disk");
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(host.vms(), ["x"]);
    assert!(host.marks().is_empty(), "{:?}", host.marks());
}

#[test]
fn the_key_line_options_are_the_tailnet_s_and_runner_peer() {
    assert_eq!(
        peers::OPTIONS,
        r#"from="100.64.0.0/10,fd7a:115c:a1e0::/48",restrict,command="/usr/local/bin/runner peer""#
    );
}

fn vm_name() -> impl Strategy<Value = String> {
    "[a-z0-9][a-z0-9-]{0,20}"
}

fn import_flags() -> impl Strategy<Value = Vec<String>> {
    let flag = prop_oneof![
        Just("--replace".to_string()),
        "(claude|github)(,(claude|github))?".prop_map(|c| format!("--creds {c}")),
        "(claude|github)".prop_map(|c| format!("--creds={c}")),
    ];
    prop::collection::vec(flag, 0..3)
}

proptest! {
    // Whatever order import's words come in, they run as sent.
    #[test]
    fn import_s_words_pass_as_they_came(vm in vm_name(), flags in import_flags(), at in 0usize..4) {
        let mut words: Vec<String> = vec![vm, "-".to_string()];
        let at = at.min(words.len());
        let flags: Vec<String> = flags.iter().flat_map(|f| f.split(' ').map(String::from)).collect();
        words.splice(at..at, flags);
        let original = format!("runner import {}", words.join(" "));
        let vetted = peers::vet(Some(OsStr::new(&original))).unwrap();
        prop_assert_eq!(vetted.join(" "), original);
    }

    // Nothing a shell would read gets through, wherever it is.
    #[test]
    fn a_character_a_shell_reads_is_refused(
        vm in vm_name(),
        bad in "[;&|<>$`'\"\\\\*?(){}\\[\\]!#~\n\t\r\0]",
        at in 0usize..40,
    ) {
        let original = format!("runner import {vm} -");
        let at = at.min(original.len());
        let original = format!("{}{bad}{}", &original[..at], &original[at..]);
        prop_assert!(peers::vet(Some(OsStr::new(&original))).is_err(), "{:?}", original);
    }
}
