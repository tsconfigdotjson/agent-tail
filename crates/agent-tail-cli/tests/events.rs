//! `agent-tail events <path>` is the debugging dump of the parse layer: one normalized event
//! per line, with provenance, and no record silently missing.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(relative)
}

fn events(relative: &str) -> Vec<Value> {
    events_at(&fixture(relative))
}

fn events_at(path: &Path) -> Vec<Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-tail"))
        .arg("events")
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("run agent-tail: {e}"));
    assert!(output.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

#[test]
fn prints_normalized_events_as_json_lines() {
    let cursor = events("cursor/unknown/any/seed-session/input/session.jsonl");
    assert!(cursor.iter().any(|event| event["type"] == "user_prompt"), "{cursor:#?}");
    assert!(cursor.iter().any(|event| event["type"] == "tool_call"), "{cursor:#?}");
    assert!(cursor.iter().any(|event| event["type"] == "turn_end"), "{cursor:#?}");
    for event in &cursor {
        assert!(event["id"].is_string());
        assert!(event["offset"].is_u64() && event["length"].is_u64());
        assert_eq!(event["harness"], "cursor");
        assert_eq!(event["source"], 0);
    }

    let codex = events("codex/0.160/linux/seed-exec/input/session.jsonl");
    assert!(codex.iter().any(|event| event["type"] == "usage"), "{codex:#?}");
    assert!(codex.iter().any(|event| event["type"] == "tool_call"), "{codex:#?}");
    assert!(codex.iter().any(|event| event["type"] == "turn_end"), "{codex:#?}");

    let claude = events("claude-code/2.1/linux/seed-session/input/session.jsonl");
    assert!(claude.iter().any(|event| event["type"] == "title"), "{claude:#?}");
    assert!(claude.iter().any(|event| event["type"] == "cost"), "{claude:#?}");

    let desktop = events("claude-desktop/handmade/any/plain-qa/input/session.jsonl");
    assert!(desktop.iter().any(|event| event["type"] == "model_change"), "{desktop:#?}");
    assert!(desktop.iter().any(|event| event["type"] == "tool_result" || event["type"] == "turn_end"), "{desktop:#?}");
    assert!(desktop.iter().any(|event| event["type"] == "cost"), "{desktop:#?}");
    assert!(desktop.iter().all(|event| event["harness"] == "claude_desktop"));

    let pi = events("pi/1.4/linux/tool-call/input/session.jsonl");
    assert!(pi.iter().any(|event| event["type"] == "tool_call"), "{pi:#?}");
    assert!(pi.iter().any(|event| event["type"] == "usage"), "{pi:#?}");
    assert!(pi.iter().any(|event| event["type"] == "turn_end"), "{pi:#?}");

    let gemini = events("gemini/0.65.0/linux/tool/input/session.jsonl");
    assert!(gemini.iter().any(|event| event["type"] == "tool_call"), "{gemini:#?}");
    assert!(gemini.iter().any(|event| event["type"] == "tool_result"), "{gemini:#?}");
    assert!(gemini.iter().any(|event| event["type"] == "turn_end"), "{gemini:#?}");

    let copilot = events("copilot/1.0.103/linux/permission-denied/input/session.jsonl");
    assert!(copilot.iter().any(|event| event["type"] == "approval_requested"), "{copilot:#?}");
    assert!(copilot.iter().any(|event| event["type"] == "approval_completed"), "{copilot:#?}");
    assert!(copilot.iter().any(|event| event["type"] == "tool_call"), "{copilot:#?}");
}

#[test]
fn committed_expected_events_match_their_inputs() {
    for input in [
        "claude-code/2.1/linux/seed-session",
        "claude-desktop/handmade/any/plain-qa",
        "claude-desktop/handmade/any/tool-turn",
        "claude-desktop/handmade/any/tool-stall",
        "codex/0.160/linux/seed-exec",
        "codex/0.160/linux/seed-resume",
        "cursor/unknown/any/seed-session",
        "pi/1.4/linux/plain-qa",
        "pi/1.4/linux/tool-call",
        "pi/1.4/linux/api-error",
        "pi/1.4/linux/branched",
        "gemini/0.65.0/linux/plain-qa",
        "gemini/0.65.0/linux/tool",
        "copilot/1.0.103/linux/plain-qa",
        "copilot/1.0.103/linux/tool",
        "copilot/1.0.103/linux/permission-denied",
        "copilot/1.0.103/linux/permission-approved",
        "copilot/1.0.103/linux/permission-user-denied",
        "copilot/1.0.103/linux/subagent-helper",
    ] {
        let got = events(&format!("{input}/input/session.jsonl"));
        let expected = fixture(&format!("{input}/expected/events.jsonl"));
        let want: Vec<Value> = std::fs::read_to_string(&expected)
            .unwrap_or_else(|e| panic!("{}: {e}", expected.display()))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
            .collect();
        assert_eq!(got, want, "{}: regenerate expected/events.jsonl", expected.display());
    }
}

#[test]
fn copilot_permission_fixtures_record_approval_versus_denial() {
    for (scenario, approved, result_kind) in [
        ("copilot/1.0.103/linux/permission-approved", true, "approved"),
        ("copilot/1.0.103/linux/permission-user-denied", false, "denied-interactively-by-user"),
    ] {
        let events = events(&format!("{scenario}/input/session.jsonl"));
        let completion = events
            .iter()
            .find(|event| event["type"] == "approval_completed")
            .unwrap_or_else(|| panic!("{scenario}: no approval_completed event"));
        assert_eq!(completion["approved"], approved, "{scenario}");
        assert_eq!(completion["result_kind"], result_kind, "{scenario}");
    }
}

#[test]
fn copilot_helper_records_name_their_helper() {
    let events = events("copilot/1.0.103/linux/subagent-helper/input/session.jsonl");
    let spawn = events
        .iter()
        .find(|event| event["type"] == "helper_spawn")
        .unwrap_or_else(|| panic!("no helper_spawn: {events:#?}"));
    let helper_id = spawn["agent_id"].as_str().expect("the helper's agent id");
    assert_eq!(spawn["tool_use_id"], "call_helper_task");
    let helper_prompt = events
        .iter()
        .find(|event| event["type"] == "user_prompt" && event["helper_id"] == helper_id)
        .unwrap_or_else(|| panic!("no helper prompt: {events:#?}"));
    assert_eq!(helper_prompt["sidechain"], true);
    assert_eq!(helper_prompt["text"], "HELPER_ONE: run the sleep command, then report done.");
    let end = events
        .iter()
        .find(|event| event["type"] == "helper_end" && event["task_id"] == helper_id)
        .unwrap_or_else(|| panic!("no helper_end: {events:#?}"));
    assert_eq!(end["status"], "completed");
}

#[test]
fn prints_the_sidechain_marker_on_helper_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude-sidechain.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user","timestamp":"2026-10-23T10:00:00Z","message":{"content":"parent"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-10-23T10:00:01Z","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"text","text":"child"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    let events = events_at(&path);
    let parent = events
        .iter()
        .find(|event| event["type"] == "user_prompt" && event["text"] == "parent")
        .expect("the parent prompt");
    assert!(parent.get("sidechain").is_none(), "a parent event has no sidechain marker");
    let child = events
        .iter()
        .find(|event| event["type"] == "assistant_text" && event["text"] == "child")
        .expect("the sidechain text");
    assert_eq!(child["sidechain"], true);
}
