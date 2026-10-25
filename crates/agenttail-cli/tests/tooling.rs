//! Command contracts for the survey drift check and the generated support matrix.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agenttail"))
}

#[test]
fn the_committed_support_matrix_is_generated_from_current_specs_and_fixtures() {
    let root = repo();
    let output = command().args(["support-matrix", "--root"]).arg(&root).arg("--check").output().unwrap();
    assert!(
        output.status.success(),
        "{}\nrun: cargo run -p agenttail-cli -- support-matrix",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(root.join("docs/support-matrix.md")).unwrap();
    for harness in ["Claude Code", "Codex", "Cursor", "Pi", "OpenCode", "Claude Desktop"] {
        assert!(text.contains(harness), "{harness} missing from the support matrix");
    }
    for status in ["CI-verified", "donated-verified", "ported-unverified", "spike", "unsupported"] {
        assert!(text.contains(status), "{status} is documented");
    }
}

#[test]
fn survey_drift_only_fails_on_a_real_shape_change() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let sessions = home.join(".pi/agent/sessions/--w--");
    std::fs::create_dir_all(&sessions).unwrap();
    let transcript = sessions.join("2026-01-01T00-00-00-000Z_s.jsonl");
    let record = |extra: &str| {
        format!(
            "{{\"type\":\"message\",\"id\":\"m1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\
             \"message\":{{\"role\":\"user\",\"content\":\"hello\"}}{extra}}}\n"
        )
    };
    std::fs::write(&transcript, record("")).unwrap();

    let baseline_dir = temp.path().join("baseline");
    std::fs::create_dir_all(&baseline_dir).unwrap();
    let baseline = temp.path().join("shape.json");
    let first = command().args(["survey", "--agent", "pi", "--out"]).arg(&baseline).arg(&transcript).output().unwrap();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    std::fs::copy(&baseline, baseline_dir.join("pi.json")).unwrap();

    let same = command()
        .args(["survey", "--agent", "pi", "--drift", "--baseline"])
        .arg(&baseline_dir)
        .arg(&transcript)
        .output()
        .unwrap();
    assert!(same.status.success(), "{}", String::from_utf8_lossy(&same.stderr));
    assert!(String::from_utf8_lossy(&same.stderr).contains("No shape changes"));

    let changed = temp.path().join("changed.jsonl");
    std::fs::write(&changed, record(",\"brandNew\":{\"field\":1}")).unwrap();
    let drift = command()
        .args(["survey", "--agent", "pi", "--drift", "--baseline"])
        .arg(&baseline_dir)
        .arg(&changed)
        .output()
        .unwrap();
    assert_eq!(drift.status.code(), Some(1));
    let report = String::from_utf8_lossy(&drift.stderr);
    assert!(report.contains("brandNew"), "{report}");
    assert!(report.contains("Shape changed: **yes**"), "{report}");

    // Ids and free text are not shape: changing them must not drift.
    let value_only = temp.path().join("value-only.jsonl");
    std::fs::write(
        &value_only,
        "{\"type\":\"message\",\"id\":\"different\",\"timestamp\":\"2027-02-02T02:02:02Z\",\
         \"message\":{\"role\":\"user\",\"content\":\"different prose\"}}\n",
    )
    .unwrap();
    let same = command()
        .args(["survey", "--agent", "pi", "--drift", "--baseline"])
        .arg(&baseline_dir)
        .arg(&value_only)
        .output()
        .unwrap();
    assert!(same.status.success(), "{}", String::from_utf8_lossy(&same.stderr));
}

#[test]
fn survey_shape_json_has_no_ids_timestamps_or_prose() {
    let temp = tempfile::tempdir().unwrap();
    let transcript = temp.path().join("session.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"message\",\"id\":\"secret-id\",\"timestamp\":\"2026-01-01T00:00:00Z\",\
         \"message\":{\"role\":\"user\",\"content\":\"secret prose\"}}\n",
    )
    .unwrap();
    let out = temp.path().join("shape.json");
    let output = command().args(["survey", "--agent", "pi", "--out"]).arg(&out).arg(&transcript).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains("secret-id"), "{text}");
    assert!(!text.contains("secret prose"), "{text}");
    assert!(!text.contains("2026-01-01"), "{text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "survey shape keys are canonical (sorted)");
}
