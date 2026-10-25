//! One adapter per harness: how its transcript entries become normalized [`Event`]s.
//! Where its transcripts live is data, its [`Layout`](crate::Layout).

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod cursor;
pub(crate) mod pi;

use crate::event::EventKind;
use crate::session::Session;
use serde_json::Value;
use std::path::Path;

/// One normalized event before the parser attaches provenance and an id.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EventSeed {
    /// Seconds since the epoch when the record itself says (or, for Cursor prompts, its
    /// `<timestamp>` block); None means the reducer should use the transcript's mtime.
    pub(crate) at: Option<f64>,
    /// The record is a Claude Code sidechain row (`isSidechain: true`): activity in a
    /// helper's conversation, not the session that owns this transcript.
    pub(crate) sidechain: bool,
    pub(crate) kind: EventKind,
}

impl EventSeed {
    pub(crate) fn new(kind: EventKind) -> Self {
        EventSeed { at: None, sidechain: false, kind }
    }

    pub(crate) fn at(mut self, at: Option<f64>) -> Self {
        self.at = at;
        self
    }
}

/// How one harness's transcripts are read.
pub(crate) trait Adapter: Sync {
    /// A session was found at `path` (local or remote), before its first entry is read.
    fn start(&self, _s: &mut Session, _path: &Path) {}
    /// Turn one transcript entry (one JSON value) into the events it carries. Never fails:
    /// what an adapter doesn't know becomes `Meta` or `Unknown`, never nothing.
    fn parse(&self, state: &mut crate::event::ParseState, entry: &Value) -> Vec<EventSeed>;
}
