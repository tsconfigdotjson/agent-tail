//! `agenttail events <path>` is the debugging dump of the parse layer: one normalized event
//! per line, with provenance, and no record silently missing.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(relative)
}

fn events(relative: &str) -> Vec<Value> {
    let path = fixture(relative);
    let output = Command::new(env!("CARGO_BIN_EXE_agenttail"))
        .arg("events")
        .arg(&path)
        .output()
        .unwrap_or_else(|e| panic!("run agenttail: {e}"));
    assert!(output.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

#[test]
fn prints_normalized_events_as_json_lines() {
    let cursor = events("legacy/cursor-synthetic.jsonl");
    assert!(cursor.iter().any(|event| event["type"] == "user_prompt"), "{cursor:#?}");
    assert!(cursor.iter().any(|event| event["type"] == "tool_call"), "{cursor:#?}");
    assert!(cursor.iter().any(|event| event["type"] == "turn_end"), "{cursor:#?}");
    for event in &cursor {
        assert!(event["id"].is_string());
        assert!(event["offset"].is_u64() && event["length"].is_u64());
        assert_eq!(event["harness"], "cursor");
        assert_eq!(event["source"], 0);
    }

    let codex = events("legacy/codex-0.160-exec.jsonl");
    assert!(codex.iter().any(|event| event["type"] == "usage"), "{codex:#?}");
    assert!(codex.iter().any(|event| event["type"] == "tool_call"), "{codex:#?}");
    assert!(codex.iter().any(|event| event["type"] == "turn_end"), "{codex:#?}");

    let claude = events("legacy/claude-code-2.1.jsonl");
    assert!(claude.iter().any(|event| event["type"] == "title"), "{claude:#?}");
    assert!(claude.iter().any(|event| event["type"] == "cost"), "{claude:#?}");
}
