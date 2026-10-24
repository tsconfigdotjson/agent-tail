//! Runs the `expected/status.jsonl` checkpoints under `fixtures/`. The checkpoint format is
//! described by `fixtures/README.md`: `after_line` counts entries read so far, and `now` is
//! the checkpoint line's timestamp plus `now_offset_s`.

use agenttail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, Timestamp};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn scenarios(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "expected")
                && path.join("status.jsonl").is_file()
                && let Some(scenario) = path.parent()
            {
                found.push(scenario.to_path_buf());
            } else {
                scenarios(&path, found);
            }
        }
    }
}

fn harness_for(scenario: &Path, root: &Path) -> Harness {
    let name = scenario.strip_prefix(root).unwrap().components().next().unwrap().as_os_str().to_string_lossy();
    match name.as_ref() {
        "claude-code" => Harness::ClaudeCode,
        "codex" => Harness::Codex,
        "cursor" => Harness::Cursor,
        other => panic!("{}: unknown harness folder {other}", scenario.display()),
    }
}

#[test]
fn every_expected_status_checkpoint_holds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut found = Vec::new();
    scenarios(&root, &mut found);
    found.sort();
    assert!(!found.is_empty(), "no status fixtures under {}", root.display());

    for scenario in found {
        let harness = harness_for(&scenario, &root);
        let input = scenario.join("input");
        let files: Vec<_> = fs::read_dir(&input)
            .unwrap_or_else(|e| panic!("{}: {e}", input.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1, "{}: one input transcript per scenario", scenario.display());
        let file = &files[0];
        let bytes = fs::read(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let checkpoints: Vec<Value> = fs::read_to_string(scenario.join("expected/status.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
        let mut offset = 0u64;
        let mut line_no = 0u64;
        let mut seen = vec![false; checkpoints.len()];
        let mut last_time = Timestamp::EPOCH;
        for raw in bytes.split_inclusive(|&b| b == b'\n') {
            let entry: Option<Value> = serde_json::from_slice(raw.trim_ascii()).ok();
            let time = entry
                .as_ref()
                .and_then(|e| e["timestamp"].as_str())
                .and_then(Timestamp::parse_rfc3339)
                .unwrap_or(last_time);
            last_time = time;
            let ctx = ChunkCtx { offset, mtime: time, path: file };
            reducer.feed(raw, ctx).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            offset += raw.len() as u64;
            line_no += 1;

            for (index, checkpoint) in checkpoints.iter().enumerate() {
                if checkpoint["after_line"].as_u64() != Some(line_no) {
                    continue;
                }
                seen[index] = true;
                let now = time + std::time::Duration::from_secs_f64(checkpoint["now_offset_s"].as_f64().unwrap());
                let state = serde_json::to_value(reducer.state(now)).unwrap();
                for key in ["activity", "certainty", "reason", "permission", "sandbox"] {
                    let Some(want) = checkpoint.get(key) else { continue };
                    assert_eq!(state.get(key), Some(want), "{} after line {line_no}: {key}", scenario.display());
                }
            }
        }
        for (index, checkpoint) in checkpoints.iter().enumerate() {
            assert!(
                seen[index],
                "{}: checkpoint after_line {} was never reached",
                scenario.display(),
                checkpoint["after_line"]
            );
        }
    }
}
