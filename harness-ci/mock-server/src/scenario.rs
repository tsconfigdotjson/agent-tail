//! The scenario file: what requests the mock expects, and what it replies with.
//!
//! A scenario is deliberately small. Each step names the next request the CLI should make
//! (optionally checking its contents) and the content blocks the model "returns". The
//! server translates those blocks into the wire format of whichever API the request came
//! from: Anthropic Messages or OpenAI Responses.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// A whole run: the turns the model plays, in order.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub steps: Vec<Step>,
}

/// One model turn.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Step {
    /// What the incoming request must look like. Empty matches anything.
    #[serde(default)]
    pub expect: Expect,
    pub reply: Reply,
}

/// A request check. Every set field must hold; the first that doesn't is the error.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Expect {
    /// A substring of all text in the conversation (user, assistant and tool results).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// A substring of the user's text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_contains: Option<String>,
    /// Requires at least one tool result with this call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_id: Option<String>,
    /// Requires a tool result containing this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_contains: Option<String>,
    /// Requires tool results to be present (`true`) or absent (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_tool_result: Option<bool>,
    /// The zero-based request number this reply is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_index: Option<usize>,
}

/// A model turn's content, in the Anthropic vocabulary. Translated for OpenAI responses.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub content: Vec<Block>,
    /// `end_turn`, `tool_use`, `max_tokens`, … Defaults from the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    /// Break the turn on purpose. The request is still consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<Fault>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        /// Defaults to a deterministic id based on the request and block index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
        input: Value,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default = "default_input_tokens")]
    pub input_tokens: u64,
    #[serde(default = "default_output_tokens")]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

fn default_input_tokens() -> u64 {
    10
}

fn default_output_tokens() -> u64 {
    5
}

impl Default for Usage {
    fn default() -> Self {
        Usage {
            input_tokens: default_input_tokens(),
            output_tokens: default_output_tokens(),
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }
}

/// Deliberate failures, for testing how the CLI and the status rules react.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fault {
    /// Return an HTTP error instead of a model turn.
    Http {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Send `message_start`/`response.created`, then close the connection.
    BrokenStream,
    /// Wait this long after the initial event before the first content.
    SlowFirstToken { delay_ms: u64 },
    /// Wait this long before the whole reply (streaming or not).
    Slow { delay_ms: u64 },
}

impl Scenario {
    pub fn from_path(path: &Path) -> Result<Scenario, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let scenario: Scenario = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        scenario.validate()?;
        Ok(scenario)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("scenario has no steps".into());
        }
        for (i, step) in self.steps.iter().enumerate() {
            if step.reply.content.is_empty() {
                return Err(format!("step {i} has no content; add a text or tool_use block"));
            }
            for block in &step.reply.content {
                if let Block::ToolUse { name, .. } = block
                    && name.is_empty()
                {
                    return Err(format!("step {i} has a tool_use with no name"));
                }
            }
        }
        Ok(())
    }

    pub fn stop_reason(&self, step: &Step) -> String {
        step.reply.stop_reason.clone().unwrap_or_else(|| {
            if step.reply.content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
                "tool_use".to_string()
            } else {
                "end_turn".to_string()
            }
        })
    }

    /// The model named in the reply, or a per-API default.
    pub fn model(&self, step: &Step, default: &str) -> String {
        step.reply.model.clone().unwrap_or_else(|| default.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_defaults_from_content() {
        let s = Scenario {
            name: "x".into(),
            description: None,
            steps: vec![Step {
                expect: Expect::default(),
                reply: Reply {
                    model: None,
                    content: vec![Block::ToolUse { id: None, name: "Bash".into(), input: Value::Null }],
                    stop_reason: None,
                    usage: Usage::default(),
                    fault: None,
                },
            }],
        };
        assert_eq!(s.stop_reason(&s.steps[0]), "tool_use");
    }
}
