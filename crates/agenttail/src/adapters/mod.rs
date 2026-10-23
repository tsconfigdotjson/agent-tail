//! One adapter per harness: how its transcript entries fold into the shared session model.
//! Where its transcripts live is data, its [`Layout`](crate::Layout).

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod cursor;

use crate::session::Session;
use serde_json::Value;
use std::path::Path;

/// How one harness's transcripts are read.
pub(crate) trait Adapter: Sync {
    /// A session was found at `path` (local or remote), before its first entry is read.
    fn start(&self, _s: &mut Session, _path: &Path) {}
    /// Fold one transcript entry (one JSON line) into the session. Never fails: what an
    /// adapter doesn't know, it skips.
    fn apply(&self, s: &mut Session, entry: &Value);
}
