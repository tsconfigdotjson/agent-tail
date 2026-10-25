//! The coding agents ("harnesses") we read, and where each keeps its transcripts, as data:
//! a local walk and a remote shell loop (StatusWall's `remote::SCRIPT`) can both follow a
//! [`Layout`] and so can't disagree about which files are sessions.

use crate::adapters::{self, Adapter};
use crate::error::Error;
use crate::time::{Timestamp, civil_from_days};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A coding agent whose sessions we read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Harness {
    /// Claude Code (the CLI, and sessions the desktop app runs).
    ClaudeCode,
    /// OpenAI's Codex CLI.
    Codex,
    /// Cursor's agent (IDE and CLI).
    Cursor,
    /// Pi (the `pi` coding-agent CLI).
    Pi,
    /// OpenCode's CLI, whose sessions live in a SQLite database.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl Harness {
    /// Every harness, in the order their folders are scanned.
    pub const ALL: [Harness; 5] =
        [Harness::ClaudeCode, Harness::Codex, Harness::Cursor, Harness::Pi, Harness::OpenCode];

    /// A stable id: `claude_code`, `codex`, `cursor`, `pi`, `opencode`.
    pub fn id(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude_code",
            Harness::Codex => "codex",
            Harness::Cursor => "cursor",
            Harness::Pi => "pi",
            Harness::OpenCode => "opencode",
        }
    }

    /// The tool's own name: "Claude Code", "Codex", "Cursor", "Pi", "OpenCode".
    pub fn name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Cursor => "Cursor",
            Harness::Pi => "Pi",
            Harness::OpenCode => "OpenCode",
        }
    }

    /// Where its transcripts live, from its `spec/discovery` entry.
    pub fn layout(self) -> &'static Layout {
        &self.discovery().layout
    }

    pub(crate) fn adapter(self) -> &'static dyn Adapter {
        match self {
            Harness::ClaudeCode => &adapters::claude::ClaudeCode,
            Harness::Codex => &adapters::codex::Codex,
            Harness::Cursor => &adapters::cursor::Cursor,
            Harness::Pi => &adapters::pi::Pi,
            Harness::OpenCode => &adapters::opencode::OpenCode,
        }
    }
}

impl FromStr for Harness {
    type Err = Error;

    /// A harness by its [`id`](Harness::id).
    fn from_str(id: &str) -> Result<Self, Error> {
        Harness::ALL.into_iter().find(|h| h.id() == id).ok_or_else(|| Error::UnknownHarness { id: id.into() })
    }
}

/// Where a harness keeps its transcripts, relative to the home folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Layout {
    /// The folder holding its transcripts, relative to the home folder. Every `*.jsonl` file
    /// below it, at most `max_depth` folders down, is one session (but see `sessions_in`).
    pub root: &'static str,
    /// How deep sessions sit below `root` (1 = directly in it); None for any depth.
    pub max_depth: Option<usize>,
    /// The folder sessions sit in inside each folder at the top of `root` (Cursor's
    /// `<project>/agent-transcripts`); None: anywhere. Nothing beside it is looked into.
    pub sessions_in: Option<&'static str>,
    /// The folder, beside a session's transcript, that holds its helpers' transcripts
    /// (Cursor's subagents: `<id>/<id>.jsonl`, and `<id>/subagents/*.jsonl` one level below
    /// `max_depth`). They aren't sessions of their own and aren't read as one: their bytes
    /// are fed to the session as helper state, so a session whose turn ended with a helper
    /// still writing is `BackgroundWork`, not `Done`. Needs a `max_depth`.
    pub helpers_in: Option<&'static str>,
    /// How a helper's file path names the session it helps (see [`HelperOwner`]).
    pub helper_owner: HelperOwner,
    /// Whether transcripts are filed in YYYY/MM/DD folders by the day their session
    /// started, and stay there; see [`may_hold_since`](Layout::may_hold_since).
    pub day_folders: bool,
    /// The extension of a transcript (`jsonl`), from the discovery spec's file pattern.
    pub session_suffix: &'static str,
}

/// How a helper transcript names the session it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HelperOwner {
    /// Cursor: a session is `<id>/<id>.jsonl`, and helpers are `<id>/subagents/*.jsonl`.
    SessionFolder,
    /// Claude Code: a session is `<id>.jsonl`, and helpers are in the sibling
    /// `<id>/subagents/*.jsonl`.
    SessionFileStem,
}

/// What a `*.jsonl` file below a harness's root is to it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// One session's transcript.
    Session,
    /// A helper's transcript (see [`Layout::helpers_in`]); the path of the session it helps,
    /// relative to the same place as the helper's.
    Helper(PathBuf),
}

impl Layout {
    /// What the file at `rel` (relative to `root`) is, if anything. A remote loop should
    /// decide the same way.
    pub fn role(&self, rel: &Path) -> Option<Role> {
        let c: Vec<_> = rel.components().map(|c| c.as_os_str()).collect();
        let n = c.len();
        if let Some(within) = self.sessions_in
            && (n < 3 || c[1] != within)
        {
            return None;
        }
        let Some(max) = self.max_depth else { return Some(Role::Session) };
        if n <= max {
            return Some(Role::Session);
        }
        let helpers = self.helpers_in?;
        let helper_depth = match self.helper_owner {
            HelperOwner::SessionFolder => max + 1,
            HelperOwner::SessionFileStem => max + 2,
        };
        if n != helper_depth || c[n - 2] != helpers || n < 3 {
            return None;
        }
        let owner = match self.helper_owner {
            HelperOwner::SessionFolder => {
                let owner = c[n - 3].to_string_lossy();
                c[..n - 2].iter().collect::<PathBuf>().join(format!("{owner}.{}", self.session_suffix))
            }
            HelperOwner::SessionFileStem => {
                let mut owner: PathBuf = c[..n - 2].iter().collect();
                owner.set_extension(self.session_suffix);
                owner
            }
        };
        Some(Role::Helper(owner))
    }

    /// How many folders down from `root` files are looked for: sessions' depth, plus one for
    /// helpers.
    pub fn walk_depth(&self) -> usize {
        let extra = match (self.helpers_in, self.helper_owner) {
            (None, _) => 0,
            (Some(_), HelperOwner::SessionFolder) => 1,
            (Some(_), HelperOwner::SessionFileStem) => 2,
        };
        self.max_depth.map_or(usize::MAX, |d| d + extra)
    }

    /// Whether `dir`, a folder below `root` (relative to it), may hold transcripts of
    /// sessions started at or after `since`. Polls skip folders that can't, between sweeps
    /// of every folder; transcripts already being followed are checked wherever they are.
    ///
    /// With `day_folders`, folders dated before the day of `since` (less a day: the day is
    /// in local time, of any time zone) only hold sessions started earlier. Folders that
    /// aren't dates may hold anything.
    pub fn may_hold_since(&self, dir: &Path, since: Timestamp) -> bool {
        if !self.day_folders {
            return true;
        }
        let (y, m, d) = civil_from_days((since.as_secs_f64() / 86400.0).floor() as i64 - 1);
        let first = [y as u32, m, d];
        // YYYY, then MM, then DD: compared in turn, the first that differs decides.
        for (i, part) in dir.components().enumerate() {
            let name = part.as_os_str().to_string_lossy();
            let Some(&first) = first.get(i) else { return true };
            let width = if i == 0 { 4 } else { 2 };
            let Some(n) =
                name.parse::<u32>().ok().filter(|_| name.len() == width && name.bytes().all(|b| b.is_ascii_digit()))
            else {
                return true;
            };
            if n != first {
                return n > first;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn harnesses_have_distinct_ids_and_roots() {
        let ids: HashSet<_> = Harness::ALL.iter().map(|h| h.id()).collect();
        let roots: HashSet<_> = Harness::ALL.iter().map(|h| h.layout().root).collect();
        assert_eq!((ids.len(), roots.len()), (Harness::ALL.len(), Harness::ALL.len()));
        for h in Harness::ALL {
            let id = h.id();
            assert!(id.starts_with(|c: char| c.is_ascii_lowercase()), "{id}");
            assert!(
                id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{id} is a template id"
            );
            assert!(!h.layout().root.starts_with('/'), "{} is relative to home", h.layout().root);
            assert_eq!(id.parse::<Harness>(), Ok(h));
            assert_eq!(serde_json::to_value(h).unwrap(), id);
        }
    }

    #[test]
    fn lists_every_harness() {
        let got: Vec<_> = Harness::ALL.iter().map(|h| (h.id(), h.name())).collect();
        assert_eq!(
            got,
            [
                ("claude_code", "Claude Code"),
                ("codex", "Codex"),
                ("cursor", "Cursor"),
                ("pi", "Pi"),
                ("opencode", "OpenCode"),
            ]
        );
    }

    #[test]
    fn unknown_ids_get_a_useful_message() {
        for (id, want) in [
            ("vim", "no agent \"vim\" (there are: claude_code, codex, cursor, pi, opencode)"),
            ("Claude Code", "no agent \"Claude Code\""),
            ("", "no agent \"\""),
        ] {
            let err = id.parse::<Harness>().unwrap_err().to_string();
            assert!(err.contains(want), "{id}: {err}");
        }
    }

    #[test]
    fn knows_cursors_files() {
        let cursor = Harness::Cursor.layout();
        let rel = |p: &str| cursor.role(Path::new(p));
        assert_eq!(rel("p/agent-transcripts/a1/a1.jsonl"), Some(Role::Session));
        assert_eq!(rel("p/agent-transcripts/a1.jsonl"), Some(Role::Session), "a flat layout, in case");
        let helper = Some(Role::Helper(PathBuf::from("p/agent-transcripts/a1/a1.jsonl")));
        assert_eq!(rel("p/agent-transcripts/a1/subagents/s9.jsonl"), helper);
        for not in [
            "a1.jsonl",
            "p/a1.jsonl",
            "p/terminals/a1/a1.jsonl",
            "p/x/agent-transcripts/a1.jsonl",
            "p/agent-transcripts/a1/other/s9.jsonl",
            "p/agent-transcripts/a1/subagents/deeper/s9.jsonl",
        ] {
            assert_eq!(rel(not), None, "{not}");
        }
        // Claude Code: the session is `p/s.jsonl`; its helpers are in the sibling folder
        // `p/s/subagents/`, not inside a session folder.
        let claude = Harness::ClaudeCode.layout();
        assert_eq!(claude.role(Path::new("p/s.jsonl")), Some(Role::Session));
        assert_eq!(claude.role(Path::new("p/s/subagents/a.jsonl")), Some(Role::Helper(PathBuf::from("p/s.jsonl"))));
        assert_eq!(claude.role(Path::new("p/s/other/a.jsonl")), None);
        assert_eq!(Harness::Codex.layout().role(Path::new("2026/10/22/r.jsonl")), Some(Role::Session));
        assert_eq!(cursor.walk_depth(), 5);
        assert_eq!(claude.walk_depth(), 4);
        assert_eq!(Harness::Codex.layout().walk_depth(), usize::MAX);
    }

    #[test]
    fn day_folders_before_the_window_hold_only_older_sessions() {
        let may = |dir: &str, since: &str| {
            Harness::Codex.layout().may_hold_since(Path::new(dir), Timestamp::parse_rfc3339(since).unwrap())
        };
        // In UTC−12 a session started on 2 March at 05:00 UTC goes in 1 March's folder.
        let since = "2026-03-02T05:00:00Z";
        for dir in ["2026", "2027", "2026/03", "2026/04", "2026/03/01", "2026/03/02", "2026/03/31", "2026/03/01/x"] {
            assert!(may(dir, since), "{dir}");
        }
        for dir in ["2025", "2026/02", "2026/01/31", "2026/02/28", "2025/12/31"] {
            assert!(!may(dir, since), "{dir}");
        }
        // Folders that aren't YYYY/MM/DD might hold anything.
        for dir in ["archive", "2026/3", "2026/03/1", "2026/03/xx", "0002026", "+202"] {
            assert!(may(dir, since), "{dir}");
        }
        // The day before can be in the year before.
        let since = "2026-01-01T03:00:00Z";
        assert!(may("2025", since) && may("2025/12", since) && may("2025/12/31", since));
        assert!(!may("2025/12/30", since) && !may("2025/11", since) && !may("2024", since));
        // Harnesses without day folders: any folder.
        assert!(Harness::ClaudeCode.layout().may_hold_since(Path::new("2001"), Timestamp::from_secs_f64(2e9)));
    }
}
