//! Claude Desktop local agent mode (Cowork / Chat): app-data
//! `local-agent-mode-sessions/<user>/<org>/local_<id>/audit.jsonl` (or the newer
//! `<user>/<org>/<first-8-hex>/audit.jsonl`).
//!
//! The audit stream is the Claude Agent SDK's stream-json, so the shared Claude record
//! reader handles `system/init`, `user`, `assistant` and `system`. This adapter adds the
//! Desktop-specific records (`result`, `rate_limit_event`, `tool_use_summary`) and the
//! sibling `local_<id>.json` sidecar. Code-tab transcripts are not read here: they are
//! Claude Code's `~/.claude/projects/…` files and stay on the Claude Code adapter.

use super::{Adapter, EventSeed};
use crate::event::{EventKind, TitleSource, TurnEndReason};
use crate::harness::Harness;
use crate::state::{PermissionMode, TokenUsage};
use serde_json::Value;

pub(crate) struct ClaudeDesktop;

impl Adapter for ClaudeDesktop {
    fn parse(&self, state: &mut crate::event::ParseState, record: &Value) -> Vec<EventSeed> {
        let at = record["timestamp"].as_str().and_then(crate::time::parse_rfc3339);
        match record["type"].as_str().unwrap_or_default() {
            // The SDK's terminal frame: a turn end (or a recorded failure), the session
            // cost, and totals that overwrite the per-message usage already seen.
            "result" => result(record, at),
            // The SDK emits these beside the conversation. They are known records with no
            // status meaning, so they are kept whole rather than reported as unknown.
            "rate_limit_event" | "tool_use_summary" => vec![meta(record, at)],
            "system" if record["subtype"] == "init" => init(record, at),
            _ => super::claude::parse_record(Harness::ClaudeDesktop, state, record),
        }
    }

    fn metadata(&self, _state: &mut crate::event::ParseState, record: &Value) -> Vec<EventSeed> {
        metadata(record)
    }
}

/// `system/init`: the SDK's session context. The shared parser already carries `cwd`,
/// `permissionMode` and a `session_id`; this adds the model and the init record itself.
fn init(record: &Value, at: Option<f64>) -> Vec<EventSeed> {
    let mut out = super::claude::parse_record(Harness::ClaudeDesktop, &mut Default::default(), record);
    if let Some(model) = record["model"].as_str().filter(|model| !model.is_empty()) {
        out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }).at(at));
    }
    out
}

/// The SDK `result` frame. `subtype: "success"` can still carry `is_error: true`, and
/// `terminal_reason` names an interrupted run. The result's usage is the session total, so
/// it replaces (rather than adds to) the per-message usage.
fn result(record: &Value, at: Option<f64>) -> Vec<EventSeed> {
    let subtype = record["subtype"].as_str().unwrap_or_default();
    let terminal = record["terminal_reason"].as_str().unwrap_or_default();
    let errors = record["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
    let is_error = record["is_error"].as_bool() == Some(true)
        || matches!(subtype, "error" | "error_max_turns" | "error_during_execution")
        || !errors.is_empty();
    let reason = if matches!(terminal, "aborted_streaming" | "aborted_tools") {
        TurnEndReason::Interrupted
    } else if is_error {
        TurnEndReason::Error
    } else {
        TurnEndReason::EndTurn
    };
    let error = (reason == TurnEndReason::Error).then(|| {
        if !errors.is_empty() {
            errors.join("; ")
        } else if let Some(result) = record["result"].as_str().filter(|result| !result.is_empty()) {
            result.into()
        } else if !subtype.is_empty() {
            subtype.into()
        } else {
            "result_error".into()
        }
    });
    let mut out = vec![
        EventSeed::new(EventKind::TurnEnd {
            reason,
            error,
            background_agents: None,
            stop_reason: None,
            resets_at: None,
        })
        .at(at),
    ];
    if let Some(cost) = record["total_cost_usd"].as_f64().or_else(|| record["total_cost_usd"].as_str()?.parse().ok()) {
        out.push(EventSeed::new(EventKind::Cost { usd: cost }).at(at));
    }
    let usage = &record["usage"];
    let reported = TokenUsage {
        input_tokens: usage["input_tokens"].as_u64(),
        output_tokens: usage["output_tokens"].as_u64(),
        cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64(),
        cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
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
            EventSeed::new(EventKind::Usage { usage: reported, reported, message_id: None, cumulative: true }).at(at),
        );
    }
    out
}

/// The sibling `local_<id>.json` sidecar: session facts the audit stream does not repeat.
fn metadata(record: &Value) -> Vec<EventSeed> {
    let mut out = Vec::new();
    let cwd = record["cwd"].as_str().filter(|cwd| !cwd.is_empty()).map(String::from);
    let session_id = record["sessionId"]
        .as_str()
        .or_else(|| record["cliSessionId"].as_str())
        .filter(|id| !id.is_empty())
        .map(String::from);
    if cwd.is_some() || session_id.is_some() {
        out.push(EventSeed::new(EventKind::SessionInfo {
            cwd,
            branch: None,
            session_id,
            entrypoint: None,
            approvals_reviewer: None,
        }));
    }
    if let Some(title) = record["title"].as_str().map(str::trim).filter(|title| !title.is_empty()) {
        out.push(EventSeed::new(EventKind::Title { title: title.into(), title_source: TitleSource::Other }));
    }
    if let Some(model) = record["model"].as_str().filter(|model| !model.is_empty()) {
        out.push(EventSeed::new(EventKind::ModelChange { model: model.into() }));
    }
    if let Some(mode) = record["permissionMode"].as_str() {
        out.push(EventSeed::new(EventKind::PermissionMode { mode: PermissionMode::parse(mode) }));
    }
    out
}

fn meta(record: &Value, at: Option<f64>) -> EventSeed {
    EventSeed::new(EventKind::Meta {
        record_type: record["type"].as_str().unwrap_or_default().into(),
        subtype: record["subtype"].as_str().map(String::from),
        record: record.clone(),
    })
    .at(at)
}

/// The tool's short activity line, matching the Claude Code adapter's wording.
#[cfg(test)]
fn describe_tool(name: &str, input: &Value) -> crate::state::ActivityLine {
    let line = super::claude::parse_record(
        Harness::ClaudeDesktop,
        &mut Default::default(),
        &serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "name": name, "input": input}]}
        }),
    );
    line.into_iter()
        .find_map(|seed| match seed.kind {
            EventKind::ToolCall { activity_line, .. } => activity_line,
            _ => None,
        })
        .unwrap_or_else(|| crate::state::ActivityLine::Tool { name: name.into(), detail: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Activity, Certainty, ChunkCtx, Harness, Reason, ReducerOptions, SessionReducer, Timestamp};
    use serde_json::json;
    use std::path::Path;

    const NOW: f64 = crate::testutil::NOW;
    const PATH: &str =
        "/h/.config/Claude/local-agent-mode-sessions/user/org/local_11111111-2222-3333-4444-555555555555/audit.jsonl";

    fn reducer() -> SessionReducer {
        let mut reducer = SessionReducer::new(Harness::ClaudeDesktop, ReducerOptions::default());
        reducer.set_path(Path::new(PATH));
        reducer
    }

    fn feed(reducer: &mut SessionReducer, records: &[Value]) {
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        let now = Timestamp::from_secs_f64(NOW);
        reducer.feed(text.as_bytes(), ChunkCtx { offset: 0, mtime: now, path: Path::new(PATH) }).unwrap();
    }

    fn init() -> Value {
        json!({
            "type": "system", "subtype": "init", "uuid": "init-1",
            "session_id": "local_11111111-2222-3333-4444-555555555555",
            "cwd": "/work/demo", "model": "claude-sonnet-4",
            "permissionMode": "default", "claude_code_version": "2.1.0",
            "tools": ["Bash", "Read"]
        })
    }

    #[test]
    fn a_text_turn_reads_as_thinking_then_done() {
        let mut reducer = reducer();
        feed(&mut reducer, &[init(), json!({"type": "user", "uuid": "u1", "message": {"content": "say hi"}})]);
        let state = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!(
            (state.activity, state.certainty, state.reason),
            (Activity::Thinking, Certainty::Firm, Reason::PromptSent)
        );
        assert_eq!(state.meta.project, "demo");
        assert_eq!(state.permission, Some(PermissionMode::Default));

        feed(
            &mut reducer,
            &[
                json!({"type": "assistant", "uuid": "a1", "message": {
                    "id": "msg_1", "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 7},
                    "content": [{"type": "text", "text": "hi"}]}}),
                json!({"type": "result", "subtype": "success", "is_error": false,
                       "total_cost_usd": 0.001, "usage": {"input_tokens": 10, "output_tokens": 7}}),
            ],
        );
        let state = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((state.activity, state.reason), (Activity::Done, Reason::TurnEnded));
        assert_eq!(state.cost_usd, Some(0.001));
        assert_eq!(state.out_tokens, 7);
        assert_eq!(state.usage.input_tokens, Some(10));
    }

    #[test]
    fn a_pending_tool_is_firmly_running_then_inferred_approval_after_the_stall() {
        let mut reducer = reducer();
        feed(
            &mut reducer,
            &[
                init(),
                json!({"type": "user", "uuid": "u1", "message": {"content": "run it"}}),
                json!({"type": "assistant", "uuid": "a1", "message": {
                    "id": "msg_1", "stop_reason": "tool_use", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test"}}]}}),
            ],
        );
        let running = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!(
            (running.activity, running.certainty, running.reason),
            (Activity::RunningTool, Certainty::Firm, Reason::ToolCallPending)
        );
        let stalled = reducer.state(Timestamp::from_secs_f64(NOW + 25.0));
        assert_eq!(
            (stalled.activity, stalled.certainty, stalled.reason),
            (Activity::AwaitingApproval, Certainty::Inferred, Reason::ToolCallStalled),
            "the audit stream has no permission request; this is the shared timing inference"
        );
    }

    #[test]
    fn a_no_prompt_mode_keeps_a_pending_tool_running() {
        let mut reducer = reducer();
        let mut init = init();
        init["permissionMode"] = json!("bypassPermissions");
        feed(
            &mut reducer,
            &[
                init,
                json!({"type": "user", "uuid": "u1", "message": {"content": "run it"}}),
                json!({"type": "assistant", "uuid": "a1", "message": {
                    "id": "msg_1", "stop_reason": "tool_use", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test"}}]}}),
            ],
        );
        let stalled = reducer.state(Timestamp::from_secs_f64(NOW + 25.0));
        assert_eq!(
            (stalled.activity, stalled.certainty, stalled.reason),
            (Activity::RunningTool, Certainty::Firm, Reason::ToolCallPending)
        );
    }

    #[test]
    fn an_error_result_is_errored_and_keeps_the_message() {
        let mut reducer = reducer();
        feed(
            &mut reducer,
            &[
                init(),
                json!({"type": "user", "uuid": "u1", "message": {"content": "go"}}),
                json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
                       "errors": ["the model refused"], "total_cost_usd": 0.2}),
            ],
        );
        let state = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((state.activity, state.reason), (Activity::Errored, Reason::Errored));
        assert_eq!(state.error.as_deref(), Some("the model refused"));
        assert_eq!(state.cost_usd, Some(0.2));
    }

    #[test]
    fn an_aborted_result_is_interrupted_not_errored() {
        let mut reducer = reducer();
        feed(
            &mut reducer,
            &[
                init(),
                json!({"type": "result", "subtype": "success", "is_error": false,
                       "terminal_reason": "aborted_streaming"}),
            ],
        );
        let state = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((state.activity, state.reason), (Activity::Interrupted, Reason::Interrupted));
    }

    #[test]
    fn the_sidecar_supplies_cwd_title_and_model_without_moving_the_status() {
        let mut reducer = reducer();
        reducer.feed_metadata(
            br#"{"sessionId":"local_11111111-2222-3333-4444-555555555555","cwd":"/work/sidecar",
                 "title":"Config review","model":"claude-sonnet-4","lastActivityAt":1760000000000}"#,
        );
        let state = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((state.meta.project.as_str(), state.meta.title.as_str()), ("sidecar", "Config review"));
        assert_eq!(state.activity, Activity::Unknown, "metadata alone is not a turn");

        reducer.feed_metadata(br#"{"cwd":"/work/renamed","title":"Renamed"}"#);
        let state = reducer.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((state.meta.project.as_str(), state.meta.title.as_str()), ("renamed", "Renamed"));
    }

    #[test]
    fn known_sdk_records_are_not_unknown() {
        let records = [
            json!({"type": "rate_limit_event", "uuid": "r1", "rate_limit_info": {"status": "allowed"}}),
            json!({"type": "tool_use_summary", "uuid": "t1", "summary": "ran a command"}),
            init(),
            json!({"type": "result", "subtype": "success", "is_error": false}),
        ];
        for record in records {
            let events = crate::event::parse_events(
                Harness::ClaudeDesktop,
                0,
                format!("{record}\n").as_bytes(),
                crate::ParseMode::Full,
            );
            assert!(
                events.iter().all(|event| !matches!(event.kind, EventKind::Unknown { .. })),
                "{record} -> {events:#?}"
            );
        }
    }

    #[test]
    fn the_code_tab_layout_is_not_this_layout() {
        // The split is structural: a Claude Code project folder has no local_<id> parent.
        assert_eq!(
            Harness::ClaudeDesktop.layout().role(Path::new("u/o/local_a/audit.jsonl")),
            Some(crate::Role::Session)
        );
        assert_eq!(Harness::ClaudeCode.layout().role(Path::new("u/o/local_a/audit.jsonl")), None);
        assert_eq!(Harness::ClaudeDesktop.layout().role(Path::new("-w-app/s.jsonl")), None);
        // And the adapters differ: a Desktop audit is not auto-detected as Claude Code by
        // its harness id, even though both read the same record vocabulary.
        assert_eq!(Harness::ClaudeDesktop.id(), "claude_desktop");
    }

    #[test]
    fn describe_tool_matches_the_shared_words() {
        assert_eq!(describe_tool("Bash", &json!({"command": "cargo test\n--all"})).to_string(), "$ cargo test");
        assert_eq!(describe_tool("Read", &json!({"path": "/a/b/main.rs"})).to_string(), "Read main.rs");
    }
}
