//! `agent-tail fixture` is the offline fixture path: redact deterministically, capture a
//! real session into the versioned layout, and fail a scan when a secret or home path is
//! still present.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-tail"))
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

#[test]
fn redact_writes_the_file_and_a_value_free_report() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("session.jsonl");
    let output = temp.path().join("redacted/session.jsonl");
    let report = temp.path().join("redacted/report.json");
    fs::write(
        &input,
        concat!(
            r#"{"z":1,"cwd":"/home/alice/work/app","note":"sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345"}"#,
            "\n",
            r#"{"text":"hello alice@real.example","host":"build.internal"}"#,
            "\n",
        ),
    )
    .unwrap();

    let out =
        command().args(["fixture", "redact"]).arg(&input).arg(&output).arg("--report").arg(&report).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = fs::read_to_string(&output).unwrap();
    assert!(!text.contains("alice"), "{text}");
    assert!(!text.contains("sk-ant-"), "{text}");
    assert!(!text.contains("build.internal"), "{text}");
    assert!(text.contains("<user>"));
    assert!(text.contains("<secret-"));
    assert!(text.contains("<email>"));
    assert!(text.contains("<host>"));
    assert!(text.starts_with(r#"{"z":1,"cwd""#), "field order kept: {text}");

    let report = fs::read_to_string(&report).unwrap();
    assert!(!report.contains("alice"));
    assert!(!report.contains("sk-ant-"));
    assert!(!report.contains("build.internal"));
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["lines"], 2);
    assert_eq!(report["offsets"].as_array().unwrap().len(), 2);
    assert!(report["replacements"]["home_path"].as_u64().unwrap() >= 1);
    assert!(report["replacements"]["secret_pattern"].as_u64().unwrap() >= 1);
}

#[test]
fn redact_lorem_keeps_length_and_whitespace() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("session.jsonl");
    let output = temp.path().join("out.jsonl");
    fs::write(&input, "{\"text\":\"Hello there\\nsecond line\"}\n").unwrap();
    let out = command().args(["fixture", "redact", "--lorem"]).arg(&input).arg(&output).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: Value = serde_json::from_str(fs::read_to_string(&output).unwrap().trim()).unwrap();
    let text = value["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), "Hello there\nsecond line".chars().count());
    assert!(text.contains('\n'));
    assert!(!text.contains("Hello"));
}

#[test]
fn capture_picks_a_session_copies_helpers_and_redacts_everything() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let session = home.join(".claude/projects/project/session-1.jsonl");
    let helper_dir = home.join(".claude/projects/project/session-1/subagents");
    fs::create_dir_all(&helper_dir).unwrap();
    fs::write(
        &session,
        concat!(
            r#"{"type":"user","version":"2.1.310","timestamp":"2026-01-01T00:00:00Z","cwd":"/home/alice/app","message":{"role":"user","content":"say hi"}}"#,
            "\n",
            r#"{"type":"assistant","version":"2.1.310","timestamp":"2026-01-01T00:00:01Z","cwd":"/home/alice/app","message":{"id":"m1","stop_reason":"end_turn","content":[{"type":"text","text":"hi alice@real.example"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    fs::write(
        helper_dir.join("agent-a1.jsonl"),
        r#"{"type":"user","timestamp":"2026-01-01T00:00:01Z","cwd":"/home/alice/app","message":{"role":"user","content":"helper"}}"#,
    )
    .unwrap();
    fs::write(helper_dir.join("agent-a1.meta.json"), r#"{"agentType":"Explore","description":"helper"}"#).unwrap();

    let root = temp.path().join("fixtures");
    let out = command()
        .env("HOME", &home)
        .args(["fixture", "capture", "--scenario", "captured-test"])
        .arg("--session")
        .arg(&session)
        .arg("--out")
        .arg(&root)
        .args(["--version", "2.1.310", "--os", "linux"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let target = root.join("claude-code/2.1.310/linux/captured-test");
    let session_text = fs::read_to_string(target.join("input/session.jsonl")).unwrap();
    assert!(session_text.contains("/home/<user>/app"), "{session_text}");
    assert!(!session_text.contains("/home/alice"));
    assert!(!session_text.contains("alice@real.example"));
    assert!(target.join("input/subagents/agent-a1.jsonl").is_file());
    assert!(target.join("input/subagents/agent-a1.meta.json").is_file());
    assert!(target.join("redaction.json").is_file());
    let meta = fs::read_to_string(target.join("meta.toml")).unwrap();
    assert!(meta.contains("harness = \"claude_code\""));
    assert!(meta.contains("version = \"2.1.310\""));
    assert!(meta.contains("capture = \"donated\""));
    assert!(meta.contains("redaction = \"redaction.json\""));

    let scan = command().args(["fixture", "scan"]).arg(&target).output().unwrap();
    assert!(
        scan.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&scan.stdout),
        String::from_utf8_lossy(&scan.stderr)
    );
}

#[test]
fn scan_fails_a_real_secret_and_passes_committed_fixtures() {
    let temp = TempDir::new().unwrap();
    let bad = temp.path().join("bad.jsonl");
    fs::write(&bad, r#"{"cwd":"/Users/real-person/app","note":"sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345"}"#)
        .unwrap();
    let out = command().args(["fixture", "scan"]).arg(&bad).output().unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("home_path"), "{stdout}");
    assert!(stdout.contains("secret_pattern"), "{stdout}");
    assert!(!stdout.contains("real-person"), "the scan must not print the value: {stdout}");

    let out = command().args(["fixture", "scan"]).arg(fixtures()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
