use crate::screen::ScreenState;
use agent_tail::{
    Activity, Certainty, ChunkCtx, Harness, ParseMode, PermissionMode, Reason, ReducerOptions, SessionReducer,
    SessionState, Timestamp,
};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

pub const WRITE_LATENCY: Duration = Duration::from_millis(1_000);

const PERMISSION: &str = "permission_prompt";

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ExpectedOne {
    Name(String),
    Spec(ExpectedSpec),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedSpec {
    pub activity: String,
    #[serde(default)]
    pub when: ExpectWhen,
    #[serde(default)]
    pub screen: Option<ScreenState>,
    #[serde(default)]
    pub certainty: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExpectWhen {
    #[default]
    End,
    Ever,
}

#[derive(Debug, Clone)]
pub struct Expectation {
    pub activity: String,
    pub when: ExpectWhen,
    pub screen: Option<String>,
    pub certainty: Option<String>,
}

impl ExpectedOne {
    pub fn expectation(&self) -> Expectation {
        match self {
            ExpectedOne::Name(activity) => {
                Expectation { activity: activity.clone(), when: ExpectWhen::End, screen: None, certainty: None }
            }
            ExpectedOne::Spec(spec) => Expectation {
                activity: spec.activity.clone(),
                when: spec.when,
                screen: spec.screen.map(|state| state.id().to_string()),
                certainty: spec.certainty.clone(),
            },
        }
    }
}

pub fn deserialize_expected<'de, D>(deserializer: D) -> Result<Vec<ExpectedOne>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(activity) => Ok(vec![ExpectedOne::Name(activity)]),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(activity) => Ok(ExpectedOne::Name(activity)),
                other => serde_json::from_value(other).map(ExpectedOne::Spec).map_err(DeError::custom),
            })
            .collect(),
        other => serde_json::from_value(other).map(|spec| vec![ExpectedOne::Spec(spec)]).map_err(DeError::custom),
    }
}

#[derive(Debug, Serialize)]
pub struct LibraryPoint {
    pub activity: Activity,
    pub certainty: Certainty,
    pub reason: Reason,
    pub permission: Option<PermissionMode>,
    pub error: Option<String>,
    pub since: Option<Timestamp>,
    pub last_write: Option<Timestamp>,
    pub background: usize,
}

#[derive(Debug, Serialize)]
pub struct Sample {
    pub elapsed_ms: u64,
    pub screen: &'static str,
    pub background_agents: bool,
    pub library: Option<LibraryPoint>,
    #[serde(default)]
    pub frozen: bool,
    #[serde(default)]
    pub now_offset_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
}

impl Default for Sample {
    fn default() -> Self {
        Sample {
            elapsed_ms: 0,
            screen: "unknown",
            background_agents: false,
            library: None,
            frozen: false,
            now_offset_ms: 0,
            event: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Disagreement {
    pub screen: String,
    pub activity: String,
    pub samples: usize,
    pub first_elapsed_ms: u64,
    pub last_elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct Score {
    pub scenario: String,
    pub description: Option<String>,
    pub harness: String,
    pub samples: usize,
    pub screen_counts: BTreeMap<String, usize>,
    pub confusion: BTreeMap<String, BTreeMap<String, usize>>,
    pub disagreements: Vec<Disagreement>,
    pub failures: Vec<String>,
    pub stall_after_ms: u64,
    pub write_latency_ms: u64,
    pub mock_requests: u64,
    pub remaining_mock_steps: usize,
    pub transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Observations::is_empty")]
    pub observations: Observations,
}

#[derive(Debug, Clone, Default)]
pub struct ObservedHelper {
    pub id: String,
    pub meta_json: Option<String>,
    pub jsonl: Option<String>,
}

pub struct ScoreRequest<'a> {
    pub scenario: &'a str,
    pub description: Option<String>,
    pub harness: &'a str,
    pub no_approval: bool,
    pub expected: &'a [Expectation],
    pub samples: &'a [Sample],
    pub mock_requests: u64,
    pub remaining_mock_steps: usize,
    pub transcript: Option<String>,
    pub min_screen_ms: &'a BTreeMap<String, u64>,
    pub transcripts: &'a [TranscriptDump],
    pub helpers: &'a [ObservedHelper],
    pub registry: &'a [RegistrySample],
    pub registry_fields: &'a [String],
    pub registry_files_after_exit: Option<usize>,
    pub expect_registry: Option<&'a RegistryExpect>,
    pub expect_observations: Option<&'a ObserveExpect>,
}

pub fn library_point(state: &SessionState) -> LibraryPoint {
    LibraryPoint {
        activity: state.activity,
        certainty: state.certainty,
        reason: state.reason,
        permission: state.permission,
        error: state.error.clone(),
        since: state.since,
        last_write: state.last_write,
        background: state.background.len(),
    }
}

pub fn score(request: &ScoreRequest<'_>) -> Score {
    let stall_after = ReducerOptions::default().stall_after;
    let stall_ms = stall_after.as_millis() as u64;
    let write_ms = WRITE_LATENCY.as_millis() as u64;
    let mut score = Score {
        scenario: request.scenario.to_string(),
        description: request.description.clone(),
        harness: request.harness.to_string(),
        samples: request.samples.len(),
        screen_counts: BTreeMap::new(),
        confusion: BTreeMap::new(),
        disagreements: Vec::new(),
        failures: Vec::new(),
        stall_after_ms: stall_ms,
        write_latency_ms: write_ms,
        mock_requests: request.mock_requests,
        remaining_mock_steps: request.remaining_mock_steps,
        transcript: request.transcript.clone(),
        observations: observe_run(request),
    };
    let mut failures = BTreeSet::new();
    for sample in request.samples {
        *score.screen_counts.entry(sample.screen.to_string()).or_default() += 1;
        if let Some(library) = &sample.library {
            *score
                .confusion
                .entry(sample.screen.to_string())
                .or_default()
                .entry(activity_id(library.activity).to_string())
                .or_default() += 1;
        }
    }
    score.disagreements = disagreements(request.samples, stall_ms, write_ms);
    hard_failures(request, stall_ms, write_ms, &mut failures);
    score.failures = failures.into_iter().collect();
    score
}

fn hard_failures(request: &ScoreRequest<'_>, stall_ms: u64, write_ms: u64, failures: &mut BTreeSet<String>) {
    for sample in request.samples {
        let Some(library) = &sample.library else { continue };
        if library.activity == Activity::AwaitingApproval {
            if approval_impossible(request.no_approval, library.permission) {
                failures.insert("AwaitingApproval in a mode where no approval is possible".into());
            } else if screen_tracks_approval(request.harness)
                && !near_screen(request.samples, sample.elapsed_ms, PERMISSION, write_ms)
            {
                failures.insert(format!("AwaitingApproval while the screen is {}", sample.screen));
            }
        }
        if library.activity == Activity::Done && library.background > 0 {
            failures.insert("Done while background agents are running".into());
        }
    }
    if background_done_persists(request.samples, write_ms) {
        failures.insert("Done while background agents are running".into());
    }
    for message in missed_prompts(request.samples, stall_ms, write_ms) {
        failures.insert(message);
    }
    for message in unmet_expectations(request.samples, request.expected, write_ms) {
        failures.insert(message);
    }
    for message in short_screens(request.samples, request.min_screen_ms) {
        failures.insert(message);
    }
    for message in unmet_observations(request) {
        failures.insert(message);
    }
}

fn screen_tracks_approval(harness: &str) -> bool {
    matches!(harness, "claude_code" | "codex")
}

fn approval_impossible(no_approval: bool, permission: Option<PermissionMode>) -> bool {
    no_approval || permission.is_some_and(|mode| !mode.can_ask_human())
}

fn near_screen(samples: &[Sample], elapsed_ms: u64, screen: &str, write_ms: u64) -> bool {
    samples.iter().any(|sample| sample.screen == screen && sample.elapsed_ms.abs_diff(elapsed_ms) <= write_ms)
}

fn screen_while_activity(
    samples: &[Sample],
    elapsed_ms: u64,
    screen: &str,
    activity: &str,
    certainty: Option<&str>,
    write_ms: u64,
) -> bool {
    samples.iter().any(|sample| {
        sample.screen == screen
            && sample.elapsed_ms.abs_diff(elapsed_ms) <= write_ms
            && sample.library.as_ref().is_none_or(|library| library_matches(library, activity, certainty))
    })
}

fn background_done_persists(samples: &[Sample], write_ms: u64) -> bool {
    let mut start = None;
    let mut last = 0u64;
    let mut persisted = false;
    for sample in samples {
        let showing = sample.background_agents
            && sample.library.as_ref().is_some_and(|library| library.activity == Activity::Done);
        if showing {
            start.get_or_insert(sample.elapsed_ms);
            last = sample.elapsed_ms;
        } else if let Some(opened) = start.take()
            && last.saturating_sub(opened) > write_ms
        {
            persisted = true;
        }
    }
    if let Some(opened) = start
        && last.saturating_sub(opened) > write_ms
    {
        persisted = true;
    }
    persisted
}

fn missed_prompts(samples: &[Sample], stall_ms: u64, write_ms: u64) -> Vec<String> {
    let mut failures = Vec::new();
    let mut index = 0;
    while index < samples.len() {
        if samples[index].screen != PERMISSION {
            index += 1;
            continue;
        }
        let start = samples[index].elapsed_ms;
        let mut end = start;
        while index < samples.len() && samples[index].screen == PERMISSION {
            end = samples[index].elapsed_ms;
            index += 1;
        }
        let duration = end.saturating_sub(start);
        if duration > stall_ms + write_ms && !saw_approval(samples, start, end + write_ms) {
            failures.push(format!("permission prompt on screen for {duration} ms without AwaitingApproval"));
        }
    }
    failures
}

fn saw_approval(samples: &[Sample], from_ms: u64, to_ms: u64) -> bool {
    samples.iter().any(|sample| {
        (from_ms..=to_ms).contains(&sample.elapsed_ms)
            && sample.library.as_ref().is_some_and(|library| {
                matches!(library.activity, Activity::AwaitingApproval | Activity::AwaitingAnswer)
            })
    })
}

fn unmet_expectations(samples: &[Sample], expected: &[Expectation], write_ms: u64) -> Vec<String> {
    let mut failures = Vec::new();
    for expect in expected {
        if !known_activity(&expect.activity) {
            failures.push(format!("expected activity {:?} is not a library activity", expect.activity));
            continue;
        }
        if let Some(certainty) = &expect.certainty
            && !known_certainty(certainty)
        {
            failures.push(format!("expected certainty {certainty:?} is not firm or inferred"));
            continue;
        }
        match expect.when {
            ExpectWhen::End => {
                let Some(index) = samples.iter().rposition(|sample| sample.library.is_some()) else {
                    failures
                        .push(format!("expected {} at end, but the library never reported a state", expect.activity));
                    continue;
                };
                let sample = &samples[index];
                let library = sample.library.as_ref().expect("library");
                let actual = activity_id(library.activity);
                if actual != expect.activity {
                    failures.push(format!("expected {} at end, saw {actual}", expect.activity));
                    continue;
                }
                if let Some(want) = &expect.certainty {
                    let actual = certainty_id(library.certainty);
                    if actual != want {
                        failures.push(format!("expected {} ({want}) at end, saw {actual}", expect.activity));
                        continue;
                    }
                }
                if let Some(screen) = &expect.screen
                    && !screen_while_activity(
                        samples,
                        sample.elapsed_ms,
                        screen,
                        &expect.activity,
                        expect.certainty.as_deref(),
                        write_ms,
                    )
                {
                    failures
                        .push(format!("expected {} on screen {screen} at end, saw {}", expect.activity, sample.screen));
                }
            }
            ExpectWhen::Ever => {
                let hits: Vec<usize> = samples
                    .iter()
                    .enumerate()
                    .filter(|(_, sample)| {
                        sample.library.as_ref().is_some_and(|library| {
                            library_matches(library, &expect.activity, expect.certainty.as_deref())
                        })
                    })
                    .map(|(index, _)| index)
                    .collect();
                if hits.is_empty() {
                    let message = match &expect.certainty {
                        Some(certainty) => {
                            format!("expected {} ({certainty}), never observed", expect.activity)
                        }
                        None => format!("expected {}, never observed", expect.activity),
                    };
                    failures.push(message);
                    continue;
                }
                if let Some(screen) = &expect.screen
                    && !hits.iter().any(|&index| {
                        screen_while_activity(
                            samples,
                            samples[index].elapsed_ms,
                            screen,
                            &expect.activity,
                            expect.certainty.as_deref(),
                            write_ms,
                        )
                    })
                {
                    failures.push(format!("expected {} on screen {screen}, never observed together", expect.activity));
                }
            }
        }
    }
    failures
}

fn library_matches(library: &LibraryPoint, activity: &str, certainty: Option<&str>) -> bool {
    activity_id(library.activity) == activity && certainty.is_none_or(|want| certainty_id(library.certainty) == want)
}

fn short_screens(samples: &[Sample], minimums: &BTreeMap<String, u64>) -> Vec<String> {
    let mut failures = Vec::new();
    for (screen, minimum) in minimums {
        let seen = screen_duration_ms(samples, screen);
        if seen < *minimum {
            failures.push(format!("{screen} was on screen for {seen} ms, expected at least {minimum} ms"));
        }
    }
    failures
}

fn screen_duration_ms(samples: &[Sample], screen: &str) -> u64 {
    let mut total = 0u64;
    let mut previous: Option<u64> = None;
    for sample in samples {
        if sample.screen == screen {
            if let Some(previous) = previous {
                total = total.saturating_add(sample.elapsed_ms.saturating_sub(previous));
            }
            previous = Some(sample.elapsed_ms);
        } else {
            previous = None;
        }
    }
    total
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Allowed,
    Disagreement,
    Unscored,
}

fn disagreements(samples: &[Sample], stall_ms: u64, write_ms: u64) -> Vec<Disagreement> {
    let ages = prompt_ages(samples);
    let fits: Vec<Fit> = samples.iter().enumerate().map(|(index, sample)| fit(sample, ages[index], stall_ms)).collect();
    let mut grouped: BTreeMap<(String, String), (usize, u64, u64)> = BTreeMap::new();
    for (index, sample) in samples.iter().enumerate() {
        if fits[index] != Fit::Disagreement || excused(index, samples, &fits, write_ms) {
            continue;
        }
        let activity = activity_id(sample.library.as_ref().expect("disagreement has a library state").activity);
        let entry = grouped.entry((sample.screen.to_string(), activity.to_string())).or_insert((
            0,
            sample.elapsed_ms,
            sample.elapsed_ms,
        ));
        entry.0 += 1;
        entry.2 = sample.elapsed_ms;
    }
    grouped
        .into_iter()
        .map(|((screen, activity), (count, first, last))| Disagreement {
            screen,
            activity,
            samples: count,
            first_elapsed_ms: first,
            last_elapsed_ms: last,
        })
        .collect()
}

fn excused(index: usize, samples: &[Sample], fits: &[Fit], write_ms: u64) -> bool {
    let elapsed = samples[index].elapsed_ms;
    fits.iter()
        .enumerate()
        .any(|(other, fit)| *fit == Fit::Allowed && samples[other].elapsed_ms.abs_diff(elapsed) <= write_ms)
}

fn fit(sample: &Sample, prompt_age_ms: Option<u64>, stall_ms: u64) -> Fit {
    let Some(library) = &sample.library else { return Fit::Unscored };
    if matches!(sample.screen, "starting" | "unknown") {
        return Fit::Unscored;
    }
    if pair_allowed(sample.screen, library.activity, prompt_age_ms.unwrap_or(0), stall_ms) {
        Fit::Allowed
    } else {
        Fit::Disagreement
    }
}

fn pair_allowed(screen: &str, activity: Activity, prompt_age_ms: u64, stall_ms: u64) -> bool {
    match screen {
        PERMISSION => {
            matches!(activity, Activity::AwaitingApproval | Activity::AwaitingAnswer)
                || (activity == Activity::RunningTool && prompt_age_ms <= stall_ms)
        }
        "ask_user_question" => activity == Activity::AwaitingAnswer,
        "idle" => matches!(activity, Activity::Done | Activity::Idle | Activity::Interrupted | Activity::Errored),
        "working" => matches!(activity, Activity::Thinking | Activity::RunningTool | Activity::BackgroundWork),
        "error" => activity == Activity::Errored,
        _ => false,
    }
}

fn prompt_ages(samples: &[Sample]) -> Vec<Option<u64>> {
    let mut ages = vec![None; samples.len()];
    let mut index = 0;
    while index < samples.len() {
        if samples[index].screen != PERMISSION {
            index += 1;
            continue;
        }
        let start = samples[index].elapsed_ms;
        while index < samples.len() && samples[index].screen == PERMISSION {
            ages[index] = Some(samples[index].elapsed_ms.saturating_sub(start));
            index += 1;
        }
    }
    ages
}

pub fn activity_id(activity: Activity) -> &'static str {
    match activity {
        Activity::AwaitingAnswer => "awaiting_answer",
        Activity::AwaitingApproval => "awaiting_approval",
        Activity::RunningTool => "running_tool",
        Activity::Thinking => "thinking",
        Activity::BackgroundWork => "background_work",
        Activity::Done => "done",
        Activity::Interrupted => "interrupted",
        Activity::Errored => "errored",
        Activity::Idle => "idle",
        Activity::Unknown => "unknown",
        _ => "unknown",
    }
}

fn certainty_id(certainty: Certainty) -> &'static str {
    match certainty {
        Certainty::Firm => "firm",
        Certainty::Inferred => "inferred",
    }
}

fn known_certainty(name: &str) -> bool {
    matches!(name, "firm" | "inferred")
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Observations {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_permissions: Vec<ToolPermission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_rejections: Vec<ToolRejection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interrupts: Vec<InterruptNote>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_errors: Vec<ApiErrorNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundNote>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<CompactionNote>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuations: Vec<ContinuationNote>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resumes: Vec<ResumeNote>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lean: Option<LeanNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<SessionsNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexNote>,
}

impl Observations {
    pub fn is_empty(&self) -> bool {
        self.permission_modes.is_empty()
            && self.tool_permissions.is_empty()
            && self.tool_rejections.is_empty()
            && self.interrupts.is_empty()
            && self.api_errors.is_empty()
            && self.background.is_none()
            && self.compactions.is_empty()
            && self.continuations.is_empty()
            && self.resumes.is_empty()
            && self.models.is_empty()
            && self.text.is_none()
            && self.lean.is_none()
            && self.sessions.is_none()
            && self.codex.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CodexNote {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_policies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandbox_policies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_events: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_aborts: Vec<CodexAbort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_totals: Vec<CodexTokens>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<CodexThread>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_index: Vec<CodexIndexEntry>,
    pub max_pending_function_calls: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexAbort {
    pub reason: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexTokens {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexThread {
    pub id: String,
    pub files: usize,
    pub prompts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexIndexEntry {
    pub id: Option<String>,
    pub thread_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionNote {
    pub file: String,
    pub trigger: Option<String>,
    pub pre_tokens: Option<u64>,
    pub post_tokens: Option<u64>,
    pub summary_follows: bool,
    pub continued_after: bool,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContinuationNote {
    pub session_id: Option<String>,
    pub continued_in_session_id: Option<String>,
    pub target_file_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResumeNote {
    pub session_id: String,
    pub files: usize,
    pub user_prompts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextNote {
    pub prompt: bool,
    pub title: bool,
    pub path: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeanNote {
    pub max_line_bytes: u64,
    pub max_tool_result_bytes: u64,
    pub lean_activity: String,
    pub full_activity: String,
    pub agree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionsNote {
    pub runs: Vec<StatusRun>,
    pub statuses: Vec<String>,
    pub waiting_for: Vec<String>,
    pub during_screen: BTreeMap<String, Vec<String>>,
    pub fields: Vec<String>,
    pub removed_on_exit: Option<bool>,
    pub files_after_exit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusRun {
    pub status: String,
    pub waiting_for: Option<String>,
    pub screen: String,
    pub samples: usize,
    pub first_elapsed_ms: u64,
    pub last_elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistrySample {
    pub elapsed_ms: u64,
    pub screen: String,
    pub pid: Option<u64>,
    pub session_id: Option<String>,
    pub status: Option<String>,
    pub waiting_for: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryExpect {
    #[serde(default)]
    pub statuses: Vec<String>,
    #[serde(default)]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub waiting_during_screen: Option<String>,
    #[serde(default)]
    pub removed_on_exit: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ObserveExpect {
    #[serde(default)]
    pub codex_approval_policies: Vec<String>,
    #[serde(default)]
    pub codex_sandbox_policies: Vec<String>,
    #[serde(default)]
    pub codex_no_approval_events: Option<bool>,
    #[serde(default)]
    pub codex_turn_abort_reasons: Vec<String>,
    #[serde(default)]
    pub codex_task_error_contains: Vec<String>,
    #[serde(default)]
    pub codex_token_output_totals: Vec<u64>,
    #[serde(default)]
    pub codex_same_file_resume: Option<bool>,
    #[serde(default)]
    pub codex_min_pending_function_calls: Option<usize>,
    #[serde(default)]
    pub codex_session_index_titles: Vec<String>,
    #[serde(default)]
    pub compaction_triggers: Vec<String>,
    #[serde(default)]
    pub compaction_same_file: Option<bool>,
    #[serde(default)]
    pub continued_in: Option<bool>,
    #[serde(default)]
    pub same_file_resume: Option<bool>,
    #[serde(default)]
    pub model_switch: Option<bool>,
    #[serde(default)]
    pub permission_modes: Vec<String>,
    #[serde(default)]
    pub non_ascii_prompt: Option<bool>,
    #[serde(default)]
    pub non_ascii_title: Option<bool>,
    #[serde(default)]
    pub non_ascii_path: Option<bool>,
    #[serde(default)]
    pub lean_agrees: Option<bool>,
    #[serde(default)]
    pub min_tool_result_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TranscriptDump {
    pub name: String,
    pub jsonl: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterruptNote {
    pub text: String,
    pub turn_duration_before_next_prompt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiErrorNote {
    pub error: String,
    pub is_api_error_message: bool,
    pub turn_duration_follows: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct BackgroundNote {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_counts: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_statuses: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub async_launched: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helpers: Vec<HelperNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelperNote {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by_user: Option<bool>,
    pub launched_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolPermission {
    pub tool_name: Option<String>,
    pub decision: Option<String>,
    pub source: Option<String>,
    pub reason_type: Option<String>,
    pub denial_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolRejection {
    pub tool_name: Option<String>,
    pub tool_use_result: Option<String>,
    pub tool_denial_kind: Option<String>,
    pub result_text: Option<String>,
    pub firm_turn_end_before_next_prompt: bool,
    pub interrupt_before_next_prompt: bool,
    pub assistant_continued: bool,
    pub ends_turn: bool,
    pub following: Vec<String>,
}

#[cfg(test)]
fn observe_transcript(jsonl: &str) -> Observations {
    let dumps = [TranscriptDump { name: "session.jsonl".into(), jsonl: jsonl.to_string() }];
    let request = ScoreRequest {
        scenario: "observe",
        description: None,
        harness: "claude_code",
        no_approval: false,
        expected: &[],
        samples: &[],
        mock_requests: 0,
        remaining_mock_steps: 0,
        transcript: None,
        min_screen_ms: &BTreeMap::new(),
        transcripts: &dumps,
        helpers: &[],
        registry: &[],
        registry_fields: &[],
        registry_files_after_exit: None,
        expect_registry: None,
        expect_observations: None,
    };
    observe_run(&request)
}

fn observe_run(request: &ScoreRequest<'_>) -> Observations {
    let files: Vec<(String, Vec<Value>)> =
        request.transcripts.iter().map(|dump| (dump.name.clone(), jsonl_records(&dump.jsonl))).collect();
    let records: Vec<Value> = files.iter().flat_map(|(_, records)| records.clone()).collect();
    let tools = tool_names(&records);
    let mut observations = Observations::default();
    for record in &records {
        if let Some(mode) = record.get("permissionMode").and_then(Value::as_str)
            && !observations.permission_modes.iter().any(|seen| seen == mode)
        {
            observations.permission_modes.push(mode.to_string());
        }
        if let Some(permission) = tool_permission(record, &tools) {
            observations.tool_permissions.push(permission);
        }
    }
    for (index, record) in records.iter().enumerate() {
        if let Some(rejection) = tool_rejection(record, &records[..index], &records[index + 1..], &tools) {
            observations.tool_rejections.push(rejection);
        }
    }
    observations.interrupts = interrupt_notes(&records);
    observations.api_errors = api_error_notes(&records);
    observations.background = background_note(&records, request.helpers);
    observations.compactions = compaction_notes(&files);
    observations.continuations = continuation_notes(&files);
    observations.resumes = resume_notes(&files);
    let models = distinct_models(&records);
    if models.len() >= 2 {
        observations.models = models;
    }
    observations.text = text_note(&records);
    observations.lean = lean_note(request.transcripts);
    observations.sessions = sessions_note(request);
    if request.harness == "codex" {
        observations.codex = codex_note(&files);
    }
    observations
}

fn codex_note(files: &[(String, Vec<Value>)]) -> Option<CodexNote> {
    let mut note = CodexNote::default();
    let mut thread_files: BTreeMap<String, usize> = BTreeMap::new();
    let mut thread_prompts: BTreeMap<String, usize> = BTreeMap::new();
    let mut pending_calls: BTreeSet<String> = BTreeSet::new();
    for (name, records) in files {
        let file_id = records.iter().find_map(codex_session_id);
        if let Some(id) = &file_id {
            *thread_files.entry(id.clone()).or_default() += 1;
        }
        for record in records {
            let kind = record.get("type").and_then(Value::as_str).unwrap_or_default();
            let payload = record.get("payload").unwrap_or(&Value::Null);
            let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or_default();
            if kind == "turn_context" {
                if let Some(policy) = payload.get("approval_policy").and_then(Value::as_str)
                    && !note.approval_policies.iter().any(|seen| seen == policy)
                {
                    note.approval_policies.push(policy.to_string());
                }
                if let Some(sandbox) =
                    payload.get("sandbox_policy").and_then(|value| value.get("type")).and_then(Value::as_str)
                    && !note.sandbox_policies.iter().any(|seen| seen == sandbox)
                {
                    note.sandbox_policies.push(sandbox.to_string());
                }
            }
            if codex_approval_event(kind, payload_type) && !note.approval_events.iter().any(|seen| seen == payload_type)
            {
                note.approval_events.push(if payload_type.is_empty() {
                    kind.to_string()
                } else {
                    payload_type.to_string()
                });
            }
            if kind == "event_msg" && payload_type == "turn_aborted" {
                note.turn_aborts.push(CodexAbort {
                    reason: payload.get("reason").and_then(Value::as_str).map(str::to_string),
                    duration_ms: payload.get("duration_ms").and_then(Value::as_u64),
                });
            }
            if kind == "event_msg"
                && matches!(payload_type, "task_complete" | "turn_complete")
                && let Some(error) = codex_task_error(payload)
            {
                note.task_errors.push(error.to_string());
            }
            if kind == "event_msg"
                && payload_type == "token_count"
                && let Some(total) =
                    payload.get("info").and_then(|info| info.get("total_token_usage")).filter(|v| !v.is_null())
            {
                note.token_totals.push(CodexTokens {
                    input_tokens: total.get("input_tokens").and_then(Value::as_u64),
                    cached_input_tokens: total.get("cached_input_tokens").and_then(Value::as_u64),
                    output_tokens: total.get("output_tokens").and_then(Value::as_u64),
                    total_tokens: total.get("total_tokens").and_then(Value::as_u64),
                });
            }
            if kind == "response_item"
                && matches!(payload_type, "function_call" | "custom_tool_call")
                && let Some(id) = payload.get("call_id").and_then(Value::as_str).filter(|id| !id.is_empty())
            {
                pending_calls.insert(id.to_string());
                note.max_pending_function_calls = note.max_pending_function_calls.max(pending_calls.len());
            }
            if kind == "response_item"
                && matches!(payload_type, "function_call_output" | "custom_tool_call_output")
                && let Some(id) = payload.get("call_id").and_then(Value::as_str)
            {
                pending_calls.remove(id);
            }
            if kind == "event_msg"
                && payload_type == "item_completed"
                && payload.get("item").and_then(|item| item.get("type")).and_then(Value::as_str) == Some("UserMessage")
            {
                let id =
                    payload.get("thread_id").and_then(Value::as_str).map(str::to_string).or_else(|| file_id.clone());
                if let Some(id) = id {
                    *thread_prompts.entry(id).or_default() += 1;
                }
            }
            if kind == "event_msg" && payload_type == "user_message" {
                let id = file_id.clone();
                if let Some(id) = id {
                    *thread_prompts.entry(id).or_default() += 1;
                }
            }
        }
        if name == "session_index.jsonl" {
            for record in records {
                let Some(title) = record.get("thread_name").and_then(Value::as_str).filter(|title| !title.is_empty())
                else {
                    continue;
                };
                note.session_index.push(CodexIndexEntry {
                    id: record.get("id").and_then(Value::as_str).map(str::to_string),
                    thread_name: title.to_string(),
                });
            }
        }
    }
    let ids: BTreeSet<String> = thread_files.keys().chain(thread_prompts.keys()).cloned().collect();
    note.threads = ids
        .into_iter()
        .map(|id| CodexThread {
            files: thread_files.get(&id).copied().unwrap_or(0),
            prompts: thread_prompts.get(&id).copied().unwrap_or(0),
            id,
        })
        .collect();
    if note.approval_policies.is_empty()
        && note.sandbox_policies.is_empty()
        && note.approval_events.is_empty()
        && note.turn_aborts.is_empty()
        && note.task_errors.is_empty()
        && note.token_totals.is_empty()
        && note.threads.is_empty()
        && note.session_index.is_empty()
        && note.max_pending_function_calls == 0
    {
        None
    } else {
        Some(note)
    }
}

fn codex_session_id(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = record.get("payload")?;
    payload
        .get("id")
        .or_else(|| payload.get("session_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn codex_task_error(payload: &Value) -> Option<&str> {
    payload
        .get("error")
        .and_then(|error| error.as_str().or_else(|| error.get("message").and_then(Value::as_str)))
        .filter(|error| !error.is_empty())
}

fn codex_approval_event(kind: &str, payload_type: &str) -> bool {
    const EVENTS: [&str; 8] = [
        "exec_approval_request",
        "apply_patch_approval_request",
        "request_permissions",
        "request_user_input",
        "elicitation_request",
        "exec_command_begin",
        "exec_command_end",
        "error",
    ];
    EVENTS.contains(&kind) || (!payload_type.is_empty() && EVENTS.contains(&payload_type))
}

fn compaction_notes(files: &[(String, Vec<Value>)]) -> Vec<CompactionNote> {
    let mut notes = Vec::new();
    for (name, records) in files {
        for (index, record) in records.iter().enumerate() {
            if record.get("type").and_then(Value::as_str) != Some("system")
                || record.get("subtype").and_then(Value::as_str) != Some("compact_boundary")
            {
                continue;
            }
            let meta = record.get("compactMetadata");
            let after = &records[index + 1..];
            let summary_at = after.iter().position(|next| next.get("isCompactSummary") == Some(&Value::Bool(true)));
            notes.push(CompactionNote {
                file: name.clone(),
                trigger: meta.and_then(|value| value.get("trigger")).and_then(Value::as_str).map(str::to_string),
                pre_tokens: meta.and_then(|value| value.get("preTokens")).and_then(Value::as_u64),
                post_tokens: meta.and_then(|value| value.get("postTokens")).and_then(Value::as_u64),
                summary_follows: summary_at.is_some_and(|at| at < 4),
                continued_after: summary_at.is_some_and(|at| {
                    after.iter().skip(at + 1).any(|next| {
                        next.get("type").and_then(Value::as_str) == Some("assistant") || is_user_prompt(next)
                    })
                }),
                session_id: record.get("sessionId").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    notes
}

fn continuation_notes(files: &[(String, Vec<Value>)]) -> Vec<ContinuationNote> {
    let mut notes = Vec::new();
    for (index, (_, records)) in files.iter().enumerate() {
        for record in records {
            if record.get("type").and_then(Value::as_str) != Some("continued-in") {
                continue;
            }
            let target = record.get("continuedInSessionId").and_then(Value::as_str).map(str::to_string);
            let target_file_present = target.as_ref().is_some_and(|id| {
                files.iter().enumerate().any(|(other, (_, other_records))| {
                    other != index
                        && other_records.iter().any(|next| next.get("sessionId").and_then(Value::as_str) == Some(id))
                })
            });
            notes.push(ContinuationNote {
                session_id: record.get("sessionId").and_then(Value::as_str).map(str::to_string),
                continued_in_session_id: target,
                target_file_present,
            });
        }
    }
    notes
}

fn resume_notes(files: &[(String, Vec<Value>)]) -> Vec<ResumeNote> {
    let mut grouped: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (_, records) in files {
        let mut seen = BTreeSet::new();
        for record in records {
            let Some(id) = record.get("sessionId").and_then(Value::as_str) else { continue };
            if seen.insert(id.to_string()) {
                grouped.entry(id.to_string()).or_insert((0, 0)).0 += 1;
            }
            if is_user_prompt(record) {
                grouped.entry(id.to_string()).or_insert((0, 0)).1 += 1;
            }
        }
    }
    grouped
        .into_iter()
        .filter(|(_, (file_count, prompts))| *file_count >= 2 || *prompts >= 2)
        .map(|(session_id, (files, user_prompts))| ResumeNote { session_id, files, user_prompts })
        .collect()
}

fn distinct_models(records: &[Value]) -> Vec<String> {
    let mut models = Vec::new();
    for record in records {
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(model) = record.get("message").and_then(|message| message.get("model")).and_then(Value::as_str) else {
            continue;
        };
        if model.is_empty() || models.last().is_some_and(|seen| seen == model) {
            continue;
        }
        models.push(model.to_string());
    }
    models
}

fn text_note(records: &[Value]) -> Option<TextNote> {
    let mut prompt = false;
    let mut title = false;
    let mut path = false;
    for record in records {
        if record.get("type").and_then(Value::as_str) == Some("ai-title")
            && record.get("aiTitle").and_then(Value::as_str).is_some_and(has_non_ascii)
        {
            title = true;
        }
        if is_user_prompt(record) && has_non_ascii(&message_text(record)) {
            prompt = true;
        }
        walk_paths(record, &mut path);
    }
    if prompt || title || path { Some(TextNote { prompt, title, path }) } else { None }
}

fn walk_paths(value: &Value, path: &mut bool) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(key.as_str(), "file_path" | "path" | "cwd") && child.as_str().is_some_and(has_non_ascii) {
                    *path = true;
                }
                walk_paths(child, path);
            }
        }
        Value::Array(items) => {
            for child in items {
                walk_paths(child, path);
            }
        }
        _ => {}
    }
}

fn has_non_ascii(text: &str) -> bool {
    !text.is_ascii()
}

const LEAN_LINE: usize = 16 * 1024;

fn lean_note(files: &[TranscriptDump]) -> Option<LeanNote> {
    let max_line_bytes =
        files.iter().flat_map(|dump| dump.jsonl.lines().map(|line| line.len() as u64)).max().unwrap_or(0);
    let max_tool_result_bytes = files
        .iter()
        .flat_map(|dump| jsonl_records(&dump.jsonl))
        .flat_map(|record| tool_result_texts(&record).map(|text| text.len() as u64).collect::<Vec<_>>())
        .max()
        .unwrap_or(0);
    if max_line_bytes <= LEAN_LINE as u64 && max_tool_result_bytes <= LEAN_LINE as u64 {
        return None;
    }
    let mut agree = true;
    let mut lean_activity = String::new();
    let mut full_activity = String::new();
    for dump in files {
        let long = dump.jsonl.lines().any(|line| line.len() > LEAN_LINE)
            || jsonl_records(&dump.jsonl)
                .iter()
                .any(|record| tool_result_texts(record).any(|text| text.len() > LEAN_LINE));
        if !long {
            continue;
        }
        let lean = activity_of(&dump.jsonl, ParseMode::Lean);
        let full = activity_of(&dump.jsonl, ParseMode::Full);
        agree &= lean == full;
        lean_activity = lean;
        full_activity = full;
    }
    Some(LeanNote { max_line_bytes, max_tool_result_bytes, lean_activity, full_activity, agree })
}

fn activity_of(jsonl: &str, mode: ParseMode) -> String {
    let options = ReducerOptions { mode, ..ReducerOptions::default() };
    let mut reducer = SessionReducer::new(Harness::ClaudeCode, options);
    let path = Path::new("session.jsonl");
    reducer.set_path(path);
    let now = Timestamp::parse_rfc3339("2026-10-24T00:00:00Z").unwrap_or_else(|| Timestamp::from_secs_f64(0.0));
    let ctx = ChunkCtx { offset: 0, mtime: now, path };
    if reducer.feed(jsonl.as_bytes(), ctx).is_err() {
        return "unparsed".into();
    }
    activity_id(reducer.state(now).activity).to_string()
}

fn sessions_note(request: &ScoreRequest<'_>) -> Option<SessionsNote> {
    if request.registry.is_empty() && request.registry_files_after_exit.is_none() {
        return None;
    }
    let mut runs: Vec<StatusRun> = Vec::new();
    for sample in request.registry {
        let status = sample.status.clone().unwrap_or_default();
        if status.is_empty() {
            continue;
        }
        if let Some(run) = runs.last_mut()
            && run.status == status
            && run.waiting_for == sample.waiting_for
            && run.screen == sample.screen
        {
            run.samples += 1;
            run.last_elapsed_ms = sample.elapsed_ms;
            continue;
        }
        runs.push(StatusRun {
            status,
            waiting_for: sample.waiting_for.clone(),
            screen: sample.screen.clone(),
            samples: 1,
            first_elapsed_ms: sample.elapsed_ms,
            last_elapsed_ms: sample.elapsed_ms,
        });
    }
    let mut statuses = Vec::new();
    let mut waiting_for = Vec::new();
    for run in &runs {
        if !statuses.iter().any(|seen: &String| seen == &run.status) {
            statuses.push(run.status.clone());
        }
        if let Some(waiting) = &run.waiting_for
            && !waiting_for.iter().any(|seen: &String| seen == waiting)
        {
            waiting_for.push(waiting.clone());
        }
    }
    let mut during_screen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for sample in request.registry {
        let Some(status) = sample.status.clone().filter(|status| !status.is_empty()) else { continue };
        let entry = during_screen.entry(sample.screen.clone()).or_default();
        if !entry.iter().any(|seen| seen == &status) {
            entry.push(status);
        }
    }
    let saw = request.registry.iter().any(|sample| sample.status.is_some());
    let removed_on_exit = request.registry_files_after_exit.map(|left| saw && left == 0);
    Some(SessionsNote {
        runs,
        statuses,
        waiting_for,
        during_screen,
        fields: request.registry_fields.to_vec(),
        removed_on_exit,
        files_after_exit: request.registry_files_after_exit,
    })
}

fn unmet_observations(request: &ScoreRequest<'_>) -> Vec<String> {
    let mut failures = Vec::new();
    if let Some(expect) = request.expect_observations {
        let observations = observe_run(request);
        for trigger in &expect.compaction_triggers {
            if !observations.compactions.iter().any(|note| note.trigger.as_deref() == Some(trigger)) {
                failures.push(format!("compaction trigger {trigger} was not recorded"));
            }
        }
        if expect.compaction_same_file == Some(true)
            && (observations.compactions.is_empty()
                || observations.compactions.iter().any(|note| !note.summary_follows || !note.continued_after)
                || observations.compactions.iter().map(|note| note.file.as_str()).collect::<BTreeSet<_>>().len() != 1)
        {
            failures.push("compaction did not continue in the same file".into());
        }
        if let Some(want) = expect.continued_in {
            if want && observations.continuations.is_empty() {
                failures.push("continued-in record was not recorded".into());
            } else if want && !observations.continuations.iter().any(|note| note.target_file_present) {
                failures.push("continued-in target file was not recorded".into());
            }
            if !want && !observations.continuations.is_empty() {
                failures.push("continued-in record was recorded".into());
            }
        }
        if expect.same_file_resume == Some(true)
            && !observations.resumes.iter().any(|note| note.files == 1 && note.user_prompts >= 2)
        {
            failures.push("resume did not continue in the same file".into());
        }
        if expect.model_switch == Some(true) && observations.models.len() < 2 {
            failures.push("model did not change".into());
        }
        for mode in &expect.permission_modes {
            if !observations.permission_modes.iter().any(|seen| seen == mode) {
                failures.push(format!("permission mode {mode} was not recorded"));
            }
        }
        let text = observations.text.unwrap_or(TextNote { prompt: false, title: false, path: false });
        if expect.non_ascii_prompt == Some(true) && !text.prompt {
            failures.push("prompt was ascii".into());
        }
        if expect.non_ascii_title == Some(true) && !text.title {
            failures.push("title was ascii".into());
        }
        if expect.non_ascii_path == Some(true) && !text.path {
            failures.push("path was ascii".into());
        }
        if expect.lean_agrees == Some(true) {
            match &observations.lean {
                None => failures.push("tool output was not long enough to exercise lean".into()),
                Some(note) if !note.agree => failures.push("lean and full parse disagree".into()),
                _ => {}
            }
        }
        if let Some(want) = expect.min_tool_result_bytes {
            let got = observations.lean.as_ref().map(|note| note.max_tool_result_bytes).unwrap_or(0);
            if got < want {
                failures.push(format!("tool result was {got} bytes, want at least {want}"));
            }
        }
        if let Some(codex) = &observations.codex {
            for policy in &expect.codex_approval_policies {
                if !codex.approval_policies.iter().any(|seen| seen == policy) {
                    failures.push(format!("Codex approval policy {policy} was not recorded"));
                }
            }
            for sandbox in &expect.codex_sandbox_policies {
                if !codex.sandbox_policies.iter().any(|seen| seen == sandbox) {
                    failures.push(format!("Codex sandbox policy {sandbox} was not recorded"));
                }
            }
            if expect.codex_no_approval_events == Some(true) && !codex.approval_events.is_empty() {
                failures.push(format!("Codex wrote approval events: {}", codex.approval_events.join(", ")));
            }
            for reason in &expect.codex_turn_abort_reasons {
                if !codex.turn_aborts.iter().any(|abort| abort.reason.as_deref() == Some(reason)) {
                    failures.push(format!("Codex turn_aborted reason {reason} was not recorded"));
                }
            }
            for text in &expect.codex_task_error_contains {
                if !codex.task_errors.iter().any(|error| error.contains(text)) {
                    failures.push(format!("Codex task_complete.error did not contain {text:?}"));
                }
            }
            if !expect.codex_token_output_totals.is_empty() {
                let got: Vec<u64> = codex.token_totals.iter().filter_map(|total| total.output_tokens).collect();
                if got != expect.codex_token_output_totals {
                    failures.push(format!(
                        "Codex token_count output totals were {got:?}, want {:?}",
                        expect.codex_token_output_totals
                    ));
                }
            }
            if expect.codex_same_file_resume == Some(true)
                && !codex.threads.iter().any(|thread| thread.files == 1 && thread.prompts >= 2)
            {
                failures.push("Codex resume did not continue in the same file".into());
            }
            if let Some(want) = expect.codex_min_pending_function_calls
                && codex.max_pending_function_calls < want
            {
                failures.push(format!(
                    "Codex had at most {} pending function calls, want at least {want}",
                    codex.max_pending_function_calls
                ));
            }
            for title in &expect.codex_session_index_titles {
                if !codex.session_index.iter().any(|entry| &entry.thread_name == title) {
                    failures.push(format!("Codex session_index title {title:?} was not recorded"));
                }
            }
        }
    }
    if let Some(expect) = request.expect_registry {
        let note = sessions_note(request);
        for status in &expect.statuses {
            let seen = note.as_ref().is_some_and(|note| note.statuses.iter().any(|got| got == status));
            if !seen {
                failures.push(format!("registry status {status} was not recorded"));
            }
        }
        if let Some(waiting) = &expect.waiting_for {
            let seen = note.as_ref().is_some_and(|note| note.waiting_for.iter().any(|got| got == waiting));
            if !seen {
                failures.push(format!("registry waitingFor {waiting} was not recorded"));
            }
        }
        if let Some(screen) = &expect.waiting_during_screen {
            let waiting = note.as_ref().is_some_and(|note| {
                note.during_screen.get(screen).is_some_and(|statuses| statuses.iter().any(|status| status == "waiting"))
            });
            if !waiting {
                failures.push(format!("registry was not waiting during {screen}"));
            }
        }
        if let Some(want) = expect.removed_on_exit {
            let got = note.as_ref().and_then(|note| note.removed_on_exit);
            if got != Some(want) {
                if want {
                    failures.push("registry file was still present after exit".into());
                } else {
                    failures.push("registry file was removed after exit".into());
                }
            }
        }
    }
    failures
}

fn interrupt_notes(records: &[Value]) -> Vec<InterruptNote> {
    let mut notes = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if !is_interrupt(record) {
            continue;
        }
        notes.push(InterruptNote {
            text: clip(&message_text(record), 120),
            turn_duration_before_next_prompt: turn_duration_before_prompt(&records[index + 1..]),
        });
    }
    notes
}

fn api_error_notes(records: &[Value]) -> Vec<ApiErrorNote> {
    let mut notes = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let flagged = record.get("isApiErrorMessage") == Some(&Value::Bool(true));
        let error = record.get("error").and_then(Value::as_str).filter(|text| !text.is_empty());
        if !flagged && error.is_none() {
            continue;
        }
        notes.push(ApiErrorNote {
            error: error.unwrap_or("api_error").to_string(),
            is_api_error_message: flagged,
            turn_duration_follows: turn_duration_before_prompt(&records[index + 1..]),
        });
    }
    notes
}

fn turn_duration_before_prompt(after: &[Value]) -> bool {
    for record in after {
        if is_user_prompt(record) {
            return false;
        }
        if record.get("type").and_then(Value::as_str) == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("turn_duration")
        {
            return true;
        }
    }
    false
}

fn background_note(records: &[Value], helpers: &[ObservedHelper]) -> Option<BackgroundNote> {
    let mut note = BackgroundNote::default();
    for record in records {
        if record.get("type").and_then(Value::as_str) == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("turn_duration")
            && let Some(count) = record.get("pendingBackgroundAgentCount").and_then(Value::as_u64)
        {
            note.pending_counts.push(count);
        }
        if record.get("origin").and_then(|origin| origin.get("kind")).and_then(Value::as_str)
            == Some("task-notification")
            && let Some(status) = task_status(&message_text(record))
        {
            note.task_statuses.push(status);
        }
        if let Some(agent_id) = async_agent_id(record) {
            note.async_launched.push(agent_id);
        }
    }
    note.helpers = helpers.iter().map(helper_note).collect();
    if note.pending_counts.is_empty()
        && note.task_statuses.is_empty()
        && note.async_launched.is_empty()
        && note.helpers.is_empty()
    {
        None
    } else {
        Some(note)
    }
}

fn task_status(text: &str) -> Option<String> {
    let rest = text.split_once("<status>")?.1;
    let status = rest.split_once("</status>")?.0.trim();
    if status.is_empty() { None } else { Some(status.to_string()) }
}

fn async_agent_id(record: &Value) -> Option<String> {
    let result = record.get("toolUseResult")?;
    match result {
        Value::Object(map) => {
            if map.get("status").and_then(Value::as_str) != Some("async_launched") {
                return None;
            }
            Some(map.get("agentId").and_then(Value::as_str).unwrap_or("unknown").to_string())
        }
        Value::String(text) if text.contains("async_launched") => Some("unknown".to_string()),
        _ => None,
    }
}

fn helper_note(helper: &ObservedHelper) -> HelperNote {
    let meta = helper.meta_json.as_deref().and_then(|text| serde_json::from_str::<Value>(text).ok());
    let field =
        |name: &str| meta.as_ref().and_then(|value| value.get(name)).and_then(Value::as_str).map(str::to_string);
    HelperNote {
        id: helper.id.clone(),
        parent_agent_id: field("parentAgentId"),
        spawn_depth: meta.as_ref().and_then(|value| value.get("spawnDepth")).and_then(Value::as_u64),
        agent_type: field("agentType"),
        finished: meta.as_ref().and_then(|value| value.get("finished")).and_then(Value::as_bool),
        stopped_by_user: meta.as_ref().and_then(|value| value.get("stoppedByUser")).and_then(Value::as_bool),
        launched_agent: helper_launched_agent(helper.jsonl.as_deref()),
    }
}

fn helper_launched_agent(jsonl: Option<&str>) -> bool {
    let Some(jsonl) = jsonl else { return false };
    jsonl_records(jsonl).iter().any(|record| {
        record.get("message").and_then(|message| message.get("content")).and_then(Value::as_array).is_some_and(
            |blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && matches!(block.get("name").and_then(Value::as_str), Some("Agent" | "Task"))
                })
            },
        )
    })
}

fn jsonl_records(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() { None } else { serde_json::from_str(line).ok() }
        })
        .collect()
}

fn tool_names(records: &[Value]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for record in records {
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = record.get("message").and_then(|message| message.get("content")).and_then(Value::as_array)
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let (Some(id), Some(name)) =
                (block.get("id").and_then(Value::as_str), block.get("name").and_then(Value::as_str))
            {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }
    names
}

fn tool_permission(record: &Value, tools: &HashMap<String, String>) -> Option<ToolPermission> {
    let decision = record.get("permissionDecision");
    let denial_kind = record.get("toolDenialKind").and_then(Value::as_str).map(str::to_string);
    if decision.is_none() && denial_kind.is_none() {
        return None;
    }
    let tool_name = first_tool_result_id(record).and_then(|id| tools.get(&id).cloned());
    Some(ToolPermission {
        tool_name,
        decision: decision.and_then(|value| value.get("decision")).and_then(Value::as_str).map(str::to_string),
        source: decision.and_then(|value| value.get("source")).and_then(Value::as_str).map(str::to_string),
        reason_type: decision.and_then(|value| value.get("reasonType")).and_then(Value::as_str).map(str::to_string),
        denial_kind,
    })
}

fn tool_rejection(
    record: &Value,
    before: &[Value],
    after: &[Value],
    tools: &HashMap<String, String>,
) -> Option<ToolRejection> {
    let tool_use_result_raw = record.get("toolUseResult").and_then(Value::as_str);
    let tool_use_result = tool_use_result_raw.map(|text| clip(text, 180));
    let tool_denial_kind = record.get("toolDenialKind").and_then(Value::as_str).map(str::to_string);
    let result_text = tool_result_texts(record).next().map(|text| clip(&text, 180));
    let short = tool_use_result_raw.is_some_and(|text| text.contains("User rejected tool use"));
    let kind = tool_denial_kind.as_deref() == Some("user-rejected");
    let prose = tool_result_texts(record).any(|text| {
        text.contains("User rejected tool use") || text.contains("doesn't want to proceed with this tool use")
    });
    if !short && !kind && !prose {
        return None;
    }
    let mut firm_turn_end_before_next_prompt = false;
    let mut interrupt_before_next_prompt =
        message_text(record).starts_with("[Request interrupted") || interrupt_since_assistant(before);
    let mut assistant_continued = false;
    let mut following = Vec::new();
    for next in after {
        if is_user_prompt(next) {
            break;
        }
        if following.len() < 6 {
            following.push(record_label(next));
        }
        if is_interrupt(next) {
            interrupt_before_next_prompt = true;
        }
        if next.get("type").and_then(Value::as_str) == Some("assistant") {
            assistant_continued = true;
        }
        if is_firm_end(next) {
            firm_turn_end_before_next_prompt = true;
        }
    }
    let ends_turn = !assistant_continued && (firm_turn_end_before_next_prompt || interrupt_before_next_prompt);
    let tool_name = first_tool_result_id(record).and_then(|id| tools.get(&id).cloned());
    Some(ToolRejection {
        tool_name,
        tool_use_result,
        tool_denial_kind,
        result_text,
        firm_turn_end_before_next_prompt,
        interrupt_before_next_prompt,
        assistant_continued,
        ends_turn,
        following,
    })
}

fn first_tool_result_id(record: &Value) -> Option<String> {
    record
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)?
        .iter()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .and_then(|block| block.get("tool_use_id").and_then(Value::as_str))
        .map(str::to_string)
}

fn tool_result_texts(record: &Value) -> impl Iterator<Item = String> + '_ {
    let blocks = record.get("message").and_then(|message| message.get("content")).and_then(Value::as_array);
    blocks.into_iter().flatten().filter_map(|block| {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            Some(content_text(block.get("content")))
        } else {
            None
        }
    })
}

fn message_text(record: &Value) -> String {
    let content = record.get("message").and_then(|message| message.get("content"));
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str).is_none_or(|kind| kind == "text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text").and_then(Value::as_str).or_else(|| part.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn interrupt_since_assistant(before: &[Value]) -> bool {
    for record in before.iter().rev() {
        if is_interrupt(record) {
            return true;
        }
        let kind = record.get("type").and_then(Value::as_str);
        if kind == Some("assistant") || is_user_prompt(record) {
            return false;
        }
    }
    false
}

fn is_user_prompt(record: &Value) -> bool {
    if record.get("type").and_then(Value::as_str) != Some("user")
        || record.get("isMeta") == Some(&Value::Bool(true))
        || record.get("isCompactSummary") == Some(&Value::Bool(true))
    {
        return false;
    }
    if first_tool_result_id(record).is_some() {
        return false;
    }
    let text = message_text(record);
    !text.is_empty() && !text.starts_with("[Request interrupted") && !text.starts_with('<')
}

fn is_interrupt(record: &Value) -> bool {
    message_text(record).starts_with("[Request interrupted")
}

fn is_firm_end(record: &Value) -> bool {
    let kind = record.get("type").and_then(Value::as_str);
    (kind == Some("system") && record.get("subtype").and_then(Value::as_str) == Some("turn_duration"))
        || (kind == Some("assistant")
            && record.get("message").and_then(|message| message.get("stop_reason")).and_then(Value::as_str)
                == Some("end_turn"))
}

fn record_label(record: &Value) -> String {
    let kind = record.get("type").and_then(Value::as_str).unwrap_or("?");
    if kind == "system"
        && let Some(subtype) = record.get("subtype").and_then(Value::as_str)
    {
        return format!("system:{subtype}");
    }
    if kind == "assistant"
        && let Some(stop) = record.get("message").and_then(|message| message.get("stop_reason")).and_then(Value::as_str)
    {
        return format!("assistant:{stop}");
    }
    if kind == "user" && is_interrupt(record) {
        return "user:interrupt".into();
    }
    if kind == "user" && first_tool_result_id(record).is_some() {
        return "user:tool_result".into();
    }
    kind.to_string()
}

fn clip(text: &str, max: usize) -> String {
    let mut clipped: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        clipped.push('…');
    }
    clipped
}

fn known_activity(name: &str) -> bool {
    matches!(
        name,
        "awaiting_answer"
            | "awaiting_approval"
            | "running_tool"
            | "thinking"
            | "background_work"
            | "done"
            | "interrupted"
            | "errored"
            | "idle"
            | "unknown"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::ScreenState;

    fn point(activity: Activity) -> LibraryPoint {
        LibraryPoint {
            activity,
            certainty: Certainty::Firm,
            reason: Reason::TurnEnded,
            permission: None,
            error: None,
            since: None,
            last_write: None,
            background: 0,
        }
    }

    fn at(elapsed_ms: u64, screen: ScreenState, activity: Option<Activity>) -> Sample {
        Sample {
            elapsed_ms,
            screen: screen.id(),
            background_agents: false,
            library: activity.map(point),
            ..Sample::default()
        }
    }

    fn score_of(samples: &[Sample], no_approval: bool, expected: &[Expectation]) -> Score {
        let minimums = BTreeMap::new();
        score(&ScoreRequest {
            scenario: "case",
            description: None,
            harness: "claude_code",
            no_approval,
            expected,
            samples,
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &minimums,
            transcripts: &[],
            helpers: &[],
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: None,
        })
    }

    fn expect_end(activity: &str) -> Expectation {
        Expectation { activity: activity.into(), when: ExpectWhen::End, screen: None, certainty: None }
    }

    #[test]
    fn confusion_counts_every_library_sample() {
        let samples = [
            at(0, ScreenState::Idle, Some(Activity::Done)),
            at(100, ScreenState::Idle, Some(Activity::Done)),
            at(200, ScreenState::Working, Some(Activity::Thinking)),
            at(300, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(400, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)),
            at(500, ScreenState::Starting, None),
        ];
        let scored = score_of(&samples, false, &[]);
        assert_eq!(scored.samples, 6);
        assert_eq!(scored.screen_counts["idle"], 2);
        assert_eq!(scored.screen_counts["starting"], 1);
        assert_eq!(scored.confusion["idle"]["done"], 2);
        assert_eq!(scored.confusion["working"]["thinking"], 1);
        assert_eq!(scored.confusion["permission_prompt"]["running_tool"], 1);
        assert_eq!(scored.confusion["permission_prompt"]["awaiting_approval"], 1);
        assert!(!scored.confusion.contains_key("starting"));
        assert!(scored.failures.is_empty());
        assert!(scored.disagreements.is_empty());
        let json = serde_json::to_value(&scored).unwrap();
        assert!(json.get("confusion").is_some());
        assert!(json.get("failures").is_some());
        assert!(json.get("observations").is_none());
        assert_eq!(json["stall_after_ms"], 20_000);
        assert_eq!(json["write_latency_ms"], 1_000);
    }

    #[test]
    fn running_tool_is_allowed_for_stall_after_and_a_second_of_slack() {
        let stall = 20_000u64;
        let samples = [
            at(0, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(stall, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(stall + 1_000, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(stall + 1_100, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
        ];
        let scored = score_of(&samples, false, &[]);
        assert!(scored.failures.iter().any(|f| f.contains("without AwaitingApproval")));
        let row = scored.disagreements.iter().find(|d| d.activity == "running_tool").unwrap();
        assert_eq!(row.samples, 1);
        assert_eq!(row.first_elapsed_ms, stall + 1_100);
    }

    #[test]
    fn approval_within_a_second_after_the_prompt_is_not_a_missed_prompt() {
        let samples = [
            at(0, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(22_000, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(22_500, ScreenState::Working, Some(Activity::AwaitingApproval)),
        ];
        let scored = score_of(&samples, false, &[]);
        assert!(
            scored
                .failures
                .iter()
                .all(|f| !f.contains("without AwaitingApproval") && !f.contains("while the screen is")),
            "{:?}",
            scored.failures
        );
        let late = [
            at(0, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(22_000, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(24_000, ScreenState::Working, Some(Activity::AwaitingApproval)),
        ];
        let scored = score_of(&late, false, &[]);
        assert!(scored.failures.iter().any(|f| f.contains("without AwaitingApproval")));
        assert!(scored.failures.iter().any(|f| f.contains("while the screen is working")));
    }

    #[test]
    fn approval_that_arrives_with_the_prompt_is_not_a_miss() {
        let mut samples = Vec::new();
        for t in (0..=20_000).step_by(1_000) {
            samples.push(at(t, ScreenState::PermissionPrompt, Some(Activity::RunningTool)));
        }
        samples.push(at(21_000, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)));
        samples.push(at(29_000, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)));
        let scored = score_of(&samples, false, &[]);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
    }

    #[test]
    fn a_later_prompt_is_not_excused_by_an_earlier_approval() {
        let samples = [
            at(0, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)),
            at(1_000, ScreenState::Idle, Some(Activity::Done)),
            at(2_000, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(24_000, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
        ];
        let scored = score_of(&samples, false, &[]);
        assert!(scored.failures.iter().any(|f| f.contains("for 22000 ms")));
    }

    #[test]
    fn awaiting_approval_within_a_second_of_the_prompt_is_allowed() {
        let early = [
            at(0, ScreenState::Working, Some(Activity::AwaitingApproval)),
            at(500, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)),
        ];
        assert!(score_of(&early, false, &[]).failures.is_empty());
        let late = [
            at(0, ScreenState::Working, Some(Activity::AwaitingApproval)),
            at(1_500, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)),
        ];
        let scored = score_of(&late, false, &[]);
        assert!(scored.failures.iter().any(|f| f.contains("while the screen is working")));
    }

    #[test]
    fn no_approval_and_bypass_modes_reject_awaiting_approval_on_a_prompt() {
        let samples = [at(0, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval))];
        assert!(score_of(&samples, true, &[]).failures.iter().any(|f| f.contains("no approval is possible")));
        let mut bypass = point(Activity::AwaitingApproval);
        bypass.permission = Some(PermissionMode::BypassPermissions);
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::PermissionPrompt.id(),
            background_agents: false,
            library: Some(bypass),
            ..Sample::default()
        }];
        assert!(score_of(&samples, false, &[]).failures.iter().any(|f| f.contains("no approval is possible")));
        let mut never = point(Activity::AwaitingApproval);
        never.permission = Some(PermissionMode::Never);
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::Working.id(),
            background_agents: false,
            library: Some(never),
            ..Sample::default()
        }];
        assert!(score_of(&samples, false, &[]).failures.iter().any(|f| f.contains("no approval is possible")));
    }

    #[test]
    fn awaiting_answer_is_allowed_in_bypass() {
        let mut library = point(Activity::AwaitingAnswer);
        library.permission = Some(PermissionMode::BypassPermissions);
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::AskUserQuestion.id(),
            background_agents: false,
            library: Some(library),
            ..Sample::default()
        }];
        assert!(score_of(&samples, true, &[]).failures.is_empty());
    }

    #[test]
    fn done_with_helpers_still_listed_is_a_hard_failure() {
        let mut library = point(Activity::Done);
        library.background = 1;
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::Idle.id(),
            background_agents: false,
            library: Some(library),
            ..Sample::default()
        }];
        let scored = score_of(&samples, false, &[]);
        assert_eq!(scored.failures, vec!["Done while background agents are running".to_string()]);
    }

    #[test]
    fn background_work_with_helpers_listed_is_not_a_failure() {
        let mut library = point(Activity::BackgroundWork);
        library.background = 2;
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::Working.id(),
            background_agents: true,
            library: Some(library),
            ..Sample::default()
        }];
        assert!(score_of(&samples, false, &[]).failures.is_empty());
    }

    #[test]
    fn done_while_the_screen_shows_agents_fails_only_past_write_latency() {
        let brief = [0u64, 200].map(|elapsed| Sample {
            elapsed_ms: elapsed,
            screen: ScreenState::Working.id(),
            background_agents: true,
            library: Some(point(Activity::Done)),
            ..Sample::default()
        });
        assert!(score_of(&brief, false, &[]).failures.is_empty(), "a sub-second overlap is write lag");
        let held = [0u64, 2_000].map(|elapsed| Sample {
            elapsed_ms: elapsed,
            screen: ScreenState::Working.id(),
            background_agents: true,
            library: Some(point(Activity::Done)),
            ..Sample::default()
        });
        assert!(score_of(&held, false, &[]).failures.iter().any(|f| f.contains("background agents")));
    }

    #[test]
    fn idle_mention_of_agents_does_not_fail_done() {
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::Idle.id(),
            background_agents: false,
            library: Some(point(Activity::Done)),
            ..Sample::default()
        }];
        assert!(score_of(&samples, false, &[]).failures.is_empty());
    }

    #[test]
    fn write_latency_hides_one_second_of_transition_and_not_five() {
        let edge = [
            at(0, ScreenState::Working, Some(Activity::Thinking)),
            at(1_000, ScreenState::Idle, Some(Activity::Thinking)),
        ];
        assert!(score_of(&edge, false, &[]).disagreements.is_empty());
        let held = [
            at(0, ScreenState::Working, Some(Activity::Thinking)),
            at(1_000, ScreenState::Idle, Some(Activity::Thinking)),
            at(6_000, ScreenState::Idle, Some(Activity::Thinking)),
        ];
        let scored = score_of(&held, false, &[]);
        assert!(scored.failures.is_empty());
        assert_eq!(scored.disagreements.len(), 1);
        assert_eq!(scored.disagreements[0].screen, "idle");
        assert_eq!(scored.disagreements[0].activity, "thinking");
        assert_eq!(scored.disagreements[0].samples, 1);
        assert_eq!(scored.disagreements[0].first_elapsed_ms, 6_000);
    }

    #[test]
    fn unknown_screens_do_not_excuse_a_real_mismatch() {
        let samples = [
            at(0, ScreenState::Unknown, Some(Activity::Thinking)),
            at(500, ScreenState::Idle, Some(Activity::Thinking)),
            at(2_000, ScreenState::Idle, Some(Activity::Thinking)),
        ];
        let scored = score_of(&samples, false, &[]);
        assert_eq!(scored.disagreements.len(), 1);
        assert_eq!(scored.disagreements[0].samples, 2);
        assert_eq!(scored.disagreements[0].first_elapsed_ms, 500);
        assert_eq!(scored.disagreements[0].last_elapsed_ms, 2_000);
    }

    #[test]
    fn expected_activity_checks_the_end_ever_and_the_screen() {
        let samples = [
            at(0, ScreenState::Working, Some(Activity::Thinking)),
            at(1_000, ScreenState::Error, Some(Activity::Errored)),
            at(3_000, ScreenState::Idle, Some(Activity::Done)),
        ];
        assert!(score_of(&samples, false, &[expect_end("done")]).failures.is_empty());
        let wrong = score_of(&samples, false, &[expect_end("thinking")]);
        assert!(wrong.failures.iter().any(|f| f.contains("saw done")));
        let ever = Expectation {
            activity: "errored".into(),
            when: ExpectWhen::Ever,
            screen: Some("error".into()),
            certainty: None,
        };
        assert!(score_of(&samples, false, &[ever]).failures.is_empty());
        let missing =
            Expectation { activity: "interrupted".into(), when: ExpectWhen::Ever, screen: None, certainty: None };
        assert!(score_of(&samples, false, &[missing]).failures.iter().any(|f| f.contains("never observed")));
        let screen = Expectation {
            activity: "done".into(),
            when: ExpectWhen::End,
            screen: Some("working".into()),
            certainty: None,
        };
        let scored = score_of(&samples, false, &[screen]);
        assert!(scored.failures.iter().any(|f| f.contains("on screen working")));
        let typo = expect_end("finished");
        assert!(score_of(&samples, false, &[typo]).failures.iter().any(|f| f.contains("not a library activity")));
        let none = [at(0, ScreenState::Idle, None)];
        assert!(score_of(&none, false, &[expect_end("done")]).failures.iter().any(|f| f.contains("never reported")));
    }

    #[test]
    fn expected_screen_allows_a_second_of_write_latency() {
        let samples =
            [at(0, ScreenState::Idle, Some(Activity::Done)), at(500, ScreenState::Working, Some(Activity::Done))];
        let expect = || Expectation {
            activity: "done".into(),
            when: ExpectWhen::End,
            screen: Some("idle".into()),
            certainty: None,
        };
        assert!(score_of(&samples, false, &[expect()]).failures.is_empty());
        let earlier =
            [at(0, ScreenState::Idle, Some(Activity::Thinking)), at(500, ScreenState::Working, Some(Activity::Done))];
        let scored = score_of(&earlier, false, &[expect()]);
        assert!(scored.failures.iter().any(|f| f.contains("on screen idle")), "{:?}", scored.failures);
    }

    #[test]
    fn dont_ask_rejects_awaiting_approval() {
        let mut library = point(Activity::AwaitingApproval);
        library.permission = Some(PermissionMode::DontAsk);
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::PermissionPrompt.id(),
            background_agents: false,
            library: Some(library),
            ..Sample::default()
        }];
        assert!(score_of(&samples, false, &[]).failures.iter().any(|f| f.contains("no approval is possible")));
    }

    #[test]
    fn background_done_overlap_fails_just_past_one_second() {
        let at_ms = |elapsed| Sample {
            elapsed_ms: elapsed,
            screen: ScreenState::Working.id(),
            background_agents: true,
            library: Some(point(Activity::Done)),
            ..Sample::default()
        };
        assert!(score_of(&[at_ms(0), at_ms(1_000)], false, &[]).failures.is_empty());
        assert!(score_of(&[at_ms(0), at_ms(1_001)], false, &[]).failures.iter().any(|f| f.contains("background")));
    }

    #[test]
    fn the_four_green_shapes_stay_clean() {
        let plain = [
            at(0, ScreenState::Starting, None),
            at(1_000, ScreenState::Working, Some(Activity::Thinking)),
            at(4_000, ScreenState::Idle, Some(Activity::Done)),
            at(9_000, ScreenState::Idle, Some(Activity::Done)),
        ];
        assert!(score_of(&plain, false, &[]).failures.is_empty(), "plain qa");

        let approval = vec![
            at(0, ScreenState::Working, Some(Activity::Thinking)),
            at(2_000, ScreenState::PermissionPrompt, Some(Activity::RunningTool)),
            at(4_000, ScreenState::Working, Some(Activity::RunningTool)),
            at(8_000, ScreenState::Idle, Some(Activity::Done)),
        ];
        assert!(score_of(&approval, false, &[]).failures.is_empty(), "approval");

        let mut pending = Vec::new();
        for t in (0..=20_000).step_by(1_000) {
            pending.push(at(t, ScreenState::PermissionPrompt, Some(Activity::RunningTool)));
        }
        pending.push(at(21_000, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)));
        pending.push(at(29_000, ScreenState::PermissionPrompt, Some(Activity::AwaitingApproval)));
        assert!(score_of(&pending, false, &[]).failures.is_empty(), "pending");

        let bypass = [
            at(0, ScreenState::Working, Some(Activity::RunningTool)),
            at(20_000, ScreenState::Working, Some(Activity::RunningTool)),
            at(24_000, ScreenState::Working, Some(Activity::RunningTool)),
        ];
        assert!(score_of(&bypass, true, &[]).failures.is_empty(), "bypass");
    }

    #[test]
    fn expected_activity_json_accepts_a_string_a_spec_and_a_list() {
        let one: Vec<ExpectedOne> = deserialize_expected(&mut serde_json::Deserializer::from_str("\"done\"")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].expectation().activity, "done");
        assert_eq!(one[0].expectation().when, ExpectWhen::End);

        let spec: Vec<ExpectedOne> = deserialize_expected(&mut serde_json::Deserializer::from_str(
            r#"{"activity":"interrupted","when":"ever","screen":"idle"}"#,
        ))
        .unwrap();
        let expectation = spec[0].expectation();
        assert_eq!(expectation.activity, "interrupted");
        assert_eq!(expectation.when, ExpectWhen::Ever);
        assert_eq!(expectation.screen.as_deref(), Some("idle"));

        let list: Vec<ExpectedOne> = deserialize_expected(&mut serde_json::Deserializer::from_str(
            r#"["done", {"activity":"errored","when":"ever"}]"#,
        ))
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].expectation().when, ExpectWhen::Ever);
        assert!(list[0].expectation().certainty.is_none());

        let firm: Vec<ExpectedOne> = deserialize_expected(&mut serde_json::Deserializer::from_str(
            r#"{"activity":"awaiting_answer","when":"ever","screen":"ask_user_question","certainty":"firm"}"#,
        ))
        .unwrap();
        let expectation = firm[0].expectation();
        assert_eq!(expectation.certainty.as_deref(), Some("firm"));
        assert_eq!(expectation.screen.as_deref(), Some("ask_user_question"));
    }

    #[test]
    fn firm_awaiting_answer_on_an_ask_screen_is_required_when_asked_for() {
        let mut samples = Vec::new();
        for t in (0..=30_000).step_by(1_000) {
            samples.push(at(t, ScreenState::AskUserQuestion, Some(Activity::AwaitingAnswer)));
        }
        samples.push(at(34_000, ScreenState::Idle, Some(Activity::Done)));
        let expect = [
            Expectation {
                activity: "awaiting_answer".into(),
                when: ExpectWhen::Ever,
                screen: Some("ask_user_question".into()),
                certainty: Some("firm".into()),
            },
            expect_end("done"),
        ];
        assert!(score_of(&samples, false, &expect).failures.is_empty(), "ask stays firm");

        let mut inferred = point(Activity::AwaitingAnswer);
        inferred.certainty = Certainty::Inferred;
        let wrong = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::AskUserQuestion.id(),
            background_agents: false,
            library: Some(inferred),
            ..Sample::default()
        }];
        let only_firm = Expectation {
            activity: "awaiting_answer".into(),
            when: ExpectWhen::Ever,
            screen: Some("ask_user_question".into()),
            certainty: Some("firm".into()),
        };
        let scored = score_of(&wrong, false, &[only_firm]);
        assert!(scored.failures.iter().any(|f| f.contains("firm")), "{:?}", scored.failures);

        let typo = Expectation {
            activity: "awaiting_answer".into(),
            when: ExpectWhen::End,
            screen: None,
            certainty: Some("certain".into()),
        };
        assert!(score_of(&samples, false, &[typo]).failures.iter().any(|f| f.contains("not firm or inferred")));
    }

    #[test]
    fn min_screen_time_counts_only_the_named_screen() {
        let mut samples = Vec::new();
        for t in (5_000..=35_000).step_by(1_000) {
            samples.push(at(t, ScreenState::AskUserQuestion, Some(Activity::AwaitingAnswer)));
        }
        let mut minimums = BTreeMap::new();
        minimums.insert("ask_user_question".into(), 25_000);
        let scored = score(&ScoreRequest {
            scenario: "ask",
            description: None,
            harness: "claude_code",
            no_approval: false,
            expected: &[],
            samples: &samples,
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &minimums,
            transcripts: &[],
            helpers: &[],
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: None,
        });
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);

        minimums.insert("ask_user_question".into(), 40_000);
        let scored = score(&ScoreRequest {
            scenario: "ask",
            description: None,
            harness: "claude_code",
            no_approval: false,
            expected: &[],
            samples: &samples,
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &minimums,
            transcripts: &[],
            helpers: &[],
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: None,
        });
        assert!(scored.failures.iter().any(|f| f.contains("expected at least 40000")), "{:?}", scored.failures);

        let flicker = [
            at(0, ScreenState::AskUserQuestion, Some(Activity::AwaitingAnswer)),
            at(100, ScreenState::Working, Some(Activity::Thinking)),
            at(30_000, ScreenState::AskUserQuestion, Some(Activity::AwaitingAnswer)),
        ];
        minimums.insert("ask_user_question".into(), 1_000);
        let scored = score(&ScoreRequest {
            scenario: "ask",
            description: None,
            harness: "claude_code",
            no_approval: false,
            expected: &[],
            samples: &flicker,
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &minimums,
            transcripts: &[],
            helpers: &[],
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: None,
        });
        assert!(scored.failures.iter().any(|f| f.contains("for 0 ms")), "{:?}", scored.failures);
    }

    #[test]
    fn a_rejection_ends_the_turn_only_when_the_model_does_not_continue() {
        let local_end = r#"
{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_2","name":"Bash"}]}}
{"type":"user","permissionMode":"default","toolUseResult":"User rejected tool use","toolDenialKind":"user-rejected","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"The user doesn't want to proceed with this tool use. The tool use was rejected."}]}}
{"type":"system","subtype":"turn_duration"}
{"type":"user","message":{"content":"next prompt"}}
{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"later"}]}}
"#;
        let observed = observe_transcript(local_end);
        assert_eq!(observed.permission_modes, vec!["default".to_string()]);
        assert_eq!(observed.tool_rejections.len(), 1);
        let rejection = &observed.tool_rejections[0];
        assert_eq!(rejection.tool_name.as_deref(), Some("Bash"));
        assert_eq!(rejection.tool_use_result.as_deref(), Some("User rejected tool use"));
        assert!(rejection.firm_turn_end_before_next_prompt);
        assert!(!rejection.assistant_continued);
        assert!(rejection.ends_turn);
        assert_eq!(rejection.following, vec!["system:turn_duration".to_string()]);
        assert!(rejection.result_text.as_deref().unwrap().contains("doesn't want to proceed"));

        let interrupt = r#"
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Bash"}]}}
{"type":"user","toolDenialKind":"user-rejected","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":[{"type":"text","text":"User rejected tool use"}]}]}}
{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}
"#;
        let observed = observe_transcript(interrupt);
        assert!(observed.tool_rejections[0].interrupt_before_next_prompt);
        assert!(observed.tool_rejections[0].ends_turn);
        assert!(!observed.tool_rejections[0].assistant_continued);

        let interrupt_first = r#"
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Bash"}]}}
{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}
{"type":"user","toolUseResult":"User rejected tool use","toolDenialKind":"user-rejected","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"The user doesn't want to proceed with this tool use."}]}}
"#;
        let observed = observe_transcript(interrupt_first);
        assert!(observed.tool_rejections[0].interrupt_before_next_prompt);
        assert!(observed.tool_rejections[0].ends_turn, "{:?}", observed.tool_rejections[0]);
        assert!(observed.tool_rejections[0].following.is_empty());

        let buried = r#"
{"type":"user","message":{"content":"[Request interrupted by user]"}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"Bash"}]}}
{"type":"user","toolUseResult":"User rejected tool use","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_9","content":"no"}]}}
"#;
        let observed = observe_transcript(buried);
        assert!(!observed.tool_rejections[0].interrupt_before_next_prompt);
        assert!(!observed.tool_rejections[0].ends_turn);

        let continued = r#"
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Bash"}]}}
{"type":"user","toolUseResult":"User rejected tool use","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"no"}]}}
{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Stopped."}]}}
{"type":"system","subtype":"turn_duration"}
"#;
        let observed = observe_transcript(continued);
        assert!(observed.tool_rejections[0].assistant_continued);
        assert!(observed.tool_rejections[0].firm_turn_end_before_next_prompt);
        assert!(!observed.tool_rejections[0].ends_turn);

        let quoted = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"The notes mention User rejected tool use."}]}}"#;
        assert!(observe_transcript(quoted).tool_rejections.is_empty());

        let auto = r#"
{"type":"permission-mode","permissionMode":"acceptEdits"}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_w","name":"Write"}]}}
{"type":"user","permissionMode":"plan","permissionDecision":{"decision":"accept","source":"config","reasonType":"mode"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_w","content":"ok"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_b","name":"Bash"}]}}
{"type":"user","permissionDecision":{"decision":"accept","source":"user_temporary","reasonType":"temporary"},"toolDenialKind":"permission-rule","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_b","content":"created"}]}}
not json
"#;
        let observed = observe_transcript(auto);
        assert_eq!(observed.permission_modes, vec!["acceptEdits".to_string(), "plan".to_string()]);
        assert!(observed.tool_rejections.is_empty());
        assert_eq!(observed.tool_permissions.len(), 2);
        assert_eq!(observed.tool_permissions[0].tool_name.as_deref(), Some("Write"));
        assert_eq!(observed.tool_permissions[0].source.as_deref(), Some("config"));
        assert_eq!(observed.tool_permissions[1].tool_name.as_deref(), Some("Bash"));
        assert_eq!(observed.tool_permissions[1].source.as_deref(), Some("user_temporary"));
        assert_eq!(observed.tool_permissions[1].denial_kind.as_deref(), Some("permission-rule"));

        let minimums = BTreeMap::new();
        let dumps = [TranscriptDump { name: "session.jsonl".into(), jsonl: local_end.to_string() }];
        let scored = score(&ScoreRequest {
            scenario: "denied",
            description: None,
            harness: "claude_code",
            no_approval: false,
            expected: &[],
            samples: &[],
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &minimums,
            transcripts: &dumps,
            helpers: &[],
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: None,
        });
        assert!(scored.observations.tool_rejections[0].ends_turn);
        let json = serde_json::to_value(&scored).unwrap();
        assert!(json["observations"]["tool_rejections"][0]["ends_turn"].as_bool().unwrap());
    }

    #[test]
    fn interrupts_record_whether_turn_duration_follows() {
        let bare = r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#;
        let noted = observe_transcript(bare);
        assert_eq!(noted.interrupts.len(), 1);
        assert!(!noted.interrupts[0].turn_duration_before_next_prompt);
        assert!(noted.api_errors.is_empty());
        assert!(noted.background.is_none());

        let closed = r#"
{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}
{"type":"system","subtype":"turn_duration"}
{"type":"user","message":{"content":"next"}}
"#;
        let noted = observe_transcript(closed);
        assert!(noted.interrupts[0].turn_duration_before_next_prompt);
        assert!(noted.interrupts[0].text.contains("for tool use"));
    }

    #[test]
    fn api_errors_keep_the_synthetic_row_and_a_following_duration() {
        let rows = r#"
{"type":"assistant","isApiErrorMessage":true,"error":"server_error","message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"API Error: 529"}]}}
{"type":"system","subtype":"turn_duration"}
"#;
        let noted = observe_transcript(rows);
        assert_eq!(noted.api_errors.len(), 1);
        assert_eq!(noted.api_errors[0].error, "server_error");
        assert!(noted.api_errors[0].is_api_error_message);
        assert!(noted.api_errors[0].turn_duration_follows);
    }

    #[test]
    fn background_notes_cover_the_count_the_notification_and_nested_helpers() {
        let rows = r#"
{"type":"user","toolUseResult":{"status":"async_launched","agentId":"agent-outer"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"launched"}]}}
{"type":"system","subtype":"turn_duration","pendingBackgroundAgentCount":1}
{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"<task-notification><status>completed</status></task-notification>"}}
"#;
        let helpers = [ObservedHelper {
            id: "agent-inner".into(),
            meta_json: Some(
                r#"{"agentType":"general-purpose","spawnDepth":2,"parentAgentId":"agent-outer","finished":true,"stoppedByUser":false}"#
                    .into(),
            ),
            jsonl: Some(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Agent","input":{}}]}}"#.into(),
            ),
        }];
        let dumps = [TranscriptDump { name: "session.jsonl".into(), jsonl: rows.to_string() }];
        let noted = observe_run(&ScoreRequest {
            scenario: "bg",
            description: None,
            harness: "claude_code",
            no_approval: false,
            expected: &[],
            samples: &[],
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &BTreeMap::new(),
            transcripts: &dumps,
            helpers: &helpers,
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: None,
        });
        let background = noted.background.expect("background");
        assert_eq!(background.pending_counts, vec![1]);
        assert_eq!(background.task_statuses, vec!["completed".to_string()]);
        assert_eq!(background.async_launched, vec!["agent-outer".to_string()]);
        assert_eq!(background.helpers.len(), 1);
        let helper = &background.helpers[0];
        assert_eq!(helper.spawn_depth, Some(2));
        assert_eq!(helper.parent_agent_id.as_deref(), Some("agent-outer"));
        assert_eq!(helper.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(helper.finished, Some(true));
        assert_eq!(helper.stopped_by_user, Some(false));
        assert!(helper.launched_agent);
        let string_result = r#"{"type":"user","toolUseResult":"status async_launched"}"#;
        assert_eq!(observe_transcript(string_result).background.unwrap().async_launched, vec!["unknown".to_string()]);
    }

    fn dump(name: &str, jsonl: &str) -> TranscriptDump {
        TranscriptDump { name: name.into(), jsonl: jsonl.into() }
    }

    fn score_dumps(
        dumps: &[TranscriptDump],
        expect: Option<&ObserveExpect>,
        registry: &[RegistrySample],
        files_after: Option<usize>,
        registry_expect: Option<&RegistryExpect>,
    ) -> Score {
        let minimums = BTreeMap::new();
        score(&ScoreRequest {
            scenario: "obs",
            description: None,
            harness: "claude_code",
            no_approval: false,
            expected: &[],
            samples: &[],
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &minimums,
            transcripts: dumps,
            helpers: &[],
            registry,
            registry_fields: &[],
            registry_files_after_exit: files_after,
            expect_registry: registry_expect,
            expect_observations: expect,
        })
    }

    #[test]
    fn compaction_continues_in_the_same_file_and_a_summary_is_not_a_later_prompt() {
        let rows = r#"
{"type":"user","sessionId":"s","message":{"content":"remember COMPACT-TOKEN-ALPHA"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"noted"}]}}
{"type":"system","subtype":"compact_boundary","sessionId":"s","compactMetadata":{"trigger":"manual","preTokens":1200,"postTokens":80}}
{"type":"user","isCompactSummary":true,"sessionId":"s","message":{"content":"summary says continued-in but is not a record"}}
{"type":"user","sessionId":"s","message":{"content":"what token"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"alpha"}]}}
"#;
        let dumps = [dump("session.jsonl", rows)];
        let expect = ObserveExpect {
            compaction_triggers: vec!["manual".into()],
            compaction_same_file: Some(true),
            ..ObserveExpect::default()
        };
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        assert_eq!(scored.observations.compactions[0].pre_tokens, Some(1200));
        assert!(scored.observations.compactions[0].summary_follows);
        assert!(scored.observations.compactions[0].continued_after);
        assert!(scored.observations.continuations.is_empty());
        assert!(scored.observations.resumes.iter().any(|note| note.files == 1 && note.user_prompts >= 2));

        let cut = rows.rsplit_once(r#"{"type":"user","sessionId":"s","message":{"content":"what token"}}"#).unwrap().0;
        let dumps = [dump("session.jsonl", cut)];
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.iter().any(|failure| failure.contains("same file")), "{:?}", scored.failures);
        let bookkeeping = format!(
            "{cut}{{\"type\":\"system\",\"subtype\":\"turn_duration\",\"sessionId\":\"s\"}}\n{{\"type\":\"user\",\"sessionId\":\"s\",\"message\":{{\"content\":\"<local-command-stdout>Compacted</local-command-stdout>\"}}}}\n"
        );
        let dumps = [dump("session.jsonl", &bookkeeping)];
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.iter().any(|failure| failure.contains("same file")), "{:?}", scored.failures);
    }

    #[test]
    fn continued_in_needs_the_other_file_and_resume_can_stay_in_one() {
        let original = r#"
{"type":"user","sessionId":"aaa","message":{"content":"remember RESUME-WORD-GAMMA"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}
{"type":"continued-in","sessionId":"aaa","continuedInSessionId":"bbb"}
"#;
        let next = r#"{"type":"user","sessionId":"bbb","message":{"content":"what word"}}
{"type":"assistant","message":{"model":"claude-haiku-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"gamma"}]}}
"#;
        let dumps = [dump("aaa.jsonl", original), dump("bbb.jsonl", next)];
        let expect = ObserveExpect { continued_in: Some(true), model_switch: Some(true), ..ObserveExpect::default() };
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        assert!(scored.observations.continuations[0].target_file_present);
        assert_eq!(scored.observations.models, vec!["claude-sonnet-4-5".to_string(), "claude-haiku-4-5".to_string()]);

        let dumps = [dump("aaa.jsonl", original)];
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.iter().any(|failure| failure.contains("target file")), "{:?}", scored.failures);

        let same = r#"
{"type":"user","sessionId":"aaa","permissionMode":"default","message":{"content":"remember RESUME-WORD-GAMMA"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}
{"type":"user","sessionId":"aaa","permissionMode":"acceptEdits","message":{"content":"what word"}}
{"type":"assistant","message":{"model":"claude-sonnet-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"gamma"}]}}
"#;
        let dumps = [dump("aaa.jsonl", same)];
        let expect = ObserveExpect {
            continued_in: Some(false),
            same_file_resume: Some(true),
            permission_modes: vec!["default".into(), "acceptEdits".into()],
            model_switch: Some(false),
            ..ObserveExpect::default()
        };
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        assert!(scored.observations.models.is_empty());

        let expect = ObserveExpect { same_file_resume: Some(true), ..ObserveExpect::default() };
        let scored = score_dumps(&dumps[..1], Some(&expect), &[], None, None);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        let split = [dump("aaa.jsonl", original), dump("bbb.jsonl", next)];
        let scored = score_dumps(&split, Some(&expect), &[], None, None);
        assert!(scored.failures.iter().any(|failure| failure.contains("same file")), "{:?}", scored.failures);
    }

    #[test]
    fn non_ascii_is_recorded_on_prompts_titles_and_paths_only() {
        let rows = r#"
{"type":"user","message":{"content":"read notes/café-📝.txt and reply こんにちは 🎉"}}
{"type":"ai-title","aiTitle":"絵文字タイトル 🎉"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/work/notes/café-📝.txt"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"bonjour café"}]}}
"#;
        let dumps = [dump("session.jsonl", rows)];
        let expect = ObserveExpect {
            non_ascii_prompt: Some(true),
            non_ascii_title: Some(true),
            non_ascii_path: Some(true),
            ..ObserveExpect::default()
        };
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        let tool_only = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"こんにちは"}]}}"#;
        assert!(observe_transcript(tool_only).text.is_none());
        assert!(observe_transcript(r#"{"type":"user","message":{"content":"ascii only"}}"#).text.is_none());
    }

    #[test]
    fn a_long_tool_result_keeps_the_same_activity_when_leaned() {
        let long = "Z".repeat(20_000);
        let rows = format!(
            r#"{{"type":"user","timestamp":"2026-10-24T00:00:00Z","message":{{"content":"long run"}}}}
{{"type":"assistant","timestamp":"2026-10-24T00:00:01Z","message":{{"id":"m1","model":"claude-sonnet-4-5","stop_reason":"tool_use","content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"head"}}}}]}}}}
{{"type":"user","timestamp":"2026-10-24T00:00:02Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"{long}"}}]}}}}
{{"type":"assistant","timestamp":"2026-10-24T00:00:03Z","message":{{"id":"m2","model":"claude-sonnet-4-5","stop_reason":"end_turn","content":[{{"type":"text","text":"done"}}]}}}}
"#
        );
        let dumps = [dump("session.jsonl", &rows)];
        let expect =
            ObserveExpect { lean_agrees: Some(true), min_tool_result_bytes: Some(20_000), ..ObserveExpect::default() };
        let scored = score_dumps(&dumps, Some(&expect), &[], None, None);
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        let lean = scored.observations.lean.expect("lean");
        assert!(lean.max_line_bytes > 16 * 1024);
        assert_eq!(lean.lean_activity, "done");
        assert_eq!(lean.full_activity, "done");
        let short = dump("session.jsonl", r#"{"type":"user","message":{"content":"short"}}"#);
        let scored = score_dumps(std::slice::from_ref(&short), Some(&expect), &[], None, None);
        assert!(scored.failures.iter().any(|failure| failure.contains("not long enough")), "{:?}", scored.failures);
    }

    #[test]
    fn the_registry_records_waiting_on_a_permission_prompt_and_removal() {
        let samples = [
            RegistrySample {
                elapsed_ms: 0,
                screen: "idle".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("idle".into()),
                waiting_for: None,
            },
            RegistrySample {
                elapsed_ms: 1_000,
                screen: "working".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("busy".into()),
                waiting_for: None,
            },
            RegistrySample {
                elapsed_ms: 2_000,
                screen: "permission_prompt".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("waiting".into()),
                waiting_for: Some("permission prompt".into()),
            },
            RegistrySample {
                elapsed_ms: 2_100,
                screen: "permission_prompt".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("waiting".into()),
                waiting_for: Some("permission prompt".into()),
            },
            RegistrySample {
                elapsed_ms: 8_000,
                screen: "idle".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("idle".into()),
                waiting_for: None,
            },
            RegistrySample {
                elapsed_ms: 8_100,
                screen: "idle".into(),
                pid: None,
                session_id: None,
                status: None,
                waiting_for: None,
            },
        ];
        let expect = RegistryExpect {
            statuses: vec!["busy".into(), "idle".into(), "waiting".into()],
            waiting_for: Some("permission prompt".into()),
            waiting_during_screen: Some("permission_prompt".into()),
            removed_on_exit: Some(true),
        };
        let scored = score_dumps(&[], None, &samples, Some(0), Some(&expect));
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        let sessions = scored.observations.sessions.expect("sessions");
        assert_eq!(sessions.statuses, vec!["idle".to_string(), "busy".to_string(), "waiting".to_string()]);
        assert_eq!(sessions.removed_on_exit, Some(true));
        assert!(sessions.runs.iter().any(|run| run.status == "waiting" && run.samples == 2));

        let busy = [
            RegistrySample {
                elapsed_ms: 2_000,
                screen: "permission_prompt".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("busy".into()),
                waiting_for: None,
            },
            RegistrySample {
                elapsed_ms: 3_000,
                screen: "idle".into(),
                pid: Some(4),
                session_id: Some("s".into()),
                status: Some("idle".into()),
                waiting_for: None,
            },
        ];
        let scored = score_dumps(&[], None, &busy, Some(1), Some(&expect));
        assert!(scored.failures.iter().any(|failure| failure.contains("was not waiting")), "{:?}", scored.failures);
        assert!(scored.failures.iter().any(|failure| failure.contains("still present")), "{:?}", scored.failures);
        assert!(scored.failures.iter().any(|failure| failure.contains("waitingFor")), "{:?}", scored.failures);
    }
    #[test]
    fn codex_observations_cover_policies_events_tokens_parallelism_and_titles() {
        let rows = r#"
{"type":"session_meta","payload":{"id":"codex-session","session_id":"codex-session","cwd":"/w/app"}}
{"type":"turn_context","payload":{"approval_policy":"on-request","sandbox_policy":{"type":"workspace-write"}}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{}"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c2","arguments":"{}"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c3","arguments":"{}"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"codex-session","item":{"type":"UserMessage","content":[{"type":"text","text":"first"}]}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"total_tokens":15}}}}
{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","duration_ms":2000}}
{"type":"event_msg","payload":{"type":"item_completed","thread_id":"codex-session","item":{"type":"UserMessage","content":[{"type":"text","text":"second"}]}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30,"cached_input_tokens":2,"output_tokens":12,"total_tokens":42}}}}
{"type":"event_msg","payload":{"type":"task_complete","error":{"message":"high demand","codex_error_info":"internal_server_error"}}}
"#;
        let index = r#"{"id":"codex-session","thread_name":"Codex title","updated_at":"2026-10-24T00:00:00Z"}"#;
        let dumps = [dump("rollout.jsonl", rows), dump("session_index.jsonl", index)];
        let expect = ObserveExpect {
            codex_approval_policies: vec!["on-request".into()],
            codex_sandbox_policies: vec!["workspace-write".into()],
            codex_no_approval_events: Some(true),
            codex_turn_abort_reasons: vec!["interrupted".into()],
            codex_task_error_contains: vec!["high demand".into()],
            codex_token_output_totals: vec![5, 12],
            codex_same_file_resume: Some(true),
            codex_min_pending_function_calls: Some(3),
            codex_session_index_titles: vec!["Codex title".into()],
            ..ObserveExpect::default()
        };
        let scored = score(&ScoreRequest {
            scenario: "codex",
            description: None,
            harness: "codex",
            no_approval: false,
            expected: &[],
            samples: &[],
            mock_requests: 0,
            remaining_mock_steps: 0,
            transcript: None,
            min_screen_ms: &BTreeMap::new(),
            transcripts: &dumps,
            helpers: &[],
            registry: &[],
            registry_fields: &[],
            registry_files_after_exit: None,
            expect_registry: None,
            expect_observations: Some(&expect),
        });
        assert!(scored.failures.is_empty(), "{:?}", scored.failures);
        let codex = scored.observations.codex.expect("codex observations");
        assert!(codex.approval_events.is_empty());
        assert_eq!(codex.max_pending_function_calls, 3);
        assert_eq!(codex.turn_aborts[0].reason.as_deref(), Some("interrupted"));
        assert_eq!(codex.task_errors, vec!["high demand".to_string()]);
        assert_eq!(codex.token_totals[1].output_tokens, Some(12));
        assert_eq!(codex.threads[0].files, 1);
        assert_eq!(codex.threads[0].prompts, 2);
        assert_eq!(codex.session_index[0].thread_name, "Codex title");
    }
}
