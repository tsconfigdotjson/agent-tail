//! What the tests share: a fixed clock, entries dated against it, and tvdash's wording of a
//! state (the lib has no labels; this shows the mapping tvdash keeps, and lets the tests
//! ported from tvdash read as they did).

use crate::harness::Harness;
use crate::reducer::{LOG_LINES, ReducerOptions};
use crate::session::Session;
use crate::state::{Activity, Certainty, Diagnostics, SessionState, StatusClass};
use crate::time::{Timestamp, civil_from_days, parse_rfc3339};
use serde_json::{Value, json};

/// The tests' "now": 2026-10-23T12:00:00Z. Nothing reads the wall clock.
pub const NOW: f64 = 1_792_756_800.0;

/// An RFC 3339 UTC time, to the millisecond, for seconds since the epoch.
pub fn rfc3339(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as i64;
    let (days, ms) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let (h, min, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{ms:03}Z")
}

/// RFC 3339 timestamp `secs_ago` seconds before `NOW`.
pub fn at(secs_ago: f64) -> String {
    rfc3339(NOW - secs_ago)
}

/// A session of `harness` after reading `events`, its transcript never written (so entries
/// with no time are dated 0).
pub fn replay(harness: Harness, events: &[Value]) -> Session {
    let mut s = Session::new(harness, LOG_LINES);
    events.iter().for_each(|e| s.apply(e));
    s
}

/// A recorded transcript's entries, re-timed so its last entry happened `ago` seconds
/// before `NOW` and the gaps between entries are kept.
pub fn fixture(jsonl: &str, ago: f64) -> Vec<Value> {
    let mut events: Vec<Value> = jsonl.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let last = events.iter().filter_map(|e| e["timestamp"].as_str().and_then(parse_rfc3339)).fold(0.0, f64::max);
    for e in &mut events {
        if let Some(t) = e["timestamp"].as_str().and_then(parse_rfc3339) {
            e["timestamp"] = json!(at(last - t + ago));
        }
    }
    events
}

pub fn jsonl(events: &[Value]) -> String {
    events.iter().map(|e| format!("{e}\n")).collect()
}

/// A session's activity lines, as tvdash shows them.
pub fn log(s: &Session) -> Vec<String> {
    s.log.iter().map(ToString::to_string).collect()
}

/// tvdash's label for a state (its #4), which it maps from the lib's enums.
pub fn label(st: &SessionState) -> &'static str {
    match (st.activity, st.certainty) {
        (Activity::AwaitingAnswer, _) => "needs your answer",
        (Activity::AwaitingApproval, Certainty::Inferred) => "may need approval",
        (Activity::AwaitingApproval, Certainty::Firm) => "needs approval",
        (Activity::RunningTool | Activity::BackgroundWork, _) => "working",
        (Activity::Thinking, _) => "thinking",
        (Activity::Done | Activity::Interrupted | Activity::Errored, _) => "done",
        (Activity::Idle | Activity::Unknown, _) => "idle",
    }
}

/// A session's class and tvdash's label, as of `NOW`.
pub fn status(s: &Session) -> (StatusClass, &'static str) {
    let st = s.state(Timestamp::from_secs_f64(NOW), &ReducerOptions::default(), Diagnostics::default());
    (st.activity.class(), label(&st))
}

#[test]
fn formats_rfc3339() {
    assert_eq!(rfc3339(0.0), "1970-01-01T00:00:00.000Z");
    assert_eq!(rfc3339(NOW - 0.25), "2026-10-23T11:59:59.750Z");
    for t in [0.0, 951868859.5, NOW, NOW + 86_399.999] {
        assert!((parse_rfc3339(&rfc3339(t)).unwrap() - t).abs() < 1e-3, "{t}");
    }
}
