//! The language-neutral conformance contract: `events <input-dir>` and
//! `status <input-dir> --checkpoints <file>`, both in canonical JSON.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-tail"))
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn scenarios() -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let root = fixtures();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "expected") {
                    continue;
                }
                stack.push(path);
            }
        }
        let Ok(rel) = dir.strip_prefix(&root) else { continue };
        let components: Vec<_> = rel.components().collect();
        if components.len() == 4
            && dir.join("expected/status.jsonl").is_file()
            && let Some(folder) = components[0].as_os_str().to_str()
        {
            found.push((dir.clone(), folder.replace('-', "_")));
        }
    }
    found.sort();
    found
}

fn run_events(input: &Path, agent: Option<&str>) -> Output {
    let mut cmd = command();
    cmd.arg("events");
    if let Some(agent) = agent {
        cmd.args(["--agent", agent]);
    }
    cmd.arg(input).output().unwrap()
}

fn run_status(input: &Path, checkpoints: &Path) -> Output {
    command().args(["status"]).arg(input).args(["--checkpoints"]).arg(checkpoints).output().unwrap()
}

fn lines(output: &Output) -> Vec<Value> {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

fn assert_sorted(value: &Value) {
    match value {
        Value::Object(object) => {
            let keys: Vec<_> = object.keys().cloned().collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted, "canonical JSON must sort keys: {value}");
            for child in object.values() {
                assert_sorted(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_sorted(item);
            }
        }
        _ => {}
    }
}

#[test]
fn events_accepts_an_input_directory_and_is_canonical() {
    let first = run_events(&fixtures().join("pi/1.4/linux/tool-call/input"), Some("pi"));
    let second = run_events(&fixtures().join("pi/1.4/linux/tool-call"), Some("pi"));
    assert_eq!(first.stdout, second.stdout, "file and scenario directories must agree");
    let events = lines(&first);
    assert!(events.iter().any(|event| event["type"] == "tool_call"));
    for event in &events {
        assert_sorted(event);
    }
    let again = run_events(&fixtures().join("pi/1.4/linux/tool-call"), Some("pi"));
    assert_eq!(first.stdout, again.stdout, "canonical output is deterministic");
}

#[test]
fn every_committed_events_file_has_a_canonical_directory_runner() {
    for (scenario, harness) in scenarios() {
        let expected = scenario.join("expected/events.jsonl");
        if !expected.is_file() {
            continue;
        }
        let output = run_events(&scenario, Some(&harness));
        let got = lines(&output);
        let want: Vec<Value> = std::fs::read_to_string(&expected)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(got, want, "{}", scenario.display());
    }
}

#[test]
fn status_runs_every_committed_checkpoint_file() {
    for (scenario, harness) in scenarios() {
        let checkpoints = scenario.join("expected/status.jsonl");
        let output = run_status(&scenario.join("input"), &checkpoints);
        assert!(output.status.success(), "{}: {}", scenario.display(), String::from_utf8_lossy(&output.stderr));
        let got = lines(&output);
        let _ = harness;
        assert_eq!(
            got.len(),
            std::fs::read_to_string(&checkpoints).unwrap().lines().filter(|line| !line.trim().is_empty()).count(),
            "{}: one output object per checkpoint",
            scenario.display()
        );
        for line in &got {
            assert_sorted(line);
            for field in [
                "activity",
                "certainty",
                "reason",
                "permission",
                "sandbox",
                "title",
                "error",
                "cost_usd",
                "out_tokens",
                "usage",
                "recent",
            ] {
                assert!(line.get(field).is_some(), "{}: {field} is always present", scenario.display());
            }
        }
    }
}

#[test]
fn status_fails_a_wrong_expected_value() {
    let scenario = fixtures().join("pi/1.4/linux/tool-call");
    let temp = tempfile::tempdir().unwrap();
    let checkpoints = temp.path().join("status.jsonl");
    let text = std::fs::read_to_string(scenario.join("expected/status.jsonl")).unwrap();
    std::fs::write(&checkpoints, text.replacen("\"activity\": \"thinking\"", "\"activity\": \"errored\"", 1)).unwrap();
    let output = run_status(&scenario.join("input"), &checkpoints);
    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("activity"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn status_rejects_a_checkpoint_that_was_never_reached() {
    let scenario = fixtures().join("pi/1.4/linux/plain-qa");
    let temp = tempfile::tempdir().unwrap();
    let checkpoints = temp.path().join("status.jsonl");
    std::fs::write(&checkpoints, "{\"after_line\":999,\"now_offset_s\":1}\n").unwrap();
    let output = run_status(&scenario.join("input"), &checkpoints);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("never reached"));
}

#[test]
fn status_rejects_a_helper_line_that_goes_backwards() {
    let scenario = fixtures().join("claude-code/2.1/linux/background-link");
    let temp = tempfile::tempdir().unwrap();
    let checkpoints = temp.path().join("status.jsonl");
    std::fs::write(
        &checkpoints,
        concat!(
            "{\"after_line\":4,\"helper\":\"agent-a1\",\"helper_line\":1,\"now_offset_s\":0}\n",
            "{\"after_line\":4,\"helper\":\"agent-a1\",\"helper_line\":0,\"now_offset_s\":0}\n",
        ),
    )
    .unwrap();
    let output = run_status(&scenario.join("input"), &checkpoints);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("went backwards"));
}
