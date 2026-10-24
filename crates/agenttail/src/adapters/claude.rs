//! Claude Code: `~/.claude/projects/<project>/<session id>.jsonl`, one JSON entry per line
//! ("user" / "assistant" messages, plus "ai-title", "cost-state" and bookkeeping we skip).

use super::Adapter;
use crate::session::{Last, Session, command, file_name, first_line, tool};
use crate::state::{ActivityLine, PermissionMode};
use serde_json::Value;

pub(crate) struct ClaudeCode;

impl Adapter for ClaudeCode {
    fn apply(&self, s: &mut Session, e: &Value) {
        if let Some(cwd) = e["cwd"].as_str() {
            s.set_cwd(cwd);
        }
        if let Some(b) = e["gitBranch"].as_str().filter(|b| !b.is_empty()) {
            s.branch = b.into();
        }
        if let Some(entrypoint) = e["entrypoint"].as_str() {
            s.set_entrypoint(entrypoint);
        }
        // A `permission-mode` record and a prompt both carry the current mode; `mode`
        // records carry something else (`normal`), so only `permissionMode` is read.
        if let Some(mode) = e["permissionMode"].as_str() {
            s.set_permission(Some(PermissionMode::parse(mode)));
        }
        match e["type"].as_str() {
            Some("ai-title") => s.title = e["aiTitle"].as_str().unwrap_or_default().into(),
            Some("cost-state") => {
                // Written as a string or a number depending on version.
                let c = &e["totalCostUSD"];
                if let Some(cost) = c.as_f64().or_else(|| c.as_str()?.parse().ok()) {
                    s.cost = Some(cost);
                }
            }
            Some("user") if s.is_helper || e["isSidechain"] != true => user(s, e),
            Some("assistant") if s.is_helper || e["isSidechain"] != true => assistant(s, e),
            Some("system") => system(s, e),
            _ => {}
        }
    }
}

fn interrupted(s: &str) -> bool {
    s.starts_with("[Request interrupted")
}

fn user(s: &mut Session, e: &Value) {
    let t = s.time_of(e);
    let content = &e["message"]["content"];
    // The same channel carries subagent completions; they are not prompts.
    if e["origin"]["kind"] == "task-notification" {
        if let Some(status) = task_status(content) {
            // completed, failed or stopped: either way the helper is no longer pending.
            if ["completed", "failed", "stopped"].contains(&status) {
                s.background_agent_finished();
            }
        }
        return;
    }
    // A rejected tool result ends the turn: on 2.1.310 it is followed by the interrupt text
    // and `turn_duration`, but some builds (and a transcript read mid-flush) may show only
    // the rejection. CDT treats the same string as an ending event.
    if tool_rejected(e) {
        s.set(Last::EndTurn, t);
        return;
    }
    if let Some(text) = content.as_str() {
        if interrupted(text) {
            s.set(Last::Interrupted, t);
        } else if e["isMeta"] != true && e["isCompactSummary"] != true && !text.starts_with('<') {
            s.prompt(text, t);
        }
    } else if let Some(items) = content.as_array() {
        let texts = || items.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str());
        if items.iter().any(|c| c["type"] == "tool_result") {
            s.set(Last::ToolResult, t);
        } else if texts().any(interrupted) {
            s.set(Last::Interrupted, t);
        } else if e["isMeta"] != true && e["isCompactSummary"] != true {
            // A prompt in blocks: text with a pasted image, say.
            if let Some(text) = texts().find(|t| !t.starts_with('<')) {
                s.prompt(text, t);
            } else if items.iter().any(|c| c["type"] == "image") {
                s.prompt("", t);
            }
        }
    }
}

fn assistant(s: &mut Session, e: &Value) {
    let t = s.time_of(e);
    let msg = &e["message"];
    // A synthetic assistant row recording an API failure is the firm Errored signal. Its
    // `stop_reason` is `stop_sequence`, which is not a turn end.
    if e["isApiErrorMessage"] == true || e["error"].is_string() {
        let error = e["error"].as_str().filter(|e| !e.is_empty()).unwrap_or("api_error");
        s.set_error(error, t);
        return;
    }
    // One API message can span several transcript entries; dedupe its usage by message id.
    let id = msg["id"].as_str().unwrap_or_default();
    let usage = &msg["usage"];
    s.record_usage(
        id,
        crate::TokenUsage {
            input_tokens: usage["input_tokens"].as_u64(),
            output_tokens: usage["output_tokens"].as_u64(),
            cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64(),
            cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
        },
    );
    for c in msg["content"].as_array().into_iter().flatten() {
        if c["type"] == "tool_use" {
            let name = c["name"].as_str().unwrap_or_default();
            let asks_user = s.harness.discovery().asks_user(name);
            s.tool_call(asks_user, Some(describe_tool(name, &c["input"])), t);
        }
    }
    if msg["stop_reason"] == "end_turn" {
        s.set(Last::EndTurn, t);
    }
}

fn system(s: &mut Session, e: &Value) {
    let t = s.time_of(e);
    match e["subtype"].as_str() {
        // A normal turn's firm end in order: assistant `end_turn`, then `stop_hook_summary`
        // when hooks ran, then `turn_duration` (about 0.06 s later). The duration record is
        // the firmest and also carries a firm count of helpers still running.
        Some("stop_hook_summary" | "turn_duration") => {
            end_turn(s, t);
            if e["subtype"] == "turn_duration"
                && let Some(n) = e["pendingBackgroundAgentCount"].as_u64()
            {
                s.set_background_agents(n as usize);
            }
        }
        // A session exiting with its helpers: the turn is over, nothing is left running.
        Some("agents_killed") => {
            end_turn(s, t);
            s.set_background_agents(0);
        }
        _ => {}
    }
}

/// A `system` record ends the turn, but never replaces a recorded API error: that row is the
/// error, and these records only timestamp the close.
fn end_turn(s: &mut Session, t: f64) {
    if s.last == Last::Errored {
        s.last_at = t;
    } else {
        s.set(Last::EndTurn, t);
    }
}

/// Whether this user record is Claude Code's record of a rejected tool use (`toolUseResult`
/// "User rejected tool use", `toolDenialKind` user-rejected, or the result text).
fn tool_rejected(e: &Value) -> bool {
    if e["toolDenialKind"].as_str() == Some("user-rejected")
        || e["toolUseResult"].as_str() == Some("User rejected tool use")
    {
        return true;
    }
    // Only tool results count here. A user's own prompt may quote the phrase (and the
    // driver's own tests do), which must not end the turn.
    let texts = e["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "tool_result")
        .filter_map(|c| result_text(&c["content"]))
        .collect::<Vec<_>>();
    texts.into_iter().any(|text| {
        text.starts_with("User rejected tool use")
            || text.starts_with("The user doesn't want to proceed with this tool use")
    })
}

fn result_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) => Some(text),
        Value::Array(parts) => parts.iter().find_map(|part| part["text"].as_str()),
        _ => None,
    }
}

/// The `<status>` a `task-notification` carries, if it has one.
fn task_status(content: &Value) -> Option<&str> {
    let text = match content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => blocks.iter().find_map(|b| b["text"].as_str())?,
        _ => return None,
    };
    let rest = text.split_once("<status>")?.1;
    rest.split_once("</status>").map(|(status, _)| status.trim())
}

fn describe_tool(name: &str, input: &Value) -> ActivityLine {
    if name == "Bash" {
        return command(input["command"].as_str().unwrap_or_default());
    }
    for key in ["file_path", "path", "pattern", "url", "query", "description", "prompt"] {
        if let Some(v) = input[key].as_str().filter(|v| !v.is_empty()) {
            let v = if key.ends_with("path") { file_name(v) } else { first_line(v).to_string() };
            return tool(name, v);
        }
    }
    ActivityLine::Tool { name: name.into(), detail: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::harness::Harness;
    use crate::testutil::{at, fixture, jsonl, log, replay, status};
    use serde_json::json;
    use std::path::Path;

    const STALL_AFTER: f64 = 20.0;

    fn session(events: &[Value]) -> Session {
        replay(Harness::ClaudeCode, events)
    }

    fn prompt(text: &str, ago: f64) -> Value {
        json!({"type": "user", "timestamp": at(ago), "cwd": "/work/proj", "gitBranch": "main",
               "message": {"role": "user", "content": text}})
    }

    fn tool_use(name: &str, input: Value, ago: f64, id: &str) -> Value {
        json!({"type": "assistant", "timestamp": at(ago),
               "message": {"id": id, "stop_reason": "tool_use", "usage": {"output_tokens": 10},
                           "content": [{"type": "tool_use", "name": name, "input": input}]}})
    }

    fn tool_result(ago: f64) -> Value {
        json!({"type": "user", "timestamp": at(ago),
               "message": {"content": [{"type": "tool_result", "content": "ok"}]}})
    }

    fn end_turn(ago: f64, id: &str) -> Value {
        json!({"type": "assistant", "timestamp": at(ago),
               "message": {"id": id, "stop_reason": "end_turn", "usage": {"output_tokens": 5},
                           "content": [{"type": "text", "text": "all done"}]}})
    }

    #[test]
    fn follows_a_turn_from_prompt_to_done() {
        let mut s = session(&[prompt("\n   fix the build\nand then the tests", 5.0)]);
        assert_eq!(status(&s), (Status::Working, "thinking"));
        assert_eq!(log(&s), ["› fix the build"]);
        assert_eq!((s.project.as_str(), s.branch.as_str()), ("proj", "main"));

        s.apply(&tool_use("Bash", json!({"command": "cargo test\n--release"}), 3.0, "m1"));
        assert_eq!(status(&s), (Status::Working, "working"));
        assert_eq!(log(&s).last().unwrap(), "$ cargo test");

        s.apply(&tool_result(2.0));
        assert_eq!(status(&s), (Status::Working, "thinking"));

        s.apply(&end_turn(1.0, "m2"));
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn a_tool_call_without_result_looks_like_an_approval_prompt() {
        let s = session(&[tool_use("Bash", json!({"command": "rm -rf build"}), STALL_AFTER + 10.0, "m1")]);
        assert_eq!(status(&s), (Status::NeedsYou, "may need approval"));
    }

    #[test]
    fn a_no_approval_mode_runs_the_tool_instead_of_stalling() {
        // The headline B1 rule: bypass/dontAsk cannot put a prompt in front of a human.
        for mode in ["bypassPermissions", "dontAsk"] {
            let mut events = vec![json!({"type": "permission-mode", "timestamp": at(31.0), "permissionMode": mode})];
            events.push(tool_use("Bash", json!({"command": "sleep 60"}), STALL_AFTER + 10.0, "m1"));
            let s = session(&events);
            assert_eq!(status(&s), (Status::Working, "working"), "{mode}");
            let st = s.state(
                crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
                &crate::ReducerOptions::default(),
                crate::Diagnostics::default(),
            );
            assert_eq!(
                (st.activity, st.certainty, st.reason),
                (crate::Activity::RunningTool, crate::Certainty::Firm, crate::Reason::ToolCallPending)
            );
            assert_eq!(st.permission, Some(PermissionMode::parse(mode)));
        }
    }

    #[test]
    fn a_default_mode_keeps_the_inferred_stall() {
        let events = [
            json!({"type": "permission-mode", "timestamp": at(31.0), "permissionMode": "default"}),
            tool_use("Bash", json!({"command": "sleep 60"}), STALL_AFTER + 10.0, "m1"),
        ];
        assert_eq!(status(&session(&events)), (Status::NeedsYou, "may need approval"));
    }

    #[test]
    fn an_sdk_entrypoint_has_no_human_to_ask() {
        let mut tool = tool_use("Bash", json!({"command": "sleep 60"}), STALL_AFTER + 10.0, "m1");
        tool["entrypoint"] = json!("sdk-cli");
        assert_eq!(status(&session(&[tool])), (Status::Working, "working"));
    }

    #[test]
    fn questions_and_plans_wait_for_the_user() {
        for &tool in Harness::ClaudeCode.discovery().asks_user {
            let s = session(&[tool_use(tool, json!({}), 1.0, "m1")]);
            assert_eq!(status(&s), (Status::NeedsYou, "needs your answer"), "{tool}");
        }
    }

    #[test]
    fn interruptions_are_their_own_activity() {
        let plain = json!({"type": "user", "timestamp": at(1.0),
                           "message": {"content": "[Request interrupted by user]"}});
        let listed = json!({"type": "user", "timestamp": at(1.0),
                            "message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}});
        for e in [plain, listed] {
            let s = session(&[tool_use("Bash", json!({}), 2.0, "m1"), e]);
            assert_eq!(status(&s), (Status::Done, "done"));
            assert_eq!(s.last, Last::Interrupted);
            assert_eq!(s.log.len(), 1, "an interruption isn't a prompt");
        }
    }

    #[test]
    fn turn_duration_and_agents_killed_end_the_turn() {
        for subtype in ["stop_hook_summary", "turn_duration", "agents_killed"] {
            let s = session(&[json!({"type": "system", "subtype": subtype, "timestamp": at(1.0)})]);
            assert_eq!(status(&s), (Status::Done, "done"), "{subtype}");
            assert_eq!(s.last, Last::EndTurn);
        }
    }

    #[test]
    fn a_turn_ends_in_order_and_last_prompt_is_not_an_end() {
        let mut s = session(&[prompt("go", 5.0)]);
        assert_eq!(s.last, Last::Prompt);

        s.apply(&end_turn(4.0, "m1"));
        assert_eq!((s.last, s.last_at), (Last::EndTurn, crate::time::parse_rfc3339(&at(4.0)).unwrap()));
        let after_end = s.last_at;

        // The hook summary and the duration record close the same turn, later.
        s.apply(&json!({"type": "system", "subtype": "stop_hook_summary", "timestamp": at(3.94)}));
        let after_hooks = s.last_at;
        assert!(after_hooks > after_end);
        assert_eq!(s.last, Last::EndTurn);
        s.apply(&json!({"type": "system", "subtype": "turn_duration", "timestamp": at(3.88)}));
        let after_duration = s.last_at;
        assert!(after_duration > after_hooks);
        assert_eq!(s.last, Last::EndTurn);

        // A metadata flush, not a turn marker: it moves nothing.
        s.apply(&json!({"type": "last-prompt", "lastPrompt": "go", "timestamp": at(3.8)}));
        assert_eq!((s.last, s.last_at), (Last::EndTurn, after_duration));
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn stop_hook_summary_after_an_api_error_stays_errored() {
        let error = json!({"type": "assistant", "timestamp": at(2.0),
                           "isApiErrorMessage": true, "error": "server_error",
                           "message": {"id": "m1", "stop_reason": "stop_sequence",
                                       "content": [{"type": "text", "text": "API error"}]}});
        let hooks = json!({"type": "system", "subtype": "stop_hook_summary", "timestamp": at(1.9)});
        let s = session(&[error, hooks]);
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!((st.activity, st.reason), (crate::Activity::Errored, crate::Reason::Errored));
        assert_eq!(st.error.as_deref(), Some("server_error"));
    }

    #[test]
    fn a_rejected_tool_use_ends_the_turn() {
        let denied = |content: Value| {
            json!({"type": "user", "timestamp": at(1.0), "permissionMode": "default",
                   "toolUseResult": "User rejected tool use", "toolDenialKind": "user-rejected",
                   "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": content}]}})
        };
        for content in [
            json!("The user doesn't want to proceed with this tool use. The tool use was rejected."),
            json!([{"type": "text", "text": "User rejected tool use"}]),
        ] {
            let s = session(&[tool_use("Bash", json!({"command": "rm -rf build"}), 2.0, "m1"), denied(content)]);
            assert_eq!(status(&s), (Status::Done, "done"));
            assert_eq!(s.last, Last::EndTurn, "a rejection is a firm end");
        }

        // The A3 transcript closes the rejected turn with its interrupt and duration, too.
        let s = session(&[
            tool_use("Bash", json!({"command": "rm -rf build"}), 4.0, "m1"),
            denied(json!("The user doesn't want to proceed with this tool use.")),
            json!({"type": "user", "timestamp": at(0.9), "message": {"content": [
                {"type": "text", "text": "[Request interrupted by user for tool use]"}]}}),
            json!({"type": "system", "subtype": "turn_duration", "timestamp": at(0.8)}),
        ]);
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(s.last, Last::EndTurn);

        // A model that merely says the words in its own reply does not end a turn.
        let quoted = json!({"type": "assistant", "timestamp": at(1.0),
                            "message": {"id": "m2", "stop_reason": "tool_use",
                                        "content": [{"type": "text", "text": "The transcript says User rejected tool use"}]}});
        let s = session(&[tool_use("Bash", json!({}), 2.0, "m1"), quoted]);
        assert_eq!(s.last, Last::ToolUse);

        let typed = prompt("User rejected tool use is what the docs say", 1.0);
        let s = session(&[end_turn(2.0, "m1"), typed]);
        assert_eq!(
            (s.last, log(&s)),
            (Last::Prompt, vec!["› User rejected tool use is what the docs say".to_string()])
        );
    }

    #[test]
    fn an_api_error_is_errored_and_keeps_the_kind() {
        let error = json!({"type": "assistant", "timestamp": at(1.0),
                           "isApiErrorMessage": true, "error": "server_error",
                           "message": {"id": "m1", "stop_reason": "stop_sequence",
                                       "usage": {"output_tokens": 0},
                                       "content": [{"type": "text", "text": "API error"}]}});
        let s = session(&[error]);
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!((st.activity, st.reason), (crate::Activity::Errored, crate::Reason::Errored));
        assert_eq!(st.error.as_deref(), Some("server_error"));
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn turn_duration_after_an_api_error_stays_errored() {
        let error = json!({"type": "assistant", "timestamp": at(1.0),
                           "isApiErrorMessage": true, "error": "server_error",
                           "message": {"id": "m1", "stop_reason": "stop_sequence",
                                       "usage": {"output_tokens": 0},
                                       "content": [{"type": "text", "text": "API error"}]}});
        let duration = json!({"type": "system", "subtype": "turn_duration", "timestamp": at(0.5),
                              "pendingBackgroundAgentCount": 2});
        let s = session(&[error, duration]);
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!((st.activity, st.reason), (crate::Activity::Errored, crate::Reason::Errored));
        assert_eq!(st.error.as_deref(), Some("server_error"));
        assert_ne!(st.activity, crate::Activity::BackgroundWork);
    }

    #[test]
    fn a_late_flushed_prompt_does_not_rewind_an_api_error() {
        let error = json!({"type": "assistant", "timestamp": at(1.0),
                           "isApiErrorMessage": true, "error": "server_error",
                           "message": {"id": "m1", "stop_reason": "stop_sequence",
                                       "content": [{"type": "text", "text": "API error"}]}});
        let duration = json!({"type": "system", "subtype": "turn_duration", "timestamp": at(0.5)});
        let late = prompt("trigger an error", 2.0);
        let s = session(&[error, duration, late]);
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!((st.activity, st.reason), (crate::Activity::Errored, crate::Reason::Errored));
        assert!(s.log.is_empty());
    }

    #[test]
    fn a_late_flushed_prompt_does_not_rewind_done() {
        let s = session(&[end_turn(1.0, "m1"), prompt("late flush", 2.0)]);
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(s.last, Last::EndTurn);
        assert!(s.log.is_empty(), "an older prompt is not a new turn");

        let s = session(&[end_turn(2.0, "m1"), prompt("next turn", 1.0)]);
        assert_eq!(status(&s), (Status::Working, "thinking"));
    }

    #[test]
    fn a_later_end_turn_clears_an_api_error() {
        let error = json!({"type": "assistant", "timestamp": at(2.0),
                           "isApiErrorMessage": true, "error": "server_error",
                           "message": {"id": "m1", "stop_reason": "stop_sequence",
                                       "content": [{"type": "text", "text": "API error"}]}});
        let s = session(&[error, end_turn(1.0, "m2")]);
        assert_eq!(s.last, Last::EndTurn);
        assert_eq!(status(&s), (Status::Done, "done"));
    }

    #[test]
    fn reads_prompts_in_blocks() {
        let blocks = json!({"type": "user", "timestamp": at(1.0), "message": {"content": [
            {"type": "text", "text": "why is this red?\nsee image"}, {"type": "image", "source": {}}]}});
        let s = session(&[end_turn(2.0, "m1"), blocks]);
        assert_eq!((status(&s), log(&s)), ((Status::Working, "thinking"), vec!["› why is this red?".to_string()]));
        let image =
            json!({"type": "user", "timestamp": at(1.0), "message": {"content": [{"type": "image", "source": {}}]}});
        let s = session(&[end_turn(2.0, "m1"), image]);
        assert_eq!((status(&s).0, s.log.len()), (Status::Working, 0), "an image alone is a prompt too");
        let meta = json!({"type": "user", "isMeta": true, "timestamp": at(1.0), "message": {"content": [{"type": "text", "text": "skill body"}]}});
        assert!(session(&[meta]).log.is_empty());
    }

    #[test]
    fn ignores_meta_command_and_sidechain_entries() {
        let meta = json!({"type": "user", "isMeta": true, "timestamp": at(1.0), "message": {"content": "caveat"}});
        let command = prompt("<command-name>/clear</command-name>", 1.0);
        let mut side = tool_use("Bash", json!({"command": "ls"}), 1.0, "m9");
        side["isSidechain"] = json!(true);
        let s = session(&[meta, command, side]);
        assert!(s.last == Last::Nothing && s.log.is_empty() && s.out_tokens == 0);
    }

    #[test]
    fn counts_tokens_once_per_api_message() {
        // One API message is split across several transcript entries with the same id.
        let s = session(&[
            tool_use("Read", json!({}), 3.0, "m1"),
            tool_use("Grep", json!({}), 3.0, "m1"),
            end_turn(1.0, "m2"),
        ]);
        assert_eq!(s.out_tokens, 15);
    }

    #[test]
    fn usage_repeated_or_growing_on_a_message_is_counted_once() {
        let row = |id: &str, input: u64, output: u64, cache_read: u64, cache_write: u64| {
            json!({"type": "assistant", "timestamp": at(1.0),
                   "message": {"id": id, "stop_reason": "tool_use",
                               "usage": {"input_tokens": input, "output_tokens": output,
                                         "cache_read_input_tokens": cache_read,
                                         "cache_creation_input_tokens": cache_write},
                               "content": [{"type": "text", "text": "…"}]}})
        };
        let s = session(&[
            row("m1", 2, 8, 0, 100),
            row("m1", 2, 8, 0, 100),   // identical row
            row("m1", 2, 135, 0, 100), // the final row grows
            row("m2", 7, 4, 20, 0),
        ]);
        assert_eq!(s.out_tokens, 139);
        assert_eq!((s.usage.input_tokens, s.usage.output_tokens), (Some(9), Some(139)));
        assert_eq!((s.usage.cache_read_input_tokens, s.usage.cache_creation_input_tokens), (Some(20), Some(100)));
    }

    #[test]
    fn reads_cost_as_string_or_number() {
        let mut s = session(&[json!({"type": "cost-state", "totalCostUSD": "1.25"})]);
        assert_eq!(s.cost, Some(1.25));
        s.apply(&json!({"type": "cost-state", "totalCostUSD": 2.5}));
        assert_eq!(s.cost, Some(2.5));
        s.apply(&json!({"type": "cost-state", "totalCostUSD": "n/a"}));
        assert_eq!(s.cost, Some(2.5), "unparseable cost keeps the last good value");
        assert_eq!(session(&[]).cost, None, "none recorded: none, not zero");
    }

    #[test]
    fn a_compact_summary_does_not_start_a_turn() {
        let s = session(&[
            prompt("remember the token", 5.0),
            end_turn(4.0, "m1"),
            json!({"type": "system", "subtype": "compact_boundary", "timestamp": at(3.0),
                   "compactMetadata": {"trigger": "manual", "preTokens": 100, "postTokens": 10}}),
            json!({"type": "user", "isCompactSummary": true, "timestamp": at(2.0),
                   "message": {"role": "user", "content": "summary of the token"}}),
        ]);
        assert_eq!(status(&s), (Status::Done, "done"));
        assert!(log(&s).iter().all(|line| !line.contains("summary of the token")));
        let blocks = session(&[
            prompt("remember the token", 5.0),
            end_turn(4.0, "m1"),
            json!({"type": "user", "isCompactSummary": true, "timestamp": at(2.0),
                   "message": {"role": "user", "content": [{"type": "text", "text": "summary in blocks"}]}}),
        ]);
        assert_eq!(status(&blocks), (Status::Done, "done"));
    }

    #[test]
    fn takes_the_generated_title() {
        let s = session(&[json!({"type": "ai-title", "aiTitle": "Fix flaky tests"})]);
        assert_eq!(s.title, "Fix flaky tests");
    }

    #[test]
    fn describes_tools_briefly() {
        let d = |name, input| describe_tool(name, &input).to_string();
        assert_eq!(d("Read", json!({"file_path": "/a/b/main.rs"})), "Read main.rs");
        assert_eq!(d("Glob", json!({"path": "/a/src/"})), "Glob src");
        assert_eq!(d("Grep", json!({"pattern": "TODO\nmore"})), "Grep TODO");
        assert_eq!(d("WebSearch", json!({"query": "dlna flags"})), "WebSearch dlna flags");
        assert_eq!(d("Bash", json!({})), "$ ");
        assert_eq!(d("Mystery", json!({"other": 1})), "Mystery");
    }

    #[test]
    fn entries_without_a_time_are_dated_by_the_write() {
        let mut s = session(&[]);
        s.mtime = crate::testutil::NOW - 5.0;
        s.apply(&json!({"type": "user", "message": {"content": "go"}}));
        assert_eq!(s.last_at, s.mtime);
    }

    /// A real Claude Code 2.1 session (scrubbed: prompts, tool inputs and outputs replaced,
    /// structure and bookkeeping entries kept): a prompt, Bash / Read / Write calls, a
    /// generated title and a cost entry.
    const RECORDED: &str = include_str!("../../../../fixtures/legacy/claude-code-2.1.jsonl");

    #[test]
    fn names_the_project_from_cwd_not_the_encoded_folder() {
        // A real cwd from this machine's ~/.claude/projects. The encoded folder ends in
        // `a197-integrations-grid`; the records moved into its `site` subdirectory.
        let path = Path::new(
            "/h/.claude/projects/-home-ubuntu-workspace--vibe-worktrees-tvdash-a197-integrations-grid/s.jsonl",
        );
        let mut s = Session::at(Harness::ClaudeCode, 5, path);
        let entry = json!({
            "type": "user",
            "timestamp": at(1.0),
            "cwd": "/home/ubuntu/workspace/.vibe-worktrees/tvdash/a197-integrations-grid/site",
            "message": {"role": "user", "content": "ship it"}
        });
        s.apply(&entry);
        assert_eq!(s.project, "site", "the cwd, not the folder's a197-integrations-grid");

        // The folder's encoding is lossy; the records still win when they disagree.
        let mut s = Session::at(Harness::ClaudeCode, 5, Path::new("/h/.claude/projects/wrong-name/s.jsonl"));
        s.apply(&json!({
            "type": "assistant",
            "timestamp": at(1.0),
            "cwd": "/home/ubuntu/workspace/.vibe-worktrees/jooba/lucky-mint-dingo",
            "message": {"id": "m1", "stop_reason": "end_turn", "content": []}
        }));
        assert_eq!(s.project, "lucky-mint-dingo");
    }

    #[test]
    fn project_folder_encoding_matches_real_cwds() {
        // Structure-only scan of ~/.claude/projects (2026-10-24): 233 of 243 folders had a
        // cwd record and every one matched this rule; the other 10 had no cwd. These are two
        // of those observed pairs.
        let encode =
            |cwd: &str| -> String { cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect() };
        for (cwd, folder) in [
            (
                "/home/ubuntu/workspace/.vibe-worktrees/jooba/lucky-mint-dingo",
                "-home-ubuntu-workspace--vibe-worktrees-jooba-lucky-mint-dingo",
            ),
            (
                "/home/ubuntu/workspace/.vibe-worktrees/tvdash/a197-integrations-grid",
                "-home-ubuntu-workspace--vibe-worktrees-tvdash-a197-integrations-grid",
            ),
        ] {
            assert_eq!(encode(cwd), folder);
        }
    }

    #[test]
    fn reads_a_recorded_session() {
        let events = fixture(RECORDED, 2.0);
        let s = session(&events);
        assert_eq!((s.project.as_str(), s.branch.as_str()), ("demo-app", "feature/fixtures"));
        assert_eq!(s.title, "Add a plan file and run the tests");
        assert!(s.cost.unwrap() > 0.0, "cost-state is read");
        assert!(s.out_tokens > 0);
        assert_eq!(status(&s), (Status::Done, "done"));
        let (bash, write) = ("$ cargo test --lib", "Write plan.md");
        assert_eq!(log(&s), [write, bash, bash, write, bash], "the last five of its eight tool calls");

        // Stopped right after its first tool call, the session is running that tool.
        let first_tool = events.iter().position(|e| e["message"]["content"][0]["type"] == "tool_use").unwrap();
        let s = session(&fixture(&jsonl(&events[..=first_tool]), 1.0));
        assert_eq!(status(&s), (Status::Working, "working"));
        assert_eq!(log(&s), ["› Prompt 1: add a plan file", "Read main.rs"]);
        // With more lines kept, more of the eight.
        let mut s = Session::new(Harness::ClaudeCode, 20);
        events.iter().for_each(|e| s.apply(e));
        assert_eq!(s.log.len(), 9, "the prompt and eight tool calls");
    }
}
