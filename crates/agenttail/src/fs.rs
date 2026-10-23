//! Finding and tailing transcripts on this machine (the `fs` feature): a [`Tracker`] polls
//! each harness's folder, reads what was appended since the last poll (new bytes only), and
//! feeds it to each session's [`SessionReducer`]. Nothing is ever written.
//!
//! Polling hundreds of projects and a year of Codex history every second has to be cheap
//! (tvdash #139): a folder whose mtime hasn't changed isn't listed again, Codex day folders
//! dated before the window are skipped, and every folder is looked through only every
//! [`SWEEP_EVERY`].

use crate::harness::{Harness, Role};
use crate::reducer::{ChunkCtx, ReducerOptions, SessionReducer};
use crate::state::SessionState;
use crate::time::{Timestamp, epoch};
use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often a poll reads every transcript folder, not only those that can hold sessions
/// started within the window. Sessions started earlier that come back to life (a resumed
/// Codex session writes to its old day folder) show up at most this late.
pub const SWEEP_EVERY: Duration = Duration::from_secs(10);

/// How much of a transcript is read at once (tvdash #203): a session's first read holds
/// this, and the line it ends in, rather than the whole file (a long session is hundreds of
/// MB).
pub const READ_CHUNK: u64 = 1 << 20;

/// Which sessions a [`Tracker`] follows.
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    /// Sessions whose transcript (or a helper's) was written this recently.
    pub window: Duration,
    /// The harnesses to read. None: every one, including ones added later. Empty: none.
    pub harnesses: Option<Vec<Harness>>,
}

impl Filter {
    /// Every harness's sessions active within `window`.
    pub fn within(window: Duration) -> Self {
        Filter { window, harnesses: None }
    }

    fn shows(&self, h: Harness) -> bool {
        self.harnesses.as_ref().is_none_or(|hs| hs.contains(&h))
    }
}

/// Which session a state is: its harness and its transcript's path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub harness: Harness,
    pub path: PathBuf,
}

/// Local sessions, read from this machine's transcripts.
///
/// ```no_run
/// use agenttail::{Filter, Tracker, Timestamp};
/// use std::time::{Duration, SystemTime};
///
/// let home = std::env::var_os("HOME").unwrap();
/// let mut t = Tracker::new(home.as_ref(), Filter::within(Duration::from_secs(3600)));
/// for (key, state) in t.poll(Timestamp::from(SystemTime::now())) {
///     println!("{} {:?} {:?}", key.path.display(), state.activity, state.reason);
/// }
/// ```
pub struct Tracker {
    roots: Vec<(Harness, PathBuf)>,
    filter: Filter,
    options: ReducerOptions,
    sessions: HashMap<PathBuf, Followed>,
    /// Helpers' transcripts (see `Layout::helpers_in`) in the window, with the session each
    /// helps: followed between sweeps as sessions are.
    helpers: HashMap<PathBuf, (Harness, PathBuf)>,
    /// When a poll last read every folder; None: the next one does.
    swept: Option<Timestamp>,
    /// Each folder looked into by the last poll, as it was then. Between sweeps a folder that
    /// hasn't changed since isn't read again: a transcript created, renamed or deleted in it
    /// would have changed its mtime, and the ones it holds that are being followed are
    /// looked at anyway. With hundreds of projects, reading every one each second cost
    /// thousands of `stat`s.
    folders: HashMap<PathBuf, Folder>,
}

/// A transcript being followed.
struct Followed {
    reducer: SessionReducer,
    /// The file's identity (its inode), to tell a replaced file from a grown one.
    ino: u64,
}

/// A folder as a poll last read it.
struct Folder {
    mtime: SystemTime,
    /// The folders in it.
    folders: Vec<PathBuf>,
}

impl Tracker {
    /// The sessions under `home` (each harness's [`Layout::root`](crate::Layout::root) in
    /// it) that `filter` lets through.
    pub fn new(home: &Path, filter: Filter) -> Self {
        Self::with_roots(Harness::ALL.iter().map(|h| (*h, home.join(h.layout().root))).collect(), filter)
    }

    /// Sessions in the given roots, one per harness (a harness's folder moved by its own
    /// setting, say).
    pub fn with_roots(roots: Vec<(Harness, PathBuf)>, filter: Filter) -> Self {
        Tracker {
            roots,
            filter,
            options: ReducerOptions::default(),
            sessions: HashMap::new(),
            helpers: HashMap::new(),
            swept: None,
            folders: HashMap::new(),
        }
    }

    /// Read with `options` (sessions already followed are read again).
    pub fn with_options(mut self, options: ReducerOptions) -> Self {
        self.options = options;
        self.sessions.clear();
        self
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Follow what `filter` lets through from the next `poll` on. A wider window reads the
    /// older transcripts it now takes in; sessions it leaves out are forgotten, and read
    /// again if they come back.
    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.swept = None;
    }

    /// Read whatever was appended since the last poll; return the sessions in the window
    /// as of `now`, most urgent first (by [`StatusClass`](crate::StatusClass)), then most
    /// recently written.
    pub fn poll(&mut self, now: Timestamp) -> Vec<(SessionKey, SessionState)> {
        let cutoff = (now - self.filter.window).to_system_time();
        // A clock that went back also sweeps.
        let sweep = self
            .swept
            .is_none_or(|t| !(0.0..SWEEP_EVERY.as_secs_f64()).contains(&(now.as_secs_f64() - t.as_secs_f64())));
        if sweep {
            self.swept = Some(now);
        }
        let mut live = HashSet::new();
        let mut helpers = HashMap::new();
        let mut helped: HashMap<PathBuf, f64> = HashMap::new();
        let mut transcripts = Vec::new();
        for (harness, path, meta, role) in self.transcripts(cutoff, sweep) {
            let Ok(mtime) = meta.modified() else { continue };
            match role {
                Role::Helper(owner) if mtime >= cutoff => {
                    let t = helped.entry(owner.clone()).or_default();
                    *t = t.max(epoch(mtime));
                    helpers.insert(path, (harness, owner));
                }
                Role::Helper(_) => {}
                Role::Session => transcripts.push((harness, path, meta, mtime)),
            }
        }
        for (harness, path, meta, mtime) in transcripts {
            // A session at work through a helper (a long subagent) is still in the window.
            if mtime < cutoff && helped.get(&path).is_none_or(|&t| t < epoch(cutoff)) {
                continue;
            }
            live.insert(path.clone());
            let options = &self.options;
            let f = self.sessions.entry(path.clone()).or_insert_with(|| Followed::new(harness, options, &path));
            // Truncated, or replaced by another file: read it from the start.
            let consumed = f.reducer.consumed();
            if meta.len() < consumed || (consumed > 0 && f.ino != identity(&meta)) {
                *f = Followed::new(harness, options, &path);
            }
            f.ino = identity(&meta);
            let mtime = Timestamp::from(mtime);
            f.reducer.transcript_written(mtime);
            if meta.len() > f.reducer.consumed() {
                read_new(&path, &mut f.reducer, mtime);
            }
        }
        self.sessions.retain(|p, _| live.contains(p));
        self.helpers = helpers;
        for (owner, t) in helped {
            if let Some(f) = self.sessions.get_mut(&owner) {
                f.reducer.helper_written(Timestamp::from_secs_f64(t));
            }
        }

        let mut views: Vec<_> = self
            .sessions
            .iter()
            .map(|(p, f)| {
                let key = SessionKey { harness: f.reducer.harness(), path: p.clone() };
                (f.reducer.active_at(), key, f.reducer.state(now))
            })
            .collect();
        views.sort_by(|(ma, ka, a), (mb, kb, b)| {
            a.activity.class().cmp(&b.activity.class()).then(mb.total_cmp(ma)).then_with(|| ka.cmp(kb))
        });
        views.into_iter().map(|(_, k, s)| (k, s)).collect()
    }

    /// The transcripts of shown harnesses, each `stat`ed once: with `sweep`, every one;
    /// otherwise those in folders that may hold sessions started since `cutoff` and changed
    /// since the last poll, plus the ones already being followed.
    fn transcripts(&mut self, cutoff: SystemTime, sweep: bool) -> Vec<Transcript> {
        let since = (!sweep).then_some(cutoff);
        let mut files = Vec::new();
        let mut walk =
            Walk { since, before: std::mem::take(&mut self.folders), after: HashMap::new(), out: &mut files };
        for (harness, root) in self.roots.iter().filter(|(h, _)| self.filter.shows(*h)) {
            walk.harness(root, *harness);
        }
        self.folders = walk.after;
        if !sweep {
            let found: HashSet<PathBuf> = files.iter().map(|(_, p, _, _)| p.clone()).collect();
            let sessions = self.sessions.iter().map(|(p, f)| (p, f.reducer.harness(), Role::Session));
            let helpers = self.helpers.iter().map(|(p, (h, owner))| (p, *h, Role::Helper(owner.clone())));
            for (path, harness, role) in sessions.chain(helpers) {
                if !found.contains(path)
                    && self.filter.shows(harness)
                    && let Ok(meta) = std::fs::symlink_metadata(path)
                {
                    files.push((harness, path.clone(), meta, role));
                }
            }
        }
        files
    }
}

impl Followed {
    fn new(harness: Harness, options: &ReducerOptions, path: &Path) -> Self {
        let mut reducer = SessionReducer::new(harness, options.clone());
        reducer.set_path(path);
        Followed { reducer, ino: 0 }
    }
}

/// A file's identity: its inode where there are inodes.
fn identity(meta: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ino(meta)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

/// A transcript found by a poll; a helper's names the session it helps by its full path.
type Transcript = (Harness, PathBuf, Metadata, Role);

/// One poll's look through the transcript folders.
struct Walk<'a> {
    /// None: a sweep, reading every folder. Otherwise, folders the harness's layout says
    /// can't hold sessions started since then are skipped, and so are the transcripts in
    /// folders that haven't changed since `before`.
    since: Option<SystemTime>,
    before: HashMap<PathBuf, Folder>,
    after: HashMap<PathBuf, Folder>,
    out: &'a mut Vec<Transcript>,
}

impl Walk<'_> {
    /// `harness`'s transcripts under `root`.
    fn harness(&mut self, root: &Path, harness: Harness) {
        self.folder(root, root, harness, harness.layout().walk_depth());
    }

    /// Every `*.jsonl` file in `dir` that `harness` takes (see `Layout::role`), and in the
    /// folders under it, at most `depth` levels down (1 = in `dir` itself), with its
    /// metadata; only those files are `stat`ed, and each folder. Symlinks are left out, as a
    /// remote loop's `find` leaves them. (A folder's mtime can't tell whether the
    /// transcripts in it changed: appending to a file doesn't change it. It does tell when
    /// one was added.)
    fn folder(&mut self, root: &Path, dir: &Path, harness: Harness, depth: usize) {
        if depth == 0 {
            return;
        }
        let layout = harness.layout();
        let Ok(mtime) = std::fs::metadata(dir).and_then(|m| m.modified()) else { return };
        let unchanged = self.since.is_some() && self.before.get(dir).is_some_and(|f| f.mtime == mtime);
        let folders = match self.before.remove(dir).filter(|_| unchanged) {
            Some(f) => f.folders,
            None => {
                let mut folders = Vec::new();
                for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let Ok(kind) = entry.file_type() else { continue };
                    let path = entry.path();
                    if kind.is_dir() {
                        folders.push(path);
                    } else if kind.is_file()
                        && path.extension().is_some_and(|e| e == "jsonl")
                        && let Some(role) = layout.role(path.strip_prefix(root).unwrap_or(&path))
                        && let Ok(meta) = entry.metadata()
                    {
                        let role = match role {
                            Role::Helper(owner) => Role::Helper(root.join(owner)),
                            r => r,
                        };
                        self.out.push((harness, path, meta, role));
                    }
                }
                folders
            }
        };
        for path in &folders {
            let rel = path.strip_prefix(root).unwrap_or(path);
            // Inside a project, only the sessions' folder.
            let wanted = layout.sessions_in.is_none_or(|w| rel.components().nth(1).is_none_or(|c| c.as_os_str() == w));
            if !wanted || !self.since.is_none_or(|t| layout.may_hold_since(rel, Timestamp::from(t))) {
                continue;
            }
            // Between sweeps, a folder of a session's own (Cursor's <id>, which a long
            // history has thousands of) that nothing was added to since the window began
            // isn't looked into: its transcripts being followed are looked at anyway, and
            // the next sweep finds what's new there.
            if let Some(t) = self.since
                && layout.sessions_in.is_some()
                && rel.components().count() > 2
                && self.before.get(path).is_some_and(|f| f.mtime < t)
            {
                self.keep(path);
                continue;
            }
            self.folder(root, path, harness, depth - 1);
        }
        self.after.insert(dir.to_path_buf(), Folder { mtime, folders });
    }

    /// Carry `dir`'s last look, and its folders', over to the next poll without reading them.
    fn keep(&mut self, dir: &Path) {
        if let Some(f) = self.before.remove(dir) {
            for sub in &f.folders {
                self.keep(sub);
            }
            self.after.insert(dir.to_path_buf(), f);
        }
    }
}

fn read_new(path: &Path, s: &mut SessionReducer, mtime: Timestamp) {
    let Ok(mut f) = File::open(path) else { return };
    if f.seek(SeekFrom::Start(s.consumed())).is_err() {
        return;
    }
    read_from(&mut f, s, READ_CHUNK, mtime, path);
}

/// Take in what `f` holds (it's at `s.consumed()`), `chunk` bytes at a time: the complete
/// lines of each, and at the end a whole last entry with no newline. A line longer than
/// `chunk` is read whole, as it must be. What the reducer held back from the last read is
/// read again, so nothing but a partial line's start is held between polls.
pub fn read_from(f: &mut impl Read, s: &mut SessionReducer, chunk: u64, mtime: Timestamp, path: &Path) {
    let mut offset = s.consumed();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if f.by_ref().take(chunk).read_to_end(&mut buf).is_err() {
            return;
        }
        let end = (buf.len() as u64) < chunk;
        let ctx = ChunkCtx { offset, mtime, path };
        let fed = if end { s.feed(&buf, ctx) } else { s.feed_part(&buf, ctx) };
        if end || fed.is_err() {
            return;
        }
        offset += buf.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::testutil::{jsonl, label, log, rfc3339};
    use crate::time::civil_from_days;
    use serde_json::{Value, json};
    use std::io::Write;

    /// The wall clock: these tests work with real files, whose mtimes the OS sets.
    fn wall() -> Timestamp {
        Timestamp::from(SystemTime::now())
    }

    /// RFC 3339 timestamp `secs_ago` seconds before the wall clock.
    fn at(secs_ago: f64) -> String {
        rfc3339(wall().as_secs_f64() - secs_ago)
    }

    fn hours(h: f64) -> Filter {
        Filter::within(Duration::from_secs_f64(h * 3600.0))
    }

    fn filter(h: f64, harnesses: Option<&[Harness]>) -> Filter {
        Filter { harnesses: harnesses.map(<[Harness]>::to_vec), ..hours(h) }
    }

    fn set_age(path: &Path, secs: u64) {
        let f = File::options().append(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    fn append(path: &Path, text: &str) {
        File::options().append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    fn claude_prompt(cwd: &str, text: &str, ago: f64) -> Value {
        json!({"type": "user", "timestamp": at(ago), "cwd": cwd, "message": {"role": "user", "content": text}})
    }

    fn claude_entry(kind: &str, content: Value, stop: &str, ago: f64) -> Value {
        json!({"type": kind, "timestamp": at(ago),
               "message": {"id": format!("m{ago}"), "stop_reason": stop, "usage": {"output_tokens": 1}, "content": content}})
    }

    /// Each session's project and tvdash's label, sorted.
    fn polled(t: &mut Tracker) -> Vec<(String, String)> {
        let mut v: Vec<_> =
            t.poll(wall()).iter().map(|(_, s)| (s.meta.project.clone(), label(s).to_string())).collect();
        v.sort();
        v
    }

    fn labels(t: &mut Tracker) -> Vec<&'static str> {
        t.poll(wall()).iter().map(|(_, s)| label(s)).collect()
    }

    fn recent(s: &SessionState) -> Vec<String> {
        s.recent.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn tracker_reads_only_complete_lines() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/proj");
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join("s1.jsonl");

        let tool = json!([{"type": "tool_use", "name": "Bash", "input": {"command": "make"}}]);
        let next = claude_entry("assistant", tool, "tool_use", 0.5).to_string();
        let (head, tail) = next.split_at(next.len() / 2);
        std::fs::write(&path, jsonl(&[claude_prompt("/w/proj", "build it", 1.0)]) + head).unwrap();

        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(labels(&mut t), ["thinking"], "the half-written entry is not applied yet");

        append(&path, &format!("{tail}\n"));
        let v = t.poll(wall());
        assert_eq!(label(&v[0].1), "working");
        assert_eq!(recent(&v[0].1), ["› build it", "$ make"]);
        assert_eq!(v[0].0, SessionKey { harness: Harness::ClaudeCode, path });
    }

    #[test]
    fn tracker_windows_and_sorts_sessions() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".claude/projects");
        let write = |name: &str, last: Value| {
            let project = root.join(name);
            std::fs::create_dir_all(&project).unwrap();
            let path = project.join("s.jsonl");
            std::fs::write(&path, jsonl(&[claude_prompt(&format!("/w/{name}"), "go", 5.0), last])).unwrap();
            path
        };
        let done = write("done", claude_entry("assistant", json!([]), "end_turn", 1.0));
        write(
            "asking",
            claude_entry(
                "assistant",
                json!([{"type": "tool_use", "name": "AskUserQuestion", "input": {}}]),
                "tool_use",
                1.0,
            ),
        );
        write("working", claude_entry("user", json!([{"type": "tool_result", "content": "ok"}]), "", 1.0));
        let old = write("old", claude_entry("assistant", json!([]), "end_turn", 1.0));
        set_age(&old, 3 * 3600);

        let mut t = Tracker::new(home.path(), hours(1.0));
        let order: Vec<_> = t.poll(wall()).into_iter().map(|(_, s)| s.meta.project).collect();
        assert_eq!(order, ["asking", "working", "done"]);

        set_age(&done, 2 * 3600);
        let order: Vec<_> = t.poll(wall()).into_iter().map(|(_, s)| s.meta.project).collect();
        assert_eq!(order, ["asking", "working"], "sessions leave once they age out of the window");
    }

    /// One Claude Code session last touched two hours ago and one Codex session now.
    fn two_harnesses(home: &Path) {
        let project = home.join(".claude/projects/-w-site");
        std::fs::create_dir_all(&project).unwrap();
        let claude = project.join("s.jsonl");
        std::fs::write(&claude, jsonl(&[claude_prompt("/w/site", "hi", 2.0 * 3600.0)])).unwrap();
        set_age(&claude, 2 * 3600);
        let day = home.join(".codex/sessions/2026/10/15");
        std::fs::create_dir_all(&day).unwrap();
        let codex = [
            json!({"timestamp": at(2.0), "type": "session_meta", "payload": {"cwd": "/w/app"}}),
            json!({"timestamp": at(1.0), "type": "event_msg", "payload": {"type": "user_message", "message": "hello"}}),
        ];
        std::fs::write(day.join("rollout-x.jsonl"), jsonl(&codex)).unwrap();
    }

    #[test]
    fn tracker_follows_a_new_filter() {
        let home = tempfile::tempdir().unwrap();
        two_harnesses(home.path());
        let ids = |t: &mut Tracker| {
            let mut v: Vec<_> = t.poll(wall()).into_iter().map(|(k, s)| (k.harness.id(), s.meta.project)).collect();
            v.sort();
            v
        };
        let (claude, codex) = (Harness::ClaudeCode, Harness::Codex);
        let mut t = Tracker::new(home.path(), filter(1.0, None));
        assert_eq!(ids(&mut t), [("codex", "app".to_string())], "Claude Code's is out of the window");

        t.set_filter(filter(3.0, None));
        let both = [("claude_code", "site".to_string()), ("codex", "app".to_string())];
        assert_eq!(ids(&mut t), both, "a wider window reads the older transcript");
        t.set_filter(filter(3.0, Some(&[codex])));
        assert_eq!(ids(&mut t), [("codex", "app".to_string())]);
        t.set_filter(filter(3.0, Some(&[claude])));
        assert_eq!(ids(&mut t), [("claude_code", "site".to_string())], "read again when it comes back");
        t.set_filter(filter(3.0, Some(&[])));
        assert_eq!(ids(&mut t), [], "no harnesses: no sessions");
        assert_eq!(t.filter(), &filter(3.0, Some(&[])));

        t.set_filter(filter(3.0, Some(&[claude, codex])));
        assert_eq!(ids(&mut t), both, "every harness, by name");
        t.set_filter(filter(1.0, None));
        assert_eq!(ids(&mut t), [("codex", "app".to_string())], "a narrower window drops it again");
    }

    /// The date `days` days after the epoch as a Codex day folder: YYYY/MM/DD.
    fn day_folder(days: i64) -> String {
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}/{m:02}/{d:02}")
    }

    #[test]
    fn polls_between_sweeps_stat_only_the_codex_days_in_the_window() {
        let home = tempfile::tempdir().unwrap();
        let today = (wall().as_secs_f64() / 86400.0).floor() as i64;
        let day = |ago: i64| home.path().join(".codex/sessions").join(day_folder(today - ago));
        let codex = |cwd: &str| {
            jsonl(&[
                json!({"timestamp": at(5.0), "type": "session_meta", "payload": {"cwd": cwd}}),
                json!({"timestamp": at(4.0), "type": "event_msg", "payload": {"type": "user_message", "message": "go"}}),
            ])
        };
        // A year of finished sessions: 365 day folders × 20 rollouts, none touched lately.
        for ago in 0..365 {
            std::fs::create_dir_all(day(ago)).unwrap();
            for i in 0..20 {
                let path = day(ago).join(format!("rollout-{i}.jsonl"));
                File::create(&path).unwrap(); // empty: never read, and light on the disk
                set_age(&path, 3 * 3600 + ago as u64 * 86400);
            }
        }
        // One session started just now, one started a month ago and still going, and one
        // from half a year ago that will be resumed.
        std::fs::write(day(0).join("rollout-new.jsonl"), codex("/w/new")).unwrap();
        let long = day(30).join("rollout-long.jsonl");
        std::fs::write(&long, codex("/w/long")).unwrap();
        let resumed = day(200).join("rollout-resumed.jsonl");
        std::fs::write(&resumed, codex("/w/resumed")).unwrap();
        set_age(&resumed, 200 * 86400);

        let mut t = Tracker::new(home.path(), hours(2.0));
        let projects = |t: &mut Tracker| {
            let mut v: Vec<_> = t.poll(wall()).into_iter().map(|(_, s)| s.meta.project).collect();
            v.sort();
            v
        };
        assert_eq!(projects(&mut t), ["long", "new"], "the first poll sweeps every folder");

        // Between sweeps: only the folders that can hold sessions started within the
        // window (a day's slack for time zones) are looked at, and only those that changed
        // since the last poll are read (#139), besides the transcripts already followed.
        let cutoff = wall() - t.filter.window;
        let first = (cutoff.as_secs_f64() / 86400.0).floor() as i64 - 1;
        let days: Vec<_> = (0..365).filter(|&ago| today - ago >= first).map(day).collect();
        assert!((2..=3).contains(&days.len()), "{days:?}");
        let cutoff = cutoff.to_system_time();
        let statted = |t: &mut Tracker| {
            let mut statted: Vec<_> = t.transcripts(cutoff, false).into_iter().map(|(_, p, _, _)| p).collect();
            statted.sort();
            statted
        };
        let mut want = vec![long.clone(), day(0).join("rollout-new.jsonl")];
        want.sort();
        assert_eq!(statted(&mut t), want, "no folder changed: only the sessions followed");
        // A session started just now is in today's folder, which changed: it's read again,
        // and the new session found between sweeps.
        std::fs::write(day(0).join("rollout-newer.jsonl"), codex("/w/newer")).unwrap();
        assert_eq!(projects(&mut t), ["long", "new", "newer"]);
        assert_eq!(statted(&mut t).len(), 3, "from then on, again only the sessions followed");

        // The resumed session is in a folder skipped between sweeps; the next sweep finds it.
        append(&resumed, &codex("/w/resumed"));
        t.swept = Some(wall());
        assert_eq!(projects(&mut t), ["long", "new", "newer"], "not until the next sweep");
        t.swept = Some(wall() - SWEEP_EVERY);
        assert_eq!(projects(&mut t), ["long", "new", "newer", "resumed"]);
        assert_eq!(t.transcripts(cutoff, false).len(), 4, "and is followed from then on");
    }

    #[test]
    fn a_clock_that_goes_back_sweeps() {
        let home = tempfile::tempdir().unwrap();
        let mut t = Tracker::new(home.path(), hours(1.0));
        t.poll(wall());
        let swept = t.swept;
        t.poll(wall() - Duration::from_secs(60));
        assert_ne!(t.swept, swept);
    }

    #[test]
    fn tracker_reads_every_harness_in_its_folder() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join(".codex/sessions/2026/10/14");
        std::fs::create_dir_all(&day).unwrap();
        let codex = [
            json!({"timestamp": at(2.0), "type": "session_meta", "payload": {"cwd": "/w/app"}}),
            json!({"timestamp": at(1.0), "type": "event_msg", "payload": {"type": "user_message", "message": "hello"}}),
        ];
        std::fs::write(day.join("rollout-x.jsonl"), jsonl(&codex)).unwrap();
        std::fs::write(day.join("rollout-old.jsonl.zst"), "compressed, not read").unwrap();
        let project = home.path().join(".claude/projects/-w-site");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("s.jsonl"), jsonl(&[claude_prompt("/w/site", "hi", 3.0)])).unwrap();
        // A Claude Code subagent's transcript sits in a folder of its own: part of the
        // session above, not another one.
        let sub = project.join("s/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-a1.jsonl"), jsonl(&[claude_prompt("/w/site", "sub", 3.0)])).unwrap();

        let mut t = Tracker::new(home.path(), hours(1.0));
        let mut v: Vec<_> = t.poll(wall()).into_iter().map(|(k, s)| (k.harness.id(), s.meta.project)).collect();
        v.sort();
        assert_eq!(v, [("claude_code", "site".to_string()), ("codex", "app".to_string())]);
    }

    #[test]
    fn tracker_takes_roots_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("elsewhere/-w-site");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("s.jsonl"), jsonl(&[claude_prompt("/w/site", "hi", 3.0)])).unwrap();
        let mut t = Tracker::with_roots(vec![(Harness::ClaudeCode, dir.path().join("elsewhere"))], hours(1.0));
        assert_eq!(polled(&mut t), [("site".into(), "thinking".into())]);
    }

    fn cursor_line(e: Value) -> String {
        format!("{e}\n")
    }

    fn cursor_prompt(text: &str) -> Value {
        json!({"role": "user", "message": {"content": [{"type": "text", "text": format!("<user_query>{text}</user_query>")}]}})
    }

    fn cursor_tool(name: &str) -> Value {
        json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": name, "input": {"command": "make"}}]}})
    }

    /// `home`'s Cursor transcript of session `id` in project folder `project`, written.
    fn cursor_session(home: &Path, project: &str, id: &str, entries: &[Value]) -> PathBuf {
        let path = home.join(format!(".cursor/projects/{project}/agent-transcripts/{id}/{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, entries.iter().cloned().map(cursor_line).collect::<String>()).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn tracker_reads_cursor_sessions_where_cursor_keeps_them() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let mut t = Tracker::new(h, hours(1.0));
        assert!(t.poll(wall()).is_empty(), "no Cursor folder: nothing, and no error");

        let one = cursor_session(h, "Users-sam-Work-demo-app", "a1", &[cursor_prompt("go"), cursor_tool("Shell")]);
        cursor_session(
            h,
            "Users-sam-Work-shade",
            "b2",
            &[cursor_prompt("hi"), json!({"type": "turn_ended", "status": "success"})],
        );
        // Not sessions: other folders and files Cursor keeps, a subagent, a link.
        let decoy = jsonl(&[cursor_prompt("not a session")]);
        for rel in [
            "Users-sam-Work-shade/terminals/1.jsonl",
            "Users-sam-Work-shade/x.jsonl",
            "loose.jsonl",
            "Users-sam-Work-shade/agent-transcripts/b2/notes/n.jsonl",
            "Users-sam-Work-shade/agent-transcripts/b2/subagents/s1.jsonl",
        ] {
            let path = h.join(".cursor/projects").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &decoy).unwrap();
        }
        let outside = h.join("elsewhere.jsonl");
        std::fs::write(&outside, &decoy).unwrap();
        let link = h.join(".cursor/projects/Users-sam-Work-shade/agent-transcripts/link");
        std::fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(&outside, link.join("link.jsonl")).unwrap();
        std::os::unix::fs::symlink(
            h.join(".cursor/projects/Users-sam-Work-demo-app"),
            h.join(".cursor/projects/Users-sam-Work-alias"),
        )
        .unwrap();

        let v = t.poll(wall());
        assert_eq!(v.len(), 2, "{:?}", v.iter().map(|(_, s)| &s.meta.project).collect::<Vec<_>>());
        assert!(v.iter().all(|(k, s)| k.harness == Harness::Cursor && s.harness == Harness::Cursor));
        assert_eq!(polled(&mut t), [("app".into(), "working".into()), ("shade".into(), "done".into())]);

        // The project's name, once the transcript tells it.
        let read = json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"path": "/Users/sam/Work/demo-app/a.rs"}}]}});
        append(&one, &cursor_line(read));
        assert_eq!(polled(&mut t)[0], ("demo-app".into(), "working".into()));

        // The harness switch.
        t.set_filter(filter(1.0, Some(&[Harness::ClaudeCode, Harness::Codex])));
        assert!(t.poll(wall()).is_empty());
        t.set_filter(filter(1.0, Some(&[Harness::Cursor])));
        assert_eq!(t.poll(wall()).len(), 2);
    }

    #[test]
    fn cursor_sessions_go_by_the_files_time() {
        let home = tempfile::tempdir().unwrap();
        let path = cursor_session(home.path(), "w-old", "a1", &[cursor_prompt("go"), cursor_tool("Shell")]);
        set_age(&path, 2 * 3600);
        let mut t = Tracker::new(home.path(), hours(3.0));
        let now = wall();
        let v = t.poll(now);
        let s = &v[0].1;
        assert_eq!(
            (s.activity.class(), label(s)),
            (Status::Idle, "idle"),
            "entries have no times: the file's is theirs"
        );
        let mins = (now.as_secs_f64() - s.since.unwrap().as_secs_f64()) / 60.0;
        assert!((119.0..=121.0).contains(&mins), "{mins}");

        append(&path, &cursor_line(cursor_tool("Shell")));
        set_age(&path, 60);
        let now = wall();
        let v = t.poll(now);
        let mins = (now.as_secs_f64() - v[0].1.since.unwrap().as_secs_f64()) / 60.0;
        assert_eq!((label(&v[0].1), mins < 2.0), ("may need approval", true), "a minute ago: {mins}");
    }

    #[test]
    fn a_subagent_at_work_keeps_its_session_working() {
        let home = tempfile::tempdir().unwrap();
        let task = json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Task", "input": {"description": "Look around"}}]}});
        let main = cursor_session(home.path(), "w-app", "a1", &[cursor_prompt("go"), task]);
        set_age(&main, 60);
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(polled(&mut t), [("app".into(), "may need approval".into())], "alone, a stalled tool call");

        let sub = main.parent().unwrap().join("subagents/s1.jsonl");
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        std::fs::write(&sub, cursor_line(cursor_tool("Grep"))).unwrap();
        assert_eq!(polled(&mut t), [("app".into(), "working".into())], "its subagent's writes count as its own");
        let v = t.poll(wall());
        assert_eq!(
            (v.len(), recent(&v[0].1)),
            (1, vec!["› go".to_string(), "Task Look around".into()]),
            "not a session, not read"
        );

        // Followed between sweeps, as sessions are: a subagent that stops shows.
        set_age(&sub, 60);
        t.swept = Some(wall());
        assert_eq!(polled(&mut t), [("app".into(), "working".into())], "the newest write seen stays");
        let cutoff = (wall() - t.filter.window).to_system_time();
        let followed: Vec<_> = t.transcripts(cutoff, false).into_iter().map(|(_, p, _, r)| (p, r)).collect();
        assert!(followed.contains(&(sub.clone(), Role::Helper(main.clone()))), "{followed:?}");
        // The session's own write after the subagent's: as usual.
        append(&main, &cursor_line(json!({"type": "turn_ended", "status": "success"})));
        assert_eq!(polled(&mut t), [("app".into(), "done".into())]);
    }

    #[test]
    fn a_last_line_with_no_newline_is_read_once_whole() {
        let home = tempfile::tempdir().unwrap();
        let path = cursor_session(home.path(), "w-app", "a1", &[cursor_prompt("go")]);
        let end = json!({"type": "turn_ended", "status": "success"}).to_string();
        let (head, tail) = end.split_at(10);
        append(&path, head);
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(polled(&mut t)[0].1, "thinking", "half an entry waits");
        append(&path, tail);
        assert_eq!(polled(&mut t)[0].1, "done", "a whole one is read without its newline");
        append(&path, &format!("\n{}", cursor_line(cursor_prompt("more"))));
        let v = t.poll(wall());
        assert_eq!(
            (label(&v[0].1), recent(&v[0].1)),
            ("thinking", vec!["› go".to_string(), "› more".into()]),
            "and not read twice"
        );
    }

    /// Read a chunk at a time (#203), a transcript comes to what it does read whole: a line
    /// longer than a chunk, lines split across chunks, and a last entry with no newline.
    #[test]
    fn reads_a_chunk_at_a_time_as_if_whole() {
        let path = Path::new("/h/.cursor/projects/w-app/agent-transcripts/a1/a1.jsonl");
        let long = json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Shell", "input": {"command": format!("echo {}", "x".repeat(300))}}]}});
        let lines: String = [cursor_prompt("one"), long, cursor_prompt("two")].into_iter().map(cursor_line).collect();
        let end = json!({"type": "turn_ended", "status": "success"}).to_string();
        let now = Timestamp::from_secs_f64(crate::testutil::NOW);
        let read = |text: &str, chunk: u64| {
            let mut s = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
            s.set_path(path);
            read_from(&mut text.as_bytes(), &mut s, chunk, now, path);
            (s.consumed(), log(s.session()), label(&s.state(now)))
        };
        let text = lines.clone() + &end;
        let whole = read(&text, u64::MAX);
        assert_eq!(whole.0, text.len() as u64, "all of it, the last entry too");
        assert_eq!(whole.2, "done");
        assert_eq!(whole.1.len(), 3, "{:?}", whole.1);
        for chunk in [1, 7, 64, 100, text.len() as u64, READ_CHUNK] {
            assert_eq!(read(&text, chunk), whole, "{chunk} bytes at a time");
        }
        // Half an entry at the end waits for the rest, however it's read.
        let cut = lines.clone() + &end[..10];
        for chunk in [1, 7, 64, u64::MAX] {
            assert_eq!(read(&cut, chunk).0, lines.len() as u64, "{chunk} bytes at a time");
        }
    }

    #[test]
    fn a_truncated_or_replaced_transcript_is_read_again() {
        let home = tempfile::tempdir().unwrap();
        let path = cursor_session(
            home.path(),
            "w-app",
            "a1",
            &[cursor_prompt("one"), cursor_prompt("two"), cursor_prompt("three")],
        );
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(recent(&t.poll(wall())[0].1), ["› one", "› two", "› three"]);
        std::fs::write(&path, cursor_line(cursor_prompt("new"))).unwrap();
        assert_eq!(recent(&t.poll(wall())[0].1), ["› new"], "shorter: from the start");
        // Replaced by a longer file (written aside, renamed over it).
        let aside = path.with_extension("tmp");
        let four = [cursor_prompt("first"), cursor_prompt("second"), cursor_prompt("third"), cursor_prompt("fourth")];
        std::fs::write(&aside, jsonl(&four)).unwrap();
        std::fs::rename(&aside, &path).unwrap();
        #[cfg(unix)]
        assert_eq!(
            recent(&t.poll(wall())[0].1),
            ["› first", "› second", "› third", "› fourth"],
            "another file: from the start"
        );
    }

    #[test]
    fn broken_and_huge_lines_dont_stall_the_tailer() {
        let home = tempfile::tempdir().unwrap();
        let huge =
            json!({"role": "assistant", "message": {"content": [{"type": "text", "text": "x".repeat(3 << 20)}]}});
        let path = cursor_session(home.path(), "w-app", "a1", &[cursor_prompt("go")]);
        let junk =
            format!("not json\n{{\"role\":\n\u{0}\u{1}\n[1,2]\n\"str\"\n{{}}\n{}\n\n{huge}\n", "{".repeat(100_000));
        append(&path, &junk);
        append(&path, &cursor_line(cursor_tool("Shell")));
        let mut t = Tracker::new(home.path(), hours(1.0));
        let v = t.poll(wall());
        assert_eq!((label(&v[0].1), recent(&v[0].1)), ("working", vec!["› go".to_string(), "$ make".into()]));
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(t.sessions[&path].reducer.consumed(), len, "past every line");
        assert_eq!(v[0].1.diagnostics.unparsed, 4, "{:?}", v[0].1.diagnostics);
    }

    #[test]
    fn polls_look_only_into_cursors_transcript_folders() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".cursor/projects");
        // Many projects, each with folders Cursor keeps besides its transcripts.
        for i in 0..200 {
            for other in ["terminals", "mcps/server/tools", "agent-tools"] {
                std::fs::create_dir_all(root.join(format!("w-p{i}/{other}"))).unwrap();
            }
        }
        cursor_session(home.path(), "w-p7", "a1", &[cursor_prompt("go")]);
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(polled(&mut t), [("p7".into(), "thinking".into())]);
        let mut looked: Vec<_> = t
            .folders
            .keys()
            .filter_map(|p| p.strip_prefix(&root).ok())
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        looked.sort();
        assert_eq!(
            looked.len(),
            1 + 200 + 2,
            "the root, each project, and the one transcripts folder and session folder in it"
        );
        assert!(
            looked.iter().all(|p| !p.contains("terminals") && !p.contains("mcps") && !p.contains("agent-tools")),
            "{looked:?}"
        );
    }

    #[test]
    fn polls_between_sweeps_skip_cursor_sessions_folders_untouched_since_the_window() {
        let home = tempfile::tempdir().unwrap();
        let old = |id: &str| {
            let path = cursor_session(home.path(), "w-app", id, &[cursor_prompt("old")]);
            set_age(&path, 3 * 3600);
            let dir = path.parent().unwrap();
            File::open(dir).unwrap().set_modified(SystemTime::now() - Duration::from_secs(3 * 3600)).unwrap();
            dir.to_path_buf()
        };
        let olds: Vec<_> = (0..50).map(|i| old(&format!("old{i}"))).collect();
        cursor_session(home.path(), "w-app", "now", &[cursor_prompt("go")]);
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(polled(&mut t), [("app".into(), "thinking".into())]);

        // A subagent starts in an old session's folder (it was resumed): not looked for
        // between sweeps; the old folders' last look is kept, not read again.
        let sub = olds[0].join("subagents/s1.jsonl");
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        std::fs::write(&sub, "{}\n").unwrap();
        append(&olds[0].join("old0.jsonl"), &cursor_line(cursor_prompt("again")));
        assert_eq!(polled(&mut t).len(), 1, "not until the next sweep");
        assert!(olds.iter().all(|d| t.folders.contains_key(d)), "kept for the next poll");
        // A new session, in a new folder, shows straight away.
        cursor_session(home.path(), "w-other", "new", &[cursor_prompt("hi")]);
        assert_eq!(polled(&mut t).len(), 2);
        // The sweep finds the resumed one, with its subagent.
        t.swept = Some(wall() - SWEEP_EVERY);
        assert_eq!(polled(&mut t).len(), 3);
        assert!(t.helpers.contains_key(&sub));
    }

    #[test]
    fn a_session_whose_subagent_works_on_stays_in_the_window() {
        let home = tempfile::tempdir().unwrap();
        let task =
            json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Task", "input": {}}]}});
        let main = cursor_session(home.path(), "w-app", "a1", &[cursor_prompt("go"), task]);
        set_age(&main, 40 * 60);
        let mut t = Tracker::new(home.path(), hours(0.5));
        assert!(t.poll(wall()).is_empty(), "out of the window on its own");
        let sub = main.parent().unwrap().join("subagents/s1.jsonl");
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        std::fs::write(&sub, "{}\n").unwrap();
        t.swept = None;
        assert_eq!(polled(&mut t), [("app".into(), "working".into())], "its subagent is at work");
    }

    #[test]
    fn options_reach_every_session() {
        let home = tempfile::tempdir().unwrap();
        cursor_session(
            home.path(),
            "w-app",
            "a1",
            &[cursor_prompt("one"), cursor_prompt("two"), cursor_prompt("three")],
        );
        let options = ReducerOptions { recent_lines: 2, ..Default::default() };
        let mut t = Tracker::new(home.path(), hours(1.0)).with_options(options);
        assert_eq!(recent(&t.poll(wall())[0].1), ["› two", "› three"]);
    }
}
