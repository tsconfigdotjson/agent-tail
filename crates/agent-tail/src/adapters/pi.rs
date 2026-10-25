//! Pi: `~/.pi/agent/sessions/<encoded-cwd>/<timestamp>_<session-id>.jsonl`, one session
//! entry per line. The file is a tree (`id`/`parentId`), but Pi appends the active branch,
//! so the latest entry is the leaf and record order is the turn clock.
//!
//! Checked against Pi 1.4.0: the session header, model/thinking changes, messages
//! (`system`, `user`, `assistant`, `toolResult`, `bashExecution`), `session_info`,
//! `usage`, `compaction`, `context_edit`, `branch_summary`, `custom` and
//! `custom_message` entries. Pi has no approval prompt: a pending tool is running, not
//! waiting for a person. It also has no built-in ask-the-user tool.

use super::{Adapter, EventSeed};
use crate::session::{command, file_name, first_line, tool};
use crate::state::{ActivityLine, TokenUsage};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Pi's `id`/`parentId` tree, tracked as entries arrive. The latest entry is the leaf Pi
/// appended last; only its parent chain is the active branch. Pi's in-memory `branch()`
/// writes nothing until the next append, so a switch-back becomes visible as a new entry
/// whose parent is the older leaf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Branch {
    parents: HashMap<String, Option<String>>,
    leaf: Option<String>,
    active: HashSet<String>,
}

/// The tree link an entry carries; the session header and records without an id are not
/// part of the per-entry tree.
pub(crate) fn branch_link(record: &Value) -> Option<(&str, Option<&str>)> {
    if record["type"] == "session" {
        return None;
    }
    let id = record["id"].as_str().filter(|id| !id.is_empty())?;
    let parent = record["parentId"].as_str().filter(|id| !id.is_empty());
    Some((id, parent))
}

impl Branch {
    /// Record one entry and return its id plus whether it extends the current leaf, or None
    /// for the header / an id-less record. A missing `parentId` is treated as appending to
    /// the current leaf (id-only hand-made/older records stay linear); an explicit `null` is
    /// a root. Pi 1.4 writes `parentId` on every tree entry.
    pub(crate) fn observe(&mut self, record: &Value) -> Option<(String, bool)> {
        let (id, parent) = branch_link(record)?;
        let parent = if record.get("parentId").is_some() { parent.map(str::to_string) } else { self.leaf.clone() };
        let extends = !self.parents.contains_key(id) && self.leaf.as_deref() == parent.as_deref();
        self.parents.insert(id.to_string(), parent);
        self.leaf = Some(id.to_string());
        if extends {
            self.active.insert(id.to_string());
        } else {
            self.recompute();
        }
        Some((id.to_string(), extends))
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.active.contains(id)
    }

    /// Whether an entry belongs in the current reduction: the header and id-less records
    /// always do; tree entries only when they are on the leaf's parent chain.
    pub(crate) fn on_active_chain(&self, record: &Value) -> bool {
        branch_link(record).is_none_or(|(id, _)| self.contains(id))
    }

    fn recompute(&mut self) {
        self.active.clear();
        let mut current = self.leaf.clone();
        while let Some(id) = current {
            if !self.active.insert(id.clone()) {
                break; // a broken file could point a parent at its own child
            }
            current = self.parents.get(&id).cloned().flatten();
        }
    }
}

pub(crate) struct Pi;

impl Adapter for Pi {
    fn parse(&self, state: &mut crate::event::ParseState, e: &Value) -> Vec<EventSeed> {
        use crate::event::{EventKind, TitleSource};
        let at = entry_time(e);
        let mut out = Vec::new();
        match e["type"].as_str().unwrap_or_default() {
            "session" => out.push(
                EventSeed::new(EventKind::SessionInfo {
                    cwd: e["cwd"].as_str().map(String::from),
                    branch: None,
                    session_id: e["id"].as_str().map(String::from),
                    entrypoint: None,
                    approvals_reviewer: None,
                })
                .at(at),
            ),
            "message" => message(state, e, at, &mut out),
            "model_change" => {
                if let Some(model) = e["modelId"].as_str().filter(|model| !model.is_empty()) {
                    out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }).at(at));
                }
            }
            // The user's display name is Pi's title. An empty name clears it and falls back
            // to the opening prompt.
            "session_info" => out.push(
                EventSeed::new(EventKind::Title {
                    title: e["name"].as_str().unwrap_or_default().into(),
                    title_source: TitleSource::SessionName,
                })
                .at(at),
            ),
            "compaction" => {
                out.push(
                    EventSeed::new(EventKind::Compaction {
                        trigger: None,
                        pre_tokens: e["tokensBefore"].as_u64(),
                        post_tokens: None,
                    })
                    .at(at),
                );
                usage_and_cost(state, &e["usage"], &format!("{}:compaction", entry_id(e)), at, &mut out);
            }
            "usage" => {
                usage_and_cost(state, &e["usage"], entry_id(e), at, &mut out);
            }
            "branch_summary" => {
                usage_and_cost(state, &e["usage"], &format!("{}:branch", entry_id(e)), at, &mut out);
                out.push(meta(e).at(at));
            }
            "thinking_level_change" | "context_edit" | "custom" | "custom_message" | "label" => {
                out.push(meta(e).at(at));
            }
            kind if known_record(kind) => out.push(meta(e).at(at)),
            kind => out.push(
                EventSeed::new(EventKind::Unknown {
                    record_type: (!kind.is_empty()).then(|| kind.into()),
                    reason: "unknown Pi session entry type".into(),
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

fn message(state: &mut crate::event::ParseState, e: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    use crate::event::{EventKind, ToolKind, TurnEndReason};
    let m = &e["message"];
    match m["role"].as_str().unwrap_or_default() {
        "user" => {
            let (text, raw) = user_text(&m["content"]);
            out.push(
                EventSeed::new(EventKind::UserPrompt { text, raw, wrappers: Vec::new(), starts_turn: true }).at(at),
            );
        }
        "assistant" => {
            usage_and_cost(state, &m["usage"], entry_id(e), at, out);
            for block in m["content"].as_array().into_iter().flatten() {
                match block["type"].as_str().unwrap_or_default() {
                    "text" => out.push(
                        EventSeed::new(EventKind::AssistantText {
                            text: block["text"].as_str().unwrap_or_default().into(),
                        })
                        .at(at),
                    ),
                    "thinking" => out.push(
                        EventSeed::new(EventKind::Reasoning {
                            signature: block["thinkingSignature"]
                                .as_str()
                                .filter(|signature| !signature.is_empty())
                                .map(String::from),
                        })
                        .at(at),
                    ),
                    "toolCall" => {
                        let name = block["name"].as_str().unwrap_or_default();
                        out.push(
                            EventSeed::new(EventKind::ToolCall {
                                name: name.into(),
                                call_id: block["id"].as_str().filter(|id| !id.is_empty()).map(String::from),
                                kind: ToolKind::of(name, &block["arguments"]),
                                input: block["arguments"].clone(),
                                asks_user: crate::Harness::Pi.discovery().asks_user(name),
                                activity_line: Some(describe_tool(name, &block["arguments"])),
                            })
                            .at(at),
                        );
                    }
                    kind if known_content(kind) => out.push(
                        EventSeed::new(EventKind::Meta {
                            record_type: "content".into(),
                            subtype: Some(kind.into()),
                            record: block.clone(),
                        })
                        .at(at),
                    ),
                    kind => out.push(
                        EventSeed::new(EventKind::Unknown {
                            record_type: Some("content".into()),
                            reason: format!("unknown Pi content type \"{kind}\""),
                            record: block.clone(),
                        })
                        .at(at),
                    ),
                }
            }
            // A terminal assistant stop reason ends the turn. `toolUse` means its tool calls
            // are still running; `pending` is not persisted but is not an end either.
            let reason = match m["stopReason"].as_str().unwrap_or_default() {
                "stop" | "length" => Some(TurnEndReason::EndTurn),
                "aborted" => Some(TurnEndReason::Interrupted),
                "error" => Some(TurnEndReason::Error),
                _ => None,
            };
            if let Some(reason) = reason {
                out.push(
                    EventSeed::new(EventKind::TurnEnd {
                        reason,
                        error: (reason == TurnEndReason::Error)
                            .then(|| m["errorMessage"].as_str().unwrap_or("error").to_string()),
                        background_agents: None,
                    })
                    .at(at),
                );
            }
        }
        "toolResult" => {
            out.push(
                EventSeed::new(EventKind::ToolResult {
                    call_id: m["toolCallId"].as_str().filter(|id| !id.is_empty()).map(String::from),
                    is_error: m["isError"].as_bool() == Some(true),
                    size: result_size(&m["content"]),
                })
                .at(at),
            );
            // A tool that performed nested model work records its own usage.
            usage_and_cost(state, &m["usage"], &format!("{}:tool", entry_id(e)), at, out);
        }
        // Direct `!` shell commands and extension messages are context, not model turns.
        _ => out.push(meta(e).at(at)),
    }
}

fn entry_time(e: &Value) -> Option<f64> {
    e["timestamp"]
        .as_str()
        .and_then(crate::time::parse_rfc3339)
        // Message timestamps are Unix milliseconds; the entry timestamp is authoritative.
        .or_else(|| e["message"]["timestamp"].as_i64().map(|ms| ms as f64 / 1000.0))
}

fn entry_id(e: &Value) -> &str {
    e["id"].as_str().unwrap_or_default()
}

fn user_text(content: &Value) -> (String, String) {
    match content {
        Value::String(text) => (text.clone(), text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect();
            (texts.join("\n"), texts.join("\n"))
        }
        _ => (String::new(), String::new()),
    }
}

fn result_size(content: &Value) -> u64 {
    match content {
        Value::String(text) => text.len() as u64,
        Value::Array(parts) => parts
            .iter()
            .map(|part| part["text"].as_str().map_or_else(|| part.to_string().len(), |text| text.len()) as u64)
            .sum(),
        Value::Null => 0,
        other => other.to_string().len() as u64,
    }
}

fn usage_and_cost(
    state: &mut crate::event::ParseState,
    usage: &Value,
    id: &str,
    at: Option<f64>,
    out: &mut Vec<EventSeed>,
) {
    use crate::event::EventKind;
    if !usage.is_object() {
        return;
    }
    let reported = TokenUsage {
        input_tokens: usage["input"].as_u64(),
        output_tokens: usage["output"].as_u64(),
        cache_read_input_tokens: usage["cacheRead"].as_u64(),
        cache_creation_input_tokens: usage["cacheWrite"].as_u64(),
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
                message_id: (!id.is_empty()).then(|| id.into()),
                cumulative: false,
            })
            .at(at),
        );
    }
    // Pi reports cost per model call; the session total is their sum.
    if let Some(cost) = usage["cost"]["total"].as_f64() {
        state.pi_cost += cost;
        out.push(EventSeed::new(EventKind::Cost { usd: state.pi_cost }).at(at));
    }
}

fn describe_tool(name: &str, input: &Value) -> ActivityLine {
    match name {
        "bash" | "powershell" => command(input["command"].as_str().unwrap_or_default()),
        "edit" => tool("Edit", file_name(input["path"].as_str().unwrap_or_default())),
        "write" => tool("Write", file_name(input["path"].as_str().unwrap_or_default())),
        "read" => tool("Read", file_name(input["path"].as_str().unwrap_or_default())),
        "grep" => tool("Grep", first_line(input["pattern"].as_str().unwrap_or_default())),
        "find" => tool("Find", first_line(input["pattern"].as_str().unwrap_or_default())),
        "ls" => tool("ls", file_name(input["path"].as_str().unwrap_or_default())),
        other => ActivityLine::Tool { name: other.into(), detail: None },
    }
}

fn meta(e: &Value) -> EventSeed {
    EventSeed::new(crate::event::EventKind::Meta {
        record_type: e["type"].as_str().unwrap_or_default().into(),
        subtype: e["message"]["role"].as_str().map(String::from),
        record: e.clone(),
    })
}

fn known_record(kind: &str) -> bool {
    matches!(
        kind,
        "session"
            | "message"
            | "model_change"
            | "thinking_level_change"
            | "session_info"
            | "compaction"
            | "usage"
            | "branch_summary"
            | "context_edit"
            | "custom"
            | "custom_message"
            | "label"
    )
}

fn known_content(kind: &str) -> bool {
    matches!(kind, "text" | "thinking" | "toolCall" | "image" | "redacted_thinking")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::harness::Harness;
    use crate::session::{Last, Session};
    use crate::testutil::{at, fixture, log, replay, status};
    use serde_json::json;

    fn session(events: &[Value]) -> Session {
        replay(Harness::Pi, events)
    }

    fn entry(kind: &str, body: Value) -> Value {
        let mut entry = body;
        entry["type"] = json!(kind);
        entry["timestamp"] = json!(at(1.0));
        entry
    }

    fn user(text: &str) -> Value {
        entry("message", json!({"message": {"role": "user", "content": [{"type": "text", "text": text}]}}))
    }

    fn assistant(content: Value, stop: &str) -> Value {
        entry(
            "message",
            json!({"message": {"role": "assistant", "content": content, "stopReason": stop,
                "usage": {"input": 10, "output": 5, "cacheRead": 2, "cacheWrite": 3, "totalTokens": 20,
                          "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3}}}}),
        )
    }

    fn tool_result(id: &str) -> Value {
        entry(
            "message",
            json!({"message": {"role": "toolResult", "toolCallId": id, "toolName": "bash",
                "content": [{"type": "text", "text": "ok"}], "isError": false}}),
        )
    }

    #[test]
    fn follows_a_turn_from_prompt_to_done() {
        let s = session(&[entry("session", json!({"id": "s1", "cwd": "/work/jooba"})), user("Fix routing\nplease")]);
        assert_eq!((s.project.as_str(), s.title.as_str()), ("jooba", "Fix routing"));
        assert_eq!(status(&s), (Status::Working, "thinking"));

        let mut s = s;
        s.apply(&assistant(
            json!([{"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "cargo test\n--all"}}]),
            "toolUse",
        ));
        assert_eq!(status(&s), (Status::Working, "working"));
        assert_eq!(log(&s).last().unwrap(), "$ cargo test");
        assert_eq!((s.usage.input_tokens, s.usage.output_tokens), (Some(10), Some(5)));
        assert_eq!((s.usage.cache_read_input_tokens, s.usage.cache_creation_input_tokens), (Some(2), Some(3)));
        assert!((s.cost.unwrap() - 0.3).abs() < 1e-9);

        s.apply(&tool_result("c1"));
        assert_eq!(status(&s), (Status::Working, "thinking"));

        s.apply(&assistant(json!([{"type": "text", "text": "done"}]), "stop"));
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(s.last, Last::EndTurn);
    }

    #[test]
    fn a_pending_tool_is_firmly_running_not_waiting_for_approval() {
        let mut s = session(&[
            user("run it"),
            assistant(
                json!([{"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "sleep 60"}}]),
                "toolUse",
            ),
        ]);
        s.last_at -= 60.0;
        let st = s.state(
            crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            &crate::ReducerOptions::default(),
            crate::Diagnostics::default(),
        );
        assert_eq!(
            (st.activity, st.certainty, st.reason),
            (crate::Activity::RunningTool, crate::Certainty::Firm, crate::Reason::ToolCallPending)
        );
        assert_eq!(st.permission, None, "Pi records no permission mode");
    }

    #[test]
    fn parallel_calls_stay_pending_until_every_result_arrives() {
        let call = |id: &str| {
            assistant(
                json!([{"type": "toolCall", "id": id, "name": "bash", "arguments": {"command": "sleep 1"}}]),
                "toolUse",
            )
        };
        let s = session(&[call("c1"), call("c2"), tool_result("c1")]);
        assert_eq!(status(&s), (Status::Working, "working"), "c2 is still pending");
        let s = session(&[call("c1"), call("c2"), tool_result("c1"), tool_result("c2")]);
        assert_eq!(status(&s), (Status::Working, "thinking"));
    }

    #[test]
    fn turns_end_however_they_end() {
        for (stop, last) in [("stop", Last::EndTurn), ("length", Last::EndTurn), ("aborted", Last::Interrupted)] {
            let s = session(&[user("go"), assistant(json!([{"type": "text", "text": "…"}]), stop)]);
            assert_eq!(s.last, last, "{stop}");
            assert_eq!(status(&s).0, Status::Done, "{stop}");
        }
        let s = session(&[assistant(json!([{"type": "text", "text": "no"}]), "error")]);
        assert_eq!(s.last, Last::Errored);
        assert_eq!(s.error.as_deref(), Some("error"));
        assert_eq!(status(&s).0, Status::Done);
    }

    #[test]
    fn an_error_keeps_its_message() {
        let mut e = assistant(json!([]), "error");
        e["message"]["errorMessage"] = json!("401: invalid credential");
        let s = session(&[e]);
        assert_eq!(s.last, Last::Errored);
        assert_eq!(s.error.as_deref(), Some("401: invalid credential"));
    }

    #[test]
    fn a_session_name_wins_and_clearing_it_falls_back_to_the_prompt() {
        let s = session(&[user("first prompt"), entry("session_info", json!({"name": "Named session"}))]);
        assert_eq!(s.title, "Named session");
        let s = session(&[
            user("first prompt"),
            entry("session_info", json!({"name": "Named session"})),
            entry("session_info", json!({"name": ""})),
        ]);
        assert_eq!(s.title, "first prompt");
    }

    #[test]
    fn model_changes_and_compaction_are_events() {
        let s = session(&[entry("model_change", json!({"provider": "p", "modelId": "m1"}))]);
        assert_eq!(s.harness, Harness::Pi);
        let events = crate::event::parse_events(
            Harness::Pi,
            0,
            format!("{}\n", entry("compaction", json!({"summary": "s", "firstKeptEntryId": "x", "tokensBefore": 50})))
                .as_bytes(),
            crate::ParseMode::Full,
        );
        assert!(
            events.iter().any(|event| matches!(event.kind, crate::EventKind::Compaction { pre_tokens: Some(50), .. }))
        );
    }

    #[test]
    fn describes_pi_tools() {
        let d = |name, input: Value| describe_tool(name, &input).to_string();
        assert_eq!(d("bash", json!({"command": "ls -la\nmore"})), "$ ls -la");
        assert_eq!(d("read", json!({"path": "/a/b/main.rs"})), "Read main.rs");
        assert_eq!(d("edit", json!({"path": "/a/b/main.rs"})), "Edit main.rs");
        assert_eq!(d("write", json!({"path": "/a/b/new.md"})), "Write new.md");
        assert_eq!(d("grep", json!({"pattern": "TODO\nmore"})), "Grep TODO");
        assert_eq!(d("find", json!({"pattern": "*.rs"})), "Find *.rs");
        assert_eq!(d("ls", json!({"path": "/a/src"})), "ls src");
    }

    const BRANCHED: &str = include_str!("../../../../fixtures/pi/1.4/linux/branched/input/session.jsonl");

    fn linked(mut entry: Value, id: &str, parent: Option<&str>) -> Value {
        entry["id"] = json!(id);
        entry["parentId"] = parent.map_or(Value::Null, |parent| json!(parent));
        entry
    }

    fn ctx(offset: u64) -> crate::ChunkCtx<'static> {
        crate::ChunkCtx {
            offset,
            mtime: crate::time::Timestamp::from_secs_f64(crate::testutil::NOW),
            path: std::path::Path::new("/h/.pi/agent/sessions/-w-app/s.jsonl"),
        }
    }

    fn feed(reducer: &mut crate::SessionReducer, offset: &mut u64, entries: &[Value]) {
        for entry in entries {
            let line = format!("{entry}\n");
            reducer.feed(line.as_bytes(), ctx(*offset)).unwrap();
            *offset += line.len() as u64;
        }
    }

    #[test]
    fn abandoned_branch_does_not_move_status_usage_recent_or_title() {
        let s = session(&fixture(BRANCHED, 1.0));
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!((s.project.as_str(), s.session_id.as_str()), ("work", "branch-demo-session"));
        assert_eq!(s.title, "main branch question", "the abandoned session_info is off-chain");
        assert_eq!((s.usage.input_tokens, s.usage.output_tokens), (Some(10), Some(4)));
        assert_eq!((s.usage.cache_read_input_tokens, s.usage.cache_creation_input_tokens), (Some(1), Some(0)));
        assert_eq!(s.cost, Some(0.5), "the abandoned 1.50 USD is not counted");
        assert_eq!(log(&s), ["› main branch question", "› active branch follow-up"]);
        assert!(!log(&s).iter().any(|line| line.contains("rm -rf")), "the abandoned tool call is not recent activity");
    }

    #[test]
    fn switch_back_to_an_older_leaf_restores_that_chain() {
        let root = linked(entry("model_change", json!({"provider": "p", "modelId": "m"})), "root", None);
        let first = linked(user("first"), "u1", Some("root"));
        let a1 = linked(assistant(json!([{"type": "text", "text": "one"}]), "stop"), "a1", Some("u1"));
        let second = linked(user("second"), "u2", Some("a1"));
        let a2 = linked(assistant(json!([{"type": "text", "text": "two"}]), "stop"), "a2", Some("u2"));
        let third = linked(user("third"), "u3", Some("a1"));
        let a3 = linked(assistant(json!([{"type": "text", "text": "three"}]), "stop"), "a3", Some("u3"));

        let mut reducer = crate::SessionReducer::new(Harness::Pi, crate::ReducerOptions::default());
        let mut offset = 0;
        feed(&mut reducer, &mut offset, &[root, first, a1, second, a2]);
        let now = crate::time::Timestamp::from_secs_f64(crate::testutil::NOW);
        let before = reducer.state(now);
        assert_eq!(before.activity, crate::Activity::Done);
        assert_eq!((before.usage.output_tokens, before.cost_usd), (Some(10), Some(0.6)));
        assert_eq!(before.meta.title, "first");

        // Pi's branch() only moves the in-memory leaf; the next append (u3, parent a1) is
        // what makes the switch-back visible in the file.
        feed(&mut reducer, &mut offset, &[third]);
        let switched = reducer.state(now);
        assert_eq!(switched.activity, crate::Activity::Thinking);
        assert_eq!(switched.meta.title, "first");
        assert_eq!((switched.usage.output_tokens, switched.cost_usd), (Some(5), Some(0.3)), "a2 is off-chain");
        assert_eq!(
            switched.recent.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["› first", "› third"],
            "u2 and a2 are gone from recent activity"
        );

        feed(&mut reducer, &mut offset, &[a3]);
        let done = reducer.state(now);
        assert_eq!(done.activity, crate::Activity::Done);
        assert_eq!((done.usage.output_tokens, done.cost_usd), (Some(10), Some(0.6)));
    }

    #[test]
    fn compaction_does_not_prune_active_ancestors() {
        let root = linked(entry("model_change", json!({"provider": "p", "modelId": "m"})), "root", None);
        let first = linked(user("first"), "u1", Some("root"));
        let a1 = linked(assistant(json!([{"type": "text", "text": "one"}]), "stop"), "a1", Some("u1"));
        let mut compaction = entry(
            "compaction",
            json!({"summary": "compacted", "firstKeptEntryId": "u1", "tokensBefore": 100,
                "usage": {"input": 1, "output": 2, "cost": {"total": 0.2}}}),
        );
        compaction = linked(compaction, "c1", Some("a1"));
        let second = linked(user("second"), "u2", Some("c1"));
        let a2 = linked(assistant(json!([{"type": "text", "text": "two"}]), "stop"), "a2", Some("u2"));

        let s = session(&[root, first, a1, compaction, second, a2]);
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(s.usage.output_tokens, Some(12), "a1 + compaction + a2");
        assert_eq!(s.cost, Some(0.8));
        assert_eq!(log(&s), ["› first", "› second"], "firstKeptEntryId shapes model context, not the session chain");
    }

    #[test]
    fn off_chain_events_are_flagged_and_linear_sessions_have_none() {
        let events = crate::event::parse_events(Harness::Pi, 0, BRANCHED.as_bytes(), crate::ParseMode::Full);
        for event in &events {
            let abandoned = (6..=8).contains(&event.provenance.line);
            assert_eq!(event.off_chain, abandoned, "line {}: {:?}", event.provenance.line, event.kind);
        }
        assert!(events.iter().any(|event| event.off_chain && event.kind_name() == "title"));
        let active_cost = events
            .iter()
            .find_map(|event| match event.kind {
                crate::EventKind::Cost { usd } if !event.off_chain => Some(usd),
                _ => None,
            })
            .expect("the active assistant has a cost event");
        assert_eq!(active_cost, 0.5, "the active cost is not cumulative over the abandoned branch");

        let linear = crate::event::parse_events(Harness::Pi, 0, RECORDED.as_bytes(), crate::ParseMode::Full);
        assert!(linear.iter().all(|event| !event.off_chain));
    }

    #[test]
    fn incremental_branch_feeds_match_a_full_prefix_rebuild() {
        let now = crate::time::Timestamp::from_secs_f64(crate::testutil::NOW);
        let mut incremental = crate::SessionReducer::new(Harness::Pi, crate::ReducerOptions::default());
        let mut offset = 0;
        let lines: Vec<&str> = BRANCHED.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let raw = format!("{line}\n");
            incremental.feed(raw.as_bytes(), ctx(offset)).unwrap();
            offset += raw.len() as u64;

            let prefix = lines[..=index].iter().map(|line| format!("{line}\n")).collect::<String>();
            let mut rebuilt = crate::SessionReducer::new(Harness::Pi, crate::ReducerOptions::default());
            rebuilt.feed(prefix.as_bytes(), ctx(0)).unwrap();
            assert_eq!(
                serde_json::to_value(incremental.state(now)).unwrap(),
                serde_json::to_value(rebuilt.state(now)).unwrap(),
                "after line {}",
                index + 1
            );
        }
    }

    const RECORDED: &str = include_str!("../../../../fixtures/pi/1.4/linux/plain-qa/input/session.jsonl");

    #[test]
    fn reads_a_recorded_session() {
        let s = session(&fixture(RECORDED, 2.0));
        assert_eq!(s.project, "work");
        assert_eq!(s.title, "say hello");
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(s.cost, Some(0.0), "the mock reports zero cost, but the amount is recorded");
        assert_eq!(s.out_tokens, 5);
    }
}
