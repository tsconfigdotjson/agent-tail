//! One session's state as its adapter folds transcript entries into it, and the rules that
//! turn that state (and its age) into an [`Activity`].

use crate::harness::Harness;
use crate::reducer::ReducerOptions;
use crate::state::{
    Activity, ActivityLine, Certainty, Diagnostics, PermissionMode, Reason, SandboxPolicy, SessionMeta, SessionState,
    TokenUsage,
};
use crate::time::{Timestamp, parse_rfc3339};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// The last thing that happened in a session, which (with its age) decides the activity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Last {
    #[default]
    Nothing,
    /// The user sent a prompt, or a turn started: the model is thinking.
    Prompt,
    /// The model called a tool and its result hasn't arrived.
    ToolUse,
    /// The model called a tool that asks the user something (a question, a plan to approve).
    Question,
    /// A tool's result came back: the model is thinking again.
    ToolResult,
    /// The turn ended normally.
    EndTurn,
    /// The user interrupted, or the turn was aborted.
    Interrupted,
    /// The harness recorded an error ending the turn.
    Errored,
}

/// One session's state, built up entry by entry by its harness's adapter. Times are seconds
/// since the epoch, 0 for none.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Session {
    pub(crate) harness: Harness,
    pub(crate) project: String,
    /// The folder an adapter files the session under, when that's where it learns the
    /// project from (Cursor's <project>).
    pub(crate) folder: String,
    pub(crate) branch: String,
    pub(crate) title: String,
    pub(crate) cost: Option<f64>,
    pub(crate) out_tokens: u64,
    /// Token counts, deduped by message id where the harness splits one message over rows.
    pub(crate) usage: TokenUsage,
    /// The highest usage each API message has reported so far.
    message_usage: HashMap<String, TokenUsage>,
    /// The permission mode the harness last recorded, if any.
    pub(crate) permission: Option<PermissionMode>,
    /// The sandbox policy the harness last recorded, if any (Codex).
    pub(crate) sandbox: Option<SandboxPolicy>,
    /// Claude Code: `entrypoint: sdk-cli` has no human at the keyboard.
    pub(crate) entrypoint_sdk: bool,
    /// Codex: an AI reviewer, not a human, handles approval requests.
    pub(crate) approval_reviewer_auto: bool,
    /// The error the harness recorded, if the last thing was an error.
    pub(crate) error: Option<String>,
    /// Claude Code's `turn_duration.pendingBackgroundAgentCount`, when it records one.
    pub(crate) background_agents: usize,
    /// Whether this is a helper's own session: its records carry `isSidechain: true`.
    pub(crate) is_helper: bool,
    pub(crate) log: VecDeque<ActivityLine>,
    log_lines: usize,
    pub(crate) last: Last,
    pub(crate) last_at: f64,
    /// Ids an adapter has already counted (e.g. API messages split over several entries).
    pub(crate) seen: HashSet<String>,
    /// Codex tool calls still waiting for their `call_id` output, and whether each asks the
    /// user something. Parallel calls stay pending until the last one comes back.
    pub(crate) pending_tools: HashMap<String, bool>,
    /// When the transcript was last written: the time of entries that carry none.
    pub(crate) mtime: f64,
    /// When a helper's transcript (a Cursor subagent's) was last written.
    pub(crate) helper_mtime: f64,
}

impl Session {
    pub(crate) fn new(harness: Harness, log_lines: usize) -> Self {
        Session {
            harness,
            project: String::new(),
            folder: String::new(),
            branch: String::new(),
            title: String::new(),
            cost: None,
            out_tokens: 0,
            usage: TokenUsage::default(),
            message_usage: HashMap::new(),
            permission: None,
            sandbox: None,
            entrypoint_sdk: false,
            approval_reviewer_auto: false,
            error: None,
            background_agents: 0,
            is_helper: false,
            log: VecDeque::with_capacity(log_lines + 1),
            log_lines,
            last: Last::Nothing,
            last_at: 0.0,
            seen: HashSet::new(),
            pending_tools: HashMap::new(),
            mtime: 0.0,
            helper_mtime: 0.0,
        }
    }

    /// The session in the transcript at `path`, before it's read.
    pub(crate) fn at(harness: Harness, log_lines: usize, path: &Path) -> Self {
        let mut s = Session::new(harness, log_lines);
        harness.adapter().start(&mut s, path);
        s
    }

    /// Fold one transcript entry into the session.
    pub(crate) fn apply(&mut self, e: &Value) {
        self.harness.adapter().apply(self, e);
    }

    /// When the session was last seen at work: its transcript's last write, or a helper's.
    pub(crate) fn active_at(&self) -> f64 {
        self.mtime.max(self.helper_mtime)
    }

    /// The session as of `now`.
    pub(crate) fn state(&self, now: Timestamp, o: &ReducerOptions, diagnostics: Diagnostics) -> SessionState {
        let (activity, certainty, reason) = self.activity(now.as_secs_f64(), o);
        let some = |t: f64| (t > 0.0).then(|| Timestamp::from_secs_f64(t));
        SessionState {
            harness: self.harness,
            activity,
            certainty,
            reason,
            since: some(if self.last_at > 0.0 { self.last_at } else { self.mtime }),
            last_at: some(self.last_at),
            last_write: some(self.active_at()),
            background: Vec::new(),
            permission: self.permission,
            sandbox: self.sandbox,
            error: self.error.clone(),
            meta: SessionMeta {
                project: self.project.clone(),
                folder: self.folder.clone(),
                branch: self.branch.clone(),
                title: self.title.clone(),
            },
            cost_usd: self.cost,
            usage: self.usage,
            out_tokens: self.out_tokens,
            recent: self.log.iter().cloned().collect(),
            diagnostics,
        }
    }

    /// The status rules, unchanged from tvdash (decided on its #4).
    fn activity(&self, now: f64, o: &ReducerOptions) -> (Activity, Certainty, Reason) {
        use {Activity::*, Certainty::*, Reason::*};
        if self.last == Last::Nothing {
            return (Idle, Firm, NoEntries);
        }
        let age = now - self.last_at.max(self.active_at());
        // A transcript untouched this long belongs to a session that was closed or
        // abandoned mid-turn, whatever its last entry says.
        if age > o.idle_after.as_secs_f64() {
            return (Idle, Firm, NoWritesFor);
        }
        match self.last {
            Last::Question => (AwaitingAnswer, Firm, AskUserTool),
            // A pending tool can only be waiting on a human when this mode can ask one.
            // Under bypass/dontAsk/never, or with no human at an SDK entrypoint, it runs.
            Last::ToolUse if !self.can_ask_human() => (RunningTool, Firm, ToolCallPending),
            Last::ToolUse if age > o.stall_after.as_secs_f64() => (AwaitingApproval, Inferred, ToolCallStalled),
            Last::ToolUse => (RunningTool, Firm, ToolCallPending),
            Last::Prompt => (Thinking, Firm, PromptSent),
            Last::ToolResult => (Thinking, Firm, ToolResultIn),
            Last::EndTurn => (Done, Firm, TurnEnded),
            Last::Interrupted => (Activity::Interrupted, Firm, Reason::Interrupted),
            Last::Errored => (Activity::Errored, Firm, Reason::Errored),
            Last::Nothing => unreachable!("handled above"),
        }
    }

    // What adapters call as they read entries.

    pub(crate) fn set(&mut self, last: Last, t: f64) {
        (self.last, self.last_at) = (last, t);
        if last != Last::Errored {
            self.error = None;
        }
        // A new turn (or its end) discards any calls left over from the last one.
        if matches!(last, Last::Prompt | Last::EndTurn | Last::Interrupted | Last::Errored) {
            self.pending_tools.clear();
        }
        if matches!(last, Last::Prompt | Last::Interrupted | Last::Errored) {
            self.background_agents = 0;
        }
    }

    /// The turn ended with an error the harness recorded.
    pub(crate) fn set_error(&mut self, error: impl Into<String>, t: f64) {
        self.set(Last::Errored, t);
        self.error = Some(error.into());
    }

    /// Record one API message's usage, keeping the highest value each field has reported.
    /// Claude Code repeats the same usage on every content-block row, and some rows grow as
    /// the message is written; summing rows would overcount, and taking the first would
    /// undercount.
    pub(crate) fn record_usage(&mut self, id: &str, usage: TokenUsage) {
        let previous = self.message_usage.get(id).copied().unwrap_or_default();
        let next = TokenUsage {
            input_tokens: max_opt(previous.input_tokens, usage.input_tokens),
            output_tokens: max_opt(previous.output_tokens, usage.output_tokens),
            cache_read_input_tokens: max_opt(previous.cache_read_input_tokens, usage.cache_read_input_tokens),
            cache_creation_input_tokens: max_opt(
                previous.cache_creation_input_tokens,
                usage.cache_creation_input_tokens,
            ),
        };
        bump(&mut self.usage.input_tokens, previous.input_tokens, next.input_tokens);
        bump(&mut self.usage.output_tokens, previous.output_tokens, next.output_tokens);
        bump(&mut self.usage.cache_read_input_tokens, previous.cache_read_input_tokens, next.cache_read_input_tokens);
        bump(
            &mut self.usage.cache_creation_input_tokens,
            previous.cache_creation_input_tokens,
            next.cache_creation_input_tokens,
        );
        self.out_tokens = self.usage.output_tokens.unwrap_or(0);
        self.message_usage.insert(id.into(), next);
    }

    /// Codex reports running session totals, not per-message usage.
    pub(crate) fn set_usage(&mut self, usage: TokenUsage) {
        self.usage = usage;
        self.out_tokens = usage.output_tokens.unwrap_or(0);
    }

    /// Claude Code's firm count of helpers still running when the turn ended.
    pub(crate) fn set_background_agents(&mut self, n: usize) {
        self.background_agents = n;
    }

    /// A `task-notification` arrived for one helper: one fewer background agent.
    pub(crate) fn background_agent_finished(&mut self) {
        self.background_agents = self.background_agents.saturating_sub(1);
    }

    /// When entry `e` happened: its RFC 3339 `timestamp`, or (with none) when the transcript
    /// was written.
    pub(crate) fn time_of(&self, e: &Value) -> f64 {
        e["timestamp"].as_str().and_then(parse_rfc3339).unwrap_or(self.mtime)
    }

    /// The harness recorded a permission mode (`None`: it doesn't record one).
    pub(crate) fn set_permission(&mut self, mode: Option<PermissionMode>) {
        self.permission = mode;
    }

    /// The harness recorded a sandbox policy (`None`: it doesn't record one).
    pub(crate) fn set_sandbox(&mut self, policy: Option<SandboxPolicy>) {
        self.sandbox = policy;
    }

    /// Claude Code: whether this session came from an SDK host with no human attached.
    pub(crate) fn set_entrypoint(&mut self, entrypoint: &str) {
        self.entrypoint_sdk = entrypoint == "sdk-cli";
    }

    /// Codex: whether approval requests go to an AI reviewer instead of a human.
    pub(crate) fn set_approval_reviewer_auto(&mut self, auto: bool) {
        self.approval_reviewer_auto = auto;
    }

    /// Whether a permission prompt could be waiting on a human in this session.
    pub(crate) fn can_ask_human(&self) -> bool {
        !self.entrypoint_sdk
            && !self.approval_reviewer_auto
            && self.permission.is_none_or(PermissionMode::can_ask_human)
    }

    pub(crate) fn set_cwd(&mut self, cwd: &str) {
        self.project = Path::new(cwd).file_name().map_or(cwd.into(), |n| n.to_string_lossy().into());
    }

    pub(crate) fn push_log(&mut self, line: ActivityLine) {
        self.log.push_back(line);
        while self.log.len() > self.log_lines {
            self.log.pop_front();
        }
    }

    /// The user sent `text`: the model is thinking, and the prompt's first line is logged.
    /// Returns that line, or None if there's no text (an image alone, say). A prompt older
    /// than activity already recorded is a late flush and is ignored.
    pub(crate) fn prompt(&mut self, text: &str, t: f64) -> Option<String> {
        if t < self.last_at {
            return None;
        }
        self.prompt_in_order(text, t)
    }

    /// A prompt for a harness whose non-prompt records carry no times of their own (Cursor):
    /// the record order is the only clock, so a prompt's own timestamp can be older than the
    /// file mtime of the activity before it and still start the next turn.
    pub(crate) fn prompt_in_order(&mut self, text: &str, t: f64) -> Option<String> {
        self.set(Last::Prompt, t);
        let first = text.lines().map(str::trim).find(|l| !l.is_empty())?.to_string();
        self.push_log(ActivityLine::Prompt { text: first.clone() });
        Some(first)
    }

    /// The model called a tool; `line` describes it for the log (None: not worth a line).
    pub(crate) fn tool_call(&mut self, asks_user: bool, line: Option<ActivityLine>, t: f64) {
        self.set(if asks_user { Last::Question } else { Last::ToolUse }, t);
        if let Some(line) = line {
            self.push_log(line);
        }
    }

    /// The model called a tool with an id. Callers that get ids (Codex) match results by
    /// id, so parallel calls stay pending until every one has output.
    pub(crate) fn tool_call_id(&mut self, call_id: &str, asks_user: bool, line: Option<ActivityLine>, t: f64) {
        self.pending_tools.insert(call_id.into(), asks_user);
        let any_question = self.pending_tools.values().any(|&asks| asks);
        self.set(if any_question { Last::Question } else { Last::ToolUse }, t);
        if let Some(line) = line {
            self.push_log(line);
        }
    }

    /// A tool result arrived for `call_id`. With other calls still pending, the session is
    /// still running a tool; the last result makes it `Thinking` again.
    pub(crate) fn tool_result_id(&mut self, call_id: &str, t: f64) {
        self.pending_tools.remove(call_id);
        if self.pending_tools.is_empty() {
            self.set(Last::ToolResult, t);
            return;
        }
        let any_question = self.pending_tools.values().any(|&asks| asks);
        self.set(if any_question { Last::Question } else { Last::ToolUse }, t);
    }
}

fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Add the increase in one reported field to the total, marking it recorded even when zero.
fn bump(total: &mut Option<u64>, previous: Option<u64>, next: Option<u64>) {
    if let Some(next) = next {
        *total = Some(total.unwrap_or(0) + next.saturating_sub(previous.unwrap_or(0)));
    }
}

/// The first line of `s`.
pub(crate) fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// The last component of a path, or the whole thing if it has none.
pub(crate) fn file_name(path: &str) -> String {
    Path::new(path.trim_end_matches('/')).file_name().map_or(path.into(), |n| n.to_string_lossy().into())
}

/// A tool call's log line: `name detail`.
pub(crate) fn tool(name: &str, detail: impl Into<String>) -> ActivityLine {
    ActivityLine::Tool { name: name.into(), detail: Some(detail.into()) }
}

/// A shell command's log line: its first line.
pub(crate) fn command(command: &str) -> ActivityLine {
    ActivityLine::Command { command: first_line(command).into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{NOW, log, status};
    use crate::{StatusClass, reducer::LOG_LINES};

    fn at_secs(ago: f64) -> f64 {
        NOW - ago
    }

    const STALL_AFTER: f64 = 20.0;
    const IDLE_AFTER: f64 = 15.0 * 60.0;

    #[test]
    fn defaults_are_tvdashs() {
        let o = ReducerOptions::default();
        assert_eq!(
            (o.stall_after.as_secs_f64(), o.idle_after.as_secs_f64(), o.recent_lines),
            (STALL_AFTER, IDLE_AFTER, 5)
        );
    }

    #[test]
    fn status_follows_the_last_event_and_its_age() {
        let mut s = Session::new(Harness::ClaudeCode, LOG_LINES);
        assert_eq!(status(&s), (StatusClass::Idle, "idle"), "nothing yet");
        let cases = [
            (Last::Prompt, 1.0, (StatusClass::Working, "thinking")),
            (Last::ToolUse, 1.0, (StatusClass::Working, "working")),
            (Last::ToolUse, STALL_AFTER + 10.0, (StatusClass::NeedsYou, "may need approval")),
            (Last::Question, 1.0, (StatusClass::NeedsYou, "needs your answer")),
            (Last::Question, STALL_AFTER + 10.0, (StatusClass::NeedsYou, "needs your answer")),
            (Last::ToolResult, 1.0, (StatusClass::Working, "thinking")),
            (Last::EndTurn, 1.0, (StatusClass::Done, "done")),
        ];
        for (last, ago, want) in cases {
            s.set(last, at_secs(ago));
            assert_eq!(status(&s), want, "{last:?} {ago}s ago");
        }
        // A transcript untouched for long is idle whatever its last entry.
        for last in [Last::Prompt, Last::ToolUse, Last::Question, Last::EndTurn] {
            s.set(last, at_secs(IDLE_AFTER + 60.0));
            assert_eq!(status(&s), (StatusClass::Idle, "idle"), "{last:?}");
        }
        // ...but a file written recently counts as activity.
        s.mtime = at_secs(1.0);
        assert_eq!(status(&s), (StatusClass::Done, "done"));
    }

    #[test]
    fn states_say_why_and_how_sure() {
        let o = ReducerOptions::default();
        let mut s = Session::new(Harness::ClaudeCode, LOG_LINES);
        let why = |s: &Session| {
            let st = s.state(Timestamp::from_secs_f64(NOW), &o, Diagnostics::default());
            (st.activity, st.certainty, st.reason)
        };
        assert_eq!(why(&s), (Activity::Idle, Certainty::Firm, Reason::NoEntries));
        s.set(Last::ToolUse, at_secs(STALL_AFTER + 1.0));
        assert_eq!(why(&s), (Activity::AwaitingApproval, Certainty::Inferred, Reason::ToolCallStalled));
        s.set(Last::ToolResult, at_secs(1.0));
        assert_eq!(why(&s), (Activity::Thinking, Certainty::Firm, Reason::ToolResultIn));
        s.set(Last::EndTurn, at_secs(IDLE_AFTER + 1.0));
        assert_eq!(why(&s), (Activity::Idle, Certainty::Firm, Reason::NoWritesFor));
        // Thresholds are options.
        let quick = ReducerOptions { stall_after: std::time::Duration::from_secs(1), ..o };
        s.set(Last::ToolUse, at_secs(2.0));
        let st = s.state(Timestamp::from_secs_f64(NOW), &quick, Diagnostics::default());
        assert_eq!(st.activity, Activity::AwaitingApproval);
    }

    #[test]
    fn prompts_log_their_first_line() {
        let mut s = Session::new(Harness::ClaudeCode, LOG_LINES);
        assert_eq!(s.prompt("\n   fix the build\nand then the tests", at_secs(10.0)).as_deref(), Some("fix the build"));
        s.set(Last::EndTurn, at_secs(9.0));
        assert!(s.prompt("fix the build", at_secs(8.0)).is_some(), "sent again, it's a new prompt");
        assert_eq!(log(&s), ["› fix the build", "› fix the build"]);
        s.set(Last::EndTurn, at_secs(7.0));
        assert_eq!(s.prompt(" \n", at_secs(1.0)), None);
        assert_eq!((s.last, s.log.len()), (Last::Prompt, 2), "no text: thinking, but nothing to log");
        s.set_error("server_error", at_secs(0.5));
        assert_eq!(s.prompt("trigger an error", at_secs(2.0)), None);
        assert_eq!(s.last, Last::Errored);
        assert_eq!(s.prompt("try again", at_secs(0.2)).as_deref(), Some("try again"));
        assert_eq!(s.last, Last::Prompt);
    }

    #[test]
    fn keeps_only_the_last_log_lines() {
        for n in [LOG_LINES, 1, 12] {
            let mut s = Session::new(Harness::ClaudeCode, n);
            for i in 0..n + 2 {
                s.push_log(ActivityLine::Tool { name: format!("l{i}"), detail: None });
            }
            assert_eq!(s.log.len(), n);
            assert_eq!(log(&s).first().unwrap(), &format!("l{}", 2));
            assert_eq!(log(&s).last().unwrap(), &format!("l{}", n + 1));
        }
    }

    #[test]
    fn since_is_the_last_change_or_the_write() {
        let o = ReducerOptions::default();
        let mut s = Session::new(Harness::ClaudeCode, LOG_LINES);
        let st = |s: &Session| s.state(Timestamp::from_secs_f64(NOW), &o, Diagnostics::default());
        assert_eq!((st(&s).since, st(&s).last_at, st(&s).last_write), (None, None, None));
        s.set(Last::ToolResult, NOW - 130.0);
        assert_eq!(st(&s).since, Some(Timestamp::from_secs_f64(NOW - 130.0)));
        s.last_at = 0.0;
        s.mtime = NOW - 300.0;
        assert_eq!(st(&s).since, Some(Timestamp::from_secs_f64(NOW - 300.0)), "falls back to the file's mtime");
        s.helper_mtime = NOW - 5.0;
        assert_eq!(st(&s).last_write, Some(Timestamp::from_secs_f64(NOW - 5.0)), "a helper's write counts");
    }

    #[test]
    fn log_lines_read_as_tvdash_wrote_them() {
        assert_eq!(command("cargo test\n--release").to_string(), "$ cargo test");
        assert_eq!(command("").to_string(), "$ ");
        assert_eq!(tool("Edit", "").to_string(), "Edit ");
        assert_eq!(tool("Read", "main.rs").to_string(), "Read main.rs");
        assert_eq!(ActivityLine::Tool { name: "Task".into(), detail: None }.to_string(), "Task");
        assert_eq!(ActivityLine::Prompt { text: "go".into() }.to_string(), "› go");
        let json = serde_json::to_value(tool("Read", "a.rs")).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "tool", "name": "Read", "detail": "a.rs"}));
    }
}
