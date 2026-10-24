use crate::screen::ScreenState;
use agenttail::{Activity, Certainty, PermissionMode, Reason, ReducerOptions, SessionState, Timestamp};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
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
}

impl ExpectedOne {
    pub fn expectation(&self) -> Expectation {
        match self {
            ExpectedOne::Name(activity) => {
                Expectation { activity: activity.clone(), when: ExpectWhen::End, screen: None }
            }
            ExpectedOne::Spec(spec) => Expectation {
                activity: spec.activity.clone(),
                when: spec.when,
                screen: spec.screen.map(|state| state.id().to_string()),
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
}

fn approval_impossible(no_approval: bool, permission: Option<PermissionMode>) -> bool {
    no_approval || permission.is_some_and(|mode| !mode.can_ask_human())
}

fn near_screen(samples: &[Sample], elapsed_ms: u64, screen: &str, write_ms: u64) -> bool {
    samples.iter().any(|sample| sample.screen == screen && sample.elapsed_ms.abs_diff(elapsed_ms) <= write_ms)
}

fn screen_while_activity(samples: &[Sample], elapsed_ms: u64, screen: &str, activity: &str, write_ms: u64) -> bool {
    samples.iter().any(|sample| {
        sample.screen == screen
            && sample.elapsed_ms.abs_diff(elapsed_ms) <= write_ms
            && sample.library.as_ref().is_none_or(|library| activity_id(library.activity) == activity)
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
        match expect.when {
            ExpectWhen::End => {
                let Some(index) = samples.iter().rposition(|sample| sample.library.is_some()) else {
                    failures
                        .push(format!("expected {} at end, but the library never reported a state", expect.activity));
                    continue;
                };
                let sample = &samples[index];
                let actual = activity_id(sample.library.as_ref().expect("library").activity);
                if actual != expect.activity {
                    failures.push(format!("expected {} at end, saw {actual}", expect.activity));
                    continue;
                }
                if let Some(screen) = &expect.screen
                    && !screen_while_activity(samples, sample.elapsed_ms, screen, &expect.activity, write_ms)
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
                        sample.library.as_ref().is_some_and(|library| activity_id(library.activity) == expect.activity)
                    })
                    .map(|(index, _)| index)
                    .collect();
                if hits.is_empty() {
                    failures.push(format!("expected {}, never observed", expect.activity));
                    continue;
                }
                if let Some(screen) = &expect.screen
                    && !hits.iter().any(|&index| {
                        screen_while_activity(samples, samples[index].elapsed_ms, screen, &expect.activity, write_ms)
                    })
                {
                    failures.push(format!("expected {} on screen {screen}, never observed together", expect.activity));
                }
            }
        }
    }
    failures
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
        })
    }

    fn expect_end(activity: &str) -> Expectation {
        Expectation { activity: activity.into(), when: ExpectWhen::End, screen: None }
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
        let ever = Expectation { activity: "errored".into(), when: ExpectWhen::Ever, screen: Some("error".into()) };
        assert!(score_of(&samples, false, &[ever]).failures.is_empty());
        let missing = Expectation { activity: "interrupted".into(), when: ExpectWhen::Ever, screen: None };
        assert!(score_of(&samples, false, &[missing]).failures.iter().any(|f| f.contains("never observed")));
        let screen = Expectation { activity: "done".into(), when: ExpectWhen::End, screen: Some("working".into()) };
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
        let expect = || Expectation { activity: "done".into(), when: ExpectWhen::End, screen: Some("idle".into()) };
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
    }
}
