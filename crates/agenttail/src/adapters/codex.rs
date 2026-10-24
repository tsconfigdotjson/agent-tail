//! Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, one entry per line:
//! {"timestamp", "type": "session_meta" | "event_msg" | "response_item" | …, "payload"}.
//! Older rollouts are compressed to `.jsonl.zst`; only live `.jsonl` files are read.
//!
//! Checked against rollouts written by Codex CLI 0.160.1 (the fixtures) and earlier ones.

use super::Adapter;
use crate::session::{Last, Session, command, file_name, tool};
use crate::state::{ActivityLine, PermissionMode};
use serde_json::Value;

pub(crate) struct Codex;

impl Adapter for Codex {
    fn apply(&self, s: &mut Session, e: &Value) {
        let t = s.time_of(e);
        let p = &e["payload"];
        match (e["type"].as_str().unwrap_or_default(), p["type"].as_str().unwrap_or_default()) {
            ("session_meta", _) => {
                if let Some(cwd) = p["cwd"].as_str() {
                    s.set_cwd(cwd);
                }
                if let Some(b) = p["git"]["branch"].as_str() {
                    s.branch = b.into();
                }
            }
            // Each turn's settings; the folder can change within a session.
            ("turn_context", _) => {
                if let Some(cwd) = p["cwd"].as_str() {
                    s.set_cwd(cwd);
                }
                if let Some(policy) = p["approval_policy"].as_str() {
                    s.set_permission(Some(PermissionMode::parse(policy)));
                }
                s.set_approval_reviewer_auto(p["approvals_reviewer"].as_str() == Some("auto_review"));
            }
            // What the user typed: a `user_message` event in older rollouts, a completed
            // `UserMessage` item in 0.160. (The `response_item` copy also holds the
            // environment context and instructions, so it isn't used.)
            ("event_msg", "user_message") => {
                if prompts_from(s, "user_message") {
                    prompt(s, p["message"].as_str().unwrap_or_default(), t);
                }
            }
            ("event_msg", "item_completed") if p["item"]["type"] == "UserMessage" => {
                if prompts_from(s, "item") {
                    let blocks = p["item"]["content"].as_array().into_iter().flatten();
                    let text: Vec<&str> = blocks.filter_map(|c| c["text"].as_str()).collect();
                    prompt(s, &text.join("\n"), t);
                }
            }
            ("event_msg", "task_started" | "turn_started") => s.set(Last::Prompt, t),
            ("event_msg", "task_complete" | "turn_complete") => {
                if let Some(error) = p["error"].as_str().filter(|e| !e.is_empty()) {
                    s.set_error(error, t);
                } else {
                    s.set(Last::EndTurn, t);
                }
            }
            ("event_msg", "turn_aborted") => s.set(Last::Interrupted, t),
            ("event_msg", "token_count") => {
                // A running total for the session, not a per-call delta.
                let total = &p["info"]["total_token_usage"];
                if !total.is_null() {
                    s.set_usage(crate::TokenUsage {
                        input_tokens: total["input_tokens"].as_u64(),
                        output_tokens: total["output_tokens"].as_u64(),
                        cache_read_input_tokens: total["cached_input_tokens"].as_u64(),
                        cache_creation_input_tokens: None,
                    });
                }
            }
            ("response_item", "function_call" | "custom_tool_call") => {
                let name = p["name"].as_str().unwrap_or_default();
                let asks_user = s.harness.discovery().asks_user(name);
                let line = describe_tool(name, p);
                match p["call_id"].as_str().filter(|id| !id.is_empty()) {
                    Some(call_id) => s.tool_call_id(call_id, asks_user, line, t),
                    None => s.tool_call(asks_user, line, t),
                }
            }
            ("response_item", "function_call_output" | "custom_tool_call_output") => {
                match p["call_id"].as_str().filter(|id| !id.is_empty()) {
                    Some(call_id) => s.tool_result_id(call_id, t),
                    None => s.set(Last::ToolResult, t),
                }
            }
            _ => {}
        }
    }
}

/// Whether this session's prompts come from entries of `kind`. A version that writes a
/// prompt both ways would otherwise count it twice; the first kind seen wins.
fn prompts_from(s: &mut Session, kind: &str) -> bool {
    let other = if kind == "item" { "prompts:user_message" } else { "prompts:item" };
    if s.seen.contains(other) {
        return false;
    }
    s.seen.insert(format!("prompts:{kind}"));
    true
}

fn prompt(s: &mut Session, text: &str, t: f64) {
    if let Some(first) = s.prompt(text, t)
        && s.title.is_empty()
    {
        // Codex has no generated title; the opening prompt is the best summary.
        s.title = first;
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
        let events = [
            event("turn_context", json!({"cwd": "/w", "approval_policy": "never"})),
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            ),
        ];
        let mut s = session(&events);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(status(&s), (Status::Working, "working"));
        assert_eq!(s.permission, Some(PermissionMode::Never));
    }

    #[test]
    fn an_on_request_policy_keeps_the_inferred_stall() {
        let events = [
            event("turn_context", json!({"cwd": "/w", "approval_policy": "on-request"})),
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            ),
        ];
        let mut s = session(&events);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(status(&s), (Status::NeedsYou, "may need approval"));
    }

    #[test]
    fn an_ai_reviewer_is_not_a_human() {
        let events = [
            event(
                "turn_context",
                json!({"cwd": "/w", "approval_policy": "on-request", "approvals_reviewer": "auto_review"}),
            ),
            event(
                "response_item",
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"sleep 60\"}"}),
            ),
        ];
        let mut s = session(&events);
        s.last_at -= STALL_AFTER * 2.0;
        assert_eq!(status(&s), (Status::Working, "working"));
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
