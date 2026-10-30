//! Usage/rate-limit stops are firm resting states with a reason, and the next turn clears it.

use agent_tail::{
    Activity, Certainty, ChunkCtx, Harness, Reason, ReducerOptions, SessionReducer, StopReason, Timestamp,
};
use serde_json::{Value, json};
use std::path::Path;

const NOW: &str = "2026-10-23T10:00:10Z";

fn state(harness: Harness, lines: &[Value]) -> agent_tail::SessionState {
    let bytes = lines.iter().map(|line| format!("{line}\n")).collect::<String>();
    let now = Timestamp::parse_rfc3339(NOW).unwrap();
    let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
    reducer.feed(bytes.as_bytes(), ChunkCtx { offset: 0, mtime: now, path: Path::new("session.jsonl") }).unwrap();
    reducer.state(now)
}

#[test]
fn claude_usage_limit_is_firm_and_records_its_reset() {
    let state = state(
        Harness::ClaudeCode,
        &[json!({
            "type": "assistant",
            "timestamp": "2026-10-23T10:00:05Z",
            "isApiErrorMessage": true,
            "error": "rate_limit",
            "apiError": "usage_limit_reached",
            "apiErrorParams": {"rate_limit_info": {"resetsAt": 1793159400}},
            "message": {"id": "m1", "stop_reason": "stop_sequence",
                        "content": [{"type": "text", "text": "You've hit your 5-hour limit · resets 3:50am (UTC)"}]}
        })],
    );
    assert_eq!((state.activity, state.certainty, state.reason), (Activity::Errored, Certainty::Firm, Reason::Errored));
    assert_eq!(state.stop_reason, Some(StopReason::UsageLimit));
    assert_eq!(state.resets_at, Some(Timestamp::from_secs_f64(1_793_159_400.0)));
}

#[test]
fn each_cli_harness_recognizes_its_own_limit_record() {
    let cases = [
        (
            Harness::Codex,
            json!({"timestamp": "2026-10-23T10:00:05Z", "type": "event_msg",
                   "payload": {"type": "task_complete",
                               "error": {"message": "exceeded retry limit, last status: 429 Too Many Requests"}}}),
        ),
        (
            Harness::Pi,
            json!({"type": "message", "timestamp": "2026-10-23T10:00:05Z",
                   "message": {"role": "assistant", "content": [], "stopReason": "error",
                               "errorMessage": "429 You've hit your 5-hour limit · resets 3:50am (UTC)"}}),
        ),
        (
            Harness::Copilot,
            json!({"type": "session.error", "timestamp": "2026-10-23T10:00:05Z",
                   "data": {"errorType": "rate_limit", "statusCode": 429,
                            "message": "Failed to get response from the AI model: 429 You've hit your 5-hour limit"}}),
        ),
        (
            Harness::OpenCode,
            json!({"table": "message", "row": {"id": "m1", "session_id": "s1",
                   "time_created": 1792756805000_u64, "time_updated": 1792756805000_u64,
                   "data": "{\"role\":\"assistant\",\"error\":{\"name\":\"APIError\",\"data\":{\"message\":\"You've hit your 5-hour limit · resets 3:50am (UTC)\",\"statusCode\":429}}}"}}),
        ),
        (
            Harness::Gemini,
            json!({"id": "e1", "timestamp": "2026-10-23T10:00:05Z", "type": "error",
                   "content": "You've hit your 5-hour limit · resets 3:50am (UTC)"}),
        ),
    ];
    for (harness, record) in cases {
        let state = state(harness, &[record]);
        assert_eq!(
            (state.activity, state.certainty, state.reason),
            (Activity::Errored, Certainty::Firm, Reason::Errored),
            "{harness:?}"
        );
        assert_eq!(state.stop_reason, Some(StopReason::UsageLimit), "{harness:?}");
    }
}

#[test]
fn gemini_rewinding_the_just_sent_prompt_is_a_firm_error() {
    let state = state(
        Harness::Gemini,
        &[
            json!({"id": "u1", "timestamp": "2026-10-23T10:00:01Z", "type": "user", "content": [{"text": "go"}]}),
            json!({"$rewindTo": "u1", "timestamp": "2026-10-23T10:00:02Z"}),
        ],
    );
    assert_eq!((state.activity, state.certainty, state.reason), (Activity::Errored, Certainty::Firm, Reason::Errored));
    assert_eq!(state.stop_reason, None, "the journal does not record the limit's cause");
}

#[test]
fn the_next_turn_clears_the_limit_reason_and_reset() {
    let first = format!(
        "{}\n",
        json!({"type": "assistant", "timestamp": "2026-10-23T10:00:05Z", "isApiErrorMessage": true,
               "error": "rate_limit", "apiError": "usage_limit_reached",
               "quotaLimits": {"resetsAt": 1793159400},
               "message": {"id": "m1", "stop_reason": "stop_sequence",
                           "content": [{"type": "text", "text": "You've hit your 5-hour limit"}]}})
    );
    let second = format!(
        "{}\n",
        json!({"type": "user", "timestamp": "2026-10-23T10:00:08Z", "message": {"role": "user", "content": "try again"}})
    );
    let now = Timestamp::parse_rfc3339(NOW).unwrap();
    let mut reducer = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
    let path = Path::new("session.jsonl");
    reducer.feed(first.as_bytes(), ChunkCtx { offset: 0, mtime: now, path }).unwrap();
    let limited = reducer.state(now);
    assert_eq!(limited.stop_reason, Some(StopReason::UsageLimit));
    reducer.feed(second.as_bytes(), ChunkCtx { offset: first.len() as u64, mtime: now, path }).unwrap();
    let next = reducer.state(now);
    assert_eq!((next.activity, next.stop_reason, next.resets_at), (Activity::Thinking, None, None));
}
