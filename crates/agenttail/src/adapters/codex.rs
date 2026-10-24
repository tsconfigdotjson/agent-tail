//! Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, one entry per line:
//! {"timestamp", "type": "session_meta" | "event_msg" | "response_item" | …, "payload"}.
//! Older rollouts are compressed to `.jsonl.zst`; only live `.jsonl` files are read.
//!
//! Checked against rollouts written by Codex CLI 0.160.1 (the fixtures) and earlier ones.

use super::{Adapter, EventSeed};
use crate::session::{command, file_name, tool};
use crate::state::{ActivityLine, PermissionMode, SandboxPolicy};
use serde_json::Value;

pub(crate) struct Codex;

impl Adapter for Codex {
    fn parse(&self, state: &mut crate::event::ParseState, e: &Value) -> Vec<EventSeed> {
        use crate::event::{EventKind, ToolKind, TurnEndReason};
        let at = e["timestamp"].as_str().and_then(crate::time::parse_rfc3339);
        let p = &e["payload"];
        let mut out = Vec::new();
        match e["type"].as_str().unwrap_or_default() {
            "session_meta" => {
                if let Some(info) = session_info(p["cwd"].as_str(), p["git"]["branch"].as_str(), p) {
                    out.push(EventSeed::new(info).at(at));
                }
            }
            // Each turn's settings; the folder can change within a session.
            "turn_context" => {
                // The reviewer is re-recorded each turn; an absent field means no AI reviewer.
                let info = crate::event::EventKind::SessionInfo {
                    cwd: p["cwd"].as_str().map(String::from),
                    branch: None,
                    session_id: p["id"].as_str().map(String::from),
                    entrypoint: None,
                    approvals_reviewer: Some(p["approvals_reviewer"].as_str().unwrap_or_default().into()),
                };
                out.push(EventSeed::new(info).at(at));
                match &p["approval_policy"] {
                    Value::String(policy) => out
                        .push(EventSeed::new(EventKind::PermissionMode { mode: PermissionMode::parse(policy) }).at(at)),
                    Value::Object(policy) => {
                        // `granular` is either the object itself or one key inside it.
                        let config = policy.get("granular").unwrap_or(&p["approval_policy"]);
                        let categories =
                            ["sandbox_approval", "rules", "skill_approval", "request_permissions", "mcp_elicitations"];
                        let mode = if !categories.iter().any(|key| config.get(*key).is_some()) {
                            // A shape this version hasn't seen: keep the timing guess rather than
                            // inventing a firm no-human answer.
                            PermissionMode::Unknown
                        } else {
                            let asks =
                                categories.iter().any(|key| config.get(*key).and_then(Value::as_bool).unwrap_or(false));
                            if asks { PermissionMode::GranularAsk } else { PermissionMode::GranularNoAsk }
                        };
                        out.push(EventSeed::new(EventKind::PermissionMode { mode }).at(at));
                    }
                    _ => {}
                }
                if let Some(kind) = p["sandbox_policy"]["type"].as_str() {
                    out.push(EventSeed::new(EventKind::SandboxPolicy { policy: SandboxPolicy::parse(kind) }).at(at));
                }
                if let Some(model) = p["model"].as_str().filter(|model| !model.is_empty()) {
                    out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }).at(at));
                }
            }
            // What the user typed: a `user_message` event in older rollouts, a completed
            // `UserMessage` item in 0.160. (The `response_item` copy also holds the
            // environment context and instructions, so it isn't used.)
            "event_msg" => match p["type"].as_str().unwrap_or_default() {
                "user_message" if prompts_from(state, "user_message") => {
                    let text = p["message"].as_str().unwrap_or_default();
                    out.push(
                        EventSeed::new(EventKind::UserPrompt {
                            text: text.into(),
                            raw: text.into(),
                            wrappers: Vec::new(),
                            starts_turn: true,
                        })
                        .at(at),
                    );
                }
                "item_completed" if p["item"]["type"] == "UserMessage" => {
                    if prompts_from(state, "item") {
                        let blocks = p["item"]["content"].as_array().into_iter().flatten();
                        let text: Vec<&str> = blocks.filter_map(|c| c["text"].as_str()).collect();
                        let text = text.join("\n");
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
                "item_completed" if p["item"]["type"] == "AgentMessage" => {
                    let text = content_text(&p["item"]["content"]);
                    out.push(EventSeed::new(EventKind::AssistantText { text }).at(at));
                }
                "agent_message" => out.push(
                    EventSeed::new(EventKind::AssistantText { text: p["message"].as_str().unwrap_or_default().into() })
                        .at(at),
                ),
                "task_started" | "turn_started" => out.push(
                    EventSeed::new(EventKind::TurnStart { turn_id: p["turn_id"].as_str().map(String::from) }).at(at),
                ),
                "task_complete" | "turn_complete" => out.push(
                    EventSeed::new(match p["error"].as_str().filter(|e| !e.is_empty()) {
                        Some(error) => EventKind::TurnEnd {
                            reason: TurnEndReason::Error,
                            error: Some(error.into()),
                            background_agents: None,
                        },
                        None => {
                            EventKind::TurnEnd { reason: TurnEndReason::EndTurn, error: None, background_agents: None }
                        }
                    })
                    .at(at),
                ),
                "turn_aborted" => out.push(
                    EventSeed::new(EventKind::TurnEnd {
                        reason: TurnEndReason::Aborted,
                        error: p["reason"].as_str().map(String::from),
                        background_agents: None,
                    })
                    .at(at),
                ),
                "token_count" => {
                    // A running total for the session, not a per-call delta.
                    let total = &p["info"]["total_token_usage"];
                    if !total.is_null() {
                        let reported = crate::TokenUsage {
                            input_tokens: total["input_tokens"].as_u64(),
                            output_tokens: total["output_tokens"].as_u64(),
                            cache_read_input_tokens: total["cached_input_tokens"].as_u64(),
                            cache_creation_input_tokens: None,
                        };
                        let delta = increase(state.codex_usage, reported);
                        state.codex_usage = reported;
                        out.push(
                            EventSeed::new(EventKind::Usage {
                                usage: delta,
                                reported,
                                message_id: p["turn_id"].as_str().map(String::from),
                                cumulative: true,
                            })
                            .at(at),
                        );
                    }
                }
                subtype if known_event_msg(subtype) => out.push(meta(e).at(at)),
                _ => out.push(meta(e).at(at)),
            },
            "response_item" => match p["type"].as_str().unwrap_or_default() {
                "function_call" | "custom_tool_call" => {
                    let name = p["name"].as_str().unwrap_or_default();
                    out.push(
                        EventSeed::new(EventKind::ToolCall {
                            name: name.into(),
                            call_id: p["call_id"].as_str().filter(|id| !id.is_empty()).map(String::from),
                            kind: ToolKind::of(name, p),
                            input: p.clone(),
                            asks_user: crate::Harness::Codex.discovery().asks_user(name),
                            line: describe_tool(name, p),
                        })
                        .at(at),
                    );
                }
                "function_call_output" | "custom_tool_call_output" => out.push(
                    EventSeed::new(EventKind::ToolResult {
                        call_id: p["call_id"].as_str().filter(|id| !id.is_empty()).map(String::from),
                        is_error: p["is_error"].as_bool() == Some(true),
                        size: p["output"].as_str().map_or(0, |output| output.len() as u64),
                    })
                    .at(at),
                ),
                "message" if p["role"] == "assistant" => {
                    let text = content_text(&p["content"]);
                    out.push(EventSeed::new(EventKind::AssistantText { text }).at(at));
                    for block in p["content"].as_array().into_iter().flatten() {
                        let kind = block["type"].as_str().unwrap_or_default();
                        if !matches!(kind, "input_text" | "output_text") && !kind.is_empty() {
                            let event = if known_content(kind) {
                                EventKind::Meta {
                                    record_type: "content".into(),
                                    subtype: Some(kind.into()),
                                    record: block.clone(),
                                }
                            } else {
                                EventKind::Unknown {
                                    record_type: Some("content".into()),
                                    reason: format!("unknown Codex content type \"{kind}\""),
                                    record: block.clone(),
                                }
                            };
                            out.push(EventSeed::new(event).at(at));
                        }
                    }
                }
                "reasoning" => out.push(
                    EventSeed::new(EventKind::Reasoning {
                        signature: p["encrypted_content"].as_str().map(String::from),
                    })
                    .at(at),
                ),
                "compacted" => out.push(
                    EventSeed::new(EventKind::Compaction {
                        trigger: p["trigger"].as_str().map(String::from),
                        pre_tokens: p["pre_tokens"].as_u64(),
                        post_tokens: p["post_tokens"].as_u64(),
                    })
                    .at(at),
                ),
                subtype if known_response_item(subtype) => out.push(meta(e).at(at)),
                _ => out.push(meta(e).at(at)),
            },
            kind if known_record(kind) => out.push(meta(e).at(at)),
            kind => out.push(
                EventSeed::new(EventKind::Unknown {
                    record_type: (!kind.is_empty()).then(|| kind.into()),
                    reason: "unknown Codex record type".into(),
                    record: e.clone(),
                })
                .at(at),
            ),
        }
        if out.is_empty() {
            out.push(meta(e).at(at));
        }
        out
    }
}

fn session_info(cwd: Option<&str>, branch: Option<&str>, p: &Value) -> Option<crate::event::EventKind> {
    let cwd = cwd.map(String::from);
    let branch = branch.map(String::from);
    let session_id = p["id"].as_str().or_else(|| p["session_id"].as_str()).map(String::from);
    let reviewer = p["approvals_reviewer"].as_str().map(String::from);
    (cwd.is_some() || branch.is_some() || session_id.is_some() || reviewer.is_some()).then_some(
        crate::event::EventKind::SessionInfo {
            cwd,
            branch,
            session_id,
            entrypoint: None,
            approvals_reviewer: reviewer,
        },
    )
}

fn meta(e: &Value) -> EventSeed {
    EventSeed::new(crate::event::EventKind::Meta {
        record_type: e["type"].as_str().unwrap_or_default().into(),
        subtype: e["payload"]["type"].as_str().map(String::from),
        record: e.clone(),
    })
}

fn known_record(kind: &str) -> bool {
    matches!(
        kind,
        "session_meta"
            | "response_item"
            | "inter_agent_communication"
            | "inter_agent_communication_metadata"
            | "compacted"
            | "turn_context"
            | "token_usage_record"
            | "world_state"
            | "retained_context"
            | "security_risk_score"
            | "event_msg"
            | "realtime_item"
    )
}

fn known_event_msg(kind: &str) -> bool {
    matches!(kind, "token_count" | "task_complete" | "task_started" | "user_message" | "agent_message")
}

fn known_response_item(kind: &str) -> bool {
    matches!(
        kind,
        "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "message"
            | "reasoning"
            | "compacted"
    )
}

/// Whether this session's prompts come from entries of `kind`. A version that writes a
/// prompt both ways would otherwise count it twice; the first kind seen wins.
fn prompts_from(state: &mut crate::event::ParseState, kind: &'static str) -> bool {
    match state.codex_prompt_source {
        None => {
            state.codex_prompt_source = Some(kind);
            true
        }
        Some(seen) => seen == kind,
    }
}

fn known_content(kind: &str) -> bool {
    matches!(
        kind,
        "input_text"
            | "output_text"
            | "input_image"
            | "computer_screenshot"
            | "encrypted_content"
            | "summary_text"
            | "reasoning_text"
    )
}

fn content_text(content: &Value) -> String {
    content.as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

fn increase(previous: crate::TokenUsage, next: crate::TokenUsage) -> crate::TokenUsage {
    let inc = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (_, None) => None,
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
    };
    crate::TokenUsage {
        input_tokens: inc(previous.input_tokens, next.input_tokens),
        output_tokens: inc(previous.output_tokens, next.output_tokens),
        cache_read_input_tokens: inc(previous.cache_read_input_tokens, next.cache_read_input_tokens),
        cache_creation_input_tokens: inc(previous.cache_creation_input_tokens, next.cache_creation_input_tokens),
    }
}

fn describe_tool(name: &str, p: &Value) -> Option<ActivityLine> {
    match name {
        // Polling a command that's still running: not a new action.
        "write_stdin" => None,
        "exec_command" | "shell" | "shell_command" => {
            let args: Value = serde_json::from_str(p["arguments"].as_str()?).ok()?;
            let cmd = args["cmd"].as_str().or(args["command"].as_str()).map(String::from).or_else(|| {
                // Older Codex: {"command": ["bash", "-lc", "..."]}
                args["command"].as_array()?.last()?.as_str().map(String::from)
            })?;
            Some(command(&cmd))
        }
        "apply_patch" => {
            let patch = p["input"].as_str().unwrap_or_default();
            let file = patch.lines().find_map(|l| {
                l.strip_prefix("*** Update File: ")
                    .or(l.strip_prefix("*** Add File: "))
                    .or(l.strip_prefix("*** Delete File: "))
            });
            Some(tool("Edit", file.map(file_name).unwrap_or_default()))
        }
        other => Some(ActivityLine::Tool { name: other.into(), detail: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::harness::Harness;
    use crate::session::{Last, Session};
    use crate::testutil::{at, fixture, jsonl, log, replay, status};
    use serde_json::json;

    const STALL_AFTER: f64 = 20.0;

    fn session(events: &[Value]) -> Session {
        replay(Harness::Codex, events)
    }

    fn event(kind: &str, payload: Value) -> Value {
        json!({"timestamp": at(1.0), "type": kind, "payload": payload})
    }

    #[test]
    fn follows_a_session() {
        let mut s = session(&[
            event("session_meta", json!({"cwd": "/work/jooba", "git": {"branch": "feat"}})),
            event("event_msg", json!({"type": "user_message", "message": "Fix routing\nplease"})),
        ]);
        assert_eq!((s.project.as_str(), s.branch.as_str(), s.title.as_str()), ("jooba", "feat", "Fix routing"));
        assert_eq!(status(&s), (Status::Working, "thinking"));

        s.apply(&event("event_msg", json!({"type": "user_message", "message": "and another"})));
        assert_eq!(s.title, "Fix routing", "title is the opening prompt");

        s.apply(&event(
            "response_item",
            json!({"type": "function_call", "name": "exec_command",
                                                "arguments": "{\"cmd\":\"rg -n foo .\"}"}),
        ));
        assert_eq!(status(&s), (Status::Working, "working"));
        assert_eq!(log(&s).last().unwrap(), "$ rg -n foo .");

        let before = s.log.len();
        s.apply(&event("response_item", json!({"type": "function_call", "name": "write_stdin", "arguments": "{}"})));
        assert_eq!(s.log.len(), before, "polling a running command isn't logged");

        s.apply(&event("response_item", json!({"type": "function_call_output", "output": "..."})));
        assert_eq!(status(&s), (Status::Working, "thinking"));

        s.apply(&event(
            "response_item",
            json!({"type": "custom_tool_call", "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: /work/jooba/src/routing.ts\n@@"}),
        ));
        assert_eq!(log(&s).last().unwrap(), "Edit routing.ts");

        for n in [123, 150] {
            s.apply(&event(
                "event_msg",
                json!({"type": "token_count",
                "info": {"total_token_usage": {"input_tokens": 900, "output_tokens": n, "cached_input_tokens": 50}}}),
            ));
            assert_eq!(s.out_tokens, n, "Codex reports running totals");
        }
        assert_eq!((s.usage.input_tokens, s.usage.cache_read_input_tokens), (Some(900), Some(50)));

        s.apply(&event("event_msg", json!({"type": "task_complete"})));
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(s.harness.name(), "Codex");
    }

    #[test]
    fn reads_prompts_in_either_format_once() {
        let item = |text: &str| {
            event(
                "event_msg",
                json!({"type": "item_completed",
                "item": {"type": "UserMessage", "content": [{"type": "text", "text": text}]}}),
            )
        };
        let s = session(&[item("Ship it\nnow")]);
        assert_eq!((s.title.as_str(), log(&s)), ("Ship it", vec!["› Ship it".to_string()]));
        let legacy = |text: &str| event("event_msg", json!({"type": "user_message", "message": text}));
        let s = session(&[item("Ship it"), legacy("Ship it"), item("Ship it")]);
        assert_eq!(log(&s), ["› Ship it", "› Ship it"], "recorded both ways, counted once each; resent, counted again");
        let s = session(&[legacy("a"), item("a"), legacy("b")]);
        assert_eq!(log(&s), ["› a", "› b"], "the first way seen wins");
        let other = event(
            "event_msg",
            json!({"type": "item_completed", "item": {"type": "AgentMessage",
            "content": [{"type": "Text", "text": "hello"}]}}),
        );
        assert!(session(&[other]).log.is_empty(), "only the user's messages are prompts");
    }

    #[test]
    fn follows_the_folder_each_turn() {
        let s = session(&[
            event("session_meta", json!({"cwd": "/w/one"})),
            event("turn_context", json!({"cwd": "/w/two", "model": "m"})),
        ]);
        assert_eq!(s.project, "two");
    }

    #[test]
    fn a_never_approval_policy_runs_the_tool_instead_of_stalling() {
        for sandbox in ["read-only", "workspace-write", "danger-full-access"] {
            let events = [
                event(
                    "turn_context",
                    json!({"cwd": "/w", "approval_policy": "never", "approvals_reviewer": "user",
                           "sandbox_policy": {"type": sandbox}}),
                ),
                event(
                    "response_item",
                    json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
                ),
            ];
            let mut s = session(&events);
            s.last_at -= STALL_AFTER * 2.0;
            let st = s.state(
                crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
                &crate::ReducerOptions::default(),
                crate::Diagnostics::default(),
            );
            assert_eq!((st.activity, st.certainty), (crate::Activity::RunningTool, crate::Certainty::Firm));
            assert_eq!(st.permission, Some(PermissionMode::Never));
            assert_eq!(s.sandbox, Some(SandboxPolicy::parse(sandbox)));
            assert_eq!(status(&s), (Status::Working, "working"), "{sandbox}");
        }
    }

    #[test]
    fn an_on_request_policy_keeps_the_inferred_stall() {
        let events = [
            event(
                "turn_context",
                json!({"cwd": "/w", "approval_policy": "on-request", "approvals_reviewer": "user",
                       "sandbox_policy": {"type": "read-only"}}),
            ),
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            ),
        ];
        let mut s = session(&events);
        s.last_at -= STALL_AFTER * 2.0;
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!((st.activity, st.certainty), (crate::Activity::AwaitingApproval, crate::Certainty::Inferred));
        assert_eq!(st.permission, Some(PermissionMode::OnRequest));
        assert_eq!(st.sandbox, Some(SandboxPolicy::ReadOnly));
        assert_eq!(status(&s), (Status::NeedsYou, "may need approval"));
    }

    #[test]
    fn an_ai_reviewer_is_not_a_human() {
        let events = [
            event(
                "turn_context",
                json!({"cwd": "/w", "approval_policy": "on-request", "approvals_reviewer": "auto_review",
                       "sandbox_policy": {"type": "workspace-write"}}),
            ),
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            ),
        ];
        let mut s = session(&events);
        s.last_at -= STALL_AFTER * 2.0;
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!((st.activity, st.certainty), (crate::Activity::RunningTool, crate::Certainty::Firm));
        assert_eq!(st.permission, Some(PermissionMode::OnRequest));
        assert_eq!(st.sandbox, Some(SandboxPolicy::WorkspaceWrite));
        assert!(!s.can_ask_human(), "the reviewer is an AI, not a person");
    }

    #[test]
    fn a_later_user_reviewer_restores_the_stall() {
        let events = [
            event(
                "turn_context",
                json!({"approval_policy": "on-request", "approvals_reviewer": "auto_review",
                       "sandbox_policy": {"type": "workspace-write"}}),
            ),
            event(
                "turn_context",
                json!({"approval_policy": "on-request", "approvals_reviewer": "user",
                       "sandbox_policy": {"type": "workspace-write"}}),
            ),
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            ),
        ];
        let mut s = session(&events);
        s.last_at -= STALL_AFTER * 2.0;
        assert!(s.can_ask_human());
        assert_eq!(status(&s), (Status::NeedsYou, "may need approval"));
    }

    #[test]
    fn granular_approval_modes_say_whether_they_can_ask() {
        let granular = |config: Value| {
            event(
                "turn_context",
                json!({"approval_policy": {"granular": config},
                       "approvals_reviewer": "user", "sandbox_policy": {"type": "workspace-write"}}),
            )
        };
        let call = || {
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            )
        };
        let no_asks = granular(json!({"sandbox_approval": false, "rules": false, "skill_approval": false,
                                      "request_permissions": false, "mcp_elicitations": false}));
        let mut s = session(&[no_asks, call()]);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(s.permission, Some(PermissionMode::GranularNoAsk));
        assert_eq!(status(&s), (Status::Working, "working"));

        let rules = granular(json!({"sandbox_approval": false, "rules": true}));
        let mut s = session(&[rules, call()]);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(s.permission, Some(PermissionMode::GranularAsk));
        assert_eq!(status(&s), (Status::NeedsYou, "may need approval"));

        let unknown = granular(json!({"something_new": true}));
        let mut s = session(&[unknown, call()]);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(s.permission, Some(PermissionMode::Unknown));
        assert_eq!(s.sandbox, Some(SandboxPolicy::WorkspaceWrite), "the rest of turn_context is still read");
        assert_eq!(status(&s), (Status::NeedsYou, "may need approval"));
    }

    #[test]
    fn approval_aliases_and_sandboxes_are_parsed() {
        assert_eq!(PermissionMode::parse("on-failure"), PermissionMode::OnRequest);
        for (policy, want) in [
            ("danger-full-access", SandboxPolicy::DangerFullAccess),
            ("read-only", SandboxPolicy::ReadOnly),
            ("external-sandbox", SandboxPolicy::ExternalSandbox),
            ("workspace-write", SandboxPolicy::WorkspaceWrite),
            ("something-new", SandboxPolicy::Unknown),
        ] {
            assert_eq!(SandboxPolicy::parse(policy), want, "{policy}");
        }
    }

    #[test]
    fn parallel_calls_stay_running_until_every_result_arrives() {
        let call = |id: &str| {
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "call_id": id,
                       "arguments": "{\"cmd\":\"sleep 1\"}"}),
            )
        };
        let out =
            |id: &str| event("response_item", json!({"type": "function_call_output", "call_id": id, "output": "ok"}));
        let s = session(&[call("c1"), call("c2")]);
        assert_eq!(status(&s), (Status::Working, "working"));

        let s = session(&[call("c1"), call("c2"), out("c1")]);
        assert_eq!(status(&s), (Status::Working, "working"), "c2 is still pending");

        let s = session(&[call("c1"), call("c2"), out("c1"), out("c2")]);
        assert_eq!(status(&s), (Status::Working, "thinking"), "all results in");

        let s = session(&[call("c1"), call("c2"), out("other"), out("c2"), out("c1")]);
        assert_eq!(status(&s), (Status::Working, "thinking"), "an unknown id doesn't end another call");
    }

    #[test]
    fn a_question_among_parallel_calls_keeps_priority() {
        let ask = event(
            "response_item",
            json!({"type": "function_call", "name": "request_user_input", "call_id": "q1", "arguments": "{}"}),
        );
        let shell = event(
            "response_item",
            json!({"type": "function_call", "name": "exec_command", "call_id": "c1",
                   "arguments": "{\"cmd\":\"ls\"}"}),
        );
        let out =
            |id: &str| event("response_item", json!({"type": "function_call_output", "call_id": id, "output": "ok"}));

        let s = session(&[shell.clone(), ask.clone()]);
        assert_eq!(status(&s), (Status::NeedsYou, "needs your answer"));
        let s = session(&[shell.clone(), ask.clone(), out("c1")]);
        assert_eq!(status(&s), (Status::NeedsYou, "needs your answer"), "the question is still pending");
        let s = session(&[shell.clone(), ask.clone(), out("q1")]);
        assert_eq!(status(&s), (Status::Working, "working"), "the shell is still pending");
        let s = session(&[shell, ask, out("q1"), out("c1")]);
        assert_eq!(status(&s), (Status::Working, "thinking"));
    }

    #[test]
    fn asking_the_user_waits_for_them() {
        let ask =
            event("response_item", json!({"type": "function_call", "name": "request_user_input", "arguments": "{}"}));
        let mut s = session(&[ask]);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(status(&s), (Status::NeedsYou, "needs your answer"));
    }

    #[test]
    fn turns_end_however_they_end() {
        for (kind, last) in
            [("task_complete", Last::EndTurn), ("turn_complete", Last::EndTurn), ("turn_aborted", Last::Interrupted)]
        {
            let s = session(&[
                event("event_msg", json!({"type": "task_started"})),
                event("event_msg", json!({"type": kind})),
            ]);
            assert_eq!(status(&s), (Status::Done, "done"), "{kind}");
            assert_eq!(s.last, last, "{kind}");
        }
        let s = session(&[event("event_msg", json!({"type": "turn_started"}))]);
        assert_eq!(status(&s), (Status::Working, "thinking"));
    }

    #[test]
    fn a_task_error_is_errored() {
        let s = session(&[event("event_msg", json!({"type": "task_complete", "error": "stream disconnected"}))]);
        assert_eq!(s.last, Last::Errored);
        assert_eq!(s.error.as_deref(), Some("stream disconnected"));
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn describes_tools() {
        let d = |name, p: Value| describe_tool(name, &p).map(|l| l.to_string());
        let shell = json!({"arguments": "{\"command\":[\"bash\",\"-lc\",\"ls -la\\nmore\"]}"});
        assert_eq!(d("shell", shell).unwrap(), "$ ls -la");
        let shell_command = json!({"arguments": "{\"command\":\"git status\"}"});
        assert_eq!(d("shell_command", shell_command).unwrap(), "$ git status");
        let add = json!({"input": "*** Begin Patch\n*** Add File: new.md\n+hi"});
        assert_eq!(d("apply_patch", add).unwrap(), "Edit new.md");
        assert_eq!(d("exec_command", json!({"arguments": "not json"})), None);
        assert_eq!(d("view_image", json!({})).unwrap(), "view_image");
    }

    /// Real rollouts from Codex CLI 0.160.1 (`codex exec`, then `codex exec resume`), run
    /// against a stand-in model; scrubbed of paths and trimmed of long instructions.
    const EXEC: &str = include_str!("../../../../fixtures/legacy/codex-0.160-exec.jsonl");
    const RESUMED: &str = include_str!("../../../../fixtures/legacy/codex-0.160-resume.jsonl");

    #[test]
    fn reads_a_recorded_session() {
        let s = session(&fixture(EXEC, 2.0));
        assert_eq!((s.project.as_str(), s.branch.as_str()), ("demo-app", "feature/fixtures"));
        assert_eq!(s.title, "Please list the files in this repo");
        assert_eq!(log(&s), ["› Please list the files in this repo", "$ ls -la"]);
        assert_eq!(s.out_tokens, 84);
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn reads_a_recorded_resumed_session() {
        let events = fixture(RESUMED, 2.0);
        let s = session(&events);
        assert_eq!(s.title, "Add a notes file with a patch", "the opening prompt, not the latest");
        assert_eq!(
            log(&s),
            [
                "› Add a notes file with a patch",
                "Edit hello.md",
                "$ ls -la",
                "› Now ask me which approach to take",
                "request_user_input"
            ]
        );
        assert_eq!(s.out_tokens, 210);
        assert_eq!(status(&s), (Status::Done, "done"));

        // Stopped while the question is open, it waits for the user.
        let ask = events.iter().position(|e| e["payload"]["name"] == "request_user_input").unwrap();
        let s = session(&fixture(&jsonl(&events[..=ask]), 60.0));
        assert_eq!(status(&s), (Status::NeedsYou, "needs your answer"));
        // And between turns of the first prompt, it's working.
        let patch = events.iter().position(|e| e["payload"]["name"] == "apply_patch").unwrap();
        let s = session(&fixture(&jsonl(&events[..=patch]), 1.0));
        assert_eq!(status(&s), (Status::Working, "working"));
    }
}
