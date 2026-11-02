// A finished run's result, for local Claude Code to trust. Everything in it is
// what the runner measured except `agent_says`, the agent's own last word,
// which is kept apart and marked untrusted (docs/design.md, Guards).
use crate::changes::{self, Changes};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Read};

// Longer than any final message worth quoting; a huge tool result, say.
const MAX_LINE: u64 = 4 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunResult {
    /// The agent's exit code; null when it didn't exit on its own (cancelled,
    /// killed, or the VM went down).
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// What became of the disk generation a run that booted its VM ran. For
    /// a run that joined its VM running, not_rolled_back if it failed or was
    /// cancelled once Claude Code had started: the run left the VM running,
    /// its writes and all, and restoring `checkpoint` rolls it back. Else null
    /// for a joined run: it commits nothing. Null too for a run that never
    /// got as far as starting its VM.
    pub outcome: Option<Outcome>,
    /// The disk generation the run started from. Null when outcome is, and
    /// for a run that joined its VM.
    pub from_generation: Option<u64>,
    /// The generation the run wrote: current if committed, kept for a look if
    /// rolled back. Null when outcome is, or when the VM's guest never
    /// answered the run, and for a run that joined its VM.
    pub generation: Option<u64>,
    /// For a run that joined its VM running: the checkpoint it took first,
    /// which `restore` brings back. Null for a run that booted its VM.
    #[serde(default)]
    pub checkpoint: Option<u64>,
    /// The paths that differ between the two generations, read from the
    /// disks; or, when there was nothing to measure or measuring failed, why.
    pub changed: Changed,
    /// Untrusted: what the agent says it did. Check it against `changed`.
    pub agent_says: AgentSays,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Committed,
    RolledBack,
    NotRolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Changed {
    Measured(Changes),
    /// The run had no disk of its own to measure, or its disks couldn't be
    /// read, so the result claims nothing about them.
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
            t.truncate(t.floor_char_boundary(Self::CAP));
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
    /// assistant message. A subagent's messages, lines that aren't JSON, like a
    /// torn last line, and lines too long to hold are skipped.
    pub fn from_stream(mut stream: impl BufRead) -> io::Result<Self> {
        let mut result = None;
        // The last assistant message with text: its id, and its text so far.
        let mut message: Option<(Value, String)> = None;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut stream).take(MAX_LINE).read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if n as u64 == MAX_LINE && line.last() != Some(&b'\n') {
                stream.skip_until(b'\n')?;
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            // A subagent's events name the tool call that started it.
            if !event["parent_tool_use_id"].is_null() {
                continue;
            }
            match event["type"].as_str() {
                Some("result") => result = event["result"].as_str().map(str::to_string),
                // Each content block comes as its own event; the blocks of one
                // message share its id.
                Some("assistant") => {
                    let id = event["message"]["id"].clone();
                    let blocks = event["message"]["content"].as_array();
                    let texts = blocks
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str());
                    for text in texts {
                        match &mut message {
                            Some((last, so_far)) if *last == id && !id.is_null() => {
                                so_far.push_str("\n\n");
                                so_far.push_str(text);
                            }
                            _ => message = Some((id.clone(), text.to_string())),
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(AgentSays::new(result.or(message.map(|(_, text)| text))))
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}
