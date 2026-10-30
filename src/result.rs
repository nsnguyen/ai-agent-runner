// A finished run's result, for local Claude Code to trust. Everything in it is
// what the runner measured except `agent_says`, the agent's own last word,
// which is kept apart and marked untrusted (docs/design.md, Guards).
use crate::changes::{self, Changes};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunResult {
    /// The agent's exit code; null when it didn't exit on its own (cancelled,
    /// killed, or the VM went down).
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub outcome: Outcome,
    /// The disk generation the run started from.
    pub from_generation: u64,
    /// The generation the run wrote: current if committed, kept for a look if
    /// rolled back.
    pub generation: u64,
    /// The paths that differ between the two generations, read from the disks.
    pub changed: Changed,
    /// Untrusted: what the agent says it did. Check it against `changed`.
    pub agent_says: AgentSays,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Changed {
    Measured(Changes),
    /// The disks couldn't be read, so the result claims nothing about them.
    Unmeasured {
        error: String,
    },
}

impl From<changes::Result<Changes>> for Changed {
    fn from(measured: changes::Result<Changes>) -> Self {
        match measured {
            Ok(changes) => Changed::Measured(changes),
            Err(e) => Changed::Unmeasured {
                error: e.to_string(),
            },
        }
    }
}

/// The agent's final message, as it wrote it. Never a measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSays {
    /// Always true.
    untrusted: bool,
    text: Option<String>,
    /// The text was cut at `CAP` bytes.
    truncated: bool,
}

impl AgentSays {
    pub const CAP: usize = 32 * 1024;

    pub fn new(text: Option<String>) -> Self {
        let mut text = text;
        let mut truncated = false;
        if let Some(t) = text.as_mut().filter(|t| t.len() > Self::CAP) {
            let mut end = Self::CAP;
            while !t.is_char_boundary(end) {
                end -= 1;
            }
            t.truncate(end);
            truncated = true;
        }
        AgentSays {
            untrusted: true,
            text,
            truncated,
        }
    }

    /// From a run's `claude -p --output-format stream-json` log: the final
    /// `result` event's text, or for a run cut off before one, its last
    /// assistant message. Lines that aren't JSON, like a torn last line, are
    /// skipped.
    pub fn from_stream(stream: impl BufRead) -> io::Result<Self> {
        let (mut result, mut message) = (None, None);
        for line in stream.split(b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(&line?) else {
                continue;
            };
            match event["type"].as_str() {
                Some("result") => result = event["result"].as_str().map(str::to_string),
                Some("assistant") => {
                    let blocks = event["message"]["content"].as_array();
                    let text: String = blocks
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect();
                    if !text.is_empty() {
                        message = Some(text);
                    }
                }
                _ => {}
            }
        }
        Ok(AgentSays::new(result.or(message)))
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}
