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
    /// The permission mode the harness last recorded, if it records one.
    pub permission: Option<PermissionMode>,
    /// The error the harness recorded, when the turn ended with one (e.g. `server_error`,
    /// `Connection failed`). Cleared when another activity takes over.
    pub error: Option<String>,
    pub meta: SessionMeta,
    /// Spend in US dollars, as the harness recorded it (Claude Code); never estimated.
    pub cost_usd: Option<f64>,
    /// Output tokens so far, as the harness reported them (each API message counted once).
    pub out_tokens: u64,
    /// The last few things the session did, oldest first (see
    /// [`ReducerOptions::recent_lines`](crate::ReducerOptions::recent_lines)).
    pub recent: Vec<ActivityLine>,
    pub diagnostics: Diagnostics,
}

/// What a session is doing. Shaped for where the library is going (PLAN.md section 4); for
/// now only some are produced, by today's rules: see [`Reason`].
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
    /// The turn ended, but helpers are still running. Not produced yet.
    BackgroundWork,
    /// The turn ended normally.
    Done,
    /// The user interrupted, or the turn was aborted.
    Interrupted,
    /// The turn ended with an error the harness recorded (see [`SessionState::error`]).
    Errored,
    /// Nothing written for `idle_after`, or nothing read yet.
    Idle,
    /// Nothing recognizable yet. Not produced yet (today that's `Idle`).
    Unknown,
}

impl Activity {
    /// The coarse class a dashboard might group by. The wording is yours.
    pub fn class(self) -> StatusClass {
        match self {
            Activity::AwaitingAnswer | Activity::AwaitingApproval => StatusClass::NeedsYou,
            Activity::RunningTool | Activity::Thinking | Activity::BackgroundWork => StatusClass::Working,
            Activity::Done | Activity::Interrupted | Activity::Errored => StatusClass::Done,
            Activity::Idle | Activity::Unknown => StatusClass::Idle,
        }
    }
}

/// Four coarse classes, most urgent first (they sort that way).
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
            "on-request" | "on_request" => PermissionMode::OnRequest,
            "untrusted" => PermissionMode::Untrusted,
            _ => PermissionMode::Unknown,
        }
    }

    /// Whether this mode can put a permission prompt in front of a human.
    pub fn can_ask_human(self) -> bool {
        !matches!(self, PermissionMode::BypassPermissions | PermissionMode::DontAsk | PermissionMode::Never)
    }
}

/// Which rule decided the activity: machine-readable, stable, one per rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reason {
    /// Nothing that moves the state has been read: `Idle`.
    NoEntries,
    /// Neither the transcript nor a helper's was written for `idle_after`, whatever the
    /// last entry said (a session closed or abandoned mid-turn): `Idle`.
    NoWritesFor,
    /// The last entry calls a tool that asks the user something: `AwaitingAnswer`.
    AskUserTool,
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
    /// Lines parsed as JSON and handed to the harness's adapter.
    pub lines: u64,
    /// Non-blank lines that weren't JSON, and so were skipped.
    pub unparsed: u64,
    /// Lines with long strings cut short before parsing ([`ParseMode::Lean`](crate::ParseMode::Lean)).
    pub cut: u64,
}
