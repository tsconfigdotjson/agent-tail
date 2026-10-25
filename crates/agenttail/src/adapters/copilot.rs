//! GitHub Copilot CLI: `~/.copilot/session-state/<uuid>/events.jsonl`.
//!
//! Each line is one event from the CLI's session recorder. Checked against Copilot CLI
//! 1.0.103. Unlike the other harnesses, a permission prompt is persisted: the CLI writes
//! `permission.requested` with the tool call id before the tool runs, then
//! `permission.completed` with the decision. A requested-but-not-completed permission is
//! therefore firm `AwaitingApproval`, not a timing inference.

use super::{Adapter, EventSeed};
use crate::event::{EventKind, ToolKind, TurnEndReason};
use crate::harness::Harness;
use crate::session::{command, file_name, first_line, tool};
use crate::state::{ActivityLine, PermissionMode, TokenUsage};
use serde_json::Value;

pub(crate) struct Copilot;

impl Adapter for Copilot {
    fn parse(&self, state: &mut crate::event::ParseState, event: &Value) -> Vec<EventSeed> {
        let at = event["timestamp"].as_str().and_then(crate::time::parse_rfc3339);
        let data = &event["data"];
        let mut out = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "session.start" => {
                out.push(
                    EventSeed::new(EventKind::SessionInfo {
                        cwd: data["context"]["cwd"].as_str().filter(|cwd| !cwd.is_empty()).map(String::from),
                        branch: None,
                        session_id: data["sessionId"].as_str().filter(|id| !id.is_empty()).map(String::from),
                        entrypoint: None,
                        approvals_reviewer: None,
                    })
                    .at(at),
                );
                if let Some(model) = data["selectedModel"].as_str().filter(|model| !model.is_empty()) {
                    out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }).at(at));
                }
            }
            "user.message" => {
                state.copilot_tools_pending = false;
                let text = content_text(&data["content"]);
                if !text.trim().is_empty() {
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
            }
            "assistant.turn_start" => out.push(
                EventSeed::new(EventKind::TurnStart { turn_id: data["turnId"].as_str().map(String::from) }).at(at),
            ),
            "assistant.message" => assistant_message(state, data, at, &mut out),
            "assistant.turn_end" => {
                // Each model call has its own turn_start/turn_end. Only a final message
                // without tool requests ends the user's interaction.
                if !state.copilot_tools_pending {
                    out.push(
                        EventSeed::new(EventKind::TurnEnd {
                            reason: TurnEndReason::EndTurn,
                            error: None,
                            background_agents: None,
                        })
                        .at(at),
                    );
                }
            }
            "tool.execution_start" => tool_start(state, data, at, &mut out),
            "tool.execution_complete" => {
                let call_id = data["toolCallId"].as_str().filter(|id| !id.is_empty()).map(String::from);
                out.push(
                    EventSeed::new(EventKind::ToolResult {
                        call_id,
                        is_error: data["success"].as_bool() == Some(false) || data.get("error").is_some(),
                        size: result_size(&data["result"]),
                    })
                    .at(at),
                );
            }
            "permission.requested" => permission_requested(data, at, &mut out),
            "permission.completed" => {
                let call_id = data["toolCallId"]
                    .as_str()
                    .or_else(|| data["permissionRequest"]["toolCallId"].as_str())
                    .or_else(|| data["promptRequest"]["toolCallId"].as_str())
                    .filter(|id| !id.is_empty())
                    .map(String::from);
                let kind = data["result"]["kind"].as_str().unwrap_or_default();
                out.push(
                    EventSeed::new(EventKind::ApprovalCompleted {
                        call_id,
                        approved: kind.starts_with("approved"),
                        result_kind: (!kind.is_empty()).then(|| kind.into()),
                    })
                    .at(at),
                );
            }
            "session.usage_record" => usage(data, at, &mut out),
            "session.shutdown" => {
                let error = data["error"]["message"].as_str().or_else(|| data["error"].as_str());
                out.push(
                    EventSeed::new(EventKind::TurnEnd {
                        reason: if error.is_some() { TurnEndReason::Error } else { TurnEndReason::EndTurn },
                        error: error.map(String::from),
                        background_agents: None,
                    })
                    .at(at),
                );
            }
            "abort" => out.push(
                EventSeed::new(EventKind::TurnEnd {
                    reason: TurnEndReason::Interrupted,
                    error: data["reason"].as_str().map(String::from),
                    background_agents: None,
                })
                .at(at),
            ),
            "session.mode_changed" => {
                if let Some(mode) = data["mode"].as_str().or_else(|| data["permissionMode"].as_str()) {
                    out.push(EventSeed::new(EventKind::PermissionMode { mode: PermissionMode::parse(mode) }).at(at));
                }
            }
            kind if known_record(kind) => out.push(meta(event, kind).at(at)),
            kind => out.push(
                EventSeed::new(EventKind::Unknown {
                    record_type: (!kind.is_empty()).then(|| kind.into()),
                    reason: "unknown Copilot event type".into(),
                    record: event.clone(),
                })
                .at(at),
            ),
        }
        if out.is_empty() {
            out.push(meta(event, event["type"].as_str().unwrap_or("event")).at(at));
        }
        out
    }
}

fn assistant_message(state: &mut crate::event::ParseState, data: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let text = content_text(&data["content"]);
    if !text.is_empty() {
        out.push(EventSeed::new(EventKind::AssistantText { text }).at(at));
    }
    let requests = data["toolRequests"].as_array().cloned().unwrap_or_default();
    state.copilot_tools_pending = !requests.is_empty();
    for request in requests {
        let id = request["toolCallId"]
            .as_str()
            .or_else(|| request["id"].as_str())
            .filter(|id| !id.is_empty())
            .map(String::from);
        if let Some(id) = &id {
            state.copilot_tools.insert(id.clone());
        }
        let name = request["name"].as_str().or_else(|| request["toolName"].as_str()).unwrap_or_default();
        let input = request.get("arguments").cloned().unwrap_or(Value::Null);
        out.push(
            EventSeed::new(EventKind::ToolCall {
                name: name.into(),
                call_id: id,
                kind: ToolKind::of(name, &input),
                input: input.clone(),
                asks_user: Harness::Copilot.discovery().asks_user(name),
                line: Some(describe_tool(name, &input)),
            })
            .at(at),
        );
    }
}

fn tool_start(state: &mut crate::event::ParseState, data: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let Some(id) = data["toolCallId"].as_str().filter(|id| !id.is_empty()) else { return };
    if state.copilot_tools.contains(id) {
        return;
    }
    state.copilot_tools.insert(id.into());
    let name = data["toolName"].as_str().or_else(|| data["name"].as_str()).unwrap_or_default();
    let input = data.get("arguments").cloned().unwrap_or(Value::Null);
    out.push(
        EventSeed::new(EventKind::ToolCall {
            name: name.into(),
            call_id: Some(id.into()),
            kind: ToolKind::of(name, &input),
            input: input.clone(),
            asks_user: Harness::Copilot.discovery().asks_user(name),
            line: Some(describe_tool(name, &input)),
        })
        .at(at),
    );
}

fn permission_requested(data: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let request = data.get("permissionRequest").unwrap_or(data);
    let prompt = data.get("promptRequest").unwrap_or(data);
    let call_id = data["toolCallId"]
        .as_str()
        .or_else(|| request["toolCallId"].as_str())
        .or_else(|| prompt["toolCallId"].as_str())
        .filter(|id| !id.is_empty())
        .map(String::from);
    let kind = prompt["kind"].as_str().or_else(|| request["kind"].as_str()).map(String::from);
    out.push(EventSeed::new(EventKind::ApprovalRequested { call_id, tool_name: None, request_kind: kind }).at(at));
    if let Some(mode) = data["permissionMode"].as_str() {
        out.push(EventSeed::new(EventKind::PermissionMode { mode: PermissionMode::parse(mode) }).at(at));
    }
}

fn usage(data: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    let usage = &data["usage"];
    let reported = TokenUsage {
        input_tokens: usage["inputTokens"].as_u64(),
        output_tokens: usage["outputTokens"].as_u64(),
        cache_read_input_tokens: usage["cacheReadTokens"].as_u64(),
        cache_creation_input_tokens: usage["cacheWriteTokens"].as_u64(),
    };
    if [
        reported.input_tokens,
        reported.output_tokens,
        reported.cache_read_input_tokens,
        reported.cache_creation_input_tokens,
    ]
    .iter()
    .any(Option::is_some)
    {
        out.push(
            EventSeed::new(EventKind::Usage {
                usage: reported,
                reported,
                message_id: usage["apiCallId"].as_str().map(String::from),
                cumulative: false,
            })
            .at(at),
        );
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str().or_else(|| part.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => content["text"].as_str().unwrap_or_default().into(),
        _ => String::new(),
    }
}

fn result_size(result: &Value) -> u64 {
    match result {
        Value::Null => 0,
        Value::String(text) => text.len() as u64,
        other => other.to_string().len() as u64,
    }
}

fn describe_tool(name: &str, input: &Value) -> ActivityLine {
    match name {
        "bash" | "shell" | "powershell" => command(input["command"].as_str().unwrap_or_default()),
        "edit" | "write" => {
            let path = input["path"].as_str().or_else(|| input["file_path"].as_str()).unwrap_or_default();
            tool(name, file_name(path))
        }
        "view" | "read" => {
            let path = input["path"].as_str().or_else(|| input["file_path"].as_str()).unwrap_or_default();
            tool(name, file_name(path))
        }
        "grep" => tool(name, first_line(input["pattern"].as_str().unwrap_or_default())),
        "glob" => tool(name, first_line(input["pattern"].as_str().unwrap_or_default())),
        "task" | "agent" => tool(name, first_line(input["description"].as_str().unwrap_or_default())),
        other => ActivityLine::Tool { name: other.into(), detail: None },
    }
}

fn meta(event: &Value, kind: &str) -> EventSeed {
    EventSeed::new(EventKind::Meta {
        record_type: kind.into(),
        subtype: event["data"]["infoType"].as_str().map(String::from),
        record: event.clone(),
    })
}

fn known_record(kind: &str) -> bool {
    matches!(
        kind,
        "session.start"
            | "user.message"
            | "system.message"
            | "assistant.turn_start"
            | "assistant.message"
            | "assistant.turn_end"
            | "assistant.idle"
            | "tool.execution_start"
            | "tool.execution_complete"
            | "permission.requested"
            | "permission.completed"
            | "session.usage_record"
            | "session.shutdown"
            | "session.info"
            | "session.mode_changed"
            | "session.plan_changed"
            | "session.background_tasks_changed"
            | "session.skills_loaded"
            | "session.mcp_servers_loaded"
            | "session.tools_updated"
            | "subagent.started"
            | "subagent.completed"
            | "abort"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::harness::Harness;
    use crate::session::Session;
    use crate::testutil::{replay, status};
    use serde_json::json;

    fn session(events: &[Value]) -> Session {
        replay(Harness::Copilot, events)
    }

    fn event(kind: &str, data: Value) -> Value {
        json!({"type":kind,"id":"e1","timestamp":"2026-10-25T01:00:00Z","data":data})
    }

    fn start() -> Value {
        event("session.start", json!({"sessionId":"s1","selectedModel":"test-model","context":{"cwd":"/work/jooba"}}))
    }

    fn user(text: &str) -> Value {
        event("user.message", json!({"content":text}))
    }

    #[test]
    fn follows_a_text_turn_to_done() {
        let s = session(&[
            start(),
            user("say hello"),
            event("assistant.turn_start", json!({"turnId":"0"})),
            event("assistant.message", json!({"content":"Hello from the mock.","toolRequests":[]})),
            event("assistant.turn_end", json!({"turnId":"0"})),
        ]);
        assert_eq!((s.project.as_str(), s.title.as_str()), ("jooba", "say hello"));
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn a_tool_turn_does_not_end_at_the_model_step_boundary() {
        let mut s = session(&[
            start(),
            user("run the tool"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 2"}}]}),
            ),
            event("assistant.turn_end", json!({"turnId":"0"})),
        ]);
        assert_eq!(status(&s), (Status::Working, "working"));
        s.apply(&event("tool.execution_complete", json!({"toolCallId":"c1","success":true,"result":{"content":"ok"}})));
        assert_eq!(status(&s), (Status::Working, "thinking"));
        s.apply(&event("assistant.message", json!({"content":"The tool finished.","toolRequests":[]})));
        s.apply(&event("assistant.turn_end", json!({"turnId":"1"})));
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn a_persisted_permission_request_is_firm_approval() {
        let s = session(&[
            start(),
            user("run it"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 60"}}]}),
            ),
            event(
                "permission.requested",
                json!({"toolCallId":"c1","permissionMode":"manual","promptRequest":{"kind":"commands","toolCallId":"c1"}}),
            ),
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
        assert_eq!(st.permission, Some(crate::PermissionMode::Manual));
    }

    #[test]
    fn a_completed_permission_clears_the_firm_approval() {
        let s = session(&[
            start(),
            user("run it"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 60"}}]}),
            ),
            event("permission.requested", json!({"toolCallId":"c1"})),
            event(
                "permission.completed",
                json!({"toolCallId":"c1","result":{"kind":"denied-no-approval-rule-and-could-not-request-from-user"}}),
            ),
        ]);
        assert!(s.pending_approvals.is_empty());
        assert_eq!(status(&s), (Status::Working, "working"));
    }

    fn completed(kind: &str) -> (bool, Option<String>) {
        let mut state = crate::event::ParseState::default();
        Copilot
            .parse(
                &mut state,
                &event(
                    "permission.completed",
                    json!({"toolCallId":"c1","result":{"kind":kind},"decisionSource":"human_response"}),
                ),
            )
            .into_iter()
            .find_map(|seed| match seed.kind {
                EventKind::ApprovalCompleted { approved, result_kind, .. } => Some((approved, result_kind)),
                _ => None,
            })
            .expect("one approval_completed event")
    }

    #[test]
    fn approved_result_kinds_use_the_approved_prefix_and_everything_else_does_not() {
        for kind in ["approved", "approved-for-session", "approved-for-location", "approved-read-only-for-session"] {
            let (approved, result_kind) = completed(kind);
            assert!(approved, "{kind} must be approved");
            assert_eq!(result_kind.as_deref(), Some(kind));
        }
        for kind in [
            "denied-interactively-by-user",
            "denied-by-rules",
            "denied-no-approval-rule-and-could-not-request-from-user",
            "denied-by-content-exclusion-policy",
            "denied-by-permission-request-hook",
            "cancelled",
            "user-not-available",
        ] {
            let (approved, result_kind) = completed(kind);
            assert!(!approved, "{kind} must not be approved");
            assert_eq!(result_kind.as_deref(), Some(kind));
        }
        let (approved, result_kind) = completed("");
        assert!(!approved);
        assert_eq!(result_kind, None);
    }

    fn approval_waits(s: &Session) -> bool {
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        (st.activity, st.certainty, st.reason)
            == (crate::Activity::AwaitingApproval, crate::Certainty::Firm, crate::Reason::ApprovalRequested)
    }

    #[test]
    fn a_completion_without_a_requested_event_is_harmless() {
        let s = session(&[
            start(),
            user("run it"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 1"}}]}),
            ),
            event(
                "permission.completed",
                json!({"toolCallId":"c1","result":{"kind":"approved"},"decisionSource":"human_response"}),
            ),
        ]);
        assert!(s.pending_approvals.is_empty());
        assert_eq!(status(&s), (Status::Working, "working"));
    }

    #[test]
    fn a_completion_without_a_join_id_does_not_clear_a_request_that_has_one() {
        let mut s = session(&[
            start(),
            user("run it"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 1"}}]}),
            ),
            event("permission.requested", json!({"toolCallId":"c1","permissionRequest":{"toolCallId":"c1"}})),
            event(
                "permission.completed",
                json!({"result":{"kind":"denied-interactively-by-user"},"decisionSource":"human_response"}),
            ),
        ]);
        assert!(s.pending_approvals.contains("c1"));
        assert!(approval_waits(&s), "an unjoinable completion must not clear an unrelated request");
        s.apply(&event(
            "tool.execution_complete",
            json!({"toolCallId":"c1","success":false,"error":{"message":"denied"}}),
        ));
        assert!(s.pending_approvals.is_empty());
        assert_eq!(status(&s), (Status::Working, "thinking"));
    }

    #[test]
    fn duplicate_completions_are_idempotent() {
        let s = session(&[
            start(),
            user("run it"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 1"}}]}),
            ),
            event("permission.requested", json!({"toolCallId":"c1"})),
            event("permission.completed", json!({"toolCallId":"c1","result":{"kind":"approved"}})),
            event("permission.completed", json!({"toolCallId":"c1","result":{"kind":"approved"}})),
        ]);
        assert!(s.pending_approvals.is_empty());
        assert_eq!(status(&s), (Status::Working, "working"));
    }

    #[test]
    fn an_unidentified_request_clears_on_an_unidentified_completion() {
        let s = session(&[
            start(),
            user("run it"),
            event(
                "assistant.message",
                json!({"content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"command":"sleep 1"}}]}),
            ),
            event("permission.requested", json!({"permissionMode":"manual"})),
            event("permission.completed", json!({"result":{"kind":"approved"},"decisionSource":"human_response"})),
        ]);
        assert!(s.pending_approvals.is_empty());
        assert_eq!(status(&s), (Status::Working, "working"));
    }

    #[test]
    fn usage_is_summed_per_api_call() {
        let s = session(&[
            start(),
            event("session.usage_record", json!({"usage":{"apiCallId":"a1","inputTokens":10,"outputTokens":5}})),
            event("session.usage_record", json!({"usage":{"apiCallId":"a2","inputTokens":20,"outputTokens":8}})),
        ]);
        assert_eq!((s.usage.input_tokens, s.usage.output_tokens), (Some(30), Some(13)));
    }
}
