//! What a session is doing, as the reducer reports it: the headline [`Activity`], how sure
//! we are of it, why, and the card details (project, title, cost, recent activity).

use crate::harness::Harness;
use crate::time::Timestamp;
use serde::Serialize;
use std::fmt;

/// One session's state at a moment: [`SessionReducer::state`](crate::SessionReducer::state).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionState {
    pub harness: Harness,
    /// What the agent is doing.
    pub activity: Activity,
    /// Whether the transcript says so, or we guess from timing.
    pub certainty: Certainty,
    /// Why this activity: which rule decided it.
    pub reason: Reason,
    /// When the activity began: the last entry that changed it, or the transcript's last
    /// write if no entry did (or the harness writes no times).
    pub since: Option<Timestamp>,
    /// When the last entry that changed the activity happened; None if none has.
    pub last_at: Option<Timestamp>,
    /// The newest write to the transcript, or to any of its helpers' transcripts.
    pub last_write: Option<Timestamp>,
    /// Helpers (subagents and background tasks) still running, as their own transcripts say.
    pub background: Vec<HelperState>,
    /// Wakeups this session has armed but not fired, deleted or cancelled. The activity can
    /// still be `done`: a downstream host that sleeps a resting agent must also keep it
    /// awake until the earliest due time.
    pub pending_wakeups: Vec<PendingWakeup>,
    /// The permission mode the harness last recorded, if it records one.
    pub permission: Option<PermissionMode>,
    /// The sandbox policy the harness last recorded (Codex), if it records one.
    pub sandbox: Option<SandboxPolicy>,
    /// The error the harness recorded, when the turn ended with one (e.g. `server_error`,
    /// `Connection failed`). Cleared when another activity takes over.
    pub error: Option<String>,
    /// Why the turn stopped, when the harness recorded a recognizable stop reason beyond an
    /// error: currently `usage_limit` for a usage/rate-limit stop. Cleared when the next turn
    /// starts.
    pub stop_reason: Option<StopReason>,
    /// When the harness said the usage/rate limit resets, when it recorded a usable time.
    /// Cleared with [`stop_reason`](Self::stop_reason).
    pub resets_at: Option<Timestamp>,
    pub meta: SessionMeta,
    /// Spend in US dollars, as the harness recorded it (Claude Code); never estimated.
    pub cost_usd: Option<f64>,
    /// Token counts as the harness reported them; `None` for a kind it doesn't record.
    /// Claude Code's per-message rows are deduped by `message.id`, taking the highest value
    /// each field reports (rows repeat or grow as a message is written).
    pub usage: TokenUsage,
    /// Output tokens so far; equal to
    /// `usage.output_tokens.unwrap_or(0)`.
    pub out_tokens: u64,
    /// The last few things the session did, oldest first (see
    /// [`ReducerOptions::recent_lines`](crate::ReducerOptions::recent_lines)).
    pub recent: Vec<ActivityLine>,
    pub diagnostics: Diagnostics,
}

/// Why a turn stopped, when an error is not specific enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// The harness stopped at a usage or rate limit. The next send starts a new turn.
    UsageLimit,
}

/// What a session is doing. Shaped for the v0.1 status contract: see
/// [`Reason`] for the rule that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Activity {
    /// An ask-the-user or plan-approval tool is pending.
    AwaitingAnswer,
    /// A tool call is pending and the harness could be asking permission.
    AwaitingApproval,
    /// A tool call is pending and can't be awaiting approval (or isn't yet).
    RunningTool,
    /// A prompt was sent or a tool result came back: the model is generating.
    Thinking,
    /// The turn ended, but helpers (subagents/background tasks) are still running.
    BackgroundWork,
    /// The turn ended normally.
    Done,
    /// The user interrupted, or the turn was aborted.
    Interrupted,
    /// The turn ended with an error the harness recorded (see [`SessionState::error`]).
    Errored,
    /// Nothing written for `idle_after`.
    Idle,
    /// Nothing recognizable yet: no entry that moves the state has been read.
    Unknown,
}

impl Activity {
    /// The coarse class a dashboard groups by: the tracker's sort key, with no wording.
    ///
    /// The mapping is fixed for v0.1:
    ///
    /// | `Activity` | `StatusClass` |
    /// |---|---|
    /// | `AwaitingAnswer`, `AwaitingApproval` | `NeedsYou` |
    /// | `RunningTool`, `Thinking`, `BackgroundWork` | `Working` |
    /// | `Done`, `Interrupted`, `Errored` | `Done` |
    /// | `Idle`, `Unknown` | `Idle` |
    ///
    /// `StatusClass` derives `Ord` with `NeedsYou < Working < Done < Idle`, so sorting by
    /// class puts the sessions that need a human first. Both the mapping and that order are
    /// a public stability promise for compatible releases. A new `Activity` variant must
    /// choose its class explicitly: `class()` matches exhaustively, and the
    /// `status_class_is_pinned_for_every_activity_variant` test pins the chosen bucket.
    pub fn class(self) -> StatusClass {
        match self {
            Activity::AwaitingAnswer | Activity::AwaitingApproval => StatusClass::NeedsYou,
            Activity::RunningTool | Activity::Thinking | Activity::BackgroundWork => StatusClass::Working,
            Activity::Done | Activity::Interrupted | Activity::Errored => StatusClass::Done,
            Activity::Idle | Activity::Unknown => StatusClass::Idle,
        }
    }
}

/// Four coarse classes, most urgent first: `NeedsYou < Working < Done < Idle`.
///
/// The order is part of the public contract (the tracker sorts by it); variants keep their
/// relative order in compatible releases. The library supplies no labels: consumers choose
/// their own wording for each class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusClass {
    NeedsYou,
    Working,
    Done,
    Idle,
}

/// How sure we are of an [`Activity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Certainty {
    /// The transcript says so.
    Firm,
    /// A timing or shape heuristic, named by the [`Reason`].
    Inferred,
}

/// What kind of scheduled wakeup a harness has armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WakeupKind {
    /// A dynamic `/loop` wakeup (`ScheduleWakeup` in Claude Code): fires once, then the
    /// model decides whether to arm another.
    Loop,
    /// A cron job (`CronCreate`/`CronDelete` in Claude Code): a wall-clock schedule,
    /// recurring or one-shot. A fixed-interval `/loop 5m` is represented by a cron job.
    Cron,
}

impl WakeupKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WakeupKind::Loop => "loop",
            WakeupKind::Cron => "cron",
        }
    }
}

/// One wakeup a session has armed and not yet consumed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingWakeup {
    /// The kind of scheduler that owns it.
    pub kind: WakeupKind,
    /// When it is due, when the transcript recorded a due time.
    pub due_at: Option<Timestamp>,
    /// A short reason the model or harness recorded, when it did.
    pub reason: Option<String>,
    /// `firm` when a tool result recorded the due time or job id, `inferred` when only the
    /// tool call is on disk.
    pub certainty: Certainty,
    /// Whether the job fires again after this run. A recurring cron stays pending after a
    /// fire; a one-shot cron and a dynamic loop wakeup are consumed by it.
    pub recurring: bool,
}

/// A harness's permission mode, as far as it changes who can answer a prompt.
///
/// Unknown strings are [`PermissionMode::Unknown`]: the status rules then keep the timing
/// guess instead of assuming a human can be asked. Codex's approval policies and Claude
/// Code's permission modes fit in the same vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    DontAsk,
    Auto,
    Manual,
    Normal,
    /// Codex: never ask for approval.
    Never,
    /// Codex: ask when a command needs it.
    OnRequest,
    /// Codex: ask for commands outside the trusted set.
    Untrusted,
    /// Codex's fine-grained policy, with at least one interactive approval category.
    GranularAsk,
    /// Codex's fine-grained policy, with every interactive category denied.
    GranularNoAsk,
    /// A value this version hasn't seen. Keep the inference, don't assume a human.
    Unknown,
}

impl PermissionMode {
    /// Parse the vocabulary Claude Code and Codex write. Case-sensitive on purpose: both
    /// harnesses write exact camelCase/snake-case values.
    pub fn parse(mode: &str) -> PermissionMode {
        match mode {
            "default" => PermissionMode::Default,
            "acceptEdits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            "dontAsk" => PermissionMode::DontAsk,
            "auto" => PermissionMode::Auto,
            "manual" => PermissionMode::Manual,
            "normal" => PermissionMode::Normal,
            "never" => PermissionMode::Never,
            // `on-failure` is Codex's older name for `on-request`.
            "on-request" | "on_request" | "on-failure" | "on_failure" => PermissionMode::OnRequest,
            "untrusted" => PermissionMode::Untrusted,
            _ => PermissionMode::Unknown,
        }
    }

    /// Whether this mode can put a permission prompt in front of a human.
    pub fn can_ask_human(self) -> bool {
        !matches!(
            self,
            PermissionMode::BypassPermissions
                | PermissionMode::DontAsk
                | PermissionMode::Never
                | PermissionMode::GranularNoAsk
        )
    }
}

/// A harness's sandbox policy, as Codex records it in `turn_context.sandbox_policy.type`.
///
/// The sandbox bounds what a command can do; the approval policy says when Codex may stop to
/// ask. Full access is `danger-full-access` together with `approval_policy: never`, so a
/// sandbox alone never proves that no human can be asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly,
    ExternalSandbox,
    WorkspaceWrite,
    /// A value this version hasn't seen.
    Unknown,
}

impl SandboxPolicy {
    /// Parse Codex's `sandbox_policy.type` vocabulary.
    pub fn parse(policy: &str) -> SandboxPolicy {
        match policy {
            "danger-full-access" => SandboxPolicy::DangerFullAccess,
            "read-only" => SandboxPolicy::ReadOnly,
            "external-sandbox" => SandboxPolicy::ExternalSandbox,
            "workspace-write" => SandboxPolicy::WorkspaceWrite,
            _ => SandboxPolicy::Unknown,
        }
    }
}

/// Which rule decided the activity: machine-readable, stable, one per rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reason {
    /// Nothing that moves the state has been read: `Unknown`.
    NoEntries,
    /// Neither the transcript nor a helper's was written for `idle_after`, whatever the
    /// last entry said (a session closed or abandoned mid-turn): `Idle`.
    NoWritesFor,
    /// The last entry calls a tool that asks the user something: `AwaitingAnswer`.
    AskUserTool,
    /// The harness persisted a permission request that has not completed: `AwaitingApproval`,
    /// firm (GitHub Copilot CLI's `permission.requested`).
    ApprovalRequested,
    /// A tool call has had no result for `stall_after`: perhaps a permission prompt.
    /// `AwaitingApproval`, inferred.
    ToolCallStalled,
    /// A tool call is waiting for its result: `RunningTool`.
    ToolCallPending,
    /// The user sent a prompt, or a turn started: `Thinking`.
    PromptSent,
    /// A tool's result came back (or, for harnesses that don't record results, the model
    /// wrote again): `Thinking`.
    ToolResultIn,
    /// The turn ended normally: `Done`.
    TurnEnded,
    /// The user interrupted, or the turn was aborted: `Interrupted`.
    Interrupted,
    /// The harness recorded an error ending the turn: `Errored`.
    Errored,
    /// The turn ended, but a helper is still writing: `BackgroundWork`.
    BackgroundHelpers,
    /// A status from Cursor's `state.vscdb` (`composerData.status`: completed, aborted or
    /// generating): `Done`, `Interrupted` or `Thinking`, firm.
    ComposerStatus,
}

/// Token counts as a harness reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

/// A helper still running: a subagent or background task that outlived the parent's turn.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HelperState {
    /// The helper's transcript identity (its path, for local sessions).
    pub id: String,
    pub activity: Activity,
    pub certainty: Certainty,
    pub reason: Reason,
    pub since: Option<Timestamp>,
    pub last_write: Option<Timestamp>,
}

/// Where and what a session is about.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SessionMeta {
    /// The project's name: the last part of the session's working folder.
    pub project: String,
    /// The folder the harness files the session under, when that's where the project's
    /// name comes from (Cursor's `<project>`, its workspace path with `/` made `-`).
    pub folder: String,
    pub branch: String,
    /// The harness's generated title, or the opening prompt's first line.
    pub title: String,
}

/// One thing a session did, for a short activity list. `Display` writes it as a line:
/// `› fix the build`, `$ cargo test`, `Edit routing.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityLine {
    /// The user's prompt: its first non-blank line.
    Prompt { text: String },
    /// A shell command: its first line.
    Command { command: String },
    /// Any other tool call, with what it works on (a file's name, a pattern) if anything.
    Tool { name: String, detail: Option<String> },
}

impl fmt::Display for ActivityLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActivityLine::Prompt { text } => write!(f, "› {text}"),
            ActivityLine::Command { command } => write!(f, "$ {command}"),
            ActivityLine::Tool { name, detail: Some(detail) } => write!(f, "{name} {detail}"),
            ActivityLine::Tool { name, detail: None } => f.write_str(name),
        }
    }
}

/// What reading a transcript ran into: data, never an error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Diagnostics {
    /// JSON values handed to the harness's adapter (two objects on one line count twice).
    pub lines: u64,
    /// Non-blank lines that weren't JSON, and so were skipped.
    pub unparsed: u64,
    /// Lines with long strings cut short before parsing ([`ParseMode::Lean`](crate::ParseMode::Lean)).
    pub cut: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_class_is_pinned_for_every_activity_variant() {
        // Keep this table in step with `Activity`. The exhaustive match in `class()` makes a
        // new variant a compile error; this test then forces a deliberate bucket choice.
        let cases = [
            (Activity::AwaitingAnswer, StatusClass::NeedsYou),
            (Activity::AwaitingApproval, StatusClass::NeedsYou),
            (Activity::RunningTool, StatusClass::Working),
            (Activity::Thinking, StatusClass::Working),
            (Activity::BackgroundWork, StatusClass::Working),
            (Activity::Done, StatusClass::Done),
            (Activity::Interrupted, StatusClass::Done),
            (Activity::Errored, StatusClass::Done),
            (Activity::Idle, StatusClass::Idle),
            (Activity::Unknown, StatusClass::Idle),
        ];
        assert_eq!(cases.len(), 10, "one case per Activity variant");
        for (activity, want) in cases {
            assert_eq!(activity.class(), want, "{activity:?}");
        }
    }

    #[test]
    fn status_classes_sort_most_urgent_first() {
        let mut classes = [StatusClass::Idle, StatusClass::Done, StatusClass::NeedsYou, StatusClass::Working];
        classes.sort();
        assert_eq!(classes, [StatusClass::NeedsYou, StatusClass::Working, StatusClass::Done, StatusClass::Idle]);
        assert_eq!(serde_json::to_value(classes).unwrap(), serde_json::json!(["needs-you", "working", "done", "idle"]));
    }
}
