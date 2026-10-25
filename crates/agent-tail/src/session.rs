//! One session's state as its adapter folds transcript entries into it, and the rules that
//! turn that state (and its age) into an [`Activity`].

use crate::adapters::pi::Branch;
use crate::harness::Harness;
use crate::reducer::ReducerOptions;
use crate::state::{
    Activity, ActivityLine, Certainty, Diagnostics, PermissionMode, Reason, SandboxPolicy, SessionMeta, SessionState,
    TokenUsage,
};
use crate::time::Timestamp;
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

/// A background helper Claude Code launched asynchronously, as its parent's `toolUseResult`
/// names it. The transcript itself is followed separately; this is the identity link from
/// the parent (`agentId`, `backgroundTaskId`, `outputFile`) to the helper's file and metadata.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LaunchedHelper {
    pub(crate) agent_id: String,
    pub(crate) task_id: String,
    pub(crate) output_file: String,
    pub(crate) tool_use_id: String,
    /// A completion notification or `.meta.json` said it ended.
    pub(crate) ended: bool,
    /// When the launch result was read.
    pub(crate) since: f64,
}

impl LaunchedHelper {
    /// The key a launch is remembered by: the file it names is the most specific identity.
    pub(crate) fn key(&self) -> String {
        if !self.output_file.is_empty() {
            format!("file:{}", self.output_file)
        } else if !self.agent_id.is_empty() {
            format!("agent:{}", self.agent_id)
        } else if !self.task_id.is_empty() {
            format!("task:{}", self.task_id)
        } else {
            format!("tool:{}", self.tool_use_id)
        }
    }

    /// Whether this launch is the helper transcript at `id` (a path, or a test's agent id).
    pub(crate) fn matches_helper(&self, id: &str) -> bool {
        if !self.output_file.is_empty()
            && (self.output_file == id || path_file_name(&self.output_file) == path_file_name(id))
        {
            return true;
        }
        if !self.agent_id.is_empty() {
            let stem = path_file_stem(id);
            if stem == self.agent_id || stem == format!("agent-{}", self.agent_id) {
                return true;
            }
        }
        false
    }

    /// Whether a parent `task-notification` names this launch.
    pub(crate) fn matches_notification(&self, task_id: &str, tool_use_id: &str, output_file: &str) -> bool {
        (!task_id.is_empty() && (self.task_id == task_id || self.agent_id == task_id))
            || (!tool_use_id.is_empty() && self.tool_use_id == tool_use_id)
            || (!output_file.is_empty()
                && (self.output_file == output_file
                    || path_file_name(&self.output_file) == path_file_name(output_file)))
    }

    /// A synthetic state while the launch is known but its transcript hasn't been read yet.
    pub(crate) fn id(&self) -> String {
        if !self.output_file.is_empty() {
            self.output_file.clone()
        } else if !self.agent_id.is_empty() {
            self.agent_id.clone()
        } else if !self.task_id.is_empty() {
            self.task_id.clone()
        } else {
            self.tool_use_id.clone()
        }
    }
}

fn path_file_name(path: &str) -> &str {
    Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path)
}

fn path_file_stem(path: &str) -> &str {
    Path::new(path).file_stem().and_then(|name| name.to_str()).unwrap_or(path)
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
    /// The opening prompt's first line, kept so an index title can fall back if it goes away.
    prompt_title: String,
    /// The last title Codex's `session_index.jsonl` gave, kept while a Cursor title wins.
    index_title: String,
    /// Whether `title` currently comes from Codex's `session_index.jsonl`.
    title_from_index: bool,
    /// Whether `title` currently comes from Cursor's `state.vscdb`.
    title_from_db: bool,
    /// Whether `title` currently comes from Pi's `session_info.name`.
    title_from_name: bool,
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
    /// Pi never prompts; Copilot records a separate permission request when it does, so a
    /// pending tool without one is firmly running.
    pub(crate) no_approval: bool,
    /// Cursor records an approval wait in `state.vscdb` (a bubble's pending decision). A
    /// transcript tool with nothing after it is not guessed to be an approval.
    pub(crate) no_stall_inference: bool,
    /// The error the harness recorded, if the last thing was an error.
    pub(crate) error: Option<String>,
    /// Claude Code's `turn_duration.pendingBackgroundAgentCount`, when it records one.
    pub(crate) background_agents: usize,
    /// How many explicit zero counts (Claude's `agents_killed`, or a `turn_duration` with no
    /// pending helpers) this session has seen. The reducer uses each increase to mark the
    /// helpers it already follows ended.
    pub(crate) background_kills: u64,
    /// When the latest explicit zero count was recorded. A helper file first seen after the
    /// kill only counts if its own entries are newer.
    pub(crate) background_killed_at: f64,
    /// Claude Code's async background launches, linked to their helper files.
    pub(crate) launched_helpers: HashMap<String, LaunchedHelper>,
    /// The Codex thread id (`session_meta.payload.id`), for `session_index.jsonl` titles.
    pub(crate) session_id: String,
    /// Whether this is a helper's own session: its records carry `isSidechain: true`.
    pub(crate) is_helper: bool,
    /// Helper conversations interleaved into this transcript, keyed by the harness's helper
    /// id (GitHub Copilot CLI writes helper records with a top-level `agentId` into the
    /// parent's `events.jsonl`). They are reduced as their own sessions so their prompts,
    /// tools and turn ends never move the parent's state.
    pub(crate) embedded_helpers: HashMap<String, Session>,
    pub(crate) log: VecDeque<ActivityLine>,
    pub(crate) log_lines: usize,
    pub(crate) last: Last,
    pub(crate) last_at: f64,
    /// Ids an adapter has already counted (e.g. API messages split over several entries).
    pub(crate) seen: HashSet<String>,
    /// Codex tool calls still waiting for their `call_id` output, and whether each asks the
    /// user something. Parallel calls stay pending until the last one comes back.
    pub(crate) pending_tools: HashMap<String, bool>,
    /// Tool calls with a persisted permission request that has not completed (Copilot).
    pub(crate) pending_approvals: HashSet<String>,
    /// When the transcript was last written: the time of entries that carry none.
    pub(crate) mtime: f64,
    /// When a helper's transcript (a Cursor subagent's) was last written.
    pub(crate) helper_mtime: f64,
    /// A status read from a harness's database (Cursor's `composerData.status`) and when it
    /// was written. The transcript rules use it only while it is at least as new as the last
    /// transcript entry, so a throttled read can't rewind a newer transcript.
    pub(crate) db_status: Option<(Activity, f64)>,
    /// State carried across records while they are parsed into events (prompt-format
    /// dedupe and usage high-water marks).
    pub(crate) parse_state: crate::event::ParseState,
    /// Pi's active branch. Only the leaf's parent chain moves this session's state.
    pub(crate) pi_branch: Branch,
    /// Pi entries seen so far, in file order, so a changed leaf can be reduced again
    /// without re-reading the transcript. Other harnesses leave this empty.
    pub(crate) pi_records: Vec<Value>,
}

impl Session {
    pub(crate) fn new(harness: Harness, log_lines: usize) -> Self {
        Session {
            harness,
            project: String::new(),
            folder: String::new(),
            branch: String::new(),
            title: String::new(),
            prompt_title: String::new(),
            index_title: String::new(),
            title_from_index: false,
            title_from_db: false,
            title_from_name: false,
            cost: None,
            out_tokens: 0,
            usage: TokenUsage::default(),
            message_usage: HashMap::new(),
            permission: None,
            sandbox: None,
            entrypoint_sdk: false,
            approval_reviewer_auto: false,
            // Pi never prompts. Copilot persists a permission request before a tool can
            // wait on a human, so a pending tool without one is firmly running.
            no_approval: matches!(harness, Harness::Pi | Harness::Copilot),
            // Cursor's database says when an approval card is up; don't also guess from age.
            no_stall_inference: harness == Harness::Cursor,
            error: None,
            background_agents: 0,
            background_kills: 0,
            background_killed_at: 0.0,
            launched_helpers: HashMap::new(),
            session_id: String::new(),
            is_helper: false,
            embedded_helpers: HashMap::new(),
            log: VecDeque::with_capacity(log_lines + 1),
            log_lines,
            last: Last::Nothing,
            last_at: 0.0,
            seen: HashSet::new(),
            pending_tools: HashMap::new(),
            pending_approvals: HashSet::new(),
            mtime: 0.0,
            helper_mtime: 0.0,
            db_status: None,
            parse_state: crate::event::ParseState::default(),
            pi_branch: Branch::default(),
            pi_records: Vec::new(),
        }
    }

    /// The session in the transcript at `path`, before it's read.
    pub(crate) fn at(harness: Harness, log_lines: usize, path: &Path) -> Self {
        let mut s = Session::new(harness, log_lines);
        harness.adapter().start(&mut s, path);
        s
    }

    /// Fold one transcript entry into the session through the same normalized events the
    /// public event layer exposes. Pi is tree-shaped: an entry that does not extend the
    /// current leaf makes the active chain change, so the retained entries are reduced again
    /// (only the new leaf's parent chain).
    pub(crate) fn apply(&mut self, e: &Value) {
        if self.harness != Harness::Pi {
            crate::event::apply_record(self, e);
            return;
        }
        // Retain the header and id-less records too: a later rebuild must keep session facts
        // (cwd, id) that are not part of the tree.
        self.pi_records.push(e.clone());
        let Some((_id, extends)) = self.pi_branch.observe(e) else {
            crate::event::apply_record(self, e);
            return;
        };
        if extends {
            crate::event::apply_record(self, e);
        } else {
            self.rebuild_pi();
        }
    }

    /// Reduce the retained Pi entries again after the active leaf changed. Normal appends
    /// stay incremental; this runs only on a branch/switch-back or a duplicate id.
    fn rebuild_pi(&mut self) {
        let records = std::mem::take(&mut self.pi_records);
        let branch = self.pi_branch.clone();
        let mut fresh = Session::new(self.harness, self.log_lines);
        fresh.mtime = self.mtime;
        fresh.helper_mtime = self.helper_mtime;
        fresh.is_helper = self.is_helper;
        for record in &records {
            if branch.on_active_chain(record) {
                crate::event::apply_record(&mut fresh, record);
            }
        }
        fresh.pi_branch = branch;
        fresh.pi_records = records;
        *self = fresh;
    }

    /// When the session was last seen at work: its transcript's last write, or a helper's.
    pub(crate) fn active_at(&self) -> f64 {
        self.mtime.max(self.helper_mtime)
    }

    /// The session as of `now`.
    pub(crate) fn state(&self, now: Timestamp, o: &ReducerOptions, diagnostics: Diagnostics) -> SessionState {
        let (activity, certainty, reason) = self.activity(now.as_secs_f64(), o);
        let since = self
            .db_signal(now.as_secs_f64(), o)
            .map(|(_, at)| at)
            .unwrap_or_else(|| if self.last_at > 0.0 { self.last_at } else { self.mtime });
        let some = |t: f64| (t > 0.0).then(|| Timestamp::from_secs_f64(t));
        SessionState {
            harness: self.harness,
            activity,
            certainty,
            reason,
            since: some(since),
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

    /// The status rules that decide a session activity.
    fn activity(&self, now: f64, o: &ReducerOptions) -> (Activity, Certainty, Reason) {
        use {Activity::*, Certainty::*, Reason::*};
        let db = self.db_signal(now, o);
        if self.last == Last::Nothing && db.is_none() {
            return (Unknown, Firm, NoEntries);
        }
        let db_at = self.db_status.map_or(0.0, |(_, at)| at);
        let age = now - self.last_at.max(self.active_at()).max(db_at);
        // Neither transcript nor database touched this long: the session was closed or
        // abandoned mid-turn, whatever its last entry says.
        if age > o.idle_after.as_secs_f64() {
            return (Idle, Firm, NoWritesFor);
        }
        if let Some((activity, _)) = db {
            return (activity, Firm, ComposerStatus);
        }
        if !self.pending_approvals.is_empty() {
            return (AwaitingApproval, Firm, ApprovalRequested);
        }
        match self.last {
            Last::Question => (AwaitingAnswer, Firm, AskUserTool),
            // A pending tool can only be waiting on a human when this mode can ask one.
            // Under bypass/dontAsk/never, or with no human at an SDK entrypoint, it runs.
            Last::ToolUse if !self.can_ask_human() || self.no_stall_inference => (RunningTool, Firm, ToolCallPending),
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

    /// A database status that may decide the activity: fresh enough, not older than the last
    /// transcript entry, and not promoting a pending ask-the-user question to thinking.
    fn db_signal(&self, now: f64, o: &ReducerOptions) -> Option<(Activity, f64)> {
        let (activity, at) = self.db_status?;
        if at < self.last_at || !(0.0..=o.idle_after.as_secs_f64()).contains(&(now - at)) {
            return None;
        }
        if self.last == Last::Question && activity == Activity::Thinking {
            return None;
        }
        Some((activity, at))
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
            self.pending_approvals.clear();
        }
        if matches!(last, Last::Interrupted | Last::Errored) {
            self.background_agents = 0;
            self.launched_helpers.clear();
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

    /// Claude Code's firm count of helpers still running when the turn ended. An explicit
    /// zero is also a kill signal: it means no background helper survives this record.
    pub(crate) fn set_background_agents(&mut self, n: usize, at: f64) {
        self.background_agents = n;
        if n == 0 {
            self.launched_helpers.clear();
            self.background_kills = self.background_kills.saturating_add(1);
            self.background_killed_at = at;
        }
    }

    /// Remember an async launch from the parent's `toolUseResult`.
    pub(crate) fn launch_background(&mut self, helper: LaunchedHelper) {
        self.launched_helpers.insert(helper.key(), helper);
    }

    /// A `task-notification` arrived for one helper. If it names a launch we know, mark that
    /// launch ended and count it down once; otherwise fall back to the parent's old count.
    pub(crate) fn background_agent_notified(&mut self, task_id: &str, tool_use_id: &str, output_file: &str) {
        if let Some(helper) = self
            .launched_helpers
            .values_mut()
            .find(|helper| helper.matches_notification(task_id, tool_use_id, output_file))
        {
            if !helper.ended {
                helper.ended = true;
                self.background_agents = self.background_agents.saturating_sub(1);
            }
        } else {
            self.background_agent_finished();
        }
    }

    /// A helper's `.meta.json` said it finished (or was stopped). End the linked launch once.
    pub(crate) fn background_helper_meta_finished(&mut self, id: &str) {
        if let Some(helper) = self.launched_helpers.values_mut().find(|helper| helper.matches_helper(id))
            && !helper.ended
        {
            helper.ended = true;
            self.background_agents = self.background_agents.saturating_sub(1);
        }
    }

    /// A `task-notification` arrived for one helper: one fewer background agent.
    pub(crate) fn background_agent_finished(&mut self) {
        self.background_agents = self.background_agents.saturating_sub(1);
    }

    /// Codex's thread id from `session_meta`, for matching `session_index.jsonl`.
    pub(crate) fn set_session_id(&mut self, id: &str) {
        if !id.is_empty() {
            self.session_id = id.into();
        }
    }

    /// The opening prompt, kept as the fallback title even after an index or database title
    /// wins.
    pub(crate) fn set_title_from_prompt(&mut self, first: String) {
        if self.prompt_title.is_empty() {
            self.prompt_title = first.clone();
        }
        if !self.title_from_index && !self.title_from_db && !self.title_from_name && self.title.is_empty() {
            self.title = first;
        }
    }

    /// A title from Codex's `session_index.jsonl`; it wins over the opening prompt. Without
    /// one (the index is absent, malformed, or no longer names this thread), fall back to
    /// that opening prompt rather than keeping a stale index title. A Cursor database title
    /// still wins while it is present.
    pub(crate) fn set_index_title(&mut self, title: Option<&str>) {
        match title.map(str::trim).filter(|title| !title.is_empty()) {
            Some(title) => {
                self.index_title = title.into();
                self.title_from_index = true;
                if !self.title_from_db {
                    self.title = title.into();
                }
            }
            None => {
                self.index_title.clear();
                if !self.title_from_db && self.title_from_index {
                    self.title_from_index = false;
                    self.title = self.prompt_title.clone();
                }
            }
        }
    }

    /// A title from Cursor's `state.vscdb` (`composerData.name`). It wins over the opening
    /// prompt and Codex's index; without one, fall back to whichever source is still there.
    pub(crate) fn set_db_title(&mut self, title: Option<&str>) {
        match title.map(str::trim).filter(|title| !title.is_empty()) {
            Some(title) => {
                self.title_from_db = true;
                self.title = title.into();
            }
            None if self.title_from_db => {
                self.title_from_db = false;
                self.title = if self.title_from_index { self.index_title.clone() } else { self.prompt_title.clone() };
            }
            None => {}
        }
    }

    /// A title from Pi's `session_info.name`; it wins over the opening prompt. Clearing it
    /// falls back to the prompt.
    pub(crate) fn set_session_name_title(&mut self, title: Option<&str>) {
        match title.map(str::trim).filter(|title| !title.is_empty()) {
            Some(title) => {
                self.title_from_name = true;
                self.title = title.into();
            }
            None if self.title_from_name => {
                self.title_from_name = false;
                self.title = if self.title_from_index { self.index_title.clone() } else { self.prompt_title.clone() };
            }
            None => {}
        }
    }

    /// A status recorded in a harness's database (Cursor's `composerData.status`), with when
    /// it was written. `None` clears it, so the transcript rules decide again.
    pub(crate) fn set_db_status(&mut self, status: Option<(Activity, f64)>) {
        self.db_status = status;
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
            && !self.no_approval
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
        // A new user prompt starts a new interaction: helpers from the last one are gone.
        // Model-step turn starts are not prompts and keep their background launches.
        self.background_agents = 0;
        self.launched_helpers.clear();
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

    /// A permission request was persisted for `call_id`: firm `AwaitingApproval` until its
    /// completion record arrives.
    pub(crate) fn approval_requested(&mut self, call_id: &str, t: f64) {
        self.pending_approvals.insert(call_id.into());
        self.set(Last::ToolUse, t);
    }

    /// The permission request was answered. The tool still has to run and report a result,
    /// so this only clears the firm approval state.
    pub(crate) fn approval_completed(&mut self, call_id: &str, t: f64) {
        self.pending_approvals.remove(call_id);
        self.last_at = self.last_at.max(t);
    }

    /// A tool result arrived for `call_id`. With other calls still pending, the session is
    /// still running a tool; the last result makes it `Thinking` again.
    pub(crate) fn tool_result_id(&mut self, call_id: &str, t: f64) {
        self.pending_tools.remove(call_id);
        self.pending_approvals.remove(call_id);
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
    fn defaults_are_as_expected() {
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
        assert_eq!(why(&s), (Activity::Unknown, Certainty::Firm, Reason::NoEntries));
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
    fn an_empty_session_is_unknown_even_when_its_file_is_old() {
        let o = ReducerOptions::default();
        let mut s = Session::new(Harness::ClaudeCode, LOG_LINES);
        s.mtime = at_secs(IDLE_AFTER + 60.0);
        let st = s.state(Timestamp::from_secs_f64(NOW), &o, Diagnostics::default());
        assert_eq!((st.activity, st.certainty, st.reason), (Activity::Unknown, Certainty::Firm, Reason::NoEntries));
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
    fn database_titles_win_and_database_statuses_are_firm_only_when_newer() {
        let o = ReducerOptions::default();
        let mut s = Session::new(Harness::Cursor, LOG_LINES);
        s.set_title_from_prompt("first prompt".into());
        s.set_db_title(Some("database title"));
        assert_eq!(s.title, "database title");
        s.set_title_from_prompt("later prompt".into());
        assert_eq!(s.title, "database title", "a later prompt doesn't overwrite it");
        s.set_index_title(Some("index title"));
        assert_eq!(s.title, "database title", "it also wins over Codex's index title");
        s.set_db_title(None);
        assert_eq!(s.title, "index title", "without it, the index title is back");
        s.set_index_title(None);
        assert_eq!(s.title, "first prompt", "then the opening prompt");

        s.set(Last::ToolUse, at_secs(1.0));
        s.set_db_status(Some((Activity::Done, at_secs(0.5))));
        let st = s.state(Timestamp::from_secs_f64(NOW), &o, Diagnostics::default());
        assert_eq!(
            (st.activity, st.certainty, st.reason, st.since),
            (Activity::Done, Certainty::Firm, Reason::ComposerStatus, Some(Timestamp::from_secs_f64(NOW - 0.5)))
        );
        s.set_db_status(Some((Activity::Thinking, at_secs(2.0))));
        assert_eq!(status(&s), (StatusClass::Working, "working"), "an older database status is ignored");
        s.set_db_status(None);
        assert_eq!(status(&s), (StatusClass::Working, "working"), "and clearing it is the transcript again");

        // A pending question is firmer than a database that only says the model is generating.
        s.set(Last::Question, at_secs(1.0));
        s.set_db_status(Some((Activity::Thinking, at_secs(0.5))));
        assert_eq!(status(&s), (StatusClass::NeedsYou, "needs your answer"));
        s.set_db_status(Some((Activity::Done, at_secs(0.5))));
        assert_eq!(status(&s), (StatusClass::Done, "done"), "but a completed status closes it");
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
    fn log_lines_read_as_written() {
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
