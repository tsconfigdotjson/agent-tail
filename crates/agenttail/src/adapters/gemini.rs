//! Gemini CLI: `~/.gemini/tmp/<project>/chats/session-<timestamp>-<id>.jsonl`.
//!
//! A chat file is a journal, not a plain transcript: message records are upserted by id,
//! `$set` records update session metadata, `$patch` records update messages and tool
//! results, and `$rewindTo` removes messages. Checked against Gemini CLI 0.65.0.
//!
//! The important status fact is negative: a tool call is written only after it completes
//! (`recordCompletedToolCalls`), with its result and status. There is no on-disk pending or
//! awaiting-approval record, and the approval mode is not recorded either. The adapter
//! therefore never claims a firm approval from a Gemini journal.

use super::{Adapter, EventSeed};
use crate::event::{EventKind, TitleSource, ToolKind, TurnEndReason};
use crate::harness::Harness;
use crate::session::{command, file_name, first_line, tool};
use crate::state::{ActivityLine, TokenUsage};
use serde_json::Value;

pub(crate) struct Gemini;

impl Adapter for Gemini {
    fn start(&self, session: &mut crate::session::Session, path: &std::path::Path) {
        // <root>/<project>/chats/session-….jsonl: the project folder is the fallback label
        // until a `directories` metadata update names the real workspace.
        let folder = path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !folder.is_empty() {
            session.folder = folder.clone();
            if session.project.is_empty() {
                session.project = folder;
            }
        }
    }

    fn parse(&self, state: &mut crate::event::ParseState, record: &Value) -> Vec<EventSeed> {
        let at = event_time(record);
        let mut out = Vec::new();
        if let Some(set) = record.get("$set") {
            metadata(state, set, at, &mut out);
        } else if let Some(rewind) = record.get("$rewindTo") {
            // A rewind changes the logical conversation, but the incremental reducer cannot
            // undo earlier activity. Keep the record for provenance.
            out.push(meta(record, "rewind").at(at));
            let _ = rewind;
        } else if let Some(patch) = record.get("$patch") {
            apply_patch(state, patch, at, &mut out);
        } else {
            match record["type"].as_str().unwrap_or_default() {
                "user" => user_message(state, record, at, &mut out),
                "gemini" => gemini_message(state, record, at, &mut out),
                "error" => out.push(
                    EventSeed::new(EventKind::TurnEnd {
                        reason: TurnEndReason::Error,
                        error: Some(message_text(record)),
                        background_agents: None,
                    })
                    .at(at),
                ),
                "info" | "warning" => out.push(meta(record, record["type"].as_str().unwrap_or_default()).at(at)),
                "" if record["sessionId"].is_string() => initial_metadata(record, at, &mut out),
                kind if known_record(kind) => out.push(meta(record, kind).at(at)),
                kind => out.push(
                    EventSeed::new(EventKind::Unknown {
                        record_type: (!kind.is_empty()).then(|| kind.into()),
                        reason: "unknown Gemini journal record type".into(),
                        record: record.clone(),
                    })
                    .at(at),
                ),
            }
        }
        if out.is_empty() {
            out.push(meta(record, record["type"].as_str().unwrap_or("metadata")).at(at));
        }
        out
    }
}

fn event_time(record: &Value) -> Option<f64> {
    ["timestamp", "startTime", "lastUpdated"]
        .iter()
        .find_map(|key| record[*key].as_str().and_then(crate::time::parse_rfc3339))
}

fn initial_metadata(record: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    out.push(
        EventSeed::new(EventKind::SessionInfo {
            cwd: None,
            branch: None,
            session_id: record["sessionId"].as_str().filter(|id| !id.is_empty()).map(String::from),
            entrypoint: None,
            approvals_reviewer: None,
        })
        .at(at),
    );
}

fn metadata(state: &mut crate::event::ParseState, set: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let _ = state;
    let mut moved = false;
    if let Some(cwd) = set["directories"]
        .as_array()
        .and_then(|items| items.iter().find_map(Value::as_str))
        .filter(|cwd| !cwd.is_empty())
    {
        out.push(
            EventSeed::new(EventKind::SessionInfo {
                cwd: Some(cwd.into()),
                branch: None,
                session_id: set["sessionId"].as_str().map(String::from),
                entrypoint: None,
                approvals_reviewer: None,
            })
            .at(at),
        );
        moved = true;
    }
    if let Some(title) = set["summary"].as_str().map(str::trim).filter(|title| !title.is_empty()) {
        out.push(EventSeed::new(EventKind::Title { title: title.into(), title_source: TitleSource::Other }).at(at));
        moved = true;
    }
    if let Some(id) = set["sessionId"].as_str().filter(|id| !id.is_empty()) {
        out.push(
            EventSeed::new(EventKind::SessionInfo {
                cwd: None,
                branch: None,
                session_id: Some(id.into()),
                entrypoint: None,
                approvals_reviewer: None,
            })
            .at(at),
        );
        moved = true;
    }
    if !moved {
        out.push(meta(set, "metadata-update").at(at));
    }
}

fn user_message(state: &mut crate::event::ParseState, record: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let content = &record["content"];
    let mut texts = Vec::new();
    let mut results = Vec::new();
    match content {
        Value::String(text) => texts.push(text.clone()),
        Value::Array(parts) => {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    texts.push(text.into());
                } else if let Some(response) = part.get("functionResponse") {
                    let call_id = response["id"]
                        .as_str()
                        .or_else(|| part["id"].as_str())
                        .filter(|id| !id.is_empty())
                        .map(String::from);
                    results.push((call_id, size(&response["response"]), response["error"].is_object()));
                }
            }
        }
        _ => {}
    }
    for (call_id, result_size, is_error) in results {
        let key = call_id.clone().unwrap_or_default();
        if !key.is_empty() && !state.gemini_results.insert(key) {
            continue;
        }
        out.push(EventSeed::new(EventKind::ToolResult { call_id, is_error, size: result_size }).at(at));
    }
    let text = texts.join("\n");
    if text.trim_start().starts_with("<session_context>") {
        if let Some(cwd) = workspace_from_context(&text) {
            out.push(
                EventSeed::new(EventKind::SessionInfo {
                    cwd: Some(cwd),
                    branch: None,
                    session_id: None,
                    entrypoint: None,
                    approvals_reviewer: None,
                })
                .at(at),
            );
        }
    } else if !text.trim().is_empty() {
        out.push(
            EventSeed::new(EventKind::UserPrompt {
                text: text.clone(),
                raw: text,
                wrappers: Vec::new(),
                starts_turn: true,
            })
            .at(at),
        );
    }
    let _ = state;
}

fn workspace_from_context(text: &str) -> Option<String> {
    let mut after = text.split("**Workspace Directories:**").nth(1)?.lines();
    after
        .find_map(|line| line.trim().strip_prefix("- ").map(str::trim).filter(|path| path.starts_with('/')))
        .map(String::from)
}

fn gemini_message(state: &mut crate::event::ParseState, record: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    if let Some(model) = record["model"].as_str().filter(|model| !model.is_empty()) {
        out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }).at(at));
    }
    if let Some(text) = content_text(&record["content"]).filter(|text| !text.is_empty()) {
        out.push(EventSeed::new(EventKind::AssistantText { text }).at(at));
    }
    for thought in record["thoughts"].as_array().into_iter().flatten() {
        out.push(
            EventSeed::new(EventKind::Reasoning {
                signature: thought["thoughtSignature"]
                    .as_str()
                    .or_else(|| thought["signature"].as_str())
                    .filter(|signature| !signature.is_empty())
                    .map(String::from),
            })
            .at(at),
        );
    }
    if let Some(tokens) = record.get("tokens").filter(|tokens| tokens.is_object()) {
        let reported = TokenUsage {
            input_tokens: tokens["input"].as_u64(),
            output_tokens: tokens["output"].as_u64(),
            cache_read_input_tokens: tokens["cached"].as_u64(),
            cache_creation_input_tokens: None,
        };
        if [reported.input_tokens, reported.output_tokens, reported.cache_read_input_tokens].iter().any(Option::is_some)
        {
            out.push(
                EventSeed::new(EventKind::Usage {
                    usage: reported,
                    reported,
                    message_id: record["id"].as_str().map(String::from),
                    cumulative: false,
                })
                .at(at),
            );
        }
    }
    let tool_calls = record["toolCalls"].as_array().cloned().unwrap_or_default();
    for call in &tool_calls {
        tool_call(state, call, at, out);
    }
    // A model message with text and no tool calls is the completed answer. Tool-call
    // messages are followed by a function-response user record and another model message.
    if tool_calls.is_empty() && content_text(&record["content"]).is_some_and(|text| !text.is_empty()) {
        out.push(
            EventSeed::new(EventKind::TurnEnd { reason: TurnEndReason::EndTurn, error: None, background_agents: None })
                .at(at),
        );
    }
}

fn apply_patch(state: &mut crate::event::ParseState, patch: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let mut patches = Vec::new();
    if patch.get("id").is_some() || patch.get("toolCalls").is_some() || patch.get("content").is_some() {
        patches.push(patch);
    }
    patches.extend(patch["updates"].as_array().into_iter().flatten());
    for update in patches {
        for call in update["toolCalls"].as_array().into_iter().flatten() {
            tool_call(state, call, at, out);
        }
    }
    if out.is_empty() {
        out.push(meta(patch, "patch").at(at));
    }
}

fn tool_call(state: &mut crate::event::ParseState, call: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let at = call["timestamp"].as_str().and_then(crate::time::parse_rfc3339).or(at);
    let id = call["id"].as_str().or_else(|| call["callId"].as_str()).filter(|id| !id.is_empty()).map(String::from);
    let name = call["name"].as_str().or_else(|| call["toolName"].as_str()).unwrap_or_default();
    let input = match &call["args"] {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone())),
        value @ Value::Object(_) => value.clone(),
        _ => call["arguments"].clone(),
    };
    let key = id.clone().unwrap_or_else(|| format!("{name}:{}", call["timestamp"].as_str().unwrap_or_default()));
    if !state.gemini_tools.contains(&key) {
        state.gemini_tools.insert(key);
        out.push(
            EventSeed::new(EventKind::ToolCall {
                name: name.into(),
                call_id: id.clone(),
                kind: ToolKind::of(name, &input),
                input: input.clone(),
                asks_user: Harness::Gemini.discovery().asks_user(name),
                activity_line: Some(describe_tool(name, &input)),
            })
            .at(at),
        );
    }
    let status = call["status"].as_str().unwrap_or_default();
    if status == "awaiting_approval" {
        out.push(
            EventSeed::new(EventKind::ApprovalRequested {
                call_id: id.clone(),
                tool_name: (!name.is_empty()).then(|| name.into()),
                request_kind: Some(status.into()),
            })
            .at(at),
        );
    }
    let result = &call["result"];
    let terminal = matches!(status, "success" | "error" | "cancelled" | "canceled" | "completed" | "failed");
    if !result.is_null() || terminal {
        let key = id.clone().unwrap_or_default();
        if key.is_empty() || state.gemini_results.insert(key) {
            out.push(
                EventSeed::new(EventKind::ToolResult {
                    call_id: id,
                    is_error: matches!(status, "error" | "failed"),
                    size: size(result),
                })
                .at(at),
            );
        }
    }
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|part| part["thought"].as_bool() != Some(true))
                .filter_map(|part| part["text"].as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn size(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::String(text) => text.len() as u64,
        other => other.to_string().len() as u64,
    }
}

fn message_text(record: &Value) -> String {
    content_text(&record["content"])
        .filter(|text| !text.is_empty())
        .or_else(|| record["message"].as_str().map(String::from))
        .or_else(|| record["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| "Gemini error".into())
}

fn describe_tool(name: &str, input: &Value) -> ActivityLine {
    match name {
        "run_shell_command" => command(input["command"].as_str().unwrap_or_default()),
        "read_file" | "write_file" | "replace" | "list_directory" => {
            let path = input["file_path"].as_str().or_else(|| input["path"].as_str()).unwrap_or_default();
            tool(name, file_name(path))
        }
        "grep_search" | "glob" => tool(name, first_line(input["pattern"].as_str().unwrap_or_default())),
        "web_fetch" => tool(name, first_line(input["url"].as_str().unwrap_or_default())),
        "google_web_search" => tool(name, first_line(input["query"].as_str().unwrap_or_default())),
        "invoke_agent" => tool(name, first_line(input["prompt"].as_str().unwrap_or_default())),
        other => ActivityLine::Tool { name: other.into(), detail: None },
    }
}

fn meta(record: &Value, record_type: &str) -> EventSeed {
    EventSeed::new(EventKind::Meta {
        record_type: record_type.into(),
        subtype: record["type"].as_str().map(String::from),
        record: record.clone(),
    })
}

fn known_record(kind: &str) -> bool {
    matches!(kind, "user" | "gemini" | "info" | "error" | "warning")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::harness::Harness;
    use crate::session::Session;
    use crate::testutil::{replay, status};
    use serde_json::json;

    fn session(records: &[Value]) -> Session {
        replay(Harness::Gemini, records)
    }

    fn user(text: &str) -> Value {
        json!({"id":"u1","timestamp":"2026-10-25T01:00:00Z","type":"user","content":[{"text":text}]})
    }

    fn gemini(text: &str) -> Value {
        json!({"id":"g1","timestamp":"2026-10-25T01:00:01Z","type":"gemini","model":"gemini-test",
               "content":[{"text":text}],"tokens":{"input":10,"output":5,"cached":2,"total":17}})
    }

    #[test]
    fn follows_a_text_turn_to_done() {
        let mut s = session(&[
            json!({"sessionId":"s1","projectHash":"h","startTime":"2026-10-25T00:59:59Z","lastUpdated":"2026-10-25T00:59:59Z","kind":"main"}),
            user("say hello"),
        ]);
        assert_eq!((s.project.as_str(), s.title.as_str()), ("", "say hello"));
        assert_eq!(status(&s), (Status::Working, "thinking"));
        s.apply(&gemini("Hello from the mock."));
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!((s.usage.input_tokens, s.usage.output_tokens), (Some(10), Some(5)));
    }

    #[test]
    fn completed_tool_calls_are_a_result_not_a_pending_approval() {
        let s = session(&[
            user("use the tool"),
            json!({"id":"g2","timestamp":"2026-10-25T01:00:01Z","type":"gemini","model":"gemini-test","content":"",
                   "toolCalls":[{"id":"c1","name":"run_shell_command","args":{"command":"printf ok"},
                                 "status":"success","result":[{"functionResponse":{"name":"run_shell_command","response":{"output":"ok"}}}]}]}),
        ]);
        assert_eq!(status(&s), (Status::Working, "thinking"), "the completed call is a tool result");
        assert_eq!(s.pending_approvals.len(), 0);
    }

    #[test]
    fn awaiting_approval_is_firm_only_when_it_is_on_disk() {
        let s = session(&[
            user("run it"),
            json!({"id":"g2","timestamp":"2026-10-25T01:00:01Z","type":"gemini","content":"",
                   "toolCalls":[{"id":"c1","name":"run_shell_command","args":{"command":"sleep 60"},"status":"awaiting_approval"}]}),
        ]);
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!(
            (st.activity, st.certainty, st.reason),
            (crate::Activity::AwaitingApproval, crate::Certainty::Firm, crate::Reason::ApprovalRequested)
        );
    }

    #[test]
    fn metadata_updates_name_the_project_and_title() {
        let s = session(&[
            user("first prompt"),
            json!({"$set":{"directories":["/work/jooba"],"summary":"Named session","lastUpdated":"2026-10-25T01:00:02Z"}}),
        ]);
        assert_eq!((s.project.as_str(), s.title.as_str()), ("jooba", "Named session"));
    }

    #[test]
    fn describes_gemini_tools() {
        let d = |name, input: Value| describe_tool(name, &input).to_string();
        assert_eq!(d("run_shell_command", json!({"command":"ls -la\nmore"})), "$ ls -la");
        assert_eq!(d("read_file", json!({"file_path":"/a/b/main.rs"})), "read_file main.rs");
        assert_eq!(d("grep_search", json!({"pattern":"TODO\nmore"})), "grep_search TODO");
        assert_eq!(d("glob", json!({"pattern":"*.rs"})), "glob *.rs");
    }
}
