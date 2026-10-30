// A run's result: what the runner measured, kept apart from what the agent says.
use runner::changes::{self, Change, Changes, Kind};
use runner::result::{AgentSays, Changed, Outcome, RunResult};
use serde_json::json;

fn result(changed: Changed, agent_says: AgentSays) -> RunResult {
    RunResult {
        exit_code: Some(0),
        duration_ms: 81234,
        outcome: Outcome::Committed,
        from_generation: 2,
        generation: 3,
        changed,
        agent_says,
    }
}

fn says(text: &str) -> AgentSays {
    AgentSays::new(Some(text.to_string()))
}

#[test]
fn the_json_shape_is_pinned() {
    let changed = Changed::Measured(Changes {
        paths: vec![Change {
            path: "/home/agent/work/main.rs".to_string(),
            change: Kind::Modified,
        }],
        more: 4,
    });
    let got = result(changed, says("Fixed the bug."));
    let want = json!({
        "exit_code": 0,
        "duration_ms": 81234,
        "outcome": "committed",
        "from_generation": 2,
        "generation": 3,
        "changed": {
            "paths": [{"path": "/home/agent/work/main.rs", "change": "modified"}],
            "more": 4
        },
        "agent_says": {"untrusted": true, "text": "Fixed the bug.", "truncated": false}
    });
    assert_eq!(serde_json::to_value(&got).unwrap(), want);
    let back: RunResult = serde_json::from_value(want).unwrap();
    assert_eq!(back, got);
}

#[test]
fn a_run_that_did_not_exit_and_could_not_be_measured_says_so() {
    let mut got = result(
        Changed::Unmeasured {
            error: "gens/3.ext4: corrupt superblock".to_string(),
        },
        AgentSays::new(None),
    );
    got.exit_code = None;
    got.outcome = Outcome::RolledBack;
    let json = serde_json::to_value(&got).unwrap();
    assert_eq!(json["exit_code"], json!(null));
    assert_eq!(json["outcome"], "rolled_back");
    assert_eq!(
        json["changed"],
        json!({"error": "gens/3.ext4: corrupt superblock"})
    );
    assert_eq!(
        json["agent_says"],
        json!({"untrusted": true, "text": null, "truncated": false})
    );
    let back: RunResult = serde_json::from_value(json).unwrap();
    assert_eq!(back, got);
}

#[test]
fn a_failed_measurement_becomes_unmeasured() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.ext4");
    let changed = Changed::from(changes::between(&missing, &missing, changes::CAP));
    let Changed::Unmeasured { error } = changed else {
        panic!("measured a disk that isn't there: {changed:?}");
    };
    assert!(error.contains("missing.ext4"), "{error}");
}

// Lines as `claude -p --verbose --output-format stream-json` writes them.
const STREAM: &str = r#"{"type":"system","subtype":"init","session_id":"s1","tools":["Bash"]}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"main.rs"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done: "},{"type":"text","text":"I fixed main.rs."}]}}
"#;

#[test]
fn the_agent_says_its_final_result() {
    let stream = format!(
        "{STREAM}{}\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"All tests pass.","session_id":"s1"}"#
    );
    let got = AgentSays::from_stream(stream.as_bytes()).unwrap();
    assert_eq!(got, says("All tests pass."));
    assert_eq!(got.text(), Some("All tests pass."));
}

#[test]
fn without_a_result_the_agent_says_its_last_message() {
    // A cancelled run, cut off mid-line, with a line that isn't JSON.
    let stream = format!("not json\n{STREAM}{{\"type\":\"assist");
    let got = AgentSays::from_stream(stream.as_bytes()).unwrap();
    assert_eq!(got, says("Done: I fixed main.rs."));
}

#[test]
fn an_empty_stream_says_nothing() {
    assert_eq!(
        AgentSays::from_stream(&b""[..]).unwrap(),
        AgentSays::new(None)
    );
}

#[test]
fn a_long_message_is_cut_on_a_character_boundary() {
    let long = "€".repeat(AgentSays::CAP); // 3 bytes each
    let got = says(&long);
    let json = serde_json::to_value(&got).unwrap();
    let text = got.text().unwrap();
    assert!(text.len() <= AgentSays::CAP && text.len() > AgentSays::CAP - 3);
    assert!(long.starts_with(text));
    assert_eq!(json["truncated"], true);
}
