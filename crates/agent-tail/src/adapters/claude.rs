//! Claude Code: `~/.claude/projects/<project>/<session id>.jsonl`, one JSON entry per line
//! ("user" / "assistant" messages, plus "ai-title", "cost-state" and bookkeeping we skip).

use super::{Adapter, EventSeed};
use crate::session::{LaunchedHelper, command, file_name, first_line, tool};
use crate::state::{ActivityLine, PermissionMode};
use serde_json::Value;

pub(crate) struct ClaudeCode;

impl Adapter for ClaudeCode {
    fn parse(&self, state: &mut crate::event::ParseState, e: &Value) -> Vec<EventSeed> {
        parse_record(crate::Harness::ClaudeCode, state, e)
    }
}

/// The record reader Claude Code and Claude Desktop share. Desktop's audit.jsonl is the
/// Claude Agent SDK's stream-json, the same user/assistant/system shape, so the Code tab's
/// adapter is reused rather than copied; only the harness name (for ask-user matching) and
/// Desktop-only sidecar/result handling differ.
pub(crate) fn parse_record(harness: crate::Harness, state: &mut crate::event::ParseState, e: &Value) -> Vec<EventSeed> {
    use crate::event::{EventKind, TitleSource};
    let at = e["timestamp"].as_str().and_then(crate::time::parse_rfc3339);
    let sidechain = e["isSidechain"] == true;
    let mut out = Vec::new();
    if let Some(info) = session_info(harness, e) {
        out.push(EventSeed::new(info).at(at));
    }
    if let Some(mode) = e["permissionMode"].as_str() {
        out.push(EventSeed::new(EventKind::PermissionMode { mode: PermissionMode::parse(mode) }).at(at));
    }
    match e["type"].as_str().unwrap_or_default() {
        "ai-title" => out.push(
            EventSeed::new(EventKind::Title {
                title: e["aiTitle"].as_str().unwrap_or_default().into(),
                title_source: TitleSource::Generated,
            })
            .at(at),
        ),
        "cost-state" => {
            // Written as a string or a number depending on version.
            let c = &e["totalCostUSD"];
            if let Some(cost) = c.as_f64().or_else(|| c.as_str()?.parse().ok()) {
                out.push(EventSeed::new(EventKind::Cost { usd: cost }).at(at));
            }
        }
        "user" => user(state, e, at, &mut out),
        "assistant" => assistant(harness, state, e, at, sidechain, &mut out),
        "system" => system(e, at, &mut out),
        "permission-mode" if e["permissionMode"].is_null() => {}
        kind if known_record(kind) => {
            out.push(meta(e).at(at));
        }
        kind => out.push(
            EventSeed::new(EventKind::Unknown {
                record_type: (!kind.is_empty()).then(|| kind.into()),
                reason: "unknown Claude Code record type".into(),
                record: e.clone(),
            })
            .at(at),
        ),
    }
    if out.is_empty() {
        out.push(meta(e).at(at));
    }
    if sidechain {
        for seed in &mut out {
            seed.sidechain = true;
        }
    }
    out
}

fn session_info(harness: crate::Harness, e: &Value) -> Option<crate::event::EventKind> {
    let cwd = e["cwd"].as_str().map(String::from);
    let branch = e["gitBranch"].as_str().map(String::from);
    let entrypoint = e["entrypoint"].as_str().map(String::from);
    // The SDK's system/init names its own session. Claude Code keeps its existing events
    // unchanged; the Codex index is the only consumer of this field today.
    let session_id = (harness == crate::Harness::ClaudeDesktop && e["type"] == "system" && e["subtype"] == "init")
        .then(|| e["session_id"].as_str().filter(|id| !id.is_empty()).map(String::from))
        .flatten();
    (cwd.is_some() || branch.is_some() || entrypoint.is_some() || session_id.is_some()).then_some(
        crate::event::EventKind::SessionInfo { cwd, branch, session_id, entrypoint, approvals_reviewer: None },
    )
}

fn meta(e: &Value) -> EventSeed {
    EventSeed::new(crate::event::EventKind::Meta {
        record_type: e["type"].as_str().unwrap_or_default().into(),
        subtype: e["subtype"].as_str().map(String::from),
        record: e.clone(),
    })
}

fn known_record(kind: &str) -> bool {
    matches!(
        kind,
        "agent-name"
            | "api-request"
            | "api-request-blob"
            | "api-request-shape"
            | "artifact-autoreact-ledger"
            | "artifact-comment-monitor"
            | "attachment"
            | "atis-latch"
            | "bridge-session"
            | "file-history-delta"
            | "file-history-snapshot"
            | "fork-context-ref"
            | "frame-link"
            | "last-prompt"
            | "mode"
            | "permission-mode"
            | "pr-link"
            | "queue-operation"
            | "summary"
    )
}

fn user(state: &mut crate::event::ParseState, e: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    use crate::event::{EventKind, TurnEndReason};
    let t = at.unwrap_or_default();
    let content = &e["message"]["content"];
    // The same channel carries subagent completions; they are not prompts.
    if e["origin"]["kind"] == "task-notification" {
        if let Some(note) = task_notification(content)
            && ["completed", "failed", "stopped"].contains(&note.status.as_str())
        {
            out.push(
                EventSeed::new(EventKind::HelperEnd {
                    task_id: (!note.task_id.is_empty()).then_some(note.task_id),
                    tool_use_id: (!note.tool_use_id.is_empty()).then_some(note.tool_use_id),
                    output_file: (!note.output_file.is_empty()).then_some(note.output_file),
                    status: note.status,
                })
                .at(at),
            );
        }
        return;
    }
    // An async Agent result names the subagent it launched (`agentId`, `outputFile`) and
    // sometimes a `backgroundTaskId`; remember the link even if its file hasn't appeared yet.
    if let Some(launched) = async_launched(e, t) {
        out.push(
            EventSeed::new(EventKind::HelperSpawn {
                agent_id: (!launched.agent_id.is_empty()).then_some(launched.agent_id),
                task_id: (!launched.task_id.is_empty()).then_some(launched.task_id),
                output_file: (!launched.output_file.is_empty()).then_some(launched.output_file),
                tool_use_id: (!launched.tool_use_id.is_empty()).then_some(launched.tool_use_id),
            })
            .at(at),
        );
    }
    // A rejected tool result ends the turn: on 2.1.310 it is followed by the interrupt text
    // and `turn_duration`, but some builds (and a transcript read mid-flush) may show only
    // the rejection. CDT treats the same string as an ending event.
    if tool_rejected(e) {
        out.push(
            EventSeed::new(EventKind::TurnEnd { reason: TurnEndReason::EndTurn, error: None, background_agents: None })
                .at(at),
        );
        return;
    }
    if let Some(text) = content.as_str() {
        if interrupted(text) {
            out.push(turn_end(TurnEndReason::Interrupted).at(at));
        } else if e["isMeta"] != true && e["isCompactSummary"] != true {
            if !text.starts_with('<') {
                out.push(
                    EventSeed::new(EventKind::UserPrompt {
                        text: text.into(),
                        raw: text.into(),
                        wrappers: Vec::new(),
                        starts_turn: true,
                    })
                    .at(at),
                );
            } else {
                let wrappers = crate::event::prompt_wrappers(text);
                if wrappers.is_empty() {
                    out.push(meta(e).at(at));
                } else {
                    out.push(
                        EventSeed::new(EventKind::UserPrompt {
                            text: String::new(),
                            raw: text.into(),
                            wrappers,
                            starts_turn: false,
                        })
                        .at(at),
                    );
                }
            }
        } else {
            out.push(meta(e).at(at));
        }
        return;
    }
    let Some(items) = content.as_array() else {
        out.push(meta(e).at(at));
        return;
    };
    let results: Vec<&Value> = items.iter().filter(|c| c["type"] == "tool_result").collect();
    if !results.is_empty() {
        for result in results {
            let call_id = result["tool_use_id"].as_str().filter(|id| !id.is_empty());
            out.push(
                EventSeed::new(EventKind::ToolResult {
                    call_id: call_id.map(String::from),
                    is_error: result["is_error"].as_bool() == Some(true),
                    size: result_size(&result["content"]),
                })
                .at(at),
            );
            if let Some(call_id) = call_id
                && let Some(seed) = wakeup_result(state, e, result, call_id, at)
            {
                out.push(seed);
            }
        }
        return;
    }
    let texts: Vec<&str> = items.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
    if texts.iter().any(|text| interrupted(text)) {
        out.push(turn_end(TurnEndReason::Interrupted).at(at));
        return;
    }
    if e["isMeta"] == true || e["isCompactSummary"] == true {
        out.push(meta(e).at(at));
        return;
    }
    if let Some(text) = texts.iter().find(|t| !t.starts_with('<')) {
        out.push(
            EventSeed::new(EventKind::UserPrompt {
                text: (*text).into(),
                raw: texts.join("\n"),
                wrappers: Vec::new(),
                starts_turn: true,
            })
            .at(at),
        );
    } else if items.iter().any(|c| c["type"] == "image") {
        out.push(
            EventSeed::new(EventKind::UserPrompt {
                text: String::new(),
                raw: texts.join("\n"),
                wrappers: Vec::new(),
                starts_turn: true,
            })
            .at(at),
        );
    } else if !texts.is_empty() {
        let raw = texts.join("\n");
        let wrappers = crate::event::prompt_wrappers(&raw);
        if !wrappers.is_empty() {
            out.push(
                EventSeed::new(EventKind::UserPrompt { text: String::new(), raw, wrappers, starts_turn: false }).at(at),
            );
        }
    }
    for block in items {
        let kind = block["type"].as_str().unwrap_or_default();
        if matches!(kind, "text" | "tool_result" | "image" | "tool_reference" | "document") {
            continue;
        }
        if kind == "tool_addition" {
            out.push(
                EventSeed::new(EventKind::Meta {
                    record_type: "content".into(),
                    subtype: Some(kind.into()),
                    record: block.clone(),
                })
                .at(at),
            );
        } else if !kind.is_empty() {
            out.push(
                EventSeed::new(EventKind::Unknown {
                    record_type: Some("content".into()),
                    reason: "unknown Claude Code content type".into(),
                    record: block.clone(),
                })
                .at(at),
            );
        }
    }
}

fn assistant(
    harness: crate::Harness,
    state: &mut crate::event::ParseState,
    e: &Value,
    at: Option<f64>,
    sidechain: bool,
    out: &mut Vec<EventSeed>,
) {
    use crate::event::{EventKind, ToolKind, TurnEndReason};
    let msg = &e["message"];
    // A synthetic assistant row recording an API failure is the firm Errored signal. Its
    // `stop_reason` is `stop_sequence`, which is not a turn end.
    if e["isApiErrorMessage"] == true || e["error"].is_string() {
        let error = e["error"].as_str().filter(|e| !e.is_empty()).unwrap_or("api_error");
        out.push(
            EventSeed::new(EventKind::TurnEnd {
                reason: TurnEndReason::Error,
                error: Some(error.into()),
                background_agents: None,
            })
            .at(at),
        );
        return;
    }
    // One API message can span several transcript entries; dedupe its usage by message id.
    let id = msg["id"].as_str().unwrap_or_default();
    let reported = crate::TokenUsage {
        input_tokens: msg["usage"]["input_tokens"].as_u64(),
        output_tokens: msg["usage"]["output_tokens"].as_u64(),
        cache_read_input_tokens: msg["usage"]["cache_read_input_tokens"].as_u64(),
        cache_creation_input_tokens: msg["usage"]["cache_creation_input_tokens"].as_u64(),
    };
    // A sidechain row belongs to a helper conversation even when it is in the parent
    // transcript; keep its usage apart so it can't suppress the parent's own row. The
    // boolean is part of the key, so no message id can collide across the two.
    let usage_key = (sidechain, id.to_string());
    let previous = state.claude_usage.get(&usage_key).copied().unwrap_or_default();
    let next = highest(previous, reported);
    if next != previous {
        out.push(
            EventSeed::new(EventKind::Usage {
                usage: increase(previous, next),
                reported: next,
                message_id: (!id.is_empty()).then(|| id.into()),
                cumulative: false,
            })
            .at(at),
        );
        state.claude_usage.insert(usage_key, next);
    }
    for c in msg["content"].as_array().into_iter().flatten() {
        match c["type"].as_str().unwrap_or_default() {
            "text" => out.push(
                EventSeed::new(EventKind::AssistantText { text: c["text"].as_str().unwrap_or_default().into() }).at(at),
            ),
            "thinking" => out.push(
                EventSeed::new(EventKind::Reasoning {
                    signature: c["signature"].as_str().filter(|s| !s.is_empty()).map(String::from),
                })
                .at(at),
            ),
            "tool_use" => {
                let name = c["name"].as_str().unwrap_or_default();
                let call_id = c["id"].as_str().filter(|id| !id.is_empty());
                out.push(
                    EventSeed::new(EventKind::ToolCall {
                        name: name.into(),
                        call_id: call_id.map(String::from),
                        kind: ToolKind::of(name, &c["input"]),
                        input: c["input"].clone(),
                        asks_user: harness.discovery().asks_user(name),
                        activity_line: Some(describe_tool(name, &c["input"])),
                    })
                    .at(at),
                );
                if let Some(call_id) = call_id
                    && let Some(seed) = wakeup_call(state, name, &c["input"], call_id, at)
                {
                    out.push(seed);
                }
            }
            kind if known_content(kind) => {
                out.push(
                    EventSeed::new(EventKind::Meta {
                        record_type: "content".into(),
                        subtype: Some(kind.into()),
                        record: c.clone(),
                    })
                    .at(at),
                );
            }
            kind => out.push(
                EventSeed::new(EventKind::Unknown {
                    record_type: Some("content".into()),
                    reason: format!("unknown Claude Code content type \"{kind}\""),
                    record: c.clone(),
                })
                .at(at),
            ),
        }
    }
    if msg["stop_reason"] == "end_turn" {
        out.push(turn_end(TurnEndReason::EndTurn).at(at));
    }
}

fn system(e: &Value, at: Option<f64>, out: &mut Vec<EventSeed>) {
    use crate::event::{EventKind, TurnEndReason};
    match e["subtype"].as_str() {
        // A normal turn's firm end in order: assistant `end_turn`, then `stop_hook_summary`
        // when hooks ran, then `turn_duration` (about 0.06 s later). The duration record is
        // the firmest and also carries a firm count of helpers still running.
        Some("stop_hook_summary") => out.push(turn_end(TurnEndReason::DurationRecord).at(at)),
        Some("turn_duration") => out.push(
            EventSeed::new(EventKind::TurnEnd {
                reason: TurnEndReason::DurationRecord,
                error: None,
                background_agents: e["pendingBackgroundAgentCount"].as_u64().map(|n| n as usize),
            })
            .at(at),
        ),
        // A session exiting with its helpers: the turn is over, nothing is left running.
        // Its in-memory wakeups die with it too.
        Some("agents_killed") => {
            out.push(
                EventSeed::new(EventKind::WakeupCleared { kind: None, job_id: None, source_call_id: None }).at(at),
            );
            out.push(
                EventSeed::new(EventKind::TurnEnd {
                    reason: TurnEndReason::EndTurn,
                    error: None,
                    background_agents: Some(0),
                })
                .at(at),
            );
        }
        Some("compact_boundary") => out.push(
            EventSeed::new(EventKind::Compaction {
                trigger: e["compactMetadata"]["trigger"].as_str().map(String::from),
                pre_tokens: e["compactMetadata"]["preTokens"].as_u64(),
                post_tokens: e["compactMetadata"]["postTokens"].as_u64(),
            })
            .at(at),
        ),
        Some("scheduled_task_fire") => {
            let kind = match e["taskKind"].as_str().or_else(|| e["cronKind"].as_str()) {
                Some("loop") => Some(crate::state::WakeupKind::Loop),
                Some("cron") => Some(crate::state::WakeupKind::Cron),
                _ => None,
            };
            if let Some(kind) = kind {
                out.push(
                    EventSeed::new(EventKind::WakeupCleared { kind: Some(kind), job_id: None, source_call_id: None })
                        .at(at),
                );
            }
            out.push(meta(e).at(at));
        }
        Some("away_summary" | "bridge_status" | "informational" | "local_command") => out.push(meta(e).at(at)),
        Some(_) | None => out.push(meta(e).at(at)),
    }
}

fn turn_end(reason: crate::event::TurnEndReason) -> EventSeed {
    EventSeed::new(crate::event::EventKind::TurnEnd { reason, error: None, background_agents: None })
}

fn known_content(kind: &str) -> bool {
    matches!(
        kind,
        // The baseline's block types, the ADR parser's 20, and PLAN section 1's addition.
        "text"
            | "tool_use"
            | "tool_result"
            | "image"
            | "document"
            | "thinking"
            | "redacted_thinking"
            | "tool_reference"
            | "search_result"
            | "server_tool_use"
            | "web_search_tool_result"
            | "web_fetch_tool_result"
            | "code_execution_tool_result"
            | "bash_code_execution_tool_result"
            | "text_editor_code_execution_tool_result"
            | "container_upload"
            | "compaction"
            | "resource"
            | "resource_link"
            | "audio"
            | "tool_addition"
    )
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

fn highest(a: crate::TokenUsage, b: crate::TokenUsage) -> crate::TokenUsage {
    crate::TokenUsage {
        input_tokens: a.input_tokens.max(b.input_tokens),
        output_tokens: a.output_tokens.max(b.output_tokens),
        cache_read_input_tokens: a.cache_read_input_tokens.max(b.cache_read_input_tokens),
        cache_creation_input_tokens: a.cache_creation_input_tokens.max(b.cache_creation_input_tokens),
    }
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

fn interrupted(s: &str) -> bool {
    s.starts_with("[Request interrupted")
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

/// The identities a `task-notification` carries. `<status>` is required; the ids are
/// optional, so a notification without them still decrements the parent's count.
struct TaskNotification {
    status: String,
    task_id: String,
    tool_use_id: String,
    output_file: String,
}

fn task_notification(content: &Value) -> Option<TaskNotification> {
    let text = match content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => blocks.iter().find_map(|b| b["text"].as_str())?,
        _ => return None,
    };
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        text.split_once(&open)?.1.split_once(&close).map(|(value, _)| value.trim().to_string())
    };
    Some(TaskNotification {
        status: tag("status")?,
        task_id: tag("task-id").unwrap_or_default(),
        tool_use_id: tag("tool-use-id").unwrap_or_default(),
        output_file: tag("output-file").unwrap_or_default(),
    })
}

/// An async launch in a tool result: `status: async_launched`, `isAsync: true`, or a
/// `backgroundTaskId`. The agent id, task id, output file and spawning tool id are kept so
/// the helper transcript, `.meta.json` and completion notification can all be matched back.
fn async_launched(e: &Value, t: f64) -> Option<LaunchedHelper> {
    let result = e["toolUseResult"].as_object()?;
    let field = |name: &str| result.get(name).and_then(Value::as_str).unwrap_or_default();
    let task_id = field("backgroundTaskId");
    let is_async = result.get("isAsync").and_then(Value::as_bool) == Some(true);
    if !is_async && task_id.is_empty() && field("status") != "async_launched" {
        return None;
    }
    let tool_use_id = e["sourceToolUseID"]
        .as_str()
        .or_else(|| e["message"]["content"].as_array()?.iter().find_map(|c| c["tool_use_id"].as_str()))
        .or_else(|| result.get("toolUseId").and_then(Value::as_str))
        .unwrap_or_default();
    let helper = LaunchedHelper {
        agent_id: field("agentId").into(),
        task_id: task_id.into(),
        output_file: field("outputFile").into(),
        tool_use_id: tool_use_id.into(),
        ended: false,
        since: t,
    };
    (!helper.agent_id.is_empty()
        || !helper.task_id.is_empty()
        || !helper.output_file.is_empty()
        || !helper.tool_use_id.is_empty())
    .then_some(helper)
}

/// A Claude scheduled-wakeup tool call: remember it for its result and emit the inferred
/// entry immediately, so a transcript that stops before the result still says something is
/// armed.
fn wakeup_call(
    state: &mut crate::event::ParseState,
    name: &str,
    input: &Value,
    call_id: &str,
    at: Option<f64>,
) -> Option<EventSeed> {
    use crate::event::{ClaudeWakeupAction, ClaudeWakeupCall, EventKind};
    use crate::state::{Certainty, WakeupKind};
    use crate::time::Timestamp;

    let (kind, action) = match name {
        "ScheduleWakeup" => (
            WakeupKind::Loop,
            if input["stop"] == true { ClaudeWakeupAction::Stop } else { ClaudeWakeupAction::Create },
        ),
        "CronCreate" => (WakeupKind::Cron, ClaudeWakeupAction::Create),
        "CronDelete" => (WakeupKind::Cron, ClaudeWakeupAction::Delete),
        _ => return None,
    };
    let reason = input["reason"]
        .as_str()
        .or_else(|| input["prompt"].as_str())
        .map(crate::session::first_line)
        .filter(|reason| !reason.is_empty())
        .map(String::from);
    state.claude_wakeups.insert(call_id.into(), ClaudeWakeupCall { kind, action, reason: reason.clone() });
    match action {
        ClaudeWakeupAction::Stop => Some(
            EventSeed::new(EventKind::WakeupCleared { kind: Some(kind), job_id: None, source_call_id: None }).at(at),
        ),
        ClaudeWakeupAction::Delete => None,
        ClaudeWakeupAction::Create => {
            let due_at = (kind == WakeupKind::Loop)
                .then(|| {
                    let delay = input["delaySeconds"].as_f64()?;
                    Some(Timestamp::from_secs_f64(at? + delay))
                })
                .flatten();
            Some(
                EventSeed::new(EventKind::WakeupScheduled {
                    kind,
                    due_at,
                    reason,
                    job_id: None,
                    recurring: false,
                    certainty: Certainty::Inferred,
                    source_call_id: Some(call_id.into()),
                })
                .at(at),
            )
        }
    }
}

/// A tool result for a scheduled-wakeup tool. The result is the firm evidence: it carries
/// the actual due time or the harness job id, not the requested delay.
fn wakeup_result(
    state: &mut crate::event::ParseState,
    e: &Value,
    result: &Value,
    call_id: &str,
    at: Option<f64>,
) -> Option<EventSeed> {
    use crate::event::{ClaudeWakeupAction, EventKind};
    use crate::state::{Certainty, WakeupKind};
    use crate::time::Timestamp;

    let call = state.claude_wakeups.remove(call_id);
    let is_error = result["is_error"].as_bool() == Some(true);
    if is_error {
        let kind = call.as_ref().map(|call| call.kind);
        return Some(
            EventSeed::new(EventKind::WakeupCleared { kind, job_id: None, source_call_id: Some(call_id.into()) })
                .at(at),
        );
    }
    let data = &e["toolUseResult"];
    let text = result_text(&result["content"]).unwrap_or_default();

    if let Some(call) = &call {
        match call.action {
            ClaudeWakeupAction::Stop => {
                return Some(
                    EventSeed::new(EventKind::WakeupCleared {
                        kind: Some(WakeupKind::Loop),
                        job_id: None,
                        source_call_id: None,
                    })
                    .at(at),
                );
            }
            ClaudeWakeupAction::Delete => {
                let id = data["id"].as_str().filter(|id| !id.is_empty());
                return Some(
                    EventSeed::new(EventKind::WakeupCleared {
                        kind: Some(WakeupKind::Cron),
                        job_id: id.map(String::from),
                        source_call_id: Some(call_id.into()),
                    })
                    .at(at),
                );
            }
            ClaudeWakeupAction::Create => {}
        }
    }

    if data["stopped"].as_bool() == Some(true) {
        return Some(
            EventSeed::new(EventKind::WakeupCleared {
                kind: Some(WakeupKind::Loop),
                job_id: None,
                source_call_id: None,
            })
            .at(at),
        );
    }
    if let Some(millis) = data["scheduledFor"].as_f64().filter(|millis| *millis > 0.0) {
        let reason = call.and_then(|call| call.reason);
        return Some(
            EventSeed::new(EventKind::WakeupScheduled {
                kind: WakeupKind::Loop,
                due_at: Some(Timestamp::from_secs_f64(millis / 1000.0)),
                reason,
                job_id: None,
                recurring: false,
                certainty: Certainty::Firm,
                source_call_id: Some(call_id.into()),
            })
            .at(at),
        );
    }
    if let Some(id) = data["id"].as_str().filter(|id| !id.is_empty()) {
        if let Some(millis) = data["nextFireAt"].as_f64().filter(|millis| *millis > 0.0) {
            let reason = call.and_then(|call| call.reason).or_else(|| data["humanSchedule"].as_str().map(String::from));
            return Some(
                EventSeed::new(EventKind::WakeupScheduled {
                    kind: WakeupKind::Cron,
                    due_at: Some(Timestamp::from_secs_f64(millis / 1000.0)),
                    reason,
                    job_id: Some(id.into()),
                    recurring: data["recurring"].as_bool().unwrap_or(false),
                    certainty: Certainty::Firm,
                    source_call_id: Some(call_id.into()),
                })
                .at(at),
            );
        }
        if let Some(call) = &call
            && call.action == ClaudeWakeupAction::Delete
        {
            return Some(
                EventSeed::new(EventKind::WakeupCleared {
                    kind: Some(WakeupKind::Cron),
                    job_id: Some(id.into()),
                    source_call_id: Some(call_id.into()),
                })
                .at(at),
            );
        }
    }
    // A partial read may start after the tool call. The result shape still identifies a
    // cancelled cron job; don't infer any other tool from an unlabelled result.
    if text.starts_with("Cancelled job ")
        && let Some(id) = data["id"].as_str().filter(|id| !id.is_empty())
    {
        return Some(
            EventSeed::new(EventKind::WakeupCleared {
                kind: Some(WakeupKind::Cron),
                job_id: Some(id.into()),
                source_call_id: Some(call_id.into()),
            })
            .at(at),
        );
    }
    // A dynamic wakeup that was not scheduled (for example, the loop aged out) must not
    // leave the inferred call-time entry behind.
    if let Some(call) = call
        && call.action == ClaudeWakeupAction::Create
        && call.kind == WakeupKind::Loop
    {
        return Some(
            EventSeed::new(EventKind::WakeupCleared {
                kind: Some(WakeupKind::Loop),
                job_id: None,
                source_call_id: Some(call_id.into()),
            })
            .at(at),
        );
    }
    None
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
    use crate::session::{Last, Session};
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
        // The headline rule: bypass/dontAsk cannot put a prompt in front of a human.
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

        // The denial transcript closes the rejected turn with its interrupt and duration, too.
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
    const RECORDED: &str = include_str!("../../../../fixtures/claude-code/2.1/linux/seed-session/input/session.jsonl");

    #[test]
    fn names_the_project_from_cwd_not_the_encoded_folder() {
        // A project cwd and its encoded folder. The encoded folder is lossy; the records
        // moved into its `site` subdirectory.
        let path = Path::new("/h/.claude/projects/-home-dev-work-my-app/s.jsonl");
        let mut s = Session::at(Harness::ClaudeCode, 5, path);
        let entry = json!({
            "type": "user",
            "timestamp": at(1.0),
            "cwd": "/home/dev/work/my-app/site",
            "message": {"role": "user", "content": "ship it"}
        });
        s.apply(&entry);
        assert_eq!(s.project, "site", "the cwd, not the folder's my-app");

        // The folder's encoding is lossy; the records still win when they disagree.
        let mut s = Session::at(Harness::ClaudeCode, 5, Path::new("/h/.claude/projects/wrong-name/s.jsonl"));
        s.apply(&json!({
            "type": "assistant",
            "timestamp": at(1.0),
            "cwd": "/home/dev/work/lucky-mint-dingo",
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
            ("/home/dev/work/lucky-mint-dingo", "-home-dev-work-lucky-mint-dingo"),
            ("/home/dev/work/my-app", "-home-dev-work-my-app"),
        ] {
            assert_eq!(encode(cwd), folder);
        }
    }

    #[test]
    fn a_schedule_wakeup_result_stays_pending_until_it_fires() {
        let call = json!({"type": "assistant", "timestamp": at(4.0),
            "message": {"id": "m1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "ScheduleWakeup",
                    "input": {"delaySeconds": 60, "noop": false, "prompt": "Check the deploy.",
                              "reason": "Watching the deploy."}}]}});
        let result = json!({"type": "user", "timestamp": at(3.9),
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Next wakeup scheduled for 10:02:00 (in 60s)."}]},
            "toolUseResult": {"scheduledFor": 1793324520000_u64, "clampedDelaySeconds": 60, "wasClamped": false}});
        let mut s = session(&[prompt("watch the deploy", 5.0), call, result, end_turn(3.0, "m2")]);
        let pending = s.pending_wakeups();
        assert_eq!(pending.len(), 1, "{pending:#?}");
        assert_eq!(pending[0].kind, crate::WakeupKind::Loop);
        assert_eq!(pending[0].due_at, Some(crate::Timestamp::from_secs_f64(1793324520.0)));
        assert_eq!(pending[0].reason.as_deref(), Some("Watching the deploy."));
        assert_eq!(pending[0].certainty, crate::Certainty::Firm);

        s.apply(&json!({"type": "system", "subtype": "scheduled_task_fire", "timestamp": at(2.0),
            "taskId": "abc", "taskKind": "loop", "cronKind": "loop", "prompt": "Check the deploy."}));
        assert!(s.pending_wakeups().is_empty(), "the fire record clears the loop wakeup");
    }

    #[test]
    fn cron_create_and_delete_use_the_harness_job_id() {
        let create = json!({"type": "assistant", "timestamp": at(4.0),
            "message": {"id": "m1", "stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "toolu_c1", "name": "CronCreate",
                 "input": {"cron": "*/5 * * * *", "prompt": "Check the deploy.", "recurring": true}}]}});
        let created = json!({"type": "user", "timestamp": at(3.9),
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_c1",
                "content": "Scheduled recurring job 083784d8 (Every 5 minutes)."}]},
            "toolUseResult": {"id": "083784d8", "humanSchedule": "Every 5 minutes", "recurring": true,
                "durable": false, "nextFireAt": 1793324620207_u64}});
        let mut s = session(&[prompt("schedule a cron", 5.0), create, created, end_turn(3.0, "m2")]);
        let pending = s.pending_wakeups();
        assert_eq!(pending.len(), 1, "{pending:#?}");
        assert_eq!(pending[0].kind, crate::WakeupKind::Cron);
        assert_eq!(pending[0].due_at, Some(crate::Timestamp::from_secs_f64(1793324620.207)));
        assert_eq!(pending[0].certainty, crate::Certainty::Firm);

        let delete = json!({"type": "assistant", "timestamp": at(1.0),
            "message": {"id": "m3", "stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "toolu_c2", "name": "CronDelete", "input": {"id": "083784d8"}}]}});
        let deleted = json!({"type": "user", "timestamp": at(0.9),
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_c2", "content": "Cancelled job 083784d8."}]},
            "toolUseResult": {"id": "083784d8"}});
        s.apply(&delete);
        s.apply(&deleted);
        assert!(s.pending_wakeups().is_empty(), "CronDelete clears the matching job");
    }

    #[test]
    fn stopping_a_dynamic_loop_clears_only_its_wakeups() {
        let call = json!({"type": "assistant", "timestamp": at(4.0),
            "message": {"id": "m1", "stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "toolu_s1", "name": "ScheduleWakeup", "input": {"stop": true}}]}});
        let result = json!({"type": "user", "timestamp": at(3.9),
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_s1", "content": "Loop stopped."}]},
            "toolUseResult": {"scheduledFor": 0, "clampedDelaySeconds": 0, "wasClamped": false,
                "stopped": true, "cancelledWakeups": 1}});
        let s = session(&[call, result]);
        assert!(s.pending_wakeups().is_empty());
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
