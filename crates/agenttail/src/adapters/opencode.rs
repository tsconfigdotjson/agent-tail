//! OpenCode: sessions in a SQLite database (`opencode.db`), not a transcript file.
//!
//! A database row is normalized to one JSON record before it reaches this adapter:
//! `{"table": "session" | "message" | "part", "row": {...columns...}}`. The reader in
//! `opencode.rs` emits a session row first, then each message row followed by its parts in
//! creation order. A row is a snapshot of the database's current value, not an append-only
//! event, so a reader rebuilds the session from all of its rows when the database changes.
//!
//! The 1.19.4 database keeps the pre-v2 `message`/`part` tables even though it also has the
//! v2 `session_message` table (which was empty in every run observed). Part payloads carry
//! tool state (`pending`, `running`, `completed`, `error`) and the finish reason, which is
//! enough to follow a turn without the live event stream. Approval requests are published
//! as ephemeral events and are not stored as rows, so a waiting tool remains an inferred
//! approval after the stall threshold, exactly like the other harnesses.

use super::{Adapter, EventSeed};
use crate::event::{EventKind, TurnEndReason};
use crate::harness::Harness;
use crate::session::{command, file_name, first_line, tool};
use crate::state::{ActivityLine, TokenUsage};
use serde_json::Value;

pub(crate) struct OpenCode;

impl Adapter for OpenCode {
    fn parse(&self, state: &mut crate::event::ParseState, record: &Value) -> Vec<EventSeed> {
        let table = record["table"].as_str().unwrap_or_default();
        let row = &record["row"];
        match table {
            "session" => session(row),
            "message" => message(state, row),
            "part" => part(state, row),
            other => vec![EventSeed::new(EventKind::Unknown {
                record_type: (!other.is_empty()).then(|| format!("opencode.{other}")),
                reason: if other.is_empty() {
                    "OpenCode row has no table".into()
                } else {
                    format!("unknown OpenCode table {other:?}")
                },
                record: record.clone(),
            })],
        }
    }
}

fn at(row: &Value) -> Option<f64> {
    seconds(&row["time_created"]).or_else(|| seconds(&row["time_updated"]))
}

fn seconds(value: &Value) -> Option<f64> {
    value.as_f64().filter(|&n| n > 0.0).map(|n| n / 1000.0)
}

fn data(row: &Value) -> Value {
    match &row["data"] {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        Value::Object(_) => row["data"].clone(),
        _ => Value::Null,
    }
}

fn session(row: &Value) -> Vec<EventSeed> {
    let mut out = Vec::new();
    let at = at(row);
    out.push(
        EventSeed::new(EventKind::SessionInfo {
            cwd: row["directory"].as_str().filter(|s| !s.is_empty()).map(String::from),
            branch: None,
            session_id: row["id"].as_str().filter(|s| !s.is_empty()).map(String::from),
            entrypoint: None,
            approvals_reviewer: None,
        })
        .at(at),
    );
    if let Some(title) = row["title"].as_str().map(str::trim).filter(|title| !title.is_empty()) {
        out.push(
            EventSeed::new(EventKind::Title { title: title.into(), source: crate::event::TitleSource::Other }).at(at),
        );
    }
    if let Some(cost) = row["cost"].as_f64().filter(|cost| cost.is_finite() && *cost >= 0.0) {
        out.push(EventSeed::new(EventKind::Cost { usd: cost }).at(at));
    }
    let usage = TokenUsage {
        input_tokens: count(row, "tokens_input"),
        output_tokens: count(row, "tokens_output"),
        cache_read_input_tokens: count(row, "tokens_cache_read"),
        cache_creation_input_tokens: count(row, "tokens_cache_write"),
    };
    if usage != TokenUsage::default() {
        out.push(
            EventSeed::new(EventKind::Usage { usage, reported: usage, message_id: None, cumulative: true }).at(at),
        );
    }
    out
}

fn count(row: &Value, key: &str) -> Option<u64> {
    row[key].as_u64()
}

fn message(state: &mut crate::event::ParseState, row: &Value) -> Vec<EventSeed> {
    let payload = data(row);
    let role = payload["role"].as_str().unwrap_or_default();
    if let Some(id) = row["id"].as_str().filter(|id| !id.is_empty()) {
        // The reader emits a message row before its parts, so a part knows whose it is.
        state.opencode_roles.insert(
            id.into(),
            match role {
                "user" => "user",
                "assistant" => "assistant",
                _ => "other",
            },
        );
    }
    let at = at(row);
    let mut out = Vec::new();
    if let Some(cwd) = payload.pointer("/path/cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()) {
        out.push(
            EventSeed::new(EventKind::SessionInfo {
                cwd: Some(cwd.into()),
                branch: None,
                session_id: None,
                entrypoint: None,
                approvals_reviewer: None,
            })
            .at(at),
        );
    }
    if role == "assistant" {
        if let Some(model) = payload["modelID"].as_str().filter(|model| !model.is_empty()) {
            out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }).at(at));
        }
        if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
            let message = error["message"].as_str().or_else(|| error.as_str()).unwrap_or("OpenCode error");
            out.push(
                EventSeed::new(EventKind::TurnEnd {
                    reason: TurnEndReason::Error,
                    error: Some(message.into()),
                    background_agents: None,
                })
                .at(at),
            );
        }
    }
    if out.is_empty() {
        out.push(
            EventSeed::new(EventKind::Meta {
                record_type: "message".into(),
                subtype: (!role.is_empty()).then(|| role.into()),
                record: row.clone(),
            })
            .at(at),
        );
    }
    out
}

fn part(state: &mut crate::event::ParseState, row: &Value) -> Vec<EventSeed> {
    let payload = data(row);
    let at = at(row);
    let role = state.opencode_roles.get(row["message_id"].as_str().unwrap_or_default()).copied().unwrap_or("other");
    let mut out = Vec::new();
    match payload["type"].as_str().unwrap_or_default() {
        "text" => {
            let text = payload["text"].as_str().unwrap_or_default();
            if role == "user" {
                out.push(
                    EventSeed::new(EventKind::UserPrompt {
                        text: text.into(),
                        raw: text.into(),
                        wrappers: Vec::new(),
                        starts_turn: true,
                    })
                    .at(at),
                );
            } else if role == "assistant" {
                out.push(EventSeed::new(EventKind::AssistantText { text: text.into() }).at(at));
            }
        }
        "reasoning" => out.push(EventSeed::new(EventKind::Reasoning { signature: None }).at(at)),
        "tool" => tool_part(&payload, at, &mut out),
        "step-start" => out.push(EventSeed::new(EventKind::TurnStart { turn_id: None }).at(at)),
        "step-finish" => {
            let reason = payload["reason"].as_str().unwrap_or_default();
            match reason {
                // A tool-calls step is followed by the tool result and another step.
                "tool-calls" => {}
                "stop" | "length" | "content-filter" | "end_turn" | "max_tokens" => {
                    out.push(
                        EventSeed::new(EventKind::TurnEnd {
                            reason: TurnEndReason::EndTurn,
                            error: None,
                            background_agents: None,
                        })
                        .at(at),
                    );
                }
                "error" | "failed" => out.push(
                    EventSeed::new(EventKind::TurnEnd {
                        reason: TurnEndReason::Error,
                        error: payload["error"]["message"].as_str().map(String::from),
                        background_agents: None,
                    })
                    .at(at),
                ),
                "aborted" | "interrupted" | "cancelled" | "canceled" => out.push(
                    EventSeed::new(EventKind::TurnEnd {
                        reason: TurnEndReason::Interrupted,
                        error: None,
                        background_agents: None,
                    })
                    .at(at),
                ),
                _ => {}
            }
        }
        "compaction" => out.push(
            EventSeed::new(EventKind::Compaction {
                trigger: payload["reason"].as_str().map(String::from),
                pre_tokens: None,
                post_tokens: None,
            })
            .at(at),
        ),
        "snapshot" | "patch" | "file" | "agent" | "subtask" => out.push(
            EventSeed::new(EventKind::Meta {
                record_type: "part".into(),
                subtype: payload["type"].as_str().map(String::from),
                record: row.clone(),
            })
            .at(at),
        ),
        other if !other.is_empty() => out.push(
            EventSeed::new(EventKind::Meta {
                record_type: "part".into(),
                subtype: Some(other.into()),
                record: row.clone(),
            })
            .at(at),
        ),
        _ => {}
    }
    if out.is_empty() {
        out.push(
            EventSeed::new(EventKind::Meta {
                record_type: "part".into(),
                subtype: payload["type"].as_str().map(String::from),
                record: row.clone(),
            })
            .at(at),
        );
    }
    out
}

fn tool_part(payload: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let name = payload["tool"].as_str().unwrap_or_default();
    let call_id = payload["callID"].as_str().filter(|id| !id.is_empty()).map(String::from);
    let state = &payload["state"];
    let status = state["status"].as_str().unwrap_or_default();
    let input = match &state["input"] {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone())),
        value @ Value::Object(_) => value.clone(),
        _ => Value::Null,
    };
    let asks_user = Harness::OpenCode.discovery().asks_user(name);
    let line = (!name.is_empty()).then(|| describe_tool(name, &input));
    out.push(
        EventSeed::new(EventKind::ToolCall {
            name: name.into(),
            call_id: call_id.clone(),
            kind: crate::event::ToolKind::of(name, &input),
            input,
            asks_user,
            line,
        })
        .at(at),
    );
    if matches!(status, "completed" | "error") {
        let output = &state["output"];
        let size = output.as_str().map_or_else(
            || {
                state["content"].as_array().map_or(0, |items| {
                    items
                        .iter()
                        .map(|item| item.as_str().map_or(0, str::len).max(item["text"].as_str().map_or(0, str::len)))
                        .sum::<usize>() as u64
                })
            },
            |text| text.len() as u64,
        );
        out.push(EventSeed::new(EventKind::ToolResult { call_id, is_error: status == "error", size }).at(at));
    }
}

fn describe_tool(name: &str, input: &Value) -> ActivityLine {
    if let Some(c) = input["command"].as_str() {
        return command(c);
    }
    for key in ["filePath", "file_path", "path", "pattern", "query", "url", "description", "prompt"] {
        if let Some(value) = input[key].as_str().filter(|value| !value.is_empty()) {
            let value = if key.contains("path") || key == "filePath" {
                file_name(value)
            } else {
                first_line(value).to_string()
            };
            return tool(name, value);
        }
    }
    ActivityLine::Tool { name: name.into(), detail: None }
}
