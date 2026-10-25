//! The parse layer between bytes and the status reducer: one normalized [`Event`] per
//! transcript record (or content block), with stable provenance and deterministic ids.
//!
//! [`parse_lines`] is the stand-alone API; [`SessionReducer`](crate::SessionReducer) uses
//! the same parser and folds the resulting events into its [`SessionState`](crate::SessionState),
//! so a status and the events it was derived from cannot disagree.

use crate::adapters;
use crate::harness::Harness;
use crate::lean::lean;
use crate::reducer::ParseMode;
use crate::session::{Last, LaunchedHelper, Session};
use crate::state::{ActivityLine, PermissionMode, SandboxPolicy, TokenUsage};
use crate::time::Timestamp;
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;

/// Where a record came from: its source (a caller's file index), byte span, physical line,
/// and the ids the harness itself wrote for it. Offsets and lengths are in the original
/// input bytes, before [`ParseMode::Lean`] cuts anything; `line` is one-based.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Provenance {
    /// The caller's source index. [`parse_lines`] uses 0; use
    /// [`parse_lines_with_source`] when concatenating several transcripts.
    pub source: u32,
    /// The record's first byte in the source.
    pub offset: u64,
    /// The record's byte length in the source (not including the newline).
    pub length: u64,
    /// The one-based physical line the record starts on.
    pub line: u64,
    /// The harness's own ids for this record, in the order found: Claude's `uuid` and
    /// `message.id`, Codex's `payload.id`/`call_id`, and so on. Empty when it writes none.
    pub record_ids: Vec<String>,
}

/// One normalized event. Its [`kind`](Event::kind) is the payload; the common fields are
/// provenance (id, source, byte span, line), the harness, and the event's time when the
/// transcript records one.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    /// Deterministic for the same harness, source, bytes and offset.
    pub id: String,
    pub harness: Harness,
    #[serde(flatten)]
    pub provenance: Provenance,
    /// The record's RFC 3339 timestamp, or a Cursor prompt's `<timestamp>` block. `None`
    /// when the harness writes no time (Cursor's assistant and `turn_ended` records); the
    /// reducer then uses the transcript's mtime.
    pub at: Option<Timestamp>,
    /// Whether this record is a Claude Code sidechain row (`isSidechain: true`): activity
    /// in a helper conversation, not in the session that owns the transcript. A non-helper
    /// [`SessionReducer`](crate::SessionReducer) ignores these events.
    #[serde(skip_serializing_if = "is_false")]
    pub sidechain: bool,
    #[serde(flatten)]
    pub kind: EventKind,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The payload of an [`Event`]: what the record says, normalized to what status, titles,
/// usage and activity lines need.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// The user sent a prompt. `text` is what they typed (Cursor's `<user_query>` inner
    /// text); `raw` keeps the record's text with wrappers; `wrappers` splits known tag
    /// blocks. `starts_turn` is false for records kept for provenance but not treated as a
    /// new turn (Claude's slash-command and metadata rows).
    UserPrompt {
        text: String,
        raw: String,
        wrappers: Vec<PromptWrapper>,
        starts_turn: bool,
    },
    /// The model wrote text.
    AssistantText {
        text: String,
    },
    /// The model reasoned. Only presence and signature are kept, not the reasoning text.
    Reasoning {
        signature: Option<String>,
    },
    /// A tool call: its full input is kept, with a normalized `kind`, the harness's call id
    /// where it has one, and whether it asks the user something.
    ToolCall {
        name: String,
        call_id: Option<String>,
        kind: ToolKind,
        input: Value,
        asks_user: bool,
        /// The short activity line the reducer logs for this call, when it logs one.
        line: Option<ActivityLine>,
    },
    /// A tool result. `size` is the result content's length in bytes.
    ToolResult {
        call_id: Option<String>,
        is_error: bool,
        size: u64,
    },
    /// A turn started (`task_started`, `turn_started`).
    TurnStart {
        turn_id: Option<String>,
    },
    /// A turn ended, however the harness says so.
    TurnEnd {
        reason: TurnEndReason,
        error: Option<String>,
        /// Claude's `turn_duration.pendingBackgroundAgentCount`, or 0 for `agents_killed`.
        background_agents: Option<usize>,
    },
    /// Token usage. `usage` is what this event contributes to the total (so summing events
    /// gives the true total); `reported` is what the harness reported for this record.
    /// Claude's rows for one `message.id` are deduped by taking each field's highest value;
    /// Codex's `token_count` is a running total, so `cumulative` is true and `usage` is its
    /// delta from the previous event.
    Usage {
        usage: TokenUsage,
        reported: TokenUsage,
        message_id: Option<String>,
        cumulative: bool,
    },
    PermissionMode {
        mode: PermissionMode,
    },
    SandboxPolicy {
        policy: SandboxPolicy,
    },
    /// The model changed (Codex's `turn_context.model`).
    ModelChange {
        model: String,
    },
    /// A title and where it came from.
    Title {
        title: String,
        source: TitleSource,
    },
    Cost {
        usd: f64,
    },
    /// An async helper (Claude's background Agent/Task) was launched.
    HelperSpawn {
        agent_id: Option<String>,
        task_id: Option<String>,
        output_file: Option<String>,
        tool_use_id: Option<String>,
    },
    /// A helper finished, as a task notification says.
    HelperEnd {
        task_id: Option<String>,
        tool_use_id: Option<String>,
        output_file: Option<String>,
        status: String,
    },
    Compaction {
        trigger: Option<String>,
        pre_tokens: Option<u64>,
        post_tokens: Option<u64>,
    },
    /// Session facts carried by many records: cwd, branch, id, entrypoint, or reviewer.
    SessionInfo {
        cwd: Option<String>,
        branch: Option<String>,
        session_id: Option<String>,
        entrypoint: Option<String>,
        approvals_reviewer: Option<String>,
    },
    /// A known record type with no richer form, kept whole.
    Meta {
        record_type: String,
        subtype: Option<String>,
        record: Value,
    },
    /// A record or line this version doesn't understand, kept whole rather than dropped.
    Unknown {
        record_type: Option<String>,
        reason: String,
        record: Value,
    },
}

/// How a tool call is classified, for consumers that don't want to match harness names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolKind {
    Shell,
    Edit,
    Read,
    Search,
    Web,
    Agent,
    Question,
    Other,
}

impl ToolKind {
    pub(crate) fn of(name: &str, input: &Value) -> ToolKind {
        match name {
            "Bash" | "bash" | "Shell" | "shell" | "exec_command" | "shell_command" | "powershell" => ToolKind::Shell,
            "Edit" | "edit" | "Write" | "write" | "StrReplace" | "apply_patch" | "apply_diff" => ToolKind::Edit,
            "Read" | "read" | "read_file" | "ReadLints" | "view_image" => ToolKind::Read,
            "Grep" | "grep" | "Glob" | "glob" | "find" | "ls" | "list" | "search" | "codebase_search"
            | "tool_search" => ToolKind::Search,
            "WebFetch" | "WebSearch" | "webfetch" | "websearch" => ToolKind::Web,
            "Agent" | "Task" | "task" => ToolKind::Agent,
            "AskUserQuestion" | "ExitPlanMode" | "request_user_input" | "AskQuestion" | "ask_question" | "question"
            | "plan_exit" => ToolKind::Question,
            _ if input["command"].is_string() => ToolKind::Shell,
            _ => ToolKind::Other,
        }
    }
}

/// Why a turn ended, in the vocabulary of PLAN.md section 5.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub enum TurnEndReason {
    #[serde(rename = "end_turn")]
    EndTurn,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "aborted")]
    Aborted,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "duration-record")]
    DurationRecord,
}

/// Where a title came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TitleSource {
    /// Claude's generated `ai-title`.
    Generated,
    /// Codex's `session_index.jsonl`.
    Index,
    /// Pi's `session_info.name`.
    SessionName,
    /// The opening prompt.
    Prompt,
    Other,
}

/// One tag block a user prompt arrived wrapped in, such as `<user_query>` or `<timestamp>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptWrapper {
    pub name: String,
    pub value: String,
}

/// Parse every complete record in `bytes` into events. The last record with no newline is
/// read when it's already a whole JSON object, as the reducer's line reader does. Malformed
/// lines and unknown record types become [`EventKind::Unknown`] rather than disappearing.
pub fn parse_lines(harness: Harness, bytes: &[u8], mode: ParseMode) -> impl Iterator<Item = Event> + '_ {
    parse_lines_with_source(harness, 0, bytes, mode)
}

/// [`parse_lines`] with a caller-defined source index, for multi-file inputs.
pub fn parse_lines_with_source(
    harness: Harness,
    source: u32,
    bytes: &[u8],
    mode: ParseMode,
) -> impl Iterator<Item = Event> + '_ {
    parse_events(harness, source, bytes, mode).into_iter()
}

/// Parse all events eagerly. Used by the public iterator and by tests; the reducer parses
/// one line at a time through the same adapter parser.
pub(crate) fn parse_events(harness: Harness, source: u32, bytes: &[u8], mode: ParseMode) -> Vec<Event> {
    let mut out = Vec::new();
    // A stand-alone parse sees every record in the file, including helper sidechains. Each
    // sidechain event says so, so a parent reducer can ignore it.
    let mut state = ParseState::default();
    let mut at = 0usize;
    let mut line_no = 1u64;
    while at < bytes.len() {
        let end = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |i| at + i);
        let has_newline = end < bytes.len();
        let line = &bytes[at..end];
        let last = !has_newline;
        if last && !ends_an_object(line) {
            break;
        }
        match parse_record_line(line, mode) {
            Ok(values) => {
                for (value, start, length) in values {
                    let provenance = Provenance {
                        source,
                        offset: (at + start) as u64,
                        length: length as u64,
                        line: line_no,
                        record_ids: record_ids(&value),
                    };
                    if !value.is_object() {
                        push_event(
                            harness,
                            &mut out,
                            provenance,
                            None,
                            EventKind::Unknown {
                                record_type: None,
                                reason: "record is not a JSON object".into(),
                                record: value,
                            },
                            false,
                        );
                        continue;
                    }
                    let seeds = harness.adapter().parse(&mut state, &value);
                    if seeds.is_empty() {
                        push_event(
                            harness,
                            &mut out,
                            provenance,
                            None,
                            EventKind::Meta {
                                record_type: record_type(&value),
                                subtype: subtype(&value),
                                record: value,
                            },
                            false,
                        );
                    } else {
                        for seed in seeds {
                            push_event(harness, &mut out, provenance.clone(), seed.at, seed.kind, seed.sidechain);
                        }
                    }
                }
            }
            Err(()) if !line.trim_ascii().is_empty() => {
                let provenance = Provenance {
                    source,
                    offset: at as u64,
                    length: line.len() as u64,
                    line: line_no,
                    record_ids: Vec::new(),
                };
                push_event(
                    harness,
                    &mut out,
                    provenance,
                    None,
                    EventKind::Unknown {
                        record_type: None,
                        reason: "malformed JSON line".into(),
                        record: Value::String(String::from_utf8_lossy(line).into_owned()),
                    },
                    false,
                );
            }
            Err(()) => {}
        }
        at = end + usize::from(has_newline);
        line_no += 1;
    }
    out
}

fn push_event(
    harness: Harness,
    out: &mut Vec<Event>,
    provenance: Provenance,
    at: Option<f64>,
    kind: EventKind,
    sidechain: bool,
) {
    let id = format!(
        "{}:{}:{}:{}:{}:{}",
        harness.id(),
        provenance.source,
        provenance.line,
        provenance.offset,
        provenance.length,
        out.len()
    );
    out.push(Event { id, harness, provenance, at: at.map(Timestamp::from_secs_f64), sidechain, kind });
}

/// Parse the JSON values on one line, returning each with its byte start and length in the
/// original line. Several objects on one line are all read (as the reducer's reader does);
/// a line with trailing junk is rejected whole.
fn parse_record_line(line: &[u8], mode: ParseMode) -> Result<Vec<(Value, usize, usize)>, ()> {
    let bytes: Cow<'_, [u8]> = match mode {
        ParseMode::Lean => lean(line),
        ParseMode::Full => Cow::Borrowed(line),
    };
    // Lean can shorten strings, so parse the original line again just for spans when it did.
    let source_spans = if matches!(bytes, Cow::Owned(_)) { Some(spans(line)?) } else { None };
    let mut stream = serde_json::Deserializer::from_slice(&bytes).into_iter::<Value>();
    let mut out = Vec::new();
    loop {
        let before = stream.byte_offset();
        match stream.next() {
            None => return Ok(out),
            Some(Ok(value)) => {
                let after = stream.byte_offset();
                let (start, length) = match &source_spans {
                    Some(spans) => *spans.get(out.len()).ok_or(())?,
                    None => {
                        let mut start = before;
                        while start < after && line.get(start).is_some_and(u8::is_ascii_whitespace) {
                            start += 1;
                        }
                        (start, after.saturating_sub(start))
                    }
                };
                out.push((value, start, length));
            }
            Some(Err(_)) => return Err(()),
        }
    }
}

/// Every JSON value's `(start, length)` in the original line, without building values.
fn spans(line: &[u8]) -> Result<Vec<(usize, usize)>, ()> {
    let mut stream = serde_json::Deserializer::from_slice(line).into_iter::<serde::de::IgnoredAny>();
    let mut out = Vec::new();
    loop {
        let before = stream.byte_offset();
        match stream.next() {
            None => return Ok(out),
            Some(Ok(_)) => {
                let after = stream.byte_offset();
                let mut start = before;
                while start < after && line.get(start).is_some_and(u8::is_ascii_whitespace) {
                    start += 1;
                }
                out.push((start, after.saturating_sub(start)));
            }
            Some(Err(_)) => return Err(()),
        }
    }
}

fn ends_an_object(tail: &[u8]) -> bool {
    let t = tail.trim_ascii();
    t.starts_with(b"{") && t.ends_with(b"}")
}

fn record_type(record: &Value) -> String {
    record["type"].as_str().unwrap_or_default().into()
}

fn subtype(record: &Value) -> Option<String> {
    record["subtype"].as_str().or_else(|| record["payload"]["type"].as_str()).map(String::from)
}

/// The harness's own ids for a record, deduplicated in the order found.
pub(crate) fn record_ids(record: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let mut add = |id: Option<&str>| {
        if let Some(id) = id.filter(|id| !id.is_empty())
            && !ids.iter().any(|seen| seen == id)
        {
            ids.push(id.to_string());
        }
    };
    for key in ["uuid", "id", "requestId", "sessionId", "session_id", "agentId", "turn_id", "thread_id"] {
        add(record[key].as_str());
    }
    add(record["message"]["id"].as_str());
    let payload = &record["payload"];
    for key in ["id", "call_id", "session_id", "turn_id", "thread_id", "response_id"] {
        add(payload[key].as_str());
    }
    add(payload["item"]["id"].as_str());
    for content in record["message"]["content"].as_array().into_iter().flatten() {
        add(content["id"].as_str());
        add(content["tool_use_id"].as_str());
    }
    for content in payload["content"].as_array().into_iter().flatten() {
        add(content["id"].as_str());
        add(content["call_id"].as_str());
    }
    ids
}

/// Parse state shared across records: which Codex prompt format appeared first, and the
/// highest usage seen per Claude message / the running Codex total.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ParseState {
    pub(crate) codex_prompt_source: Option<&'static str>,
    pub(crate) claude_usage: HashMap<(bool, String), TokenUsage>,
    pub(crate) codex_usage: TokenUsage,
    /// Pi reports cost per model call; this is the session total so far.
    pub(crate) pi_cost: f64,
    /// OpenCode message id -> the role its message row named, so its parts know whose
    /// text they carry (a part row has no role of its own).
    pub(crate) opencode_roles: HashMap<String, &'static str>,
}

/// A prompt's tag wrappers (`<user_query>…</user_query>`, `<timestamp>…</timestamp>`, …).
pub(crate) fn prompt_wrappers(text: &str) -> Vec<PromptWrapper> {
    let mut wrappers = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after_open = &rest[open + 1..];
        let Some(name_end) = after_open.find('>') else { break };
        let name = &after_open[..name_end];
        if name.is_empty() || name.starts_with('/') || name.contains(char::is_whitespace) {
            rest = &after_open[name_end + 1..];
            continue;
        }
        let close = format!("</{name}>");
        let Some(value_end) = after_open.find(&close) else { break };
        let value = &after_open[name_end + 1..value_end];
        wrappers.push(PromptWrapper { name: name.into(), value: value.into() });
        rest = &after_open[value_end + close.len()..];
    }
    wrappers
}

/// The reducer's event application: the one place an event becomes session state.
impl Session {
    pub(crate) fn apply_event(&mut self, event: &Event) {
        if event.sidechain && !self.is_helper {
            return;
        }
        let at = event.at.map(Timestamp::as_secs_f64);
        let t = at.unwrap_or(self.mtime);
        match &event.kind {
            EventKind::SessionInfo { cwd, branch, session_id, entrypoint, approvals_reviewer } => {
                if let Some(cwd) = cwd {
                    self.set_cwd(cwd);
                }
                if let Some(branch) = branch.as_deref().filter(|b| !b.is_empty()) {
                    self.branch = branch.into();
                }
                if let Some(id) = session_id {
                    self.set_session_id(id);
                }
                if let Some(entrypoint) = entrypoint {
                    self.set_entrypoint(entrypoint);
                }
                if let Some(reviewer) = approvals_reviewer {
                    self.set_approval_reviewer_auto(reviewer == "auto_review");
                }
            }
            EventKind::PermissionMode { mode } => self.set_permission(Some(*mode)),
            EventKind::SandboxPolicy { policy } => self.set_sandbox(Some(*policy)),
            EventKind::Title { title, source } => match source {
                TitleSource::SessionName => self.set_session_name_title(Some(title)),
                _ => self.title = title.clone(),
            },
            EventKind::Cost { usd } => self.cost = Some(*usd),
            EventKind::UserPrompt { text, raw, starts_turn, .. } => {
                if !starts_turn {
                    return;
                }
                let t = match self.harness {
                    // Cursor's prompts are dated by their own timestamp, but never after the
                    // file was written; and record order is its clock, not the timestamp.
                    Harness::Cursor => at.map_or(self.mtime, |t| t.min(self.mtime)),
                    _ => t,
                };
                if self.harness == Harness::Cursor {
                    adapters::cursor::find_project(
                        self,
                        raw.split(|c: char| c.is_whitespace() || "\"'`()<>,;".contains(c)),
                    );
                }
                match self.harness {
                    Harness::Cursor => {
                        if let Some(first) = self.prompt_in_order(text, t) {
                            self.set_title_from_prompt(first);
                        }
                    }
                    Harness::Codex | Harness::Pi => {
                        if let Some(first) = self.prompt(text, t) {
                            self.set_title_from_prompt(first);
                        }
                    }
                    _ => {
                        self.prompt(text, t);
                    }
                }
            }
            EventKind::ToolCall { name: _name, call_id, asks_user, line, input, .. } => {
                if self.harness == Harness::Cursor {
                    adapters::cursor::find_project(self, adapters::cursor::strings(input));
                }
                match call_id.as_deref() {
                    Some(call_id) if !call_id.is_empty() => {
                        self.tool_call_id(call_id, *asks_user, line.clone(), t);
                    }
                    _ => self.tool_call(*asks_user, line.clone(), t),
                }
            }
            EventKind::ToolResult { call_id, .. } => match call_id.as_deref() {
                Some(call_id) if !call_id.is_empty() => self.tool_result_id(call_id, t),
                _ => self.set(Last::ToolResult, t),
            },
            EventKind::TurnStart { .. } => self.set(Last::Prompt, t),
            EventKind::TurnEnd { reason, error, background_agents } => {
                match reason {
                    TurnEndReason::EndTurn => self.set(Last::EndTurn, t),
                    TurnEndReason::DurationRecord => {
                        if self.last == Last::Errored {
                            self.last_at = t;
                        } else {
                            self.set(Last::EndTurn, t);
                        }
                    }
                    TurnEndReason::Interrupted | TurnEndReason::Aborted => self.set(Last::Interrupted, t),
                    TurnEndReason::Error => self.set_error(error.as_deref().unwrap_or("error"), t),
                }
                if let Some(n) = background_agents {
                    self.set_background_agents(*n, t);
                }
            }
            EventKind::Usage { reported, message_id, cumulative, .. } => {
                if *cumulative {
                    self.set_usage(*reported);
                } else {
                    self.record_usage(message_id.as_deref().unwrap_or_default(), *reported);
                }
            }
            EventKind::AssistantText { .. } => {
                // Cursor records no tool results: the model writing again means they're in.
                if self.harness == Harness::Cursor {
                    self.set(Last::ToolResult, t);
                }
            }
            EventKind::Reasoning { .. } => {
                if self.harness == Harness::Cursor {
                    self.set(Last::ToolResult, t);
                }
            }
            EventKind::HelperSpawn { agent_id, task_id, output_file, tool_use_id } => {
                let helper = LaunchedHelper {
                    agent_id: agent_id.clone().unwrap_or_default(),
                    task_id: task_id.clone().unwrap_or_default(),
                    output_file: output_file.clone().unwrap_or_default(),
                    tool_use_id: tool_use_id.clone().unwrap_or_default(),
                    ended: false,
                    since: t,
                };
                if !helper.agent_id.is_empty()
                    || !helper.task_id.is_empty()
                    || !helper.output_file.is_empty()
                    || !helper.tool_use_id.is_empty()
                {
                    self.launch_background(helper);
                }
            }
            EventKind::HelperEnd { task_id, tool_use_id, output_file, .. } => {
                self.background_agent_notified(
                    task_id.as_deref().unwrap_or_default(),
                    tool_use_id.as_deref().unwrap_or_default(),
                    output_file.as_deref().unwrap_or_default(),
                );
            }
            EventKind::Compaction { .. }
            | EventKind::ModelChange { .. }
            | EventKind::Meta { .. }
            | EventKind::Unknown { .. } => {}
        }
    }
}

/// Read one record's seeds through the harness's parser and apply them to a session. This is
/// what [`Session::apply`] and the reducer's chunk reader use.
pub(crate) fn apply_record(session: &mut Session, record: &Value) {
    let seeds = session.harness.adapter().parse(&mut session.parse_state, record);
    for seed in seeds {
        let event = Event {
            id: String::new(),
            harness: session.harness,
            provenance: Provenance::default(),
            at: seed.at.map(Timestamp::from_secs_f64),
            sidechain: seed.sidechain,
            kind: seed.kind,
        };
        session.apply_event(&event);
    }
}

// Event has no Deref; this tiny trait impl keeps `record_type` reachable for tests without
// exposing parser internals.
impl Event {
    /// The event's payload classification, for code that only needs the variant name.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            EventKind::UserPrompt { .. } => "user_prompt",
            EventKind::AssistantText { .. } => "assistant_text",
            EventKind::Reasoning { .. } => "reasoning",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::ToolResult { .. } => "tool_result",
            EventKind::TurnStart { .. } => "turn_start",
            EventKind::TurnEnd { .. } => "turn_end",
            EventKind::Usage { .. } => "usage",
            EventKind::PermissionMode { .. } => "permission_mode",
            EventKind::SandboxPolicy { .. } => "sandbox_policy",
            EventKind::ModelChange { .. } => "model_change",
            EventKind::Title { .. } => "title",
            EventKind::Cost { .. } => "cost",
            EventKind::HelperSpawn { .. } => "helper_spawn",
            EventKind::HelperEnd { .. } => "helper_end",
            EventKind::Compaction { .. } => "compaction",
            EventKind::SessionInfo { .. } => "session_info",
            EventKind::Meta { .. } => "meta",
            EventKind::Unknown { .. } => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkCtx, ReducerOptions, SessionReducer};
    use serde_json::{Value, json};
    use std::path::Path;

    fn parsed(harness: Harness, text: &str) -> Vec<Event> {
        parse_events(harness, 0, text.as_bytes(), ParseMode::Full)
    }

    fn no_unknown(harness: Harness, record: Value) -> Vec<Event> {
        let text = format!("{record}\n");
        let events = parsed(harness, &text);
        assert!(!events.is_empty(), "{harness:?}: {record}");
        assert!(events.iter().all(|event| event.kind_name() != "unknown"), "{harness:?}: {record} -> {events:#?}");
        events
    }

    #[test]
    fn provenance_is_stable_and_points_at_each_record() {
        let text = concat!(
            r#"{"type":"attachment","n":1}{"type":"attachment","n":2}"#,
            "\n",
            r#"{"type":"attachment","n":3}"#,
            "\n",
            r#"{"type":"attachment","n":4}"#,
        );
        let first = parsed(Harness::ClaudeCode, text);
        let second = parsed(Harness::ClaudeCode, text);
        assert_eq!(first, second, "same bytes, same events and ids");
        assert_eq!(first.len(), 4);
        assert_eq!(first.iter().map(|e| e.provenance.line).collect::<Vec<_>>(), [1, 1, 2, 3]);
        let one = r#"{"type":"attachment","n":1}"#;
        let two = r#"{"type":"attachment","n":2}"#;
        let three = r#"{"type":"attachment","n":3}"#;
        let first_offset = text.find(one).unwrap() as u64;
        let second_offset = text.find(two).unwrap() as u64;
        assert_eq!((first[0].provenance.offset, first[0].provenance.length), (first_offset, one.len() as u64));
        assert_eq!((first[1].provenance.offset, first[1].provenance.length), (second_offset, two.len() as u64));
        assert_eq!(first[2].provenance.offset, text.find(three).unwrap() as u64);
        assert_eq!(first[2].provenance.length, three.len() as u64);
        // Ids are unique within a parse and derived from the source span, not iteration luck.
        let ids: std::collections::HashSet<_> = first.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), first.len());

        let sourced = parse_events(Harness::ClaudeCode, 7, text.as_bytes(), ParseMode::Full);
        assert_eq!(sourced[0].provenance.source, 7);
        assert_ne!(sourced[0].id, first[0].id, "the source is part of the id");
    }

    #[test]
    fn lean_and_full_agree_on_provenance_and_ids() {
        let huge = json!({"type":"assistant","message":{"id":"m1","usage":{"output_tokens":2},
                        "content":[{"type":"text","text":"x".repeat(20_000)}]}});
        let text = format!("{huge}\n{}\n", json!({"type":"attachment"}));
        let lean = parse_events(Harness::ClaudeCode, 3, text.as_bytes(), ParseMode::Lean);
        let full = parse_events(Harness::ClaudeCode, 3, text.as_bytes(), ParseMode::Full);
        let shape = |events: &[Event]| {
            events
                .iter()
                .map(|event| (event.id.clone(), event.provenance.clone(), event.kind_name()))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&lean), shape(&full));
    }

    #[test]
    fn record_ids_keep_the_harness_ones() {
        let events = parsed(
            Harness::ClaudeCode,
            &format!("{}\n", json!({"type":"assistant","uuid":"u1","message":{"id":"m1","content":[]}})),
        );
        let meta = events.iter().find(|e| e.kind_name() == "meta").unwrap();
        assert_eq!(meta.provenance.record_ids, ["u1", "m1"]);

        let events = parsed(
            Harness::Codex,
            &format!("{}\n", json!({"type":"response_item","payload":{"type":"function_call","call_id":"c1"}})),
        );
        assert!(events[0].provenance.record_ids.iter().any(|id| id == "c1"));
    }

    #[test]
    fn malformed_lines_and_unknown_types_are_events_not_gaps() {
        let text = concat!(
            "not json\n",
            r#"{"type":"something_new","payload":{"x":1}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"go"}}"#,
            "\n",
        );
        let events = parsed(Harness::ClaudeCode, text);
        assert!(matches!(&events[0].kind, EventKind::Unknown { reason, .. } if reason.contains("malformed")));
        assert!(matches!(&events[1].kind, EventKind::Unknown { record_type: Some(t), .. } if t == "something_new"));
        assert!(matches!(&events[2].kind, EventKind::UserPrompt { text, starts_turn: true, .. } if text == "go"));
        assert_eq!(events.len(), 3, "nothing is silently dropped");

        // A half-written last line stays out, exactly as the reducer's line reader leaves it.
        let events = parsed(Harness::ClaudeCode, r#"{"type":"attachment"}"#.trim_end_matches('}'));
        assert!(events.is_empty());
    }

    #[test]
    fn baseline_record_types_and_content_types_all_map_to_events() {
        let claude: Value = serde_json::from_str(include_str!("../../../spec/baseline/claude-code.json")).unwrap();
        for group in ["main", "subagent"] {
            for kind in claude["record_types"][group].as_array().unwrap().iter().filter_map(Value::as_str) {
                let record = match kind {
                    "assistant" => json!({"type":"assistant","message":{"content":[]}}),
                    "user" => json!({"type":"user","message":{"content":""}}),
                    "system" => json!({"type":"system","subtype":"informational"}),
                    other => json!({"type":other}),
                };
                no_unknown(Harness::ClaudeCode, record);
            }
        }
        for kind in [
            "text",
            "tool_use",
            "tool_result",
            "image",
            "document",
            "thinking",
            "redacted_thinking",
            "tool_reference",
            "search_result",
            "server_tool_use",
            "web_search_tool_result",
            "web_fetch_tool_result",
            "code_execution_tool_result",
            "bash_code_execution_tool_result",
            "text_editor_code_execution_tool_result",
            "container_upload",
            "compaction",
            "resource",
            "resource_link",
            "audio",
            "tool_addition",
        ] {
            no_unknown(Harness::ClaudeCode, json!({"type":"assistant","message":{"content":[{"type":kind}]}}));
        }
        for kind in ["text", "image", "tool_result"] {
            let block = if kind == "tool_result" {
                json!({"type":kind,"content":[{"type":"text","text":"ok"}]})
            } else {
                json!({"type":kind})
            };
            no_unknown(Harness::ClaudeCode, json!({"type":"user","message":{"content":[block]}}));
        }

        let codex: Value = serde_json::from_str(include_str!("../../../spec/baseline/codex.json")).unwrap();
        for (top, subtypes) in codex["record_types"].as_object().unwrap() {
            for subtype in subtypes.as_array().unwrap() {
                let mut record = json!({"type":top,"payload":{}});
                if let Some(subtype) = subtype.as_str() {
                    record["payload"]["type"] = json!(subtype);
                }
                no_unknown(Harness::Codex, record);
            }
        }
        // ADR's wider Codex record catalogue, which the one-machine baseline doesn't contain.
        for kind in [
            "inter_agent_communication",
            "inter_agent_communication_metadata",
            "compacted",
            "token_usage_record",
            "world_state",
            "retained_context",
            "security_risk_score",
            "realtime_item",
        ] {
            no_unknown(Harness::Codex, json!({"type":kind,"payload":{}}));
        }
        for kind in ["input_text", "output_text"] {
            no_unknown(
                Harness::Codex,
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":kind}]}}),
            );
        }

        let pi: Value = serde_json::from_str(include_str!("../../../spec/baseline/pi.json")).unwrap();
        for kind in pi["record_types"].as_array().unwrap().iter().filter_map(Value::as_str) {
            let record = match kind {
                "message" => json!({"type":"message","message":{"role":"assistant","content":[]}}),
                "session" => json!({"type":"session","version":3,"id":"s","cwd":"/w"}),
                "compaction" => json!({"type":"compaction","summary":"s","firstKeptEntryId":"x","tokensBefore":1}),
                "usage" => json!({"type":"usage","kind":"cache_warm","usage":{}}),
                "branch_summary" => json!({"type":"branch_summary","summary":"s","fromId":null}),
                other => json!({"type":other}),
            };
            no_unknown(Harness::Pi, record);
        }
        for role in ["system", "user", "assistant", "toolResult", "bashExecution", "custom"] {
            no_unknown(Harness::Pi, json!({"type":"message","message":{"role":role,"content":[]}}));
        }
        for kind in ["text", "thinking", "toolCall"] {
            no_unknown(Harness::Pi, json!({"type":"message","message":{"role":"assistant","content":[{"type":kind}]}}));
        }
        no_unknown(
            Harness::Pi,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
        );

        let cursor: Value = serde_json::from_str(include_str!("../../../spec/baseline/cursor.json")).unwrap();
        for kind in cursor["record_types"].as_array().unwrap().iter().filter_map(Value::as_str) {
            no_unknown(Harness::Cursor, json!({"type":kind,"status":"success"}));
        }
        for role in cursor["roles"].as_array().unwrap().iter().filter_map(Value::as_str) {
            no_unknown(Harness::Cursor, json!({"role":role,"message":{"content":[]}}));
        }
        for kind in ["text", "tool_use"] {
            no_unknown(
                Harness::Cursor,
                json!({"role":"assistant","message":{"content":[{"type":kind,"name":"Read","input":{}}]}}),
            );
        }
        no_unknown(Harness::Cursor, json!({"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}));
    }

    #[test]
    fn prompts_keep_their_wrappers() {
        let cursor = "Friday, Oct 16, 2026, 9:41 AM (UTC-7)";
        let prompt = format!("<timestamp>{cursor}</timestamp>\n<user_query>fix the build</user_query>");
        let events = parsed(
            Harness::Cursor,
            &format!("{}\n", json!({"role":"user","message":{"content":[{"type":"text","text":prompt}]}})),
        );
        let EventKind::UserPrompt { text, raw, wrappers, starts_turn } = &events[0].kind else {
            panic!("{events:#?}");
        };
        assert_eq!((text.as_str(), starts_turn), ("fix the build", &true));
        assert_eq!(raw, &prompt);
        assert!(wrappers.iter().any(|w| w.name == "user_query" && w.value == "fix the build"));
        assert!(wrappers.iter().any(|w| w.name == "timestamp" && w.value == cursor));
        assert_eq!(events[0].at, Timestamp::parse_rfc3339("2026-10-16T16:41:00Z"));

        let command = parsed(
            Harness::ClaudeCode,
            &format!("{}\n", json!({"type":"user","message":{"content":"<command-name>/clear</command-name>"}})),
        );
        let EventKind::UserPrompt { wrappers, starts_turn, .. } = &command[0].kind else {
            panic!("{command:#?}");
        };
        assert!(!starts_turn, "a slash command is not a new turn");
        assert!(wrappers.iter().any(|w| w.name == "command-name" && w.value == "/clear"));
    }

    #[test]
    fn usage_events_sum_to_the_reported_total() {
        let rows = [
            json!({"type":"assistant","message":{"id":"m1","usage":{"output_tokens":8},"content":[]}}),
            json!({"type":"assistant","message":{"id":"m1","usage":{"output_tokens":8},"content":[]}}),
            json!({"type":"assistant","message":{"id":"m1","usage":{"output_tokens":135},"content":[]}}),
            json!({"type":"assistant","message":{"id":"m2","usage":{"output_tokens":4},"content":[]}}),
        ];
        let text: String = rows.iter().map(|row| format!("{row}\n")).collect();
        let events = parsed(Harness::ClaudeCode, &text);
        let usage: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Usage { usage, reported, .. } => Some((usage.to_owned(), reported.to_owned())),
                _ => None,
            })
            .collect();
        assert_eq!(usage.iter().filter_map(|(u, _)| u.output_tokens).sum::<u64>(), 139);
        assert_eq!(usage.last().unwrap().1.output_tokens, Some(4));

        let codex = [
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"output_tokens":42}}}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"output_tokens":42}}}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2400,"output_tokens":84}}}}),
        ];
        let text: String = codex.iter().map(|row| format!("{row}\n")).collect();
        let events = parsed(Harness::Codex, &text);
        let usage: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Usage { usage, reported, cumulative: true, .. } => {
                    Some((usage.to_owned(), reported.to_owned()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(usage.iter().filter_map(|(u, _)| u.input_tokens).sum::<u64>(), 2400);
        assert_eq!(usage.iter().filter_map(|(u, _)| u.output_tokens).sum::<u64>(), 84);
        assert_eq!(usage.last().unwrap().1.input_tokens, Some(2400));
    }

    #[test]
    fn sidechain_records_do_not_change_the_parent_when_fed_as_events() {
        let text = concat!(
            r#"{"type":"user","timestamp":"2026-10-23T10:00:00Z","message":{"content":"parent"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-23T10:00:01Z","message":{"id":"m1","stop_reason":"end_turn","usage":{"output_tokens":2},"content":[{"type":"text","text":"child"}]}}"#,
            "\n",
        );
        let now = Timestamp::parse_rfc3339("2026-10-23T10:00:05Z").unwrap();
        let path = Path::new("/h/.claude/projects/-w-app/s.jsonl");

        let mut fed = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        fed.set_path(path);
        fed.transcript_written(now);
        fed.feed(text.as_bytes(), ChunkCtx { offset: 0, mtime: now, path }).unwrap();
        assert_eq!(fed.state(now).activity, crate::Activity::Thinking, "the sidechain is not the parent's turn");

        let events = parse_events(Harness::ClaudeCode, 0, text.as_bytes(), ParseMode::Full);
        let parent = events
            .iter()
            .find(|event| matches!(&event.kind, EventKind::UserPrompt { text, .. } if text == "parent"))
            .expect("the parent prompt");
        assert!(!parent.sidechain);
        let child = events
            .iter()
            .find(|event| matches!(&event.kind, EventKind::AssistantText { text } if text == "child"))
            .expect("the helper text");
        assert!(child.sidechain, "the public event says it is a sidechain record");
        assert!(
            events.iter().filter(|event| event.provenance.line == 2).all(|event| event.sidechain),
            "every event from a sidechain record carries the marker"
        );

        let mut from_events = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        from_events.set_path(path);
        from_events.transcript_written(now);
        from_events.feed_events(&events);
        assert_eq!(from_events.state(now).activity, crate::Activity::Thinking);
    }

    #[test]
    fn a_parent_with_sidechains_feeds_the_same_state_from_bytes_and_events() {
        // The sidechain rows carry facts a parent would otherwise pick up (a cwd, a prompt,
        // a tool call, a turn end and a background-helper count); none of them are the
        // parent's. The parent finishes on its own row at the end.
        let text = concat!(
            r#"{"type":"user","timestamp":"2026-10-23T10:00:00Z","cwd":"/w/app","message":{"content":"parent"}}"#,
            "\n",
            r#"{"type":"user","isSidechain":true,"timestamp":"2026-10-23T10:00:01Z","cwd":"/w/helper","message":{"content":"helper prompt"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-23T10:00:02Z","message":{"id":"m1","stop_reason":"tool_use","usage":{"output_tokens":2},"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 9"}}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-23T10:00:03Z","message":{"id":"m1","stop_reason":"end_turn","usage":{"output_tokens":3},"content":[{"type":"text","text":"helper done"}]}}"#,
            "\n",
            r#"{"type":"system","isSidechain":true,"subtype":"turn_duration","timestamp":"2026-10-23T10:00:04Z","pendingBackgroundAgentCount":4}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-10-23T10:00:05Z","message":{"id":"m2","stop_reason":"end_turn","usage":{"output_tokens":5},"content":[{"type":"text","text":"parent done"}]}}"#,
            "\n",
        );
        let now = Timestamp::parse_rfc3339("2026-10-23T10:00:10Z").unwrap();
        let path = Path::new("/h/.claude/projects/-w-app/s.jsonl");
        let options = ReducerOptions { mode: ParseMode::Full, ..Default::default() };

        let mut fed = SessionReducer::new(Harness::ClaudeCode, options.clone());
        fed.set_path(path);
        fed.transcript_written(now);
        fed.feed(text.as_bytes(), ChunkCtx { offset: 0, mtime: now, path }).unwrap();

        let events = parse_events(Harness::ClaudeCode, 0, text.as_bytes(), ParseMode::Full);
        assert!(
            events
                .iter()
                .filter(|event| event.provenance.line > 1 && event.provenance.line < 5)
                .all(|event| event.sidechain)
        );
        let mut from_events = SessionReducer::new(Harness::ClaudeCode, options);
        from_events.set_path(path);
        from_events.transcript_written(now);
        from_events.feed_events(&events);

        let mut a = serde_json::to_value(fed.state(now)).unwrap();
        let mut b = serde_json::to_value(from_events.state(now)).unwrap();
        for key in ["diagnostics", "last_write"] {
            a.as_object_mut().unwrap().remove(key);
            b.as_object_mut().unwrap().remove(key);
        }
        assert_eq!(a, b, "status from raw bytes and from events");
        assert_eq!(fed.state(now).activity, crate::Activity::Done);
        assert_eq!(fed.state(now).meta.project, "app", "the helper's cwd did not move the parent");
        assert_eq!(fed.state(now).background.len(), 0, "the helper's turn end and count did not leak");
    }

    #[test]
    fn sidechain_usage_does_not_mask_the_parent_rows() {
        // Anchor the adversarial case: a sidechain row and a parent row share a message id.
        // Their usage high-water marks are kept apart, so the parent still reports its own.
        let text = concat!(
            r#"{"type":"user","timestamp":"2026-10-23T10:00:00Z","message":{"content":"parent"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-23T10:00:01Z","message":{"id":"m1","usage":{"output_tokens":8},"content":[]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-10-23T10:00:02Z","message":{"id":"m1","stop_reason":"end_turn","usage":{"output_tokens":2},"content":[]}}"#,
            "\n",
        );
        let now = Timestamp::parse_rfc3339("2026-10-23T10:00:05Z").unwrap();
        let path = Path::new("/h/.claude/projects/-w-app/s.jsonl");

        let mut fed = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        fed.set_path(path);
        fed.transcript_written(now);
        fed.feed(text.as_bytes(), ChunkCtx { offset: 0, mtime: now, path }).unwrap();

        let events = parse_events(Harness::ClaudeCode, 0, text.as_bytes(), ParseMode::Full);
        let mut from_events = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        from_events.set_path(path);
        from_events.transcript_written(now);
        from_events.feed_events(&events);
        assert_eq!(fed.state(now).usage.output_tokens, Some(2));
        assert_eq!(from_events.state(now).usage.output_tokens, Some(2));
    }

    #[test]
    fn only_a_boolean_true_is_a_sidechain() {
        for value in [json!(null), json!("true"), json!(1), json!(false), json!({})] {
            let record = json!({"type":"assistant","isSidechain":value,"message":{"content":[]}});
            let events = no_unknown(Harness::ClaudeCode, record);
            assert!(events.iter().all(|event| !event.sidechain), "{value}");
        }
        let record = json!({"type":"assistant","isSidechain":true,"message":{"content":[]}});
        let events = no_unknown(Harness::ClaudeCode, record);
        assert!(events.iter().all(|event| event.sidechain));
        // Only the record's own top-level field counts; a nested one is just content.
        let nested = json!({"type":"assistant","message":{"isSidechain":true,"content":[]}});
        let events = no_unknown(Harness::ClaudeCode, nested);
        assert!(events.iter().all(|event| !event.sidechain));
    }

    #[test]
    fn the_reducer_consumes_the_events_parse_lines_returns() {
        const FIXTURES: [(Harness, &str); 4] = [
            (
                Harness::ClaudeCode,
                include_str!("../../../fixtures/claude-code/2.1/linux/seed-session/input/session.jsonl"),
            ),
            (Harness::Codex, include_str!("../../../fixtures/codex/0.160/linux/seed-exec/input/session.jsonl")),
            (Harness::Codex, include_str!("../../../fixtures/codex/0.160/linux/seed-resume/input/session.jsonl")),
            (Harness::Cursor, include_str!("../../../fixtures/cursor/unknown/any/seed-session/input/session.jsonl")),
        ];
        let path = Path::new("/h/.cursor/projects/Users-sam-Work-demo-app/agent-transcripts/a1/a1.jsonl");
        let now = Timestamp::from_secs_f64(1_792_756_800.0);
        for (harness, text) in FIXTURES {
            let options = ReducerOptions { mode: ParseMode::Full, ..Default::default() };
            let mut fed = SessionReducer::new(harness, options.clone());
            fed.set_path(path);
            fed.transcript_written(now);
            fed.feed(text.as_bytes(), ChunkCtx { offset: 0, mtime: now, path }).unwrap();

            let mut from_events = SessionReducer::new(harness, options);
            from_events.set_path(path);
            from_events.transcript_written(now);
            from_events.feed_events(&parse_events(harness, 0, text.as_bytes(), ParseMode::Full));

            let mut a = serde_json::to_value(fed.state(now)).unwrap();
            let mut b = serde_json::to_value(from_events.state(now)).unwrap();
            for key in ["diagnostics", "last_write"] {
                a.as_object_mut().unwrap().remove(key);
                b.as_object_mut().unwrap().remove(key);
            }
            assert_eq!(a, b, "{harness:?}: status from raw bytes and from events");
        }
    }
}
