// A run's result: what the runner measured, kept apart from what the agent says.
use runner::changes::{self, Change, Changes, Kind};
use runner::result::{AgentSays, Changed, Outcome, RunResult};
use serde_json::json;

fn result(changed: Changed, agent_says: AgentSays) -> RunResult {
    RunResult {
        exit_code: Some(0),
        duration_ms: 81234,
        outcome: Some(Outcome::Committed),
        from_generation: Some(2),
        generation: Some(3),
        checkpoint: None,
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
        paths: vec![
            Change {
                path: "/home/agent/work/main.rs".to_string(),
                change: Kind::Modified,
                contains: None,
            },
            Change {
                path: "/home/agent/work/target".to_string(),
                change: Kind::Added,
                contains: Some(812),
            },
        ],
        more: 4,
    });
    let got = result(changed, says("Fixed the bug."));
    let want = json!({
        "exit_code": 0,
        "duration_ms": 81234,
        "outcome": "committed",
        "from_generation": 2,
        "generation": 3,
        "checkpoint": null,
        "changed": {
            "paths": [
                {"path": "/home/agent/work/main.rs", "change": "modified"},
                {"path": "/home/agent/work/target", "change": "added", "contains": 812}
            ],
            "more": 4
        },
        "agent_says": {"untrusted": true, "text": "Fixed the bug.", "truncated": false}
    });
    assert_eq!(serde_json::to_value(&got).unwrap(), want);
    let back: RunResult = serde_json::from_value(want.clone()).unwrap();
    assert_eq!(back, got);
    // As a run from before checkpoints wrote it.
    let mut old = want;
    old.as_object_mut().unwrap().remove("checkpoint");
    let back: RunResult = serde_json::from_value(old).unwrap();
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
    got.outcome = Some(Outcome::RolledBack);
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

// A run that joined a running VM: there's no disk from before it, only the
// checkpoint it took.
#[test]
fn a_joined_run_has_no_outcome_or_generations() {
    let mut got = result(
        Changed::Unmeasured {
            error: "joined dev running".to_string(),
        },
        says("Done."),
    );
    (got.outcome, got.from_generation, got.generation) = (None, None, None);
    got.checkpoint = Some(4);
    let want = json!({
        "exit_code": 0,
        "duration_ms": 81234,
        "outcome": null,
        "from_generation": null,
        "generation": null,
        "checkpoint": 4,
        "changed": {"error": "joined dev running"},
        "agent_says": {"untrusted": true, "text": "Done.", "truncated": false}
    });
    assert_eq!(serde_json::to_value(&got).unwrap(), want);
    let back: RunResult = serde_json::from_value(want).unwrap();
    assert_eq!(back, got);
}

#[test]
fn a_failed_measurement_becomes_unmeasured() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.ext4");
    let changed = Changed::from(changes::between(
        &missing,
        &missing,
        changes::CAP,
        changes::TIME,
    ));
    let Changed::Unmeasured { error } = changed else {
        panic!("measured a disk that isn't there: {changed:?}");
    };
    assert!(error.contains("missing.ext4"), "{error}");
}

// Lines as `claude -p --verbose --output-format stream-json` writes them: one
// content block per event, so a message with several spans events that share
// its id.
const STREAM: &str = r#"{"type":"system","subtype":"init","session_id":"s1","tools":["Bash"]}
{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Looking."}]},"parent_tool_use_id":null,"session_id":"s1"}
{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":null,"session_id":"s1"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"main.rs"}]},"parent_tool_use_id":null,"session_id":"s1"}
{"type":"assistant","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"Done."}]},"parent_tool_use_id":null,"session_id":"s1"}
{"type":"assistant","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"I fixed main.rs."}]},"parent_tool_use_id":null,"session_id":"s1"}
"#;

const LAST: &str = "Done.\n\nI fixed main.rs.";

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
    assert_eq!(got, says(LAST));
}

#[test]
fn a_subagent_does_not_speak_for_the_agent() {
    let sub = r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"Subagent here."}]},"parent_tool_use_id":"t2"}"#;
    let got = AgentSays::from_stream(format!("{STREAM}{sub}\n").as_bytes()).unwrap();
    assert_eq!(got, says(LAST));
}

// A line too long to hold, like a huge tool result, is skipped, not read in,
// even when it would parse.
#[test]
fn an_overlong_line_is_skipped() {
    let padding = " ".repeat(5 << 20);
    let end = r#"{"type":"result","result":"Hidden."}"#;
    let got = AgentSays::from_stream(format!("{STREAM}{padding}{end}\n").as_bytes()).unwrap();
    assert_eq!(got, says(LAST));
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

mod props {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // A log is the agent's output, so any bytes at all.
        #[test]
        fn any_stream_reads_without_error(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            prop_assert!(AgentSays::from_stream(&bytes[..]).is_ok());
        }

        #[test]
        fn the_text_kept_is_a_capped_prefix(text in ".{0,64}", copies in 0usize..1200) {
            let long = text.repeat(copies);
            let got = AgentSays::new(Some(long.clone()));
            let kept = got.text().unwrap();
            prop_assert!(kept.len() <= AgentSays::CAP);
            prop_assert!(long.starts_with(kept));
            let json = serde_json::to_value(&got).unwrap();
            prop_assert_eq!(json["truncated"].as_bool(), Some(kept.len() < long.len()));
        }
    }
}
