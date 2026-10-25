//! Harness discovery facts, embedded from `spec/discovery/*.toml` at build time.
//!
//! [`Layout`] is generated from these files, so the local walk, a remote loop, and the
//! adapters can't drift from the documented roots and folder shapes. The files also carry
//! the ask-user names, observed never-prompt tools, mode vocabulary, timestamp rules, and
//! quirks with their evidence.

use crate::harness::{Harness, HelperOwner, Layout};

/// How an ask-the-user tool name is matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AskMatch {
    /// Exactly as the harness writes it (Claude Code and Codex).
    Exact,
    /// Letters and digits only, lowercased (Cursor's inconsistent names).
    AlnumLower,
}

/// A root and the environment overrides observed for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    /// The transcript root below the user's home folder.
    pub default: &'static str,
    /// Environment variables that move it.
    pub env: &'static [EnvRoot],
    /// Where this was observed.
    pub evidence: &'static str,
}

/// One environment override of a harness's root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvRoot {
    /// The variable's name.
    pub var: &'static str,
    /// The folder appended to its value (for example `projects`).
    pub append: &'static str,
    /// Whether a relative value is ignored, as both current harnesses do.
    pub absolute: bool,
    /// What the variable means, in one line.
    pub semantics: &'static str,
    /// Where this was observed.
    pub evidence: &'static str,
}

/// A tool observed to run without a human prompt, and the modes it was observed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeverPrompt {
    pub tool: &'static str,
    pub modes: &'static [&'static str],
    pub evidence: &'static str,
}

/// One permission-mode value and whether it can ask a human.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeRule {
    pub value: &'static str,
    pub can_ask_human: bool,
    /// Whether this value was seen in an artifact; false means it came from docs or a binary.
    pub observed: bool,
    pub evidence: &'static str,
}

/// How a harness writes event times.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimestampRule {
    pub field: &'static str,
    pub format: &'static str,
    pub timezone: &'static str,
    pub fallback: &'static str,
    pub evidence: &'static str,
}

/// Where a session's project name comes from, and how its folder is encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRule {
    pub source: &'static str,
    pub encoding: &'static str,
    pub examples: &'static [ProjectExample],
    pub evidence: &'static str,
}

/// One real cwd and the project folder it was filed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectExample {
    pub cwd: &'static str,
    pub folder: &'static str,
}

/// A format quirk and its evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quirk {
    pub id: &'static str,
    pub summary: &'static str,
    pub evidence: &'static str,
}

/// A harness that keeps sessions in a SQLite database instead of transcript files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Database {
    /// The default database file below the harness root.
    pub file: &'static str,
    /// Channel-specific database files (`opencode-<channel>.db`).
    pub channel_pattern: &'static str,
    /// The environment variable that names the database file.
    pub env: Option<&'static str>,
    pub evidence: &'static str,
}

/// What the Claude Code `sessions/<pid>.json` registry showed, and whether we read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    pub path: &'static str,
    pub used: bool,
    pub statuses: &'static [&'static str],
    pub waiting_for: &'static [&'static str],
    pub removed_on_graceful_exit: bool,
    pub removed_on_sigkill: bool,
    pub reason: &'static str,
    pub evidence: &'static [&'static str],
}

/// Everything observed about one harness's discovery and format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    pub harness: Harness,
    pub roots: Roots,
    pub layout: Layout,
    pub asks_user: &'static [&'static str],
    pub asks_user_match: AskMatch,
    pub never_prompt: &'static [NeverPrompt],
    pub modes: &'static [ModeRule],
    pub timestamps: TimestampRule,
    pub project: ProjectRule,
    pub quirks: &'static [Quirk],
    pub registry: Option<Registry>,
    pub database: Option<Database>,
}

include!(concat!(env!("OUT_DIR"), "/discovery_generated.rs"));

impl Discovery {
    /// Whether `name` is one of this harness's ask-the-user tools.
    pub fn asks_user(&self, name: &str) -> bool {
        match self.asks_user_match {
            AskMatch::Exact => self.asks_user.contains(&name),
            AskMatch::AlnumLower => {
                let name = alnum_lower(name);
                self.asks_user.iter().any(|tool| alnum_lower(tool) == name)
            }
        }
    }

    /// The override for environment variable `var`, if this harness has one.
    pub fn env_root(&self, var: &str) -> Option<&'static EnvRoot> {
        self.roots.env.iter().find(|env| env.var == var)
    }

    /// The never-prompt tools observed under `mode`.
    pub fn never_prompt_in(&self, mode: &str) -> impl Iterator<Item = &'static NeverPrompt> {
        self.never_prompt.iter().filter(move |tool| tool.modes.contains(&mode))
    }
}

impl Harness {
    /// The discovery facts embedded from `spec/discovery/<id>.toml`.
    pub fn discovery(self) -> &'static Discovery {
        ALL_DISCOVERY.iter().find(|discovery| discovery.harness == self).expect("every harness has a discovery spec")
    }
}

fn alnum_lower(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PermissionMode;
    use std::collections::HashSet;

    #[test]
    fn every_harness_has_its_own_spec_in_all() {
        assert_eq!(ALL_DISCOVERY.len(), Harness::ALL.len());
        let ids: HashSet<_> = ALL_DISCOVERY.iter().map(|discovery| discovery.harness.id()).collect();
        assert_eq!(ids, Harness::ALL.iter().map(|harness| harness.id()).collect());
        for harness in Harness::ALL {
            let discovery = harness.discovery();
            assert_eq!(discovery.harness, harness);
            assert_eq!(discovery.layout, *harness.layout(), "the layout comes from the same spec");
            assert_eq!(discovery.layout.root, discovery.roots.default);
            assert!(!discovery.quirks.is_empty());
        }
    }

    #[test]
    fn mode_vocabularies_agree_with_the_parser() {
        for harness in Harness::ALL {
            for mode in harness.discovery().modes {
                let parsed = PermissionMode::parse(mode.value);
                assert_ne!(parsed, PermissionMode::Unknown, "{}: {}", harness.id(), mode.value);
                assert_eq!(
                    parsed.can_ask_human(),
                    mode.can_ask_human,
                    "{}: {} can_ask_human",
                    harness.id(),
                    mode.value
                );
            }
        }
    }

    #[test]
    fn ask_user_names_match_the_way_each_harness_writes_them() {
        let claude = Harness::ClaudeCode.discovery();
        assert!(claude.asks_user("AskUserQuestion"));
        assert!(claude.asks_user("ExitPlanMode"));
        assert!(!claude.asks_user("askuserquestion"), "Claude's names are exact");

        let codex = Harness::Codex.discovery();
        assert!(codex.asks_user("request_user_input"));
        assert!(!codex.asks_user("RequestUserInput"), "Codex's names are exact");

        let cursor = Harness::Cursor.discovery();
        for name in ["AskQuestion", "ask_question", "AskUserQuestion", "request_user_input"] {
            assert!(cursor.asks_user(name), "{name}");
        }

        // Pi has no built-in ask tool; an empty list is the honest answer.
        assert!(Harness::Pi.discovery().asks_user.is_empty());
    }

    #[test]
    fn never_prompt_lists_are_only_what_was_observed() {
        let claude = Harness::ClaudeCode.discovery();
        let tools: Vec<_> = claude.never_prompt.iter().map(|tool| (tool.tool, tool.modes)).collect();
        assert_eq!(tools, [("Write", &["acceptEdits"][..]), ("Read", &["plan"][..]), ("ToolSearch", &["plan"][..])]);
        assert_eq!(claude.never_prompt_in("default").count(), 0);
        assert_eq!(claude.never_prompt_in("acceptEdits").map(|tool| tool.tool).collect::<Vec<_>>(), ["Write"]);
        // A4 and the Cursor corpus didn't observe one; an empty list is the honest answer.
        assert!(Harness::Codex.discovery().never_prompt.is_empty());
        assert!(Harness::Cursor.discovery().never_prompt.is_empty());
        assert!(Harness::Pi.discovery().never_prompt.is_empty());
    }

    #[test]
    fn roots_and_registry_carry_the_b8_decisions() {
        let claude = Harness::ClaudeCode.discovery();
        assert_eq!(claude.env_root("CLAUDE_CONFIG_DIR").unwrap().append, "projects");
        assert_eq!(Harness::Codex.discovery().env_root("CODEX_HOME").unwrap().append, "sessions");
        let pi = Harness::Pi.discovery();
        assert_eq!(pi.env_root("PI_CODING_AGENT_DIR").unwrap().append, "sessions");
        assert_eq!(pi.env_root("PI_CODING_AGENT_SESSION_DIR").unwrap().append, "");
        assert!(Harness::Cursor.discovery().roots.env.is_empty());
        let registry = claude.registry.as_ref().expect("Claude has a registry decision");
        assert!(!registry.used, "the tracker does not read the registry");
        assert!(registry.removed_on_graceful_exit);
        assert!(!registry.removed_on_sigkill);
        assert!(registry.waiting_for.contains(&"dialog open"), "waiting is broader than a permission prompt");
        assert!(Harness::Codex.discovery().registry.is_none());
        assert!(Harness::Cursor.discovery().registry.is_none());
        assert!(Harness::Pi.discovery().registry.is_none());
    }
}
