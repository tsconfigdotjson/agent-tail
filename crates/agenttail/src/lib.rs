//! What every coding agent on this machine is doing right now, read from the transcripts
//! the agents write: Claude Code, Codex and Cursor.
//!
//! The core is sans-IO. A [`SessionReducer`] takes one transcript's bytes, a chunk at a
//! time, with where they came from ([`ChunkCtx`]), and says what that session is doing as
//! of a time you give it ([`SessionReducer::state`]). It never reads the clock or the disk,
//! so local files and chunks shipped from another machine go through the same code, and a
//! state is a pure function of the bytes fed and `now`.
//!
//! With the `fs` feature (on by default), a [`Tracker`] finds and tails every harness's
//! transcripts under a home folder, read-only.
//!
//! ```
//! use agenttail::{Activity, ChunkCtx, Harness, ReducerOptions, SessionReducer, Timestamp};
//! use std::path::Path;
//!
//! let mut s = SessionReducer::new(Harness::Codex, ReducerOptions::default());
//! let rollout = concat!(
//!     r#"{"timestamp":"2026-10-23T10:00:00Z","type":"session_meta","payload":{"cwd":"/w/app"}}"#, "\n",
//!     r#"{"timestamp":"2026-10-23T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}"#, "\n",
//! );
//! let written = Timestamp::parse_rfc3339("2026-10-23T10:00:01Z").unwrap();
//! s.feed(rollout.as_bytes(), ChunkCtx { offset: 0, mtime: written, path: Path::new("rollout.jsonl") }).unwrap();
//!
//! let st = s.state(written + std::time::Duration::from_secs(5));
//! assert_eq!(st.activity, Activity::RunningTool);
//! // A tool call with no result for 20 s may be waiting for approval: a guess, and it says so.
//! let st = s.state(written + std::time::Duration::from_secs(30));
//! assert_eq!(st.activity, Activity::AwaitingApproval);
//! assert_eq!(st.certainty, agenttail::Certainty::Inferred);
//! assert_eq!(st.recent[0].to_string(), "$ cargo test");
//! ```

mod adapters;
mod error;
#[cfg(feature = "fs")]
mod fs;
mod harness;
mod lean;
mod reducer;
mod session;
mod state;
#[cfg(test)]
mod testutil;
mod time;

pub use error::Error;
#[cfg(feature = "fs")]
pub use fs::{Filter, READ_CHUNK, SWEEP_EVERY, SessionKey, Tracker, read_from};
pub use harness::{Harness, Layout, Role};
pub use reducer::{ChunkCtx, ParseMode, ReducerOptions, SessionReducer};
pub use state::{
    Activity, ActivityLine, Certainty, Diagnostics, PermissionMode, Reason, SessionMeta, SessionState, StatusClass,
    TokenUsage,
};
pub use time::Timestamp;
