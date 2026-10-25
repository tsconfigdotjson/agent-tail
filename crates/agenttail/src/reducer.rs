//! The sans-IO core: bytes of one transcript in, its session's state out. Local files and
//! chunks shipped from another machine feed the same code.

use crate::error::Error;
use crate::harness::Harness;
use crate::lean::lean;
use crate::session::Session;
use crate::state::{Activity, Certainty, Diagnostics, HelperState, Reason, SessionState};
use crate::time::Timestamp;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many recent activity lines a session keeps by default.
pub(crate) const LOG_LINES: usize = 5;

/// How transcript lines are parsed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParseMode {
    /// Long strings in big lines (tool output, mostly) are cut to their first 4 KB before
    /// parsing, which no rule reads past; a long session's first read is many times faster.
    /// Lines cut are counted in [`Diagnostics::cut`]. Codex's tool `arguments` are kept whole.
    #[default]
    Lean,
    /// Every line parsed whole.
    Full,
}

/// How a reducer reads, and the thresholds its status rules use.
#[derive(Clone, Debug, PartialEq)]
pub struct ReducerOptions {
    pub mode: ParseMode,
    /// A tool call with no result for this long may be awaiting approval (20 s).
    pub stall_after: Duration,
    /// A session whose transcripts weren't written for this long is idle (15 min).
    pub idle_after: Duration,
    /// How many recent activity lines to keep (5).
    pub recent_lines: usize,
}

impl Default for ReducerOptions {
    fn default() -> Self {
        ReducerOptions {
            mode: ParseMode::Lean,
            stall_after: Duration::from_secs(20),
            idle_after: Duration::from_secs(15 * 60),
            recent_lines: LOG_LINES,
        }
    }
}

/// Where a chunk of transcript came from.
#[derive(Clone, Copy, Debug)]
pub struct ChunkCtx<'a> {
    /// Its byte offset in the transcript.
    pub offset: u64,
    /// The transcript's mtime when it was read: when entries that carry no time of their own
    /// happened (Cursor's). Use the time the chunk arrived if the mtime is unknown.
    pub mtime: Timestamp,
    /// The transcript's path, on whatever machine it is. Some harnesses file sessions by
    /// project (Cursor), so the path names it; the first chunk's is the one used.
    pub path: &'a Path,
}

/// One transcript's session, fed a chunk at a time. Only complete lines are read; the
/// unfinished end of a chunk is held back until the rest arrives.
///
/// ```
/// use agenttail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, Timestamp, Activity};
/// use std::path::Path;
///
/// let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
/// let line = br#"{"type":"user","timestamp":"2026-10-23T10:00:00Z","cwd":"/w/app","message":{"content":"fix it"}}"#;
/// let now = Timestamp::parse_rfc3339("2026-10-23T10:00:05Z").unwrap();
/// let ctx = ChunkCtx { offset: 0, mtime: now, path: Path::new("/h/.claude/projects/-w-app/s.jsonl") };
/// s.feed(&line[..20], ctx).unwrap(); // half a line: held back
/// s.feed(&line[20..], ChunkCtx { offset: 20, ..ctx }).unwrap();
/// assert_eq!(s.state(now).activity, Activity::Thinking);
/// assert_eq!(s.state(now).meta.project, "app");
/// ```
#[derive(Clone, Debug)]
pub struct SessionReducer {
    session: Session,
    options: ReducerOptions,
    /// Bytes of the transcript taken in: the end of its last complete line.
    consumed: u64,
    /// What follows `consumed`: the start of a line not yet complete.
    pending: Vec<u8>,
    path: Option<PathBuf>,
    fed: bool,
    diagnostics: Diagnostics,
    /// Helpers (subagents/background tasks), each followed with the same reducer.
    helpers: HashMap<String, SessionReducer>,
    /// A helper's `.meta.json`, keyed like `helpers`: `finished`/`stoppedByUser` is the
    /// firm end that a transcript's `end_turn` alone isn't.
    helper_meta: HashMap<String, HelperMeta>,
    /// This reducer is following a helper transcript, whose records are `isSidechain`.
    helper: bool,
}

/// The fields of Claude Code's `<agent>.meta.json` that decide whether a helper ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HelperMeta {
    finished: bool,
    stopped_by_user: bool,
}

impl HelperMeta {
    fn ended(self) -> bool {
        self.finished || self.stopped_by_user
    }
}

impl SessionReducer {
    pub fn new(harness: Harness, options: ReducerOptions) -> Self {
        SessionReducer {
            session: Session::new(harness, options.recent_lines),
            options,
            consumed: 0,
            pending: Vec::new(),
            path: None,
            fed: false,
            diagnostics: Diagnostics::default(),
            helpers: HashMap::new(),
            helper_meta: HashMap::new(),
            helper: false,
        }
    }

    /// Tell the reducer where its transcript is before any of it is fed (a chunk's
    /// [`ChunkCtx::path`] does too): a session can be known by its path alone (Cursor's
    /// project) while its file is still empty. Only the first path counts.
    pub fn set_path(&mut self, path: &Path) {
        if self.path.is_none() {
            let is_helper = self.session.is_helper;
            self.session = Session::at(self.session.harness, self.options.recent_lines, path);
            self.session.is_helper = is_helper;
            self.path = Some(path.into());
        }
    }

    pub fn harness(&self) -> Harness {
        self.session.harness
    }

    pub fn options(&self) -> &ReducerOptions {
        &self.options
    }

    /// Bytes taken in: where a reader should resume after losing what it held back (a
    /// remote connection's restart). The end of the last complete line.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Bytes received, including the unfinished line held back: where the next chunk
    /// carries on.
    pub fn received(&self) -> u64 {
        self.consumed + self.pending.len() as u64
    }

    /// Take in a chunk of the transcript, the latest there is for now: its complete lines,
    /// and an unfinished last line if it's already a whole JSON object (a writer that adds
    /// the newline late, as Cursor does, would otherwise keep its last entry from being
    /// read). The rest is held back for the next chunk.
    ///
    /// The chunk must start at [`received`](Self::received); or at
    /// [`consumed`](Self::consumed) (the held-back line, sent again: what's held is
    /// dropped); or at 0 (the file was replaced or truncated: the session starts over). A
    /// new reducer takes any offset, and carries on from there without the beginning.
    pub fn feed(&mut self, bytes: &[u8], ctx: ChunkCtx<'_>) -> Result<(), Error> {
        self.take(bytes, ctx, true)
    }

    /// Like [`feed`](Self::feed), for a chunk that more follows right away (a long file read
    /// in parts): only lines with their newline are taken.
    pub fn feed_part(&mut self, bytes: &[u8], ctx: ChunkCtx<'_>) -> Result<(), Error> {
        self.take(bytes, ctx, false)
    }

    /// The transcript was written at `mtime`, with or without new bytes.
    pub fn transcript_written(&mut self, mtime: Timestamp) {
        self.session.mtime = mtime.as_secs_f64();
    }

    /// One of the session's helpers (a subagent) wrote to its own transcript at `mtime`.
    /// Helpers' transcripts aren't read yet; their writes count as the session's activity,
    /// so a session waiting on a subagent isn't taken for one waiting on the user.
    pub fn helper_written(&mut self, mtime: Timestamp) {
        self.session.helper_mtime = self.session.helper_mtime.max(mtime.as_secs_f64());
    }

    /// Apply one event from the public parse layer to this reducer. The reducer's own
    /// chunk reader goes through the same event application, so status and events agree.
    /// Event times are used when present; a harness without times (Cursor) falls back to
    /// the transcript mtime, as [`feed`](Self::feed) does. Sidechain events are ignored
    /// here; a helper transcript belongs to [`feed_helper`](Self::feed_helper).
    pub fn feed_event(&mut self, event: &crate::Event) {
        self.session.apply_event(event);
    }

    /// Apply a batch of events, in order.
    pub fn feed_events<'a>(&mut self, events: impl IntoIterator<Item = &'a crate::Event>) {
        for event in events {
            self.feed_event(event);
        }
    }

    /// The session's state as of `now`: a pure function of what was fed and `now`.
    pub fn state(&self, now: Timestamp) -> SessionState {
        let mut state = self.session.state(now, &self.options, self.diagnostics);
        let mut background = Vec::new();
        for (id, helper) in &self.helpers {
            // `.meta.json` finished/stopped is firmer than the helper's own last entry; a
            // notification or metadata that ended the parent's launch also ends it here.
            let meta_ended = self.helper_meta.get(id).is_some_and(|meta| meta.ended());
            let launch_ended =
                self.session.launched_helpers.values().any(|launch| launch.ended && launch.matches_helper(id));
            if meta_ended || launch_ended {
                continue;
            }
            let helper = helper.state(now);
            // A finished or stale helper no longer keeps the parent at work.
            if matches!(
                helper.activity,
                Activity::Done | Activity::Interrupted | Activity::Errored | Activity::Idle | Activity::Unknown
            ) {
                continue;
            }
            background.push(HelperState {
                id: id.clone(),
                activity: helper.activity,
                certainty: helper.certainty,
                reason: helper.reason,
                since: helper.since,
                last_write: helper.last_write,
            });
        }
        // An async launch is known before its file is read. Once the helper's own transcript
        // is followed, that transcript (and its metadata) is the more precise state.
        for launch in self.session.launched_helpers.values().filter(|launch| !launch.ended) {
            if self.helpers.keys().any(|id| launch.matches_helper(id)) {
                continue;
            }
            let since = (launch.since > 0.0).then(|| Timestamp::from_secs_f64(launch.since));
            background.push(HelperState {
                id: launch.id(),
                activity: Activity::RunningTool,
                certainty: Certainty::Inferred,
                reason: Reason::ToolCallPending,
                since,
                last_write: since,
            });
        }
        background.sort_by(|a, b| a.id.cmp(&b.id));
        let pending = self.session.background_agents;
        if state.activity == Activity::Done && (pending > 0 || !background.is_empty()) {
            state.activity = Activity::BackgroundWork;
            state.reason = Reason::BackgroundHelpers;
            // The parent's pending count is firm; a helper's own transcript end is inferred.
            state.certainty = if pending > 0 { Certainty::Firm } else { Certainty::Inferred };
        }
        state.background = background;
        state
    }

    /// Feed one helper's transcript (a subagent or background task). `id` is stable for the
    /// helper, usually its transcript path. Helpers use the same parser and status rules as
    /// their parent.
    pub fn feed_helper(&mut self, id: &str, bytes: &[u8], ctx: ChunkCtx<'_>) -> Result<(), Error> {
        let harness = self.session.harness;
        let options = self.options.clone();
        let helper = self.helpers.entry(id.to_string()).or_insert_with(|| {
            let mut reducer = SessionReducer::new(harness, options);
            reducer.helper = true;
            reducer.session.is_helper = true;
            reducer
        });
        helper.feed(bytes, ctx)
    }

    /// Feed a helper's `.meta.json` (Claude Code's `agent-<id>.meta.json`), keyed by the same
    /// `id` as its transcript. `finished: true` (or `stoppedByUser: true`) is a firm end,
    /// even when the transcript's last entry is still a pending tool call. Malformed JSON is
    /// ignored: the transcript, notification and parent count still apply.
    pub fn feed_helper_meta(&mut self, id: &str, bytes: &[u8]) {
        let Ok(meta) = serde_json::from_slice::<Value>(bytes) else { return };
        let Some(meta) = meta.as_object() else { return };
        let meta = HelperMeta {
            finished: meta.get("finished").and_then(Value::as_bool).unwrap_or(false),
            stopped_by_user: meta.get("stoppedByUser").and_then(Value::as_bool).unwrap_or(false),
        };
        if meta.ended() {
            self.session.background_helper_meta_finished(id);
        }
        self.helper_meta.insert(id.to_string(), meta);
    }

    /// The Codex thread id this session's `session_meta` recorded.
    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    /// Use a title from Codex's `session_index.jsonl`; later prompts don't overwrite it.
    pub fn set_index_title(&mut self, title: Option<&str>) {
        self.session.set_index_title(title);
    }

    /// Use a title from Cursor's `state.vscdb` (`composerData.name`); it wins over prompts
    /// and Codex's index title, and the transcript remains the fallback when it goes away.
    pub fn set_composer_title(&mut self, title: Option<&str>) {
        self.session.set_db_title(title);
    }

    /// Use a status from Cursor's `state.vscdb`, dated when Cursor wrote it. `None` returns
    /// to the transcript rules. A status older than the last transcript entry is ignored.
    pub fn set_composer_status(&mut self, status: Option<(Activity, Timestamp)>) {
        self.session.set_db_status(status.map(|(activity, at)| (activity, at.as_secs_f64())));
    }

    /// When the session was last seen at work: its transcript's last write, or a helper's.
    #[cfg(feature = "fs")]
    pub(crate) fn active_at(&self) -> f64 {
        self.session.active_at()
    }

    fn take(&mut self, bytes: &[u8], ctx: ChunkCtx<'_>, last: bool) -> Result<(), Error> {
        let received = self.received();
        if ctx.offset != received {
            if !self.fed {
                // One a reader had forgotten while the writer still followed it.
                self.consumed = ctx.offset;
            } else if ctx.offset == self.consumed {
                self.pending.clear();
            } else if ctx.offset == 0 {
                self.restart();
            } else {
                return Err(Error::OffsetMismatch { expected: received, got: ctx.offset });
            }
        }
        self.set_path(ctx.path);
        self.fed = true;
        self.session.mtime = ctx.mtime.as_secs_f64();
        self.pending.extend_from_slice(bytes);
        // Still inside one (long) line: nothing to parse yet.
        if !bytes.contains(&b'\n') && (!last || !self.pending.trim_ascii_end().ends_with(b"}")) {
            return Ok(());
        }
        let mut data = std::mem::take(&mut self.pending);
        let (mode, diag, session) = (self.options.mode, &mut self.diagnostics, &mut self.session);
        let used = if last {
            complete_lines(&data, mode, diag, |e| session.apply(&e))
        } else {
            whole_lines(&data, mode, diag, |e| session.apply(&e))
        };
        self.consumed += used as u64;
        data.drain(..used);
        self.pending = data;
        Ok(())
    }

    /// Back to an empty session at the same path.
    fn restart(&mut self) {
        let path = self.path.take();
        let helper = self.helper;
        *self = SessionReducer::new(self.session.harness, self.options.clone());
        self.helper = helper;
        self.session.is_helper = helper;
        if let Some(path) = path {
            self.set_path(&path);
        }
    }

    #[cfg(test)]
    pub(crate) fn session(&self) -> &Session {
        &self.session
    }
}

/// Every JSON value in `line`, parsed in `mode`, and whether anything was cut first. A line
/// with trailing junk is rejected whole, as before: partial entries are not guessed at.
fn parse_many(line: &[u8], mode: ParseMode) -> (Vec<Value>, bool) {
    let bytes = match mode {
        ParseMode::Lean => lean(line),
        ParseMode::Full => Cow::Borrowed(line),
    };
    let mut values = Vec::new();
    let stream = serde_json::Deserializer::from_slice(&bytes).into_iter::<Value>();
    for value in stream {
        match value {
            Ok(value) => values.push(value),
            Err(_) => return (Vec::new(), matches!(bytes, Cow::Owned(_))),
        }
    }
    (values, matches!(bytes, Cow::Owned(_)))
}

/// Hands each entry in `data`'s complete lines to `f`, one at a time, and returns how many
/// bytes those lines took. A partially written entry at the end is left for next time;
/// lines that aren't JSON are skipped (and counted). A line can hold more than one JSON
/// object (ADR does this); each is handed over.
///
/// A last entry with no newline after it yet counts once it's a whole JSON object (no
/// shorter part of one is): a writer that adds the newline before each entry, or late,
/// would otherwise keep its last entry (Cursor's "turn ended", say) from being read.
pub(crate) fn complete_lines(data: &[u8], mode: ParseMode, diag: &mut Diagnostics, mut f: impl FnMut(Value)) -> usize {
    let end = whole_lines(data, mode, diag, &mut f);
    if ends_an_object(&data[end..]) {
        let (values, cut) = parse_many(&data[end..], mode);
        if !values.is_empty() && values.iter().all(Value::is_object) {
            diag.lines += values.len() as u64;
            diag.cut += u64::from(cut);
            for value in values {
                f(value);
            }
            return data.len();
        }
    }
    end
}

/// `complete_lines` without the last entry that has no newline after it: the lines up to the
/// last newline in `data`. How many bytes they took.
pub(crate) fn whole_lines(data: &[u8], mode: ParseMode, diag: &mut Diagnostics, mut f: impl FnMut(Value)) -> usize {
    let end = memchr::memrchr(b'\n', data).map_or(0, |i| i + 1);
    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', &data[..end]) {
        let line = &data[std::mem::replace(&mut start, nl + 1)..nl];
        let (values, cut) = parse_many(line, mode);
        diag.cut += u64::from(cut);
        if values.is_empty() {
            if !line.trim_ascii().is_empty() {
                diag.unparsed += 1;
            }
        } else {
            diag.lines += values.len() as u64;
            for value in values {
                f(value);
            }
        }
    }
    end
}

/// Whether `tail` may be a whole JSON object: worth trying to parse.
fn ends_an_object(tail: &[u8]) -> bool {
    let t = tail.trim_ascii();
    t.starts_with(b"{") && t.ends_with(b"}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{NOW, jsonl, log};
    use serde_json::json;

    fn ctx(offset: u64) -> ChunkCtx<'static> {
        ChunkCtx {
            offset,
            mtime: Timestamp::from_secs_f64(NOW),
            path: Path::new("/h/.cursor/projects/w-app/agent-transcripts/a1/a1.jsonl"),
        }
    }

    fn cursor_prompt(text: &str) -> Value {
        json!({"role": "user", "message": {"content": [{"type": "text", "text": format!("<user_query>{text}</user_query>")}]}})
    }

    #[test]
    fn reads_two_objects_on_one_line() {
        let text = format!("{}{}\n", cursor_prompt("one"), cursor_prompt("two"));
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        s.feed(text.as_bytes(), ctx(0)).unwrap();
        assert_eq!(log(s.session()), ["› one", "› two"]);
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).diagnostics.lines, 2);

        let last = format!("{}{}", cursor_prompt("three"), cursor_prompt("four"));
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        s.feed(last.as_bytes(), ctx(0)).unwrap();
        assert_eq!(log(s.session()), ["› three", "› four"], "without a newline too");
    }

    #[test]
    fn reads_crlf_and_an_offset_timestamp() {
        let text = format!(
            "{}\r\n{}\r\n",
            json!({"type": "user", "timestamp": "2026-10-23T14:00:00+02:00", "cwd": "/w/app",
                   "message": {"content": "go"}}),
            json!({"type": "assistant", "timestamp": "2026-10-23T12:00:01Z",
                   "message": {"id": "m1", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                               "content": [{"type": "text", "text": "done"}]}}),
        );
        let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        s.feed(text.as_bytes(), ctx(0)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.activity, st.diagnostics.unparsed), (crate::Activity::Done, 0));
        assert_eq!(st.last_at, Some(Timestamp::from_secs_f64(NOW + 1.0)));
    }

    #[test]
    fn complete_lines_takes_a_whole_last_object() {
        let mut seen = Vec::new();
        let d = &mut Diagnostics::default();
        let mode = ParseMode::Lean;
        assert_eq!(complete_lines(b"{\"a\":1}\n{\"b\":", mode, d, |e| seen.push(e)), 8);
        assert_eq!(complete_lines(b"{\"a\":1}\n {\"b\":2} ", mode, d, |e| seen.push(e)), 17);
        assert_eq!(
            complete_lines(b"{\"a\":1}\n{\"b\":{}", mode, d, |e| seen.push(e)),
            8,
            "an object's start that ends in a brace"
        );
        assert_eq!(complete_lines(b"[1]", mode, d, |e| seen.push(e)), 0, "only an object counts");
        assert_eq!(seen.len(), 4);
        assert_eq!(*d, Diagnostics { lines: 4, unparsed: 0, cut: 0 });
    }

    #[test]
    fn a_last_line_with_no_newline_is_read_once_whole() {
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        let first = jsonl(&[cursor_prompt("go")]);
        let end = json!({"type": "turn_ended", "status": "success"}).to_string();
        let (head, tail) = end.split_at(10);
        let mut fed = 0u64;
        let mut feed = |s: &mut SessionReducer, text: &str| {
            s.feed(text.as_bytes(), ctx(fed)).unwrap();
            fed += text.len() as u64;
        };
        feed(&mut s, &(first.clone() + head));
        let label = |s: &SessionReducer| crate::testutil::label(&s.state(Timestamp::from_secs_f64(NOW)));
        assert_eq!(label(&s), "thinking", "half an entry waits");
        assert_eq!((s.consumed(), s.received()), (first.len() as u64, (first.len() + 10) as u64));
        feed(&mut s, tail);
        assert_eq!(label(&s), "done", "a whole one is read without its newline");
        feed(&mut s, &format!("\n{}", jsonl(&[cursor_prompt("more")])));
        assert_eq!(
            (label(&s), log(s.session())),
            ("thinking", vec!["› go".to_string(), "› more".into()]),
            "and not read twice"
        );
    }

    #[test]
    fn chunks_must_carry_on_where_the_last_ended() {
        let text = jsonl(&[cursor_prompt("one"), cursor_prompt("two")]);
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        s.feed(&text.as_bytes()[..30], ctx(0)).unwrap();
        assert_eq!(s.feed(b"x", ctx(5)), Err(Error::OffsetMismatch { expected: 30, got: 5 }));
        assert_eq!(s.received(), 30, "a refused chunk changes nothing");
        // The held-back line, sent again from the end of the last complete one.
        s.feed(&text.as_bytes()[..30], ctx(0)).unwrap();
        s.feed(&text.as_bytes()[30..], ctx(30)).unwrap();
        assert_eq!(log(s.session()), ["› one", "› two"]);
        let consumed = s.consumed();
        s.feed(b"{\"role\":", ctx(consumed)).unwrap();
        s.feed(&text.as_bytes()[..text.len() / 2], ctx(consumed)).unwrap();
        assert_eq!(s.received(), consumed + text.len() as u64 / 2, "what was held back is dropped");
        // From 0 again: replaced or truncated, the session starts over.
        s.feed(jsonl(&[cursor_prompt("new")]).as_bytes(), ctx(0)).unwrap();
        assert_eq!(log(s.session()), ["› new"]);
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).meta.project, "app", "at the same path");
        // A new reducer carries on from wherever it's told.
        let mut late = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        let line = jsonl(&[cursor_prompt("later")]);
        late.feed(line.as_bytes(), ctx(1000)).unwrap();
        assert_eq!((late.consumed(), log(late.session())), (1000 + line.len() as u64, vec!["› later".to_string()]));
    }

    #[test]
    fn a_path_alone_names_a_cursor_project() {
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        s.set_path(Path::new("/h/.cursor/projects/Users-sam-shade/agent-transcripts/a/a.jsonl"));
        s.set_path(Path::new("/elsewhere/a.jsonl"));
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.meta.project.as_str(), st.meta.folder.as_str()), ("shade", "Users-sam-shade"));
    }

    #[test]
    fn counts_what_it_skips_and_cuts() {
        let huge =
            json!({"role": "assistant", "message": {"content": [{"type": "text", "text": "x".repeat(100_000)}]}});
        let text = format!("not json\n\n{huge}\n{}\n", cursor_prompt("go"));
        for (mode, cut) in [(ParseMode::Lean, 1), (ParseMode::Full, 0)] {
            let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions { mode, ..Default::default() });
            s.feed(text.as_bytes(), ctx(0)).unwrap();
            assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).diagnostics, Diagnostics { lines: 2, unparsed: 1, cut });
        }
    }

    #[test]
    fn a_helper_reducer_applies_sidechain_events() {
        let text = jsonl(&[json!({"type": "assistant", "isSidechain": true,
                                  "timestamp": crate::testutil::at(1.0),
                                  "message": {"id": "h1", "stop_reason": "tool_use",
                                              "content": [{"type": "tool_use", "id": "t1", "name": "Bash",
                                                           "input": {"command": "sleep 9"}}]}})]);
        let events = crate::event::parse_events(Harness::ClaudeCode, 0, text.as_bytes(), ParseMode::Full);
        assert!(events.iter().all(|event| event.sidechain));

        let mut helper = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        helper.helper = true;
        helper.session.is_helper = true;
        helper.feed_events(&events);
        assert_eq!(helper.state(Timestamp::from_secs_f64(NOW)).activity, Activity::RunningTool);
    }

    #[test]
    fn helpers_keep_a_finished_turn_at_work() {
        let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        let parent = jsonl(&[
            json!({"type": "assistant", "timestamp": crate::testutil::at(3.0),
                   "message": {"id": "m1", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                               "content": [{"type": "text", "text": "done"}]}}),
            json!({"type": "system", "subtype": "turn_duration", "timestamp": crate::testutil::at(2.0),
                   "pendingBackgroundAgentCount": 1}),
        ]);
        s.feed(parent.as_bytes(), ctx(0)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!(
            (st.activity, st.certainty, st.reason),
            (Activity::BackgroundWork, Certainty::Firm, Reason::BackgroundHelpers)
        );

        let helper = jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": crate::testutil::at(1.0),
                                    "message": {"id": "h1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                                "content": [{"type": "tool_use", "name": "Bash", "input": {"command": "sleep 9"}}]}})]);
        s.feed_helper("agent-a1", helper.as_bytes(), ctx(0)).unwrap();
        // The helper is running a tool, not asking for approval: it is background work.
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!(st.background.len(), 1);
        assert_eq!(st.background[0].id, "agent-a1");
        assert_eq!(st.background[0].activity, Activity::RunningTool);

        let finished =
            jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": crate::testutil::at(0.5),
                                      "message": {"id": "h2", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                                                  "content": [{"type": "text", "text": "done"}]}})]);
        s.feed_helper("agent-a1", finished.as_bytes(), ctx(helper.len() as u64)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!(st.background.len(), 0);
        // The parent count is still 1 until its task-notification arrives.
        assert_eq!((st.activity, st.certainty), (Activity::BackgroundWork, Certainty::Firm));

        let note = jsonl(&[
            json!({"type": "user", "origin": {"kind": "task-notification"}, "timestamp": crate::testutil::at(0.2),
                                  "message": {"content": "<task-notification><status>completed</status></task-notification>"}}),
        ]);
        let offset = parent.len() as u64;
        s.feed(note.as_bytes(), ctx(offset)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.activity, st.background.len()), (Activity::Done, 0));
        assert_eq!(log(s.session()), Vec::<String>::new(), "a task-notification is not a prompt");
    }

    fn linked_parent() -> (SessionReducer, String) {
        let output = "/h/.claude/projects/-w-app/s/subagents/agent-a1.jsonl";
        let parent = jsonl(&[
            json!({"type": "assistant", "timestamp": crate::testutil::at(4.0),
                   "message": {"id": "m1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                               "content": [{"type": "tool_use", "id": "toolu_1", "name": "Agent",
                                            "input": {"description": "look", "prompt": "look"}}]}}),
            json!({"type": "user", "timestamp": crate::testutil::at(3.5), "sourceToolUseID": "toolu_1",
                   "toolUseResult": {"status": "async_launched", "isAsync": true, "agentId": "a1",
                                     "outputFile": output, "backgroundTaskId": "task-a1"},
                   "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "launched"}]}}),
            json!({"type": "assistant", "timestamp": crate::testutil::at(3.0),
                   "message": {"id": "m2", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                               "content": [{"type": "text", "text": "started"}]}}),
        ]);
        let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        s.feed(parent.as_bytes(), ctx(0)).unwrap();
        (s, output.into())
    }

    fn running_helper() -> String {
        jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": crate::testutil::at(1.0),
                       "message": {"id": "h1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                   "content": [{"type": "tool_use", "name": "Bash", "input": {"command": "sleep 9"}}]}})])
    }

    #[test]
    fn an_async_launch_names_the_helper_and_its_meta_ends_it() {
        let (mut s, output) = linked_parent();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!(
            (st.activity, st.certainty, st.reason),
            (Activity::BackgroundWork, Certainty::Inferred, Reason::BackgroundHelpers)
        );
        assert_eq!(st.background.len(), 1, "the launch is known before its file is read");
        assert_eq!(st.background[0].id, output);

        let helper = running_helper();
        s.feed_helper(&output, helper.as_bytes(), ctx(0)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.background.len(), st.background[0].id.as_str()), (1, output.as_str()));
        assert_eq!(st.background[0].activity, Activity::RunningTool);

        s.feed_helper_meta(&output, br#"{"finished":false}"#);
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).background.len(), 1, "false is still running");

        s.feed_helper_meta(&output, br#"{"finished":true,"stoppedByUser":false}"#);
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.activity, st.background.len()), (Activity::Done, 0), "the meta end beats a pending tool");
    }

    #[test]
    fn malformed_or_someone_elses_meta_does_not_end_a_helper() {
        let (mut s, output) = linked_parent();
        s.feed_helper(&output, running_helper().as_bytes(), ctx(0)).unwrap();
        s.feed_helper_meta(&output, b"{not json");
        s.feed_helper_meta("/h/.claude/projects/-w-app/s/subagents/agent-other.jsonl", br#"{"finished":true}"#);
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).background.len(), 1);
        s.feed_helper_meta(&output, br#"{"stoppedByUser":true}"#);
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).background.len(), 0);
    }

    #[test]
    fn a_matching_notification_ends_a_still_running_helper() {
        let (mut s, output) = linked_parent();
        s.feed_helper(&output, running_helper().as_bytes(), ctx(0)).unwrap();
        let note = jsonl(&[json!({"type": "user", "origin": {"kind": "task-notification"},
                                  "timestamp": crate::testutil::at(0.5),
                                  "message": {"content": "<task-notification><task-id>task-a1</task-id>\
                                               <tool-use-id>toolu_1</tool-use-id><status>completed</status>\
                                               </task-notification>"}})]);
        let offset = s.consumed();
        s.feed(note.as_bytes(), ctx(offset)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.activity, st.background.len()), (Activity::Done, 0));
    }

    #[test]
    fn is_async_and_background_task_id_are_enough_to_link_a_launch() {
        let parent = jsonl(&[
            json!({"type": "user", "timestamp": crate::testutil::at(3.0), "sourceToolUseID": "toolu_9",
                   "toolUseResult": {"isAsync": true, "backgroundTaskId": "task-9"},
                   "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_9", "content": "launched"}]}}),
            json!({"type": "assistant", "timestamp": crate::testutil::at(2.0),
                   "message": {"id": "m2", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                               "content": [{"type": "text", "text": "done"}]}}),
        ]);
        let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        s.feed(parent.as_bytes(), ctx(0)).unwrap();
        let st = s.state(Timestamp::from_secs_f64(NOW));
        assert_eq!((st.activity, st.background[0].id.as_str()), (Activity::BackgroundWork, "task-9"));
        let note = jsonl(&[json!({"type": "user", "origin": {"kind": "task-notification"},
                                  "timestamp": crate::testutil::at(0.5),
                                  "message": {"content": "<task-notification><task-id>task-9</task-id><status>completed</status></task-notification>"}})]);
        let offset = s.consumed();
        s.feed(note.as_bytes(), ctx(offset)).unwrap();
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).activity, Activity::Done);
    }

    #[test]
    fn helpers_writes_keep_the_newest() {
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        for t in [5.0, 20.0, 10.0] {
            s.helper_written(Timestamp::from_secs_f64(t));
        }
        s.transcript_written(Timestamp::from_secs_f64(10.0));
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).last_write, Some(Timestamp::from_secs_f64(20.0)));
    }
}
