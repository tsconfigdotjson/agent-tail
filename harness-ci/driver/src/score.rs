use crate::screen::ScreenState;
use agenttail::{Activity, Certainty, PermissionMode, Reason, ReducerOptions, SessionState, Timestamp};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    pub transcript_jsonl: Option<&'a str>,
    pub helpers: &'a [ObservedHelper],
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
        observations: observe_run(request.transcript_jsonl, request.helpers),
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
            } else if !near_screen(request.samples, sample.elapsed_ms, PERMISSION, write_ms) {
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
}

impl Observations {
    pub fn is_empty(&self) -> bool {
        self.permission_modes.is_empty()
            && self.tool_permissions.is_empty()
            && self.tool_rejections.is_empty()
            && self.interrupts.is_empty()
            && self.api_errors.is_empty()
            && self.background.is_none()
    }
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
    observe_run(Some(jsonl), &[])
}

fn observe_run(jsonl: Option<&str>, helpers: &[ObservedHelper]) -> Observations {
    let records = jsonl.map(jsonl_records).unwrap_or_default();
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
    observations.background = background_note(&records, helpers);
    observations
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
    if record.get("type").and_then(Value::as_str) != Some("user") || record.get("isMeta") == Some(&Value::Bool(true)) {
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
    if kind == "system" {
        if let Some(subtype) = record.get("subtype").and_then(Value::as_str) {
            return format!("system:{subtype}");
        }
    }
    if kind == "assistant" {
        if let Some(stop) = record.get("message").and_then(|message| message.get("stop_reason")).and_then(Value::as_str)
        {
            return format!("assistant:{stop}");
        }
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
        Sample { elapsed_ms, screen: screen.id(), background_agents: false, library: activity.map(point) }
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
            transcript_jsonl: None,
            helpers: &[],
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
        }];
        assert!(score_of(&samples, false, &[]).failures.iter().any(|f| f.contains("no approval is possible")));
        let mut never = point(Activity::AwaitingApproval);
        never.permission = Some(PermissionMode::Never);
        let samples = [Sample {
            elapsed_ms: 0,
            screen: ScreenState::Working.id(),
            background_agents: false,
            library: Some(never),
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
        });
        assert!(score_of(&brief, false, &[]).failures.is_empty(), "a sub-second overlap is write lag");
        let held = [0u64, 2_000].map(|elapsed| Sample {
            elapsed_ms: elapsed,
            screen: ScreenState::Working.id(),
            background_agents: true,
            library: Some(point(Activity::Done)),
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
            transcript_jsonl: None,
            helpers: &[],
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
            transcript_jsonl: None,
            helpers: &[],
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
            transcript_jsonl: None,
            helpers: &[],
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
            transcript_jsonl: Some(local_end),
            helpers: &[],
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
        let noted = observe_run(Some(rows), &helpers);
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
}
