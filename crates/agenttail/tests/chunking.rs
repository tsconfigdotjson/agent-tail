//! However a transcript arrives (in chunks cut anywhere, or re-read from the end of its last
//! complete line as a poller does), a session comes to the state it has read whole.

use agenttail::{ChunkCtx, Harness, ParseMode, ReducerOptions, SessionReducer, SessionState, Timestamp};
use proptest::prelude::*;
use serde_json::json;
use std::path::Path;

const NOW: f64 = 1_792_756_800.0; // 2026-10-23T12:00:00Z

fn now() -> Timestamp {
    Timestamp::from_secs_f64(NOW)
}

fn path(h: Harness) -> &'static Path {
    Path::new(match h {
        Harness::Cursor => "/h/.cursor/projects/Users-sam-demo-app/agent-transcripts/a1/a1.jsonl",
        Harness::Codex => "/h/.codex/sessions/2026/10/23/rollout-x.jsonl",
        _ => "/h/.claude/projects/-w-app/s1.jsonl",
    })
}

fn ctx(h: Harness, offset: u64) -> ChunkCtx<'static> {
    ChunkCtx { offset, mtime: Timestamp::from_secs_f64(NOW - 3.0), path: path(h) }
}

fn reducer(h: Harness, mode: ParseMode) -> SessionReducer {
    SessionReducer::new(h, ReducerOptions { mode, ..Default::default() })
}

/// The state after reading `text` in one go.
fn whole(h: Harness, mode: ParseMode, text: &[u8]) -> (SessionState, u64) {
    let mut s = reducer(h, mode);
    s.feed(text, ctx(h, 0)).unwrap();
    (s.state(now()), s.consumed())
}

/// The state after reading `text` cut at `cuts`, each piece fed as it comes.
fn chunked(h: Harness, mode: ParseMode, text: &[u8], cuts: &[usize]) -> (SessionState, u64) {
    let mut s = reducer(h, mode);
    let mut at = 0;
    for &cut in cuts.iter().chain([&text.len()]) {
        s.feed(&text[at..cut], ctx(h, at as u64)).unwrap();
        at = cut;
    }
    (s.state(now()), s.consumed())
}

/// The state after `text` was written in pieces ending at `cuts`, a poller reading after
/// each write from the end of the last complete line (as `Tracker` does), in `chunk`-sized
/// parts.
fn polled(h: Harness, mode: ParseMode, text: &[u8], cuts: &[usize], chunk: usize) -> (SessionState, u64) {
    let mut s = reducer(h, mode);
    s.set_path(path(h));
    for &written in cuts.iter().chain([&text.len()]) {
        s.transcript_written(ctx(h, 0).mtime);
        let mut at = s.consumed() as usize;
        while at < written {
            let end = (at + chunk).min(written);
            let ctx = ctx(h, at as u64);
            if end < written { s.feed_part(&text[at..end], ctx) } else { s.feed(&text[at..end], ctx) }.unwrap();
            at = end;
        }
    }
    (s.state(now()), s.consumed())
}

/// One transcript line of `h`'s, of kind `kind`, with `text` in it; or not JSON at all.
fn line(h: Harness, kind: u8, text: &str, n: usize) -> String {
    let ts = format!("2026-10-23T11:59:{:02}.000Z", n % 60);
    let e = match (h, kind % 8) {
        (_, 6) => return ["not json", "", "[1,2]", "{\"role\":", "\"str\"", "   "][n % 6].into(),
        (_, 7) => json!({"type": "something_new", "text": text}),
        (Harness::ClaudeCode, k) => match k {
            0 => json!({"type": "user", "timestamp": ts, "cwd": format!("/w/{text}"), "message": {"content": text}}),
            1 => {
                json!({"type": "assistant", "timestamp": ts, "message": {"id": format!("m{n}"), "usage": {"output_tokens": 3},
                        "content": [{"type": "tool_use", "name": "Bash", "input": {"command": text}}]}})
            }
            2 => {
                json!({"type": "user", "timestamp": ts, "message": {"content": [{"type": "tool_result", "content": text.repeat(500)}]}})
            }
            3 => {
                json!({"type": "assistant", "timestamp": ts, "message": {"id": format!("m{n}"), "stop_reason": "end_turn", "content": []}})
            }
            4 => json!({"type": "ai-title", "aiTitle": text}),
            _ => json!({"type": "cost-state", "totalCostUSD": n as f64 / 10.0}),
        },
        (Harness::Codex, k) => match k {
            0 => json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "user_message", "message": text}}),
            1 => {
                json!({"timestamp": ts, "type": "response_item", "payload": {"type": "function_call", "name": "exec_command",
                        "arguments": json!({"cmd": text}).to_string()}})
            }
            2 => {
                json!({"timestamp": ts, "type": "response_item", "payload": {"type": "function_call_output", "output": text.repeat(500)}})
            }
            3 => json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "task_complete"}}),
            4 => {
                json!({"timestamp": ts, "type": "session_meta", "payload": {"cwd": format!("/w/{text}"), "git": {"branch": text}}})
            }
            _ => {
                json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": {"output_tokens": n}}}})
            }
        },
        (_, k) => match k {
            0 => {
                json!({"role": "user", "message": {"content": [{"type": "text", "text": format!("<user_query>{text}</user_query>")}]}})
            }
            1 => {
                json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"path": format!("/Users/sam/demo-app/{text}")}}]}})
            }
            2 => json!({"role": "assistant", "message": {"content": [{"type": "text", "text": text.repeat(500)}]}}),
            3 => json!({"type": "turn_ended", "status": "success"}),
            4 => {
                json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "AskQuestion", "input": {}}]}})
            }
            _ => json!({"role": "user", "message": {"content": text}}),
        },
    };
    e.to_string()
}

fn harness() -> impl Strategy<Value = Harness> {
    prop_oneof![Just(Harness::ClaudeCode), Just(Harness::Codex), Just(Harness::Cursor)]
}

fn mode() -> impl Strategy<Value = ParseMode> {
    prop_oneof![Just(ParseMode::Lean), Just(ParseMode::Full)]
}

/// A transcript of `h`'s, its last line perhaps without a newline (Cursor writes it late),
/// and cut points in it.
fn transcript() -> impl Strategy<Value = (Harness, Vec<u8>, Vec<usize>)> {
    let lines = prop::collection::vec((any::<u8>(), "\\PC{0,40}"), 0..40);
    (harness(), lines, any::<bool>(), prop::collection::vec(any::<prop::sample::Index>(), 0..12)).prop_map(
        |(h, lines, newline_last, cuts)| {
            let mut text = String::new();
            for (n, (kind, t)) in lines.iter().enumerate() {
                text += &line(h, *kind, t, n);
                text.push('\n');
            }
            if !newline_last && text.ends_with('\n') {
                text.pop();
            }
            let mut cuts: Vec<usize> = cuts.iter().map(|i| i.index(text.len() + 1)).collect();
            cuts.sort();
            (h, text.into_bytes(), cuts)
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn chunks_cut_anywhere_read_as_whole((h, text, cuts) in transcript(), mode in mode()) {
        prop_assert_eq!(chunked(h, mode, &text, &cuts), whole(h, mode, &text));
    }

    #[test]
    fn polls_of_partial_writes_read_as_whole((h, text, cuts) in transcript(), mode in mode(), chunk in 1usize..4096) {
        prop_assert_eq!(polled(h, mode, &text, &cuts, chunk), whole(h, mode, &text));
    }
}

const FIXTURES: [(Harness, &str); 4] = [
    (Harness::ClaudeCode, include_str!("../../../fixtures/claude-code/2.1/linux/seed-session/input/session.jsonl")),
    (Harness::Codex, include_str!("../../../fixtures/codex/0.160/linux/seed-exec/input/session.jsonl")),
    (Harness::Codex, include_str!("../../../fixtures/codex/0.160/linux/seed-resume/input/session.jsonl")),
    (Harness::Cursor, include_str!("../../../fixtures/cursor/unknown/any/seed-session/input/session.jsonl")),
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// The recorded fixtures, fed in chunks of random sizes, come to what they do whole.
    #[test]
    fn fixtures_in_random_chunks_read_as_whole(sizes in prop::collection::vec(1usize..3000, 1..200), mode in mode()) {
        for (h, text) in FIXTURES {
            let text = text.as_bytes();
            let mut cuts = Vec::new();
            let mut at = 0;
            for size in sizes.iter().cycle() {
                at += size;
                if at >= text.len() {
                    break;
                }
                cuts.push(at);
            }
            let want = whole(h, mode, text);
            prop_assert_eq!(&chunked(h, mode, text, &cuts), &want);
            prop_assert_eq!(&polled(h, mode, text, &cuts, sizes[0]), &want);
        }
    }
}

#[test]
fn fixtures_read_whole_as_tvdash_read_them() {
    // Every fixture's lines parse, and none is long enough to be cut.
    for (h, text) in FIXTURES {
        let lines = text.lines().count() as u64;
        let (st, consumed) = whole(h, ParseMode::Lean, text.as_bytes());
        assert_eq!((consumed, st.diagnostics.lines, st.diagnostics.unparsed), (text.len() as u64, lines, 0), "{h:?}");
        assert_eq!(whole(h, ParseMode::Full, text.as_bytes()).0, st, "{h:?}: lean and full agree");
    }
}
