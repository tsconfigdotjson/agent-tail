//! Cursor's `state.vscdb`: a read-only snapshot of the IDE's `cursorDiskKV` composer data.
//! OpenCode's database reader lives in `opencode.rs` and uses the same snapshot helper.
//!
//! Cursor keeps every IDE composer in
//! `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (macOS) or
//! `~/.config/Cursor/User/globalStorage/state.vscdb` (Linux). Live writes land in the
//! `-wal` sidecar, so the database is never opened in place: it is copied with `-wal` and
//! `-shm` into a private temporary folder, opened `mode=ro`, and the folder is deleted when
//! the read ends.
//!
//! [`read_state_vscdb`] reads every composer's status, title and time, and every bubble's
//! `toolFormerData`. [`read_state_vscdb_for`] does the same but only loads tool data for the
//! composer ids a tracker is following, which keeps a large database's read small.

use crate::error::Error;
use crate::state::Activity;
use crate::time::Timestamp;
use rusqlite::{Connection, OpenFlags, params};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Where Cursor's global `state.vscdb` is below a home folder.
///
/// macOS and Linux are the platforms Cursor's IDE format was surveyed on; Windows uses the
/// VS Code lineage's `AppData/Roaming` layout.
pub fn state_vscdb_path(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    }
    #[cfg(target_os = "linux")]
    {
        home.join(".config/Cursor/User/globalStorage/state.vscdb")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb")
    }
}

/// The status Cursor records on `composerData.status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerStatus {
    /// `none` (or no status recorded): the composer isn't reporting a turn state.
    None,
    /// `completed`: the last turn ended normally.
    Completed,
    /// `generating`: the model is writing a bubble.
    Generating,
    /// `aborted`: the turn was stopped, so the session is interrupted.
    Aborted,
    /// A value this version hasn't seen.
    Unknown(String),
}

impl ComposerStatus {
    fn parse(status: Option<&str>) -> Self {
        match status.map(str::trim).filter(|status| !status.is_empty()) {
            None | Some("none") => ComposerStatus::None,
            Some("completed") => ComposerStatus::Completed,
            Some("generating") => ComposerStatus::Generating,
            Some("aborted") => ComposerStatus::Aborted,
            Some(other) => ComposerStatus::Unknown(other.into()),
        }
    }
}

/// One Cursor composer: a session's title and turn status, and the tool calls in its bubbles.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorComposer {
    /// The composer id, which is also the `agent-transcripts/<id>` folder and file stem.
    pub id: String,
    /// `composerData.name`, Cursor's title for the chat.
    pub name: Option<String>,
    pub status: ComposerStatus,
    /// How many `generatingBubbleIds` the composer lists.
    pub generating_bubbles: usize,
    /// `composerData.lastUpdatedAt`, in milliseconds in the database.
    pub last_updated: Option<Timestamp>,
    /// `composerData.createdAt`, in milliseconds in the database.
    pub created_at: Option<Timestamp>,
    /// Tool calls from `bubbleId:<id>:*`'s `toolFormerData`.
    pub tools: Vec<CursorToolCall>,
}

impl CursorComposer {
    /// What this composer's own status says, without looking at the transcript. `None` means
    /// the status carries no activity (`none`/an unknown value) or no time to date it.
    ///
    /// A finished or aborted turn wins over a bubble left behind from it. Otherwise a
    /// persisted pending tool decision (the approval card, or an ask-user card) is firmer
    /// than `generating`: Cursor writes the bubble before it waits, and the composer may still
    /// say `generating` until the decision arrives.
    ///
    /// `generatingBubbleIds` is used only when the status itself is `none`: old Cursor
    /// versions leave stale generating ids behind after a completed turn.
    pub fn activity(&self) -> Option<Activity> {
        match self.status {
            ComposerStatus::Completed => Some(Activity::Done),
            ComposerStatus::Aborted => Some(Activity::Interrupted),
            _ if self.waiting_activity().is_some() => self.waiting_activity(),
            ComposerStatus::Generating => Some(Activity::Thinking),
            ComposerStatus::None | ComposerStatus::Unknown(_) if self.generating_bubbles > 0 => {
                Some(Activity::Thinking)
            }
            ComposerStatus::None | ComposerStatus::Unknown(_) => None,
        }
    }

    /// What a bubble says while it is waiting on the user, if any tool is. An ask-user bubble
    /// waits for an answer; every other pending tool waits for approval.
    pub fn waiting_activity(&self) -> Option<Activity> {
        self.tools.iter().rev().find_map(CursorToolCall::waiting_activity)
    }

    /// When the composer's status was last written: `lastUpdatedAt`, or `createdAt` for an
    /// old row with no update time.
    pub fn updated_at(&self) -> Option<Timestamp> {
        self.last_updated.or(self.created_at)
    }
}

/// One `toolFormerData` record from a Cursor bubble. Cursor embeds tool calls in assistant
/// bubbles rather than writing the separate tool-result rows the transcripts have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorToolCall {
    /// The part of `bubbleId:<composer>:<bubble>` after the last colon.
    pub bubble_id: String,
    pub name: String,
    /// `toolFormerData.status`: `completed`, `loading`, `error` or `cancelled`.
    pub status: Option<String>,
    /// `toolFormerData.additionalData.status`, when Cursor puts the tool state there.
    pub additional_status: Option<String>,
    /// `userDecision` at either level: `accepted`, `rejected` or `pending`.
    pub user_decision: Option<String>,
    /// `toolFormerData.error`, kept as the JSON string Cursor wrote.
    pub error: Option<String>,
}

impl CursorToolCall {
    /// What this tool is waiting on, when its persisted decision is still `pending`.
    ///
    /// Cursor writes the value at both levels depending on the tool and version:
    /// `toolFormerData.additionalData.status` while the shell/write/review card is up (3.27),
    /// and `userDecision` (or `status` in some exports) in the public storage shape. An
    /// ask-user tool is waiting for an answer; the rest are waiting for approval.
    pub fn waiting_activity(&self) -> Option<Activity> {
        let pending = [self.user_decision.as_deref(), self.additional_status.as_deref(), self.status.as_deref()]
            .into_iter()
            .flatten()
            .any(|value| value.eq_ignore_ascii_case("pending"));
        if !pending {
            return None;
        }
        Some(if crate::Harness::Cursor.discovery().asks_user(&self.name) {
            Activity::AwaitingAnswer
        } else {
            Activity::AwaitingApproval
        })
    }
}

/// Every composer read from a snapshot, sorted by id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CursorState {
    pub composers: Vec<CursorComposer>,
}

impl CursorState {
    /// The composer with `id`, if this snapshot has one.
    pub fn composer(&self, id: &str) -> Option<&CursorComposer> {
        self.composers.iter().find(|composer| composer.id == id)
    }
}

/// Read every composer and bubble from a copied snapshot of `state.vscdb`.
///
/// The live database is never opened. `-wal` and `-shm` are copied with it, so writes still
/// in the WAL are read; the copy lives in a private temporary folder that is deleted before
/// this returns.
pub fn read_state_vscdb(path: &Path) -> Result<CursorState, Error> {
    read_snapshot(path, None)
}

/// Read every composer's status and title, but only load tool calls for `composer_ids`.
///
/// This is what the [`Tracker`](crate::Tracker) uses: a Cursor database can be gigabytes,
/// and a poll only needs the tool data of the sessions in its window.
pub fn read_state_vscdb_for(path: &Path, composer_ids: &[&str]) -> Result<CursorState, Error> {
    read_snapshot(path, Some(composer_ids))
}

fn read_snapshot(path: &Path, composer_ids: Option<&[&str]>) -> Result<CursorState, Error> {
    let snapshot = Snapshot::copy(path)?;
    let connection = Connection::open_with_flags(&snapshot.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| sqlite_error(path, error))?;
    let mut composers = read_composers(&connection, path)?;
    for composer in &mut composers {
        let wanted = composer_ids.is_none_or(|ids| ids.contains(&composer.id.as_str()));
        if wanted {
            composer.tools = read_tools(&connection, path, Some(&composer.id))?;
        }
    }
    Ok(CursorState { composers })
}

const COMPOSER_SQL: &str = "
    SELECT substr(key, 14),
           json_extract(v, '$.name'),
           json_extract(v, '$.status'),
           json_array_length(v, '$.generatingBubbleIds'),
           CAST(json_extract(v, '$.lastUpdatedAt') AS INTEGER),
           CAST(json_extract(v, '$.createdAt') AS INTEGER)
    FROM (SELECT key, CAST(value AS TEXT) AS v FROM cursorDiskKV
          WHERE key >= 'composerData:' AND key < 'composerData;')
    WHERE json_valid(v) AND json_type(v, '$') = 'object'
    ORDER BY key";

const TOOL_SQL: &str = "
    SELECT key,
           json_extract(v, '$.toolFormerData.name'),
           json_extract(v, '$.toolFormerData.status'),
           json_extract(v, '$.toolFormerData.userDecision'),
           json_extract(v, '$.toolFormerData.additionalData.userDecision'),
           json_extract(v, '$.toolFormerData.additionalData.status'),
           json_extract(v, '$.toolFormerData.error')
    FROM (SELECT key, CAST(value AS TEXT) AS v FROM cursorDiskKV
          WHERE key >= ?1 AND key < ?2)
    WHERE json_valid(v) AND json_type(v, '$.toolFormerData') = 'object'
    ORDER BY key";

fn read_composers(connection: &Connection, path: &Path) -> Result<Vec<CursorComposer>, Error> {
    let mut statement = connection.prepare(COMPOSER_SQL).map_err(|error| sqlite_error(path, error))?;
    let rows = statement
        .query_map([], |row| {
            let name: Option<String> = row.get(1)?;
            let status: Option<String> = row.get(2)?;
            let generating: Option<i64> = row.get(3)?;
            Ok(CursorComposer {
                id: row.get(0)?,
                name: text(name),
                status: ComposerStatus::parse(status.as_deref()),
                generating_bubbles: generating.unwrap_or(0).max(0) as usize,
                last_updated: millis(row.get(4)?),
                created_at: millis(row.get(5)?),
                tools: Vec::new(),
            })
        })
        .map_err(|error| sqlite_error(path, error))?;
    rows.map(|row| row.map_err(|error| sqlite_error(path, error))).collect()
}

fn read_tools(connection: &Connection, path: &Path, composer: Option<&str>) -> Result<Vec<CursorToolCall>, Error> {
    let (start, end) = match composer {
        Some(id) => (format!("bubbleId:{id}:"), format!("bubbleId:{id};")),
        None => ("bubbleId:".to_string(), "bubbleId;".to_string()),
    };
    let mut statement = connection.prepare(TOOL_SQL).map_err(|error| sqlite_error(path, error))?;
    let rows = statement
        .query_map(params![start, end], |row| {
            let key: String = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            let status: Option<String> = row.get(2)?;
            let decision: Option<String> = row.get(3)?;
            let additional_decision: Option<String> = row.get(4)?;
            let additional_status: Option<String> = row.get(5)?;
            let error: Option<String> = row.get(6)?;
            let bubble_id = key.strip_prefix("bubbleId:").unwrap_or(&key);
            let bubble_id = bubble_id.split_once(':').map_or(bubble_id, |(_, bubble)| bubble).to_string();
            Ok(CursorToolCall {
                bubble_id,
                name: name.unwrap_or_default(),
                status: text(status),
                additional_status: text(additional_status),
                user_decision: text(decision).or_else(|| text(additional_decision)),
                error: text(error),
            })
        })
        .map_err(|error| sqlite_error(path, error))?;
    rows.map(|row| row.map_err(|error| sqlite_error(path, error))).collect()
}

fn text(value: Option<String>) -> Option<String> {
    value.map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

fn millis(value: Option<i64>) -> Option<Timestamp> {
    value.filter(|&millis| millis > 0).map(|millis| Timestamp::from_secs_f64(millis as f64 / 1000.0))
}

fn sqlite_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Sqlite { path: path.into(), message: error.to_string() }
}

/// A private copy of the database and its sidecars, removed when dropped.
pub(crate) struct Snapshot {
    dir: PathBuf,
    pub(crate) path: PathBuf,
}

impl Snapshot {
    fn copy(source: &Path) -> Result<Self, Error> {
        Self::copy_as(source, "state.vscdb", "agent-tail-vscdb")
    }

    /// Copy `source` and its `-wal`/`-shm` sidecars into a private folder, under
    /// `file_name`, removing the folder when dropped. `prefix` names the private folder.
    pub(crate) fn copy_as(source: &Path, file_name: &str, prefix: &str) -> Result<Self, Error> {
        if !source.is_file() {
            return Err(sqlite_error(source, format!("{file_name} does not exist")));
        }
        let dir = private_temp_dir(source, prefix)?;
        let snapshot = Snapshot { path: dir.join(file_name), dir };
        for suffix in ["", "-wal", "-shm"] {
            let from = if suffix.is_empty() { source.to_path_buf() } else { sidecar(source, suffix) };
            if !from.is_file() {
                continue;
            }
            let to = if suffix.is_empty() { snapshot.path.clone() } else { sidecar(&snapshot.path, suffix) };
            fs::copy(&from, &to).map_err(|error| sqlite_error(&from, error))?;
            private_file(&to).map_err(|error| sqlite_error(&to, error))?;
        }
        if !snapshot.path.is_file() {
            return Err(sqlite_error(source, format!("{file_name} was not copied")));
        }
        Ok(snapshot)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

fn private_temp_dir(source: &Path, prefix: &str) -> Result<PathBuf, Error> {
    loop {
        let n = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        match create_private_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(sqlite_error(source, error)),
        }
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const ROWS: &str = include_str!("../../../fixtures/cursor/handmade/vscdb-reader/input/state-vscdb.jsonl");

    fn rows() -> Vec<(String, String)> {
        ROWS.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let row: Value = serde_json::from_str(line).unwrap();
                (row["key"].as_str().unwrap().into(), serde_json::to_string(&row["value"]).unwrap())
            })
            .collect()
    }

    fn database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection.pragma_update(None, "journal_mode", "WAL").unwrap();
        connection.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        connection
            .execute_batch("CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);")
            .unwrap();
        for (key, value) in rows() {
            connection.execute("INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)", params![key, value]).unwrap();
        }
        connection
    }

    #[test]
    fn reads_composers_titles_and_tools_from_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("state.vscdb");
        let writer = database(&live);
        assert!(sidecar(&live, "-wal").is_file(), "the rows are still in the WAL");

        let state = read_state_vscdb(&live).unwrap();
        assert_eq!(state.composers.len(), 8, "one malformed composer row is skipped");
        let composer = state.composer("composer-1").unwrap();
        assert_eq!(composer.name.as_deref(), Some("Add a health check"));
        assert_eq!(composer.status, ComposerStatus::Completed);
        assert_eq!(composer.activity(), Some(Activity::Done));
        assert_eq!(composer.updated_at().unwrap().as_secs_f64(), 1_792_756_800.0);
        assert_eq!(composer.tools.len(), 2);
        assert_eq!(composer.tools[0].bubble_id, "bubble-1");
        assert_eq!(composer.tools[0].name, "run_terminal_command_v2");
        assert_eq!(composer.tools[0].status.as_deref(), Some("completed"));
        assert_eq!(composer.tools[0].user_decision.as_deref(), Some("accepted"));
        assert_eq!(composer.tools[1].name, "ask_question");
        assert_eq!(composer.tools[1].additional_status.as_deref(), Some("loading"));
        assert_eq!(composer.tools[1].user_decision.as_deref(), Some("pending"));

        let live = state.composer("composer-2").unwrap();
        assert_eq!((live.status.clone(), live.activity()), (ComposerStatus::Generating, Some(Activity::Thinking)));
        assert_eq!(live.generating_bubbles, 1);
        let aborted = state.composer("composer-3").unwrap();
        assert_eq!(
            (aborted.status.clone(), aborted.activity()),
            (ComposerStatus::Aborted, Some(Activity::Interrupted))
        );
        let none = state.composer("composer-4").unwrap();
        assert_eq!((none.status.clone(), none.activity()), (ComposerStatus::None, None));

        // While the approval card is up Cursor writes a pending decision on the bubble; the
        // composer may still say generating, so the pending decision is the firmer signal.
        let waiting = state.composer("composer-5").unwrap();
        assert_eq!(waiting.status, ComposerStatus::Generating);
        assert_eq!(waiting.activity(), Some(Activity::AwaitingApproval));
        assert_eq!(waiting.waiting_activity(), Some(Activity::AwaitingApproval));
        // Accepting moves the bubble to loading/accepted; denying records the rejection. The
        // composer status then decides again (generating or completed), with no stale wait.
        let approved = state.composer("composer-6").unwrap();
        assert_eq!(approved.activity(), Some(Activity::Thinking));
        let denied = state.composer("composer-7").unwrap();
        assert_eq!(denied.activity(), Some(Activity::Done));
        // A pending ask-user tool waits for an answer, not a yes/no approval.
        let question = state.composer("composer-8").unwrap();
        assert_eq!(question.activity(), Some(Activity::AwaitingAnswer));

        // The reader never writes to the live database, and the writer still has it open.
        writer
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:after', '{\"status\":\"completed\"}')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn reads_tools_only_for_the_requested_composers() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("state.vscdb");
        let _writer = database(&live);
        let state = read_state_vscdb_for(&live, &["composer-1"]).unwrap();
        assert_eq!(state.composer("composer-1").unwrap().tools.len(), 2);
        assert!(state.composer("composer-2").unwrap().tools.is_empty());
        assert!(!state.composers.iter().flat_map(|c| &c.tools).any(|tool| tool.bubble_id.starts_with("other-")));
    }

    #[test]
    fn the_snapshot_is_deleted_and_missing_databases_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("state.vscdb");
        let _writer = database(&live);
        let snapshot = Snapshot::copy(&live).unwrap();
        let snapshot_dir = snapshot.dir.clone();
        assert!(snapshot.path.is_file());
        assert!(snapshot.path.starts_with(std::env::temp_dir()));
        drop(snapshot);
        assert!(!snapshot_dir.exists(), "the private copy is gone");

        let missing = dir.path().join("missing/state.vscdb");
        let error = read_state_vscdb(&missing).unwrap_err();
        assert!(matches!(error, Error::Sqlite { .. }), "{error}");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn status_values_read_as_their_turn_states() {
        for (status, activity) in [
            ("none", None),
            ("completed", Some(Activity::Done)),
            ("generating", Some(Activity::Thinking)),
            ("aborted", Some(Activity::Interrupted)),
            ("something-new", None),
        ] {
            let composer = CursorComposer {
                id: "c".into(),
                name: None,
                status: ComposerStatus::parse(Some(status)),
                generating_bubbles: 0,
                last_updated: Some(Timestamp::from_secs_f64(1.0)),
                created_at: None,
                tools: Vec::new(),
            };
            assert_eq!(composer.activity(), activity, "{status}");
        }
        let stale = CursorComposer {
            id: "c".into(),
            name: None,
            status: ComposerStatus::Completed,
            generating_bubbles: 1,
            last_updated: Some(Timestamp::from_secs_f64(1.0)),
            created_at: None,
            tools: Vec::new(),
        };
        assert_eq!(stale.activity(), Some(Activity::Done), "a completed status wins over stale ids");
    }

    #[test]
    fn updated_rows_are_read_from_a_fresh_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("state.vscdb");
        {
            let connection = database(&live);
            connection
                .execute(
                    "UPDATE cursorDiskKV SET value = ?1 WHERE key = 'composerData:composer-2'",
                    params![r#"{"name":"New title","status":"completed","lastUpdatedAt":1792756800000}"#],
                )
                .unwrap();
        }
        let state = read_state_vscdb_for(&live, &[]).unwrap();
        assert_eq!(state.composer("composer-2").unwrap().status, ComposerStatus::Completed);
        assert_eq!(state.composer("composer-2").unwrap().name.as_deref(), Some("New title"));
    }
}
