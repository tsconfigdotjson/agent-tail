//! The sans-IO core: bytes of one transcript in, its session's state out. Local files and
//! chunks shipped from another machine feed the same code.

use crate::error::Error;
use crate::harness::Harness;
use crate::lean::lean;
use crate::session::Session;
use crate::state::{Diagnostics, SessionState};
use crate::time::Timestamp;
use serde_json::Value;
use std::borrow::Cow;
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
        }
    }

    /// Tell the reducer where its transcript is before any of it is fed (a chunk's
    /// [`ChunkCtx::path`] does too): a session can be known by its path alone (Cursor's
    /// project) while its file is still empty. Only the first path counts.
    pub fn set_path(&mut self, path: &Path) {
        if self.path.is_none() {
            self.session = Session::at(self.session.harness, self.options.recent_lines, path);
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

    /// The session's state as of `now`: a pure function of what was fed and `now`.
    pub fn state(&self, now: Timestamp) -> SessionState {
        self.session.state(now, &self.options, self.diagnostics)
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
        *self = SessionReducer::new(self.session.harness, self.options.clone());
        if let Some(path) = path {
            self.set_path(&path);
        }
    }

    #[cfg(test)]
    pub(crate) fn session(&self) -> &Session {
        &self.session
    }
}

/// `line` parsed in `mode`, and whether anything was cut first.
fn parse(line: &[u8], mode: ParseMode) -> (serde_json::Result<Value>, bool) {
    let bytes = match mode {
        ParseMode::Lean => lean(line),
        ParseMode::Full => Cow::Borrowed(line),
    };
    (serde_json::from_slice(&bytes), matches!(bytes, Cow::Owned(_)))
}

/// Hands each entry in `data`'s complete lines to `f`, one at a time, and returns how many
/// bytes those lines took. A partially written entry at the end is left for next time;
/// lines that aren't JSON are skipped (and counted).
///
/// A last entry with no newline after it yet counts once it's a whole JSON object (no
/// shorter part of one is): a writer that adds the newline before each entry, or late,
/// would otherwise keep its last entry (Cursor's "turn ended", say) from being read.
pub(crate) fn complete_lines(data: &[u8], mode: ParseMode, diag: &mut Diagnostics, mut f: impl FnMut(Value)) -> usize {
    let end = whole_lines(data, mode, diag, &mut f);
    if ends_an_object(&data[end..])
        && let (Ok(Value::Object(e)), cut) = parse(&data[end..], mode)
    {
        diag.lines += 1;
        diag.cut += u64::from(cut);
        f(Value::Object(e));
        return data.len();
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
        let (parsed, cut) = parse(line, mode);
        diag.cut += u64::from(cut);
        match parsed {
            Ok(e) => {
                diag.lines += 1;
                f(e);
            }
            Err(_) if line.trim_ascii().is_empty() => {}
            Err(_) => diag.unparsed += 1,
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
    fn helpers_writes_keep_the_newest() {
        let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
        for t in [5.0, 20.0, 10.0] {
            s.helper_written(Timestamp::from_secs_f64(t));
        }
        s.transcript_written(Timestamp::from_secs_f64(10.0));
        assert_eq!(s.state(Timestamp::from_secs_f64(NOW)).last_write, Some(Timestamp::from_secs_f64(20.0)));
    }
}
