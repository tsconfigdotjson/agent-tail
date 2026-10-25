//! Finding and tailing transcripts on this machine (the `fs` feature): a [`Tracker`] polls
//! each harness's folder, reads what was appended since the last poll (new bytes only), and
//! feeds it to each session's [`SessionReducer`]. Nothing is ever written.
//!
//! Polling hundreds of projects and a year of Codex history every second has to be cheap
//!: a folder whose mtime hasn't changed isn't listed again, Codex day folders
//! dated before the window are skipped, and every folder is looked through only every
//! [`SWEEP_EVERY`].

use crate::harness::{Harness, Role};
#[cfg(feature = "sqlite")]
use crate::opencode::{self, OpenCodeState};
use crate::reducer::{ChunkCtx, ReducerOptions, SessionReducer};
#[cfg(feature = "sqlite")]
use crate::sqlite::{self, CursorState};
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

/// How often Cursor's `state.vscdb` is copied and read while a Cursor session is in the
/// window. The database can be several gigabytes, so its stamp is checked on every poll but
/// it is only copied at this interval.
#[cfg(feature = "sqlite")]
pub const CURSOR_STATE_DB_EVERY: Duration = Duration::from_secs(30);

/// How much of a transcript is read at once: a session's first read holds
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
/// use agent_tail::{Filter, Tracker, Timestamp};
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
    /// How much of each helper transcript has been fed to its session.
    helper_offsets: HashMap<PathBuf, u64>,
    /// When each helper's `.meta.json` was last read, and its length then.
    helper_metas: HashMap<PathBuf, (SystemTime, u64)>,
    /// When each Claude Desktop `local_<id>.json` sidecar was last read, and its length then.
    desktop_metas: HashMap<PathBuf, (SystemTime, u64)>,
    /// Codex thread titles from `session_index.jsonl`, by thread id.
    codex_index: HashMap<String, String>,
    /// When `session_index.jsonl` was last read and how long it was; None: it was missing.
    codex_index_stamp: Option<(SystemTime, u64)>,
    /// When a poll last read every folder; None: the next one does.
    swept: Option<Timestamp>,
    /// Each folder looked into by the last poll, as it was then. Between sweeps a folder that
    /// hasn't changed since isn't read again: a transcript created, renamed or deleted in it
    /// would have changed its mtime, and the ones it holds that are being followed are
    /// looked at anyway. With hundreds of projects, reading every one each second cost
    /// thousands of `stat`s.
    folders: HashMap<PathBuf, Folder>,
    /// Cursor's global `state.vscdb`, if the tracker should read one.
    #[cfg(feature = "sqlite")]
    cursor_db_path: Option<PathBuf>,
    /// The last snapshot of it, and when it was read.
    #[cfg(feature = "sqlite")]
    cursor_db: Option<CursorDatabase>,
    /// OpenCode's database snapshot. Its rows are rebuilt when the file stamp changes.
    #[cfg(feature = "sqlite")]
    opencode_db: Option<OpenCodeDatabase>,
    /// Whether `OPENCODE_DB` may move the database path (only `from_env`).
    #[cfg(feature = "sqlite")]
    opencode_env: bool,
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

/// A Cursor database snapshot and the sidecar stamps it was taken at.
#[cfg(feature = "sqlite")]
struct CursorDatabase {
    path: PathBuf,
    stamp: DbStamp,
    read_at: Timestamp,
    state: CursorState,
}

/// An OpenCode database snapshot. Its rows are a mutable state, not an append log, so a
/// changed stamp means every session in the window is rebuilt from its current rows.
#[cfg(feature = "sqlite")]
struct OpenCodeDatabase {
    path: PathBuf,
    stamp: DbStamp,
    state: OpenCodeState,
}

/// The mtimes and lengths of a database, `-wal` and `-shm`. A changed sidecar is a changed
/// database even when the main file's mtime hasn't moved.
#[cfg(feature = "sqlite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DbStamp {
    main: Option<FileStamp>,
    wal: Option<FileStamp>,
    shm: Option<FileStamp>,
}

#[cfg(feature = "sqlite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
}

impl Tracker {
    /// The sessions under `home` (each harness's [`Layout::root`](crate::Layout::root) in
    /// it) that `filter` lets through.
    pub fn new(home: &Path, filter: Filter) -> Self {
        let tracker = Self::with_roots(Self::roots(home, |_| None), filter);
        #[cfg(feature = "sqlite")]
        let tracker = {
            let mut tracker = tracker;
            tracker.cursor_db_path = Some(sqlite::state_vscdb_path(home));
            tracker
        };
        tracker
    }

    /// Like [`new`](Self::new), but honouring the harnesses' own folder overrides:
    /// `CLAUDE_CONFIG_DIR` (the config folder, whose `projects/` holds transcripts) and
    /// `CODEX_HOME` (the folder whose `sessions/` holds rollouts). Claude Code 2.1.310
    /// treats `CLAUDE_CONFIG_DIR` as one path, not a comma-separated list; a relative path
    /// is ignored, as Claude Code does.
    pub fn from_env(home: &Path, filter: Filter) -> Self {
        let tracker = Self::with_roots(Self::roots(home, |var| std::env::var_os(var)), filter);
        #[cfg(feature = "sqlite")]
        let tracker = {
            let mut tracker = tracker;
            tracker.cursor_db_path = Some(sqlite::state_vscdb_path(home));
            tracker.opencode_env = true;
            tracker
        };
        tracker
    }

    /// Each harness's root, with the overrides from its discovery spec. `env` reads a
    /// variable, so tests don't have to touch the process environment.
    fn roots(home: &Path, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<(Harness, PathBuf)> {
        let mut out = Vec::new();
        for harness in Harness::ALL {
            let discovery = harness.discovery();
            // A platform root replaces the default (Claude Desktop's mac/Linux/Windows
            // app-data folders); harnesses without one keep their single default root.
            let mut roots: Vec<PathBuf> =
                discovery.roots.for_os(std::env::consts::OS).into_iter().map(|root| home.join(root)).collect();
            for override_ in discovery.roots.env {
                let Some(value) = env(override_.var) else { continue };
                let dir = PathBuf::from(value);
                if !override_.absolute || dir.is_absolute() {
                    let root = if override_.append.is_empty() { dir } else { dir.join(override_.append) };
                    roots = vec![root];
                }
            }
            out.extend(roots.into_iter().map(|root| (harness, root)));
        }
        out
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
            helper_offsets: HashMap::new(),
            helper_metas: HashMap::new(),
            desktop_metas: HashMap::new(),
            codex_index: HashMap::new(),
            codex_index_stamp: None,
            swept: None,
            folders: HashMap::new(),
            #[cfg(feature = "sqlite")]
            cursor_db_path: None,
            #[cfg(feature = "sqlite")]
            cursor_db: None,
            #[cfg(feature = "sqlite")]
            opencode_db: None,
            #[cfg(feature = "sqlite")]
            opencode_env: false,
        }
    }

    /// Read Cursor's `state.vscdb` from `path` instead of the platform default below `home`.
    /// This is the file to point at a copy in tests or a non-standard Cursor install.
    #[cfg(feature = "sqlite")]
    pub fn with_cursor_state_db(mut self, path: impl Into<PathBuf>) -> Self {
        self.cursor_db_path = Some(path.into());
        self.cursor_db = None;
        self
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

    /// Read Codex's `session_index.jsonl` when it changed. It sits beside `sessions/`
    /// (also under `CODEX_HOME`), so the parent of the Codex root is the right place.
    fn refresh_codex_index(&mut self) {
        let Some((_, root)) = self.roots.iter().find(|(harness, _)| *harness == Harness::Codex) else { return };
        let path = root.parent().unwrap_or(root).join("session_index.jsonl");
        let stamp = std::fs::metadata(&path).and_then(|meta| Ok((meta.modified()?, meta.len()))).ok();
        if stamp == self.codex_index_stamp {
            return;
        }
        self.codex_index.clear();
        self.codex_index_stamp = stamp;
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(id) = value["id"].as_str().filter(|id| !id.is_empty()) else { continue };
            let Some(title) = value["thread_name"].as_str().map(str::trim).filter(|title| !title.is_empty()) else {
                continue;
            };
            self.codex_index.insert(id.into(), title.into());
        }
    }

    /// Read each followed Claude Desktop session's sibling `local_<id>.json` when it changes.
    /// The sidecar names the cwd, title and model; the audit stream does not repeat them.
    fn refresh_desktop_metadata(&mut self) {
        let mut live = HashSet::new();
        for (path, followed) in &mut self.sessions {
            if followed.reducer.harness() != Harness::ClaudeDesktop {
                continue;
            }
            let Some(meta_path) = claude_desktop_metadata(path) else { continue };
            let Ok(meta) = std::fs::metadata(&meta_path) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            let stamp = (mtime, meta.len());
            if self.desktop_metas.get(&meta_path) == Some(&stamp) {
                live.insert(meta_path);
                continue;
            }
            if let Ok(bytes) = std::fs::read(&meta_path) {
                followed.reducer.feed_metadata(&bytes);
                self.desktop_metas.insert(meta_path.clone(), stamp);
                live.insert(meta_path);
            }
        }
        self.desktop_metas.retain(|path, _| live.contains(path));
    }

    /// Snapshot Cursor's database when a Cursor session is in the window, at most every
    /// [`CURSOR_STATE_DB_EVERY`]. The stamp is checked every poll, so an unchanged database
    /// is not copied again.
    #[cfg(feature = "sqlite")]
    fn refresh_cursor_db(&mut self, now: Timestamp) {
        if !self.filter.shows(Harness::Cursor) {
            return;
        }
        let Some(path) = self.cursor_db_path.clone() else { return };
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, followed)| followed.reducer.harness() == Harness::Cursor)
            .filter_map(|(path, _)| cursor_composer_id(path).map(str::to_string))
            .collect();
        if ids.is_empty() {
            return;
        }
        let stamp = db_stamp(&path);
        if stamp.main.is_none() {
            self.cursor_db = None;
            return;
        }
        let due = self.cursor_db.as_ref().is_none_or(|database| {
            database.path != path
                || now.as_secs_f64() - database.read_at.as_secs_f64() >= CURSOR_STATE_DB_EVERY.as_secs_f64()
        });
        if !due {
            return;
        }
        if let Some(database) = self.cursor_db.as_mut()
            && database.path == path
            && database.stamp == stamp
        {
            database.read_at = now;
            return;
        }
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        match sqlite::read_state_vscdb_for(&path, &ids) {
            Ok(state) => self.cursor_db = Some(CursorDatabase { path, stamp, read_at: now, state }),
            Err(_) => {
                if let Some(database) = self.cursor_db.as_mut().filter(|database| database.path == path) {
                    database.stamp = stamp;
                    database.read_at = now;
                } else {
                    self.cursor_db = Some(CursorDatabase { path, stamp, read_at: now, state: CursorState::default() });
                }
            }
        }
    }

    /// The OpenCode database path: `OPENCODE_DB` (absolute, or relative to the data root),
    /// else `opencode.db`, else the newest channel database (`opencode-*.db`).
    #[cfg(feature = "sqlite")]
    fn opencode_db_path(&self) -> Option<PathBuf> {
        let (_, root) = self.roots.iter().find(|(harness, _)| *harness == Harness::OpenCode)?;
        let database = Harness::OpenCode.discovery().database.as_ref()?;
        if self.opencode_env
            && let Some(var) = database.env
            && let Some(value) = std::env::var_os(var)
        {
            let path = PathBuf::from(value);
            if path == Path::new(":memory:") {
                return None;
            }
            if !path.as_os_str().is_empty() {
                return Some(if path.is_absolute() { path } else { root.join(path) });
            }
        }
        let default = root.join(database.file);
        if default.is_file() {
            return Some(default);
        }
        let prefix = database.channel_pattern.split('*').next().unwrap_or("opencode-");
        let suffix = database.channel_pattern.split('*').nth(1).unwrap_or(".db");
        let mut candidates: Vec<_> = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix) && name.ends_with(suffix) && path.is_file())
            })
            .collect();
        candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|meta| meta.modified()).ok());
        candidates.pop()
    }

    /// Snapshot OpenCode's database when its stamp changes. Rows are rewritten in place, so
    /// there is no append offset to follow; each changed snapshot rebuilds the sessions.
    #[cfg(feature = "sqlite")]
    fn refresh_opencode_db(&mut self) {
        if !self.filter.shows(Harness::OpenCode) {
            return;
        }
        let Some(path) = self.opencode_db_path() else {
            self.opencode_db = None;
            return;
        };
        let stamp = db_stamp(&path);
        if stamp.main.is_none() {
            self.opencode_db = None;
            return;
        }
        if self.opencode_db.as_ref().is_some_and(|database| database.path == path && database.stamp == stamp) {
            return;
        }
        match opencode::read_opencode_db(&path) {
            Ok(state) => self.opencode_db = Some(OpenCodeDatabase { path, stamp, state }),
            // A database we can no longer read must not keep showing the last good state:
            // that would turn schema drift into a silent old snapshot.
            Err(_) => self.opencode_db = None,
        }
    }

    /// Apply the snapshot's title and firm turn status to each Cursor session. A status older
    /// than the transcript's last entry is ignored by the reducer, so a throttled read never
    /// rewinds newer transcript activity.
    #[cfg(feature = "sqlite")]
    fn apply_cursor_db(&mut self, now: Timestamp) {
        let idle = self.options.idle_after.as_secs_f64();
        let database = self.cursor_db.as_ref();
        for (path, followed) in &mut self.sessions {
            if followed.reducer.harness() != Harness::Cursor {
                continue;
            }
            let composer =
                database.and_then(|database| cursor_composer_id(path).and_then(|id| database.state.composer(id)));
            followed.reducer.set_composer_title(composer.and_then(|composer| composer.name.as_deref()));
            let status = composer.and_then(|composer| {
                let at = composer.updated_at()?;
                let activity = composer.activity()?;
                let fresh = at.as_secs_f64() <= now.as_secs_f64() && now.as_secs_f64() - at.as_secs_f64() <= idle;
                fresh.then_some((activity, at))
            });
            followed.reducer.set_composer_status(status);
        }
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
        // Feed each helper's new bytes to the session it helps. A helper's own transcript
        // says whether it is still running; the parent then reports BackgroundWork.
        let current = std::mem::take(&mut self.helpers);
        let mut live_helpers = HashSet::new();
        let mut live_metas = HashSet::new();
        for (path, (harness, owner)) in &current {
            if !self.filter.shows(*harness) || !live.contains(owner) {
                continue;
            }
            let Some(f) = self.sessions.get_mut(owner) else { continue };
            let Ok(meta) = std::fs::metadata(path) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            let len = meta.len();
            let id = path.to_string_lossy();
            let offset = self.helper_offsets.get(path).copied().unwrap_or(0);
            let offset = if len < offset { 0 } else { offset };
            if len > offset {
                let Ok(mut file) = File::open(path) else { continue };
                if file.seek(SeekFrom::Start(offset)).is_err() {
                    continue;
                }
                let mut bytes = Vec::new();
                if file.read_to_end(&mut bytes).is_err() {
                    continue;
                }
                let ctx = ChunkCtx { offset, mtime: Timestamp::from(mtime), path };
                if f.reducer.feed_helper(&id, &bytes, ctx).is_ok() {
                    self.helper_offsets.insert(path.clone(), len);
                } else {
                    self.helper_offsets.insert(path.clone(), 0);
                }
            }
            // Claude Code writes `<agent>.meta.json` beside the helper transcript. Read it
            // when it changes; `finished`/`stoppedByUser` is a firm end even if the last
            // transcript entry is still a pending tool.
            if *harness == Harness::ClaudeCode {
                let meta_path = path.with_extension("meta.json");
                if let Ok(meta) = std::fs::metadata(&meta_path)
                    && let Ok(meta_mtime) = meta.modified()
                {
                    let seen = self.helper_metas.get(&meta_path).copied();
                    if seen != Some((meta_mtime, meta.len()))
                        && let Ok(bytes) = std::fs::read(&meta_path)
                    {
                        f.reducer.feed_helper_meta(&id, &bytes);
                        self.helper_metas.insert(meta_path.clone(), (meta_mtime, meta.len()));
                    }
                    live_metas.insert(meta_path);
                }
            }
            live_helpers.insert(path.clone());
        }
        self.helpers = current;
        self.helper_offsets.retain(|p, _| live_helpers.contains(p));
        self.helper_metas.retain(|p, _| live_metas.contains(p));
        for (owner, t) in helped {
            if let Some(f) = self.sessions.get_mut(&owner) {
                f.reducer.helper_written(Timestamp::from_secs_f64(t));
            }
        }

        self.refresh_codex_index();
        let index = &self.codex_index;
        for followed in self.sessions.values_mut() {
            if followed.reducer.harness() == Harness::Codex {
                followed.reducer.set_index_title(index.get(followed.reducer.session_id()).map(String::as_str));
            }
        }
        self.refresh_desktop_metadata();

        #[cfg(feature = "sqlite")]
        {
            self.refresh_cursor_db(now);
            self.apply_cursor_db(now);
            self.refresh_opencode_db();
        }

        let mut views: Vec<_> = self
            .sessions
            .iter()
            .map(|(p, f)| {
                let key = SessionKey { harness: f.reducer.harness(), path: p.clone() };
                (f.reducer.active_at(), key, f.reducer.state(now))
            })
            .collect();
        #[cfg(feature = "sqlite")]
        if let Some(database) = &self.opencode_db {
            let sessions = &database.state.sessions;
            for session in sessions.iter().filter(|session| session.parent_id.is_none()) {
                let own = session.updated_at.map(|at| at.as_secs_f64()).unwrap_or(0.0);
                let child = sessions
                    .iter()
                    .filter(|child| child.parent_id.as_deref() == Some(session.id.as_str()))
                    .filter_map(|child| child.updated_at.map(|at| at.as_secs_f64()))
                    .fold(0.0, f64::max);
                let updated = own.max(child);
                if updated < epoch(cutoff) {
                    continue;
                }
                let path = database.path.join(&session.id);
                let mut reducer = SessionReducer::new(Harness::OpenCode, self.options.clone());
                reducer.set_path(&path);
                if let Some(at) = session.updated_at {
                    reducer.transcript_written(at);
                }
                session.feed(&mut reducer);
                for child in sessions.iter().filter(|child| child.parent_id.as_deref() == Some(session.id.as_str())) {
                    let child_path = database.path.join(&child.id);
                    let child_at = child.updated_at.unwrap_or(now);
                    let bytes = child.to_jsonl();
                    let ctx = ChunkCtx { offset: 0, mtime: child_at, path: &child_path };
                    let _ = reducer.feed_helper(&child_path.to_string_lossy(), bytes.as_bytes(), ctx);
                }
                let state = reducer.state(now);
                let active = state.last_write.map_or(updated, |at| at.as_secs_f64());
                views.push((active, SessionKey { harness: Harness::OpenCode, path }, state));
            }
        }
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
        for (harness, root) in
            self.roots.iter().filter(|(h, _)| self.filter.shows(*h) && h.discovery().database.is_none())
        {
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

/// Claude Desktop's sidecar is the sibling of the session folder:
/// `<org>/local_<id>.json` for `<org>/local_<id>/audit.jsonl`. Newer Desktop builds name the
/// folder with the first eight hex characters of that id (`<org>/ab12cd34/audit.jsonl`), so
/// there the sidecar has to be found by that prefix.
fn claude_desktop_metadata(path: &Path) -> Option<PathBuf> {
    if path.file_name()? != "audit.jsonl" {
        return None;
    }
    let session_dir = path.parent()?;
    let parent = session_dir.parent()?;
    let name = session_dir.file_name()?.to_string_lossy();
    if name.starts_with("local_") {
        return Some(parent.join(format!("{name}.json")));
    }
    if name.len() != 8 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let prefix = name.to_ascii_lowercase();
    let entries = std::fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path();
        if !candidate.is_file() {
            continue;
        }
        let Some(stem) = candidate.file_name()?.to_str().and_then(|file| file.strip_suffix(".json")) else {
            continue;
        };
        let Some(id) = stem.strip_prefix("local_") else { continue };
        if id.get(..8).is_some_and(|start| start.eq_ignore_ascii_case(&prefix)) {
            return Some(candidate);
        }
    }
    None
}

/// Cursor's composer id is the transcript's file stem (`.../<id>/<id>.jsonl`).
#[cfg(feature = "sqlite")]
fn cursor_composer_id(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|stem| stem.to_str()).filter(|stem| !stem.is_empty())
}

#[cfg(feature = "sqlite")]
fn db_stamp(path: &Path) -> DbStamp {
    DbStamp { main: file_stamp(path), wal: file_stamp(&sidecar(path, "-wal")), shm: file_stamp(&sidecar(path, "-shm")) }
}

#[cfg(feature = "sqlite")]
fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp { mtime: meta.modified().ok()?, len: meta.len() })
}

#[cfg(feature = "sqlite")]
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
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
                        && path.extension().is_some_and(|e| e == layout.session_suffix)
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

    /// Each session's project and test label, sorted, as of `now`.
    fn polled_at(t: &mut Tracker, now: Timestamp) -> Vec<(String, String)> {
        let mut v: Vec<_> = t.poll(now).iter().map(|(_, s)| (s.meta.project.clone(), label(s).to_string())).collect();
        v.sort();
        v
    }

    /// Each session's project and test label, sorted.
    fn polled(t: &mut Tracker) -> Vec<(String, String)> {
        polled_at(t, wall())
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
    fn tracker_reads_pi_sessions_and_their_names() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".pi/agent/sessions/--w-app--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-10-25T01-00-00-000Z_s1.jsonl");
        std::fs::write(
            &path,
            jsonl(&[
                json!({"type": "session", "version": 3, "id": "s1", "timestamp": at(3.0), "cwd": "/w/app"}),
                json!({"type": "message", "id": "e1", "timestamp": at(2.0),
                       "message": {"role": "user", "content": [{"type": "text", "text": "opening prompt"}]}}),
                json!({"type": "message", "id": "e2", "timestamp": at(1.0),
                       "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}],
                                   "stopReason": "stop",
                                   "usage": {"input": 3, "output": 4, "cacheRead": 0, "cacheWrite": 0,
                                             "totalTokens": 7, "cost": {"total": 0.01}}}}),
            ]),
        )
        .unwrap();
        let mut t = Tracker::new(home.path(), hours(1.0));
        let v = t.poll(wall());
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0.harness, Harness::Pi);
        assert_eq!(
            (v[0].1.meta.project.as_str(), v[0].1.meta.title.as_str(), v[0].1.activity),
            ("app", "opening prompt", crate::Activity::Done)
        );
        assert_eq!(v[0].1.usage.output_tokens, Some(4));

        append(&path, &jsonl(&[json!({"type": "session_info", "id": "e3", "timestamp": at(0.5), "name": "Named"})]));
        let v = t.poll(wall());
        assert_eq!(v[0].1.meta.title, "Named");
    }

    #[test]
    fn tracker_reads_a_flat_pi_session_dir() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("custom-pi-sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("2026-10-25T01-00-00-000Z_s1.jsonl"),
            jsonl(&[
                json!({"type": "session", "version": 3, "id": "s1", "timestamp": at(3.0), "cwd": "/w/custom"}),
                json!({"type": "message", "id": "e1", "timestamp": at(2.0),
                       "message": {"role": "user", "content": [{"type": "text", "text": "flat prompt"}]}}),
            ]),
        )
        .unwrap();
        let mut t = Tracker::with_roots(vec![(Harness::Pi, root)], hours(1.0));
        let v = t.poll(wall());
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].1.meta.project.as_str(), v[0].1.meta.title.as_str()), ("custom", "flat prompt"));
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
    fn tracker_feeds_claude_subagents_and_reports_background_work() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-w-site");
        std::fs::create_dir_all(project.join("s/subagents")).unwrap();
        let parent = project.join("s.jsonl");
        std::fs::write(
            &parent,
            jsonl(&[
                claude_prompt("/w/site", "go", 5.0),
                json!({"type": "assistant", "timestamp": at(3.0),
                       "message": {"id": "m1", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                                   "content": [{"type": "text", "text": "done"}]}}),
                json!({"type": "system", "subtype": "turn_duration", "timestamp": at(2.0),
                       "pendingBackgroundAgentCount": 1}),
            ]),
        )
        .unwrap();
        let helper = project.join("s/subagents/agent-a1.jsonl");
        std::fs::write(
            &helper,
            jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": at(1.0),
                           "message": {"id": "h1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                       "content": [{"type": "tool_use", "name": "Bash", "input": {"command": "sleep 9"}}]}})]),
        )
        .unwrap();

        let mut t = Tracker::new(home.path(), hours(1.0));
        let v = t.poll(wall());
        assert_eq!(v.len(), 1, "the helper is not a session of its own");
        let state = &v[0].1;
        assert_eq!(state.activity, crate::Activity::BackgroundWork);
        assert_eq!(state.certainty, crate::Certainty::Firm, "the parent's count is firm");
        assert_eq!(state.background.len(), 1);
        let id = state.background[0].id.clone();
        assert!(id.ends_with("agent-a1.jsonl"), "{id}");

        append(
            &helper,
            &jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": at(0.8),
                            "message": {"id": "h2", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                                        "content": [{"type": "text", "text": "done"}]}})]),
        );
        let state = &t.poll(wall())[0].1;
        assert_eq!(state.background.len(), 0, "the helper's own end is read");

        append(
            &parent,
            &jsonl(&[json!({"type": "user", "origin": {"kind": "task-notification"}, "timestamp": at(0.5),
                            "message": {"content": "<task-notification><status>completed</status></task-notification>"}})]),
        );
        let state = &t.poll(wall())[0].1;
        assert_eq!((state.activity, state.background.len()), (crate::Activity::Done, 0));
    }

    #[test]
    fn tracker_agents_killed_ends_a_followed_helper() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-w-site");
        std::fs::create_dir_all(project.join("s/subagents")).unwrap();
        let parent = project.join("s.jsonl");
        std::fs::write(
            &parent,
            jsonl(&[
                claude_prompt("/w/site", "delegate", 5.0),
                json!({"type": "assistant", "timestamp": at(3.0),
                       "message": {"id": "m1", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                                   "content": [{"type": "text", "text": "done"}]}}),
                json!({"type": "system", "subtype": "turn_duration", "timestamp": at(2.0),
                       "pendingBackgroundAgentCount": 1}),
                json!({"type": "system", "subtype": "agents_killed", "timestamp": at(0.5)}),
            ]),
        )
        .unwrap();
        let helper = project.join("s/subagents/agent-a1.jsonl");
        std::fs::write(
            &helper,
            jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": at(1.0),
                           "message": {"id": "h1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                       "content": [{"type": "tool_use", "name": "Bash", "input": {"command": "sleep 9"}}]}})]),
        )
        .unwrap();

        let mut t = Tracker::new(home.path(), hours(1.0));
        let state = &t.poll(wall())[0].1;
        assert_eq!((state.activity, state.background.len()), (crate::Activity::Done, 0));

        // The tracker re-feeds the helper file later; the kill still wins.
        append(
            &helper,
            &jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": at(0.4),
                            "message": {"id": "h2", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                        "content": [{"type": "tool_use", "name": "Bash",
                                                     "input": {"command": "sleep 9"}}]}})]),
        );
        let state = &t.poll(wall())[0].1;
        assert_eq!((state.activity, state.background.len()), (crate::Activity::Done, 0));
    }

    #[test]
    fn tracker_links_an_async_agent_to_its_helper_and_meta_end() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-w-site");
        std::fs::create_dir_all(project.join("s/subagents")).unwrap();
        let helper = project.join("s/subagents/agent-a1.jsonl");
        std::fs::write(
            &helper,
            jsonl(&[json!({"type": "assistant", "isSidechain": true, "timestamp": at(1.0),
                           "message": {"id": "h1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                       "content": [{"type": "tool_use", "name": "Bash", "input": {"command": "sleep 9"}}]}})]),
        )
        .unwrap();
        let parent = project.join("s.jsonl");
        std::fs::write(
            &parent,
            jsonl(&[
                claude_prompt("/w/site", "delegate", 5.0),
                json!({"type": "assistant", "timestamp": at(4.0),
                       "message": {"id": "m1", "stop_reason": "tool_use", "usage": {"output_tokens": 1},
                                   "content": [{"type": "tool_use", "id": "toolu_1", "name": "Agent",
                                                "input": {"description": "look", "prompt": "look"}}]}}),
                json!({"type": "user", "timestamp": at(3.5), "sourceToolUseID": "toolu_1",
                       "toolUseResult": {"status": "async_launched", "isAsync": true, "agentId": "a1",
                                         "outputFile": helper.to_string_lossy(), "backgroundTaskId": "task-a1"},
                       "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "launched"}]}}),
                json!({"type": "assistant", "timestamp": at(3.0),
                       "message": {"id": "m2", "stop_reason": "end_turn", "usage": {"output_tokens": 1},
                                   "content": [{"type": "text", "text": "started"}]}}),
            ]),
        )
        .unwrap();

        let mut t = Tracker::new(home.path(), hours(1.0));
        let state = &t.poll(wall())[0].1;
        assert_eq!(state.activity, crate::Activity::BackgroundWork);
        assert_eq!(state.certainty, crate::Certainty::Inferred, "the launch alone is not firm");
        assert_eq!(state.background.len(), 1);
        assert_eq!(state.background[0].id, helper.to_string_lossy(), "the parent names the helper file");

        let meta = helper.with_extension("meta.json");
        std::fs::write(&meta, "{not json").unwrap();
        let state = &t.poll(wall())[0].1;
        assert_eq!(state.activity, crate::Activity::BackgroundWork, "malformed metadata is ignored");
        std::fs::write(&meta, "{\"finished\":false}\n").unwrap();
        let state = &t.poll(wall())[0].1;
        assert_eq!(state.activity, crate::Activity::BackgroundWork, "finished false is still running");
        std::fs::write(&meta, "{\"finished\":true}\n").unwrap();
        let state = &t.poll(wall())[0].1;
        assert_eq!(
            (state.activity, state.background.len()),
            (crate::Activity::Done, 0),
            "metadata ends a helper whose transcript is still a pending tool"
        );
    }

    #[test]
    fn codex_titles_come_from_the_session_index_after_the_first_prompt() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join(".codex/sessions/2026/10/14");
        std::fs::create_dir_all(&day).unwrap();
        let session = day.join("rollout-x.jsonl");
        std::fs::write(
            &session,
            jsonl(&[
                json!({"timestamp": at(3.0), "type": "session_meta", "payload": {"id": "thread-1", "cwd": "/w/app"}}),
                json!({"timestamp": at(2.0), "type": "event_msg",
                       "payload": {"type": "user_message", "message": "First prompt\nwith detail"}}),
            ]),
        )
        .unwrap();

        // No index yet: the opening prompt is the fallback title.
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(t.poll(wall())[0].1.meta.title, "First prompt");

        // The index appears and wins, even after the first prompt was already read.
        std::fs::write(
            home.path().join(".codex/session_index.jsonl"),
            "not json\n{\"id\":\"thread-other\",\"thread_name\":\"Someone else\"}\n\
             {\"id\":\"thread-1\",\"thread_name\":\"Indexed title\",\"updated_at\":\"2026-10-24T12:00:00Z\"}\n",
        )
        .unwrap();
        assert_eq!(t.poll(wall())[0].1.meta.title, "Indexed title");

        // A later prompt changes the activity and log, not an index title.
        append(
            &session,
            &jsonl(&[json!({"timestamp": at(1.0), "type": "event_msg",
                            "payload": {"type": "user_message", "message": "A later prompt"}})]),
        );
        let state = &t.poll(wall())[0].1;
        assert_eq!(state.meta.title, "Indexed title");
        assert_eq!(state.recent.last().unwrap().to_string(), "› A later prompt");

        // The index goes away (or becomes unreadable): the opening prompt is the fallback,
        // not the stale index title.
        std::fs::write(home.path().join(".codex/session_index.jsonl"), "not json\n").unwrap();
        assert_eq!(t.poll(wall())[0].1.meta.title, "First prompt");
    }

    #[test]
    fn codex_index_sits_beside_its_sessions_root_and_malformed_entries_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("custom-codex");
        let day = codex_home.join("sessions/2026/10/14");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-x.jsonl"),
            jsonl(&[
                json!({"timestamp": at(3.0), "type": "session_meta", "payload": {"id": "thread-1", "cwd": "/w/app"}}),
                json!({"timestamp": at(2.0), "type": "event_msg",
                       "payload": {"type": "user_message", "message": "Opening prompt"}}),
            ]),
        )
        .unwrap();
        std::fs::write(
            codex_home.join("session_index.jsonl"),
            "{\"id\":\"thread-1\"}\n{\"thread_name\":\"No id\"}\n{\"id\":\"thread-1\",\"thread_name\":\"  \"}\n",
        )
        .unwrap();
        let mut t = Tracker::with_roots(vec![(Harness::Codex, codex_home.join("sessions"))], hours(1.0));
        assert_eq!(t.poll(wall())[0].1.meta.title, "Opening prompt", "no usable index title: fallback");
    }

    #[test]
    fn harness_folder_overrides_are_absolute_paths() {
        let home = Path::new("/h");
        let custom = |var: &str| match var {
            "CLAUDE_CONFIG_DIR" => Some("/custom/claude".into()),
            "CODEX_HOME" => Some("/custom/codex".into()),
            "PI_CODING_AGENT_DIR" => Some("/custom/pi-agent".into()),
            _ => None,
        };
        let roots = Tracker::roots(home, custom);
        let root = |harness| roots.iter().find(|(known, _)| *known == harness).unwrap().1.clone();
        assert_eq!(root(Harness::ClaudeCode), PathBuf::from("/custom/claude/projects"));
        assert_eq!(root(Harness::Codex), PathBuf::from("/custom/codex/sessions"));
        assert_eq!(root(Harness::Cursor), PathBuf::from("/h/.cursor/projects"));
        assert_eq!(root(Harness::Pi), PathBuf::from("/custom/pi-agent/sessions"));
        // Claude Desktop's platform roots are all scanned, not moved by another harness's env.
        assert_eq!(
            roots
                .iter()
                .filter(|(harness, _)| *harness == Harness::ClaudeDesktop)
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>(),
            [
                PathBuf::from("/h/.config/Claude/local-agent-mode-sessions"),
                PathBuf::from("/h/.config/Claude-3p/local-agent-mode-sessions")
            ]
        );
        // Relative overrides are ignored, as the harnesses ignore them.
        let relative = |var: &str| match var {
            "CLAUDE_CONFIG_DIR" | "CODEX_HOME" | "PI_CODING_AGENT_DIR" | "PI_CODING_AGENT_SESSION_DIR" => {
                Some("relative".into())
            }
            _ => None,
        };
        let roots = Tracker::roots(home, relative);
        let root = |harness| roots.iter().find(|(known, _)| *known == harness).unwrap().1.clone();
        assert_eq!(root(Harness::ClaudeCode), PathBuf::from("/h/.claude/projects"));
        assert_eq!(root(Harness::Codex), PathBuf::from("/h/.codex/sessions"));
        assert_eq!(root(Harness::Pi), PathBuf::from("/h/.pi/agent/sessions"));

        // A custom Pi session directory replaces the whole sessions/ layout, and wins over
        // PI_CODING_AGENT_DIR.
        let pi_session = |var: &str| match var {
            "PI_CODING_AGENT_DIR" => Some("/custom/pi-agent".into()),
            "PI_CODING_AGENT_SESSION_DIR" => Some("/custom/pi-sessions".into()),
            _ => None,
        };
        let roots = Tracker::roots(home, pi_session);
        let root = |harness| roots.iter().find(|(known, _)| *known == harness).unwrap().1.clone();
        assert_eq!(root(Harness::Pi), PathBuf::from("/custom/pi-sessions"));
    }

    #[test]
    fn tracker_reads_a_claude_desktop_sidecar_beside_its_audit_log() {
        let home = tempfile::tempdir().unwrap();
        let org = home.path().join(".config/Claude/local-agent-mode-sessions/user/org");
        let session = org.join("local_11111111-2222-3333-4444-555555555555");
        std::fs::create_dir_all(&session).unwrap();
        let audit = session.join("audit.jsonl");
        std::fs::write(
            &audit,
            jsonl(&[
                json!({"type": "system", "subtype": "init", "uuid": "init-1",
                       "session_id": "local_11111111-2222-3333-4444-555555555555",
                       "cwd": "/work/audit-cwd", "model": "claude-sonnet-4",
                       "permissionMode": "default"}),
                json!({"type": "user", "uuid": "u1", "message": {"content": "read it"}}),
            ]),
        )
        .unwrap();
        let mut t = Tracker::new(home.path(), filter(1.0, Some(&[Harness::ClaudeDesktop])));
        let first = t.poll(wall());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0.harness, Harness::ClaudeDesktop);
        assert_eq!(first[0].1.meta.project, "audit-cwd", "the audit's init cwd works alone");
        assert_eq!(first[0].1.meta.title, "", "no title until the sidecar appears");

        let sidecar = org.join("local_11111111-2222-3333-4444-555555555555.json");
        std::fs::write(&sidecar, r#"{"sessionId":"local_11111111-2222-3333-4444-555555555555","cwd":"/work/sidecar","title":"Config review","model":"claude-sonnet-4"}"#).unwrap();
        let second = t.poll(wall());
        assert_eq!(second[0].1.meta.project, "sidecar");
        assert_eq!(second[0].1.meta.title, "Config review");
        assert_eq!(second[0].1.activity, crate::Activity::Thinking);
        assert_eq!(second[0].1.permission, Some(crate::PermissionMode::Default), "the init record still wins for mode");
    }

    #[test]
    fn tracker_reads_a_claude_desktop_sidecar_beside_a_short_session_folder() {
        let home = tempfile::tempdir().unwrap();
        let org = home.path().join(".config/Claude/local-agent-mode-sessions/user/org");
        let session = org.join("ab12cd34");
        std::fs::create_dir_all(&session).unwrap();
        let audit = session.join("audit.jsonl");
        std::fs::write(
            &audit,
            jsonl(&[
                json!({"type": "system", "subtype": "init", "uuid": "init-1",
                       "session_id": "local_ab12cd34-2222-3333-4444-555555555555",
                       "cwd": "/work/audit-cwd", "model": "claude-sonnet-4",
                       "permissionMode": "default"}),
                json!({"type": "user", "uuid": "u1", "message": {"content": "read it"}}),
            ]),
        )
        .unwrap();
        let mut t = Tracker::new(home.path(), filter(1.0, Some(&[Harness::ClaudeDesktop])));
        let first = t.poll(wall());
        assert_eq!(first.len(), 1, "the eight-hex folder is a Desktop session");
        assert_eq!(first[0].0.path, audit);
        assert_eq!(first[0].1.meta.project, "audit-cwd", "the audit works before its sidecar");

        std::fs::write(
            org.join("local_ab12cd34-2222-3333-4444-555555555555.json"),
            r#"{"sessionId":"local_ab12cd34-2222-3333-4444-555555555555","cwd":"/work/sidecar","title":"Short folder review"}"#,
        )
        .unwrap();
        let second = t.poll(wall());
        assert_eq!(second[0].1.meta.project, "sidecar");
        assert_eq!(second[0].1.meta.title, "Short folder review");
    }

    #[test]
    fn project_comes_from_the_claude_cwd_not_the_folder_name() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-home-dev-work-my-app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("s.jsonl"), jsonl(&[claude_prompt("/home/dev/work/my-app/site", "go", 1.0)]))
            .unwrap();
        let mut t = Tracker::new(home.path(), hours(1.0));
        assert_eq!(polled(&mut t), [("site".into(), "thinking".into())]);
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

        let one = cursor_session(h, "Users-dev-Work-demo-app", "a1", &[cursor_prompt("go"), cursor_tool("Shell")]);
        cursor_session(
            h,
            "Users-dev-Work-shade",
            "b2",
            &[cursor_prompt("hi"), json!({"type": "turn_ended", "status": "success"})],
        );
        // Not sessions: other folders and files Cursor keeps, a subagent, a link.
        let decoy = jsonl(&[cursor_prompt("not a session")]);
        for rel in [
            "Users-dev-Work-shade/terminals/1.jsonl",
            "Users-dev-Work-shade/x.jsonl",
            "loose.jsonl",
            "Users-dev-Work-shade/agent-transcripts/b2/notes/n.jsonl",
        ] {
            let path = h.join(".cursor/projects").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &decoy).unwrap();
        }
        // A subagent is not a session, but it is read as a helper; make it finished so the
        // parent stays done.
        let helper = h.join(".cursor/projects/Users-dev-Work-shade/agent-transcripts/b2/subagents/s1.jsonl");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(helper, cursor_line(json!({"type": "turn_ended", "status": "success"}))).unwrap();
        let outside = h.join("elsewhere.jsonl");
        std::fs::write(&outside, &decoy).unwrap();
        let link = h.join(".cursor/projects/Users-dev-Work-shade/agent-transcripts/link");
        std::fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(&outside, link.join("link.jsonl")).unwrap();
        std::os::unix::fs::symlink(
            h.join(".cursor/projects/Users-dev-Work-demo-app"),
            h.join(".cursor/projects/Users-dev-Work-alias"),
        )
        .unwrap();

        let v = t.poll(wall());
        assert_eq!(v.len(), 2, "{:?}", v.iter().map(|(_, s)| &s.meta.project).collect::<Vec<_>>());
        assert!(v.iter().all(|(k, s)| k.harness == Harness::Cursor && s.harness == Harness::Cursor));
        assert_eq!(polled(&mut t), [("app".into(), "working".into()), ("shade".into(), "done".into())]);

        // The project's name, once the transcript tells it.
        let read = json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"path": "/Users/dev/work/demo-app/a.rs"}}]}});
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

        let dir = main.parent().unwrap();
        let sub = dir.join("subagents/s1.jsonl");
        // A coarse filesystem (whole seconds, say) can leave this folder's mtime at the stamp
        // the last poll cached even though subagents/ was just created in it: the new folder
        // would go unseen between sweeps. Put the stamp back there deliberately,
        // so the test reproduces that case on every filesystem, then move it past the cached
        // stamp instead of sleeping for a finer clock: a session folder's mtime is what tells
        // the walk to look inside it.
        let cached = std::fs::metadata(dir).unwrap().modified().unwrap();
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        std::fs::write(&sub, cursor_line(cursor_tool("Grep"))).unwrap();
        File::open(dir).unwrap().set_modified(cached).unwrap();
        File::open(dir).unwrap().set_modified(cached + Duration::from_secs(2)).unwrap();
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
        // The session's own turn ends while its subagent still runs: background work.
        append(&main, &cursor_line(json!({"type": "turn_ended", "status": "success"})));
        assert_eq!(polled(&mut t), [("app".into(), "working".into())], "the helper is still running");
        // The helper's own end lets the session finish.
        append(&sub, &cursor_line(json!({"type": "turn_ended", "status": "success"})));
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
    #[cfg(feature = "sqlite")]
    fn cursor_state_db(path: &Path, composer: &str, status: &str, name: &str, updated_ms: i64) {
        use rusqlite::{Connection, params};
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);")
            .unwrap();
        let value = json!({
            "name": name,
            "status": status,
            "lastUpdatedAt": updated_ms,
            "createdAt": updated_ms - 60_000,
            "generatingBubbleIds": [],
        });
        connection
            .execute(
                "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params![format!("composerData:{composer}"), value.to_string()],
            )
            .unwrap();
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn cursor_db_gives_a_firm_title_and_turn_status_until_the_transcript_moves_on() {
        let home = tempfile::tempdir().unwrap();
        cursor_session(home.path(), "w-app", "a1", &[cursor_prompt("fix it")]);
        let db = home.path().join("state.vscdb");
        // The transcript's mtime has sub-millisecond precision but `lastUpdatedAt` doesn't.
        // Poll a second later so the database row is unambiguously newer than the file.
        let now = Timestamp::from_secs_f64(wall().as_secs_f64() + 1.0);
        let updated = (now.as_secs_f64() * 1000.0) as i64;
        cursor_state_db(&db, "a1", "completed", "Database title", updated);
        let mut t = Tracker::new(home.path(), hours(1.0)).with_cursor_state_db(&db);
        let state = &t.poll(now)[0].1;
        assert_eq!(state.meta.title, "Database title");
        assert_eq!(
            (state.activity, state.certainty, state.reason),
            (crate::Activity::Done, crate::Certainty::Firm, crate::Reason::ComposerStatus)
        );

        // The tracker throttles a large database but notices a changed one when it is due.
        // A coarse filesystem (whole seconds, say) can leave the database's mtime at the stamp
        // the last poll cached even though the row was just replaced: the rename would go
        // unseen when the throttled read came due. Put the stamp back there
        // deliberately, so the test reproduces that case on every filesystem, then move it
        // past the cached stamp instead of sleeping for a finer clock: the database's stamp
        // is what tells the tracker it changed.
        let cached = std::fs::metadata(&db).unwrap().modified().unwrap();
        cursor_state_db(&db, "a1", "aborted", "Renamed", updated + 1);
        File::open(&db).unwrap().set_modified(cached).unwrap();
        File::open(&db).unwrap().set_modified(cached + Duration::from_secs(2)).unwrap();
        if let Some(database) = t.cursor_db.as_mut() {
            database.read_at = Timestamp::from_secs_f64(now.as_secs_f64() - CURSOR_STATE_DB_EVERY.as_secs_f64());
        }
        let later = Timestamp::from_secs_f64(now.as_secs_f64() + 1.0);
        let state = &t.poll(later)[0].1;
        assert_eq!(state.meta.title, "Renamed");
        assert_eq!(state.activity, crate::Activity::Interrupted);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn cursor_db_is_read_only_while_a_cursor_session_is_in_the_window() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("state.vscdb");
        let now = wall();
        cursor_state_db(&db, "a1", "completed", "Database title", (now.as_secs_f64() * 1000.0) as i64);
        let project = home.path().join(".claude/projects/-w-site");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("s.jsonl"), jsonl(&[claude_prompt("/w/site", "hi", 1.0)])).unwrap();

        let mut t = Tracker::new(home.path(), hours(1.0)).with_cursor_state_db(&db);
        assert_eq!(polled(&mut t), [("site".into(), "thinking".into())]);
        assert!(t.cursor_db.is_none(), "no Cursor session: the database was not read");

        cursor_session(home.path(), "w-app", "a1", &[cursor_prompt("go")]);
        // As above, poll after the transcript's sub-millisecond mtime can't outrun the row.
        let now = Timestamp::from_secs_f64(wall().as_secs_f64() + 1.0);
        let updated = (now.as_secs_f64() * 1000.0) as i64;
        cursor_state_db(&db, "a1", "completed", "Database title", updated);
        t.swept = None;
        assert_eq!(polled_at(&mut t, now), [("app".into(), "done".into()), ("site".into(), "thinking".into())]);
        assert!(t.cursor_db.is_some(), "a Cursor session: the database is read");
    }
}
