//! OpenCode's SQLite database: a read-only snapshot of `opencode.db`.
//!
//! OpenCode does not write a transcript file. Its `session`, `message` and `part` tables are
//! the record, and rows are rewritten as state moves (a tool part goes pending -> running ->
//! completed), so this reader rebuilds a session from all of its rows whenever the database
//! changes. The live database and its `-wal`/`-shm` sidecars are copied to a private folder,
//! opened read-only, and that folder is removed before the read returns.
//!
//! [`OpenCodeSession::rows`] is the normalized record stream the adapter in
//! `adapters/opencode.rs` reads: a session row, then each message row followed by its parts,
//! each as `{"table": "...", "row": {...columns...}}`. [`OpenCodeSession::to_jsonl`] writes
//! the same records as JSON lines for fixtures and for `agenttail events`.

use crate::error::Error;
use crate::event::parse_events;
use crate::reducer::ParseMode;
use crate::sqlite::Snapshot;
use crate::time::Timestamp;
use crate::{Event, Harness, SessionReducer};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Map, Value};
use std::path::Path;

/// One OpenCode session as stored in the database.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenCodeSession {
    pub id: String,
    /// The parent session for a subagent session; `None` for a top-level session.
    pub parent_id: Option<String>,
    pub directory: String,
    pub title: String,
    pub version: String,
    /// `session.time_updated`, the database's own last-write time.
    pub updated_at: Option<Timestamp>,
    /// The row stream, in feed order: session, then message/parts by creation time.
    pub rows: Vec<Value>,
}

impl OpenCodeSession {
    /// The rows as JSON lines, exactly as the adapter and fixtures see them.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&serde_json::to_string(row).expect("a database row serializes"));
            out.push('\n');
        }
        out
    }

    /// Parse this session's rows into normalized events.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        parse_events(Harness::OpenCode, 0, self.to_jsonl().as_bytes(), ParseMode::Full).into_iter()
    }

    /// Feed this session's rows to `reducer`, through the same events the public parser
    /// produces. The caller is responsible for the reducer's path and transcript time.
    pub fn feed(&self, reducer: &mut SessionReducer) {
        let jsonl = self.to_jsonl();
        for event in parse_events(Harness::OpenCode, 0, jsonl.as_bytes(), ParseMode::Full) {
            reducer.feed_event(&event);
        }
    }
}

/// Every session read from one snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenCodeState {
    pub sessions: Vec<OpenCodeSession>,
}

impl OpenCodeState {
    pub fn session(&self, id: &str) -> Option<&OpenCodeSession> {
        self.sessions.iter().find(|session| session.id == id)
    }

    /// The most recently updated top-level session.
    pub fn latest(&self) -> Option<&OpenCodeSession> {
        self.sessions.iter().filter(|session| session.parent_id.is_none()).max_by(|a, b| {
            a.updated_at
                .map(|at| at.as_secs_f64())
                .unwrap_or(0.0)
                .total_cmp(&b.updated_at.map(|at| at.as_secs_f64()).unwrap_or(0.0))
        })
    }
}

/// Read every session from a copied snapshot of `path`.
pub fn read_opencode_db(path: &Path) -> Result<OpenCodeState, Error> {
    let snapshot = Snapshot::copy_as(path, "opencode.db", "agenttail-opencode")?;
    let connection = Connection::open_with_flags(&snapshot.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| sqlite_error(path, error))?;
    let mut sessions = read_sessions(&connection, path)?;
    sessions.sort_by(|a, b| {
        a.updated_at
            .map(|at| at.as_secs_f64())
            .unwrap_or(0.0)
            .total_cmp(&b.updated_at.map(|at| at.as_secs_f64()).unwrap_or(0.0))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(OpenCodeState { sessions })
}

fn read_sessions(connection: &Connection, path: &Path) -> Result<Vec<OpenCodeSession>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT id, parent_id, directory, title, version, cost,
                    tokens_input, tokens_output, tokens_cache_read, tokens_cache_write,
                    time_created, time_updated
             FROM session ORDER BY time_updated, id",
        )
        .map_err(|error| sqlite_error(path, error))?;
    let rows = statement
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let parent_id: Option<String> = row.get(1)?;
            let session = Value::Object(session_row(row, &id)?);
            Ok(OpenCodeSession {
                id,
                parent_id: parent_id.filter(|id| !id.is_empty()),
                directory: row.get::<_, String>(2).unwrap_or_default(),
                title: row.get::<_, String>(3).unwrap_or_default(),
                version: row.get::<_, String>(4).unwrap_or_default(),
                updated_at: millis(row.get::<_, Option<i64>>(11).ok().flatten()),
                rows: vec![record("session", session)],
            })
        })
        .map_err(|error| sqlite_error(path, error))?;
    let mut sessions = Vec::new();
    for row in rows {
        let mut session = row.map_err(|error| sqlite_error(path, error))?;
        read_messages(connection, path, &mut session)?;
        sessions.push(session);
    }
    Ok(sessions)
}

fn session_row(row: &rusqlite::Row<'_>, id: &str) -> rusqlite::Result<Map<String, Value>> {
    let mut map = Map::new();
    map.insert("id".into(), Value::String(id.into()));
    map.insert("parent_id".into(), option_string(row.get::<_, Option<String>>(1)?));
    map.insert("directory".into(), Value::String(row.get::<_, Option<String>>(2)?.unwrap_or_default()));
    map.insert("title".into(), Value::String(row.get::<_, Option<String>>(3)?.unwrap_or_default()));
    map.insert("version".into(), Value::String(row.get::<_, Option<String>>(4)?.unwrap_or_default()));
    map.insert("cost".into(), row.get::<_, Option<f64>>(5)?.map_or(Value::Null, Value::from));
    for (key, index) in [("tokens_input", 6), ("tokens_output", 7), ("tokens_cache_read", 8), ("tokens_cache_write", 9)]
    {
        map.insert(key.into(), row.get::<_, Option<i64>>(index)?.map_or(Value::Null, Value::from));
    }
    for (key, index) in [("time_created", 10), ("time_updated", 11)] {
        map.insert(key.into(), row.get::<_, Option<i64>>(index)?.map_or(Value::Null, Value::from));
    }
    Ok(map)
}

fn read_messages(connection: &Connection, path: &Path, session: &mut OpenCodeSession) -> Result<(), Error> {
    let mut statement = connection
        .prepare(
            "SELECT id, time_created, time_updated, data FROM message
             WHERE session_id = ?1 ORDER BY time_created, id",
        )
        .map_err(|error| sqlite_error(path, error))?;
    let messages = statement
        .query_map(params![session.id], |row| {
            let id: String = row.get(0)?;
            Ok((id, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?, row.get::<_, String>(3)?))
        })
        .map_err(|error| sqlite_error(path, error))?;
    for message in messages {
        let (id, created, updated, data) = message.map_err(|error| sqlite_error(path, error))?;
        session.rows.push(record("message", Value::Object(message_row(&id, &session.id, created, updated, &data))));
        read_parts(connection, path, &session.id, &id, &mut session.rows)?;
    }
    read_v2_messages(connection, path, &session.id, &mut session.rows)?;
    Ok(())
}

/// The v2 projection exists beside the legacy rows. 1.19.4 left it empty; if a future
/// version starts using it, keep its records rather than silently dropping them.
fn read_v2_messages(connection: &Connection, path: &Path, session_id: &str, out: &mut Vec<Value>) -> Result<(), Error> {
    let exists = connection
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_message'")
        .and_then(|mut statement| statement.exists([]))
        .map_err(|error| sqlite_error(path, error))?;
    if !exists {
        return Ok(());
    }
    let mut statement = connection
        .prepare(
            "SELECT id, type, seq, time_created, time_updated, data FROM session_message
             WHERE session_id = ?1 ORDER BY seq, id",
        )
        .map_err(|error| sqlite_error(path, error))?;
    let rows = statement
        .query_map(params![session_id], |row| {
            let mut map = Map::new();
            map.insert("id".into(), Value::String(row.get(0)?));
            map.insert("session_id".into(), Value::String(session_id.into()));
            map.insert("type".into(), Value::String(row.get(1)?));
            map.insert("seq".into(), row.get::<_, Option<i64>>(2)?.map_or(Value::Null, Value::from));
            map.insert("time_created".into(), row.get::<_, Option<i64>>(3)?.map_or(Value::Null, Value::from));
            map.insert("time_updated".into(), row.get::<_, Option<i64>>(4)?.map_or(Value::Null, Value::from));
            map.insert("data".into(), Value::String(row.get(5)?));
            Ok(record("session_message", Value::Object(map)))
        })
        .map_err(|error| sqlite_error(path, error))?;
    for row in rows {
        out.push(row.map_err(|error| sqlite_error(path, error))?);
    }
    Ok(())
}

fn message_row(
    id: &str,
    session_id: &str,
    created: Option<i64>,
    updated: Option<i64>,
    data: &str,
) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("id".into(), Value::String(id.into()));
    map.insert("session_id".into(), Value::String(session_id.into()));
    map.insert("time_created".into(), created.map_or(Value::Null, Value::from));
    map.insert("time_updated".into(), updated.map_or(Value::Null, Value::from));
    map.insert("data".into(), Value::String(data.into()));
    map
}

fn read_parts(
    connection: &Connection,
    path: &Path,
    session_id: &str,
    message_id: &str,
    out: &mut Vec<Value>,
) -> Result<(), Error> {
    let mut statement = connection
        .prepare(
            "SELECT id, time_created, time_updated, data FROM part
             WHERE session_id = ?1 AND message_id = ?2 ORDER BY time_created, id",
        )
        .map_err(|error| sqlite_error(path, error))?;
    let parts = statement
        .query_map(params![session_id, message_id], |row| {
            let id: String = row.get(0)?;
            Ok((id, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?, row.get::<_, String>(3)?))
        })
        .map_err(|error| sqlite_error(path, error))?;
    for part in parts {
        let (id, created, updated, data) = part.map_err(|error| sqlite_error(path, error))?;
        let mut map = Map::new();
        map.insert("id".into(), Value::String(id));
        map.insert("message_id".into(), Value::String(message_id.into()));
        map.insert("session_id".into(), Value::String(session_id.into()));
        map.insert("time_created".into(), created.map_or(Value::Null, Value::from));
        map.insert("time_updated".into(), updated.map_or(Value::Null, Value::from));
        map.insert("data".into(), Value::String(data));
        out.push(record("part", Value::Object(map)));
    }
    Ok(())
}

fn record(table: &str, row: Value) -> Value {
    let mut record = Map::new();
    record.insert("table".into(), Value::String(table.into()));
    record.insert("row".into(), row);
    Value::Object(record)
}

fn option_string(value: Option<String>) -> Value {
    value.filter(|value| !value.is_empty()).map_or(Value::Null, Value::String)
}

fn millis(value: Option<i64>) -> Option<Timestamp> {
    value.filter(|&millis| millis > 0).map(|millis| Timestamp::from_secs_f64(millis as f64 / 1000.0))
}

fn sqlite_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Sqlite { path: path.into(), message: error.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReducerOptions;
    use crate::state::Activity;
    use crate::state::Certainty;
    use rusqlite::params;
    use serde_json::Value;

    const ROWS: &str = include_str!("../../../fixtures/opencode/1.19.4/linux/plain-qa/input/session.jsonl");

    fn database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT,
                    slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL,
                    version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER,
                    summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT,
                    metadata TEXT, cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0,
                    tokens_output INTEGER NOT NULL DEFAULT 0, tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_write INTEGER NOT NULL DEFAULT 0, revert TEXT, permission TEXT,
                    agent TEXT, model TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                    time_compacting INTEGER, time_archived INTEGER);
                 CREATE TABLE message (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE part (
                    id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
            )
            .unwrap();
        for line in ROWS.lines().filter(|line| !line.trim().is_empty()) {
            let record: Value = serde_json::from_str(line).unwrap();
            let row = &record["row"];
            match record["table"].as_str().unwrap() {
                "session" => {
                    connection
                        .execute(
                            "INSERT INTO session
                             (id, project_id, parent_id, slug, directory, title, version, cost,
                              tokens_input, tokens_output, tokens_cache_read, tokens_cache_write,
                              time_created, time_updated)
                             VALUES (?1, 'global', ?2, 's', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                            params![
                                row["id"].as_str(),
                                row["parent_id"].as_str(),
                                row["directory"].as_str(),
                                row["title"].as_str(),
                                row["version"].as_str(),
                                row["cost"].as_f64(),
                                row["tokens_input"].as_i64(),
                                row["tokens_output"].as_i64(),
                                row["tokens_cache_read"].as_i64(),
                                row["tokens_cache_write"].as_i64(),
                                row["time_created"].as_i64(),
                                row["time_updated"].as_i64(),
                            ],
                        )
                        .unwrap();
                }
                "message" => {
                    connection
                        .execute(
                            "INSERT INTO message (id, session_id, time_created, time_updated, data)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                row["id"].as_str(),
                                row["session_id"].as_str(),
                                row["time_created"].as_i64(),
                                row["time_updated"].as_i64(),
                                row["data"].as_str(),
                            ],
                        )
                        .unwrap();
                }
                "part" => {
                    connection
                        .execute(
                            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                row["id"].as_str(),
                                row["message_id"].as_str(),
                                row["session_id"].as_str(),
                                row["time_created"].as_i64(),
                                row["time_updated"].as_i64(),
                                row["data"].as_str(),
                            ],
                        )
                        .unwrap();
                }
                other => panic!("unexpected table {other}"),
            }
        }
    }

    #[test]
    fn reads_a_real_row_export_and_reduces_it_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        database(&path);
        let state = read_opencode_db(&path).unwrap();
        assert_eq!(state.sessions.len(), 1);
        let session = state.latest().unwrap();
        assert_eq!(session.title, "Mock session title");
        assert_eq!(session.version, "1.19.4");
        assert_eq!(session.parent_id, None);
        assert!(session.rows.len() > 5);
        assert_eq!(session.rows[0]["table"], "session");

        let mut reducer = SessionReducer::new(Harness::OpenCode, ReducerOptions::default());
        reducer.set_path(&path);
        session.feed(&mut reducer);
        let now = session.updated_at.unwrap();
        let state = reducer.state(now);
        assert_eq!((state.activity, state.certainty), (Activity::Done, Certainty::Firm));
        assert_eq!(state.usage.input_tokens, Some(10));
        assert_eq!(state.usage.output_tokens, Some(5));
        assert_eq!(state.meta.project, "harness-ci");
        assert_eq!(state.recent.first().map(ToString::to_string).as_deref(), Some("› say hello"));
    }

    #[test]
    fn reads_a_snapshot_left_in_the_wal_and_removes_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT,
                  parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT,
                  title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER,
                  summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, metadata TEXT,
                  cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0,
                  tokens_output INTEGER NOT NULL DEFAULT 0, tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                  tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                  tokens_cache_write INTEGER NOT NULL DEFAULT 0, revert TEXT, permission TEXT,
                  agent TEXT, model TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                  time_compacting INTEGER, time_archived INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                  session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                  time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
            )
            .unwrap();
        writer
            .execute(
                "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
                 VALUES ('s1', 'global', 's', '/w/app', 'In WAL', '1.19.4', 1, 2)",
                [],
            )
            .unwrap();
        assert!(dir.path().join("opencode.db-wal").is_file(), "the row is still in the WAL");
        let state = read_opencode_db(&path).unwrap();
        assert_eq!(state.sessions[0].title, "In WAL");
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("agenttail-opencode-"))
            })
            .collect();
        assert!(leftovers.is_empty(), "the private snapshot is removed: {leftovers:?}");
    }
}
