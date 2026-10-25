//! Runs the `expected/status.jsonl` checkpoints under `fixtures/`. The checkpoint format is
//! described by `fixtures/README.md`: `after_line` counts transcript entries read so far, and
//! `now` is the checkpoint line's timestamp plus `now_offset_s`. Database-only Cursor
//! snapshots (`input/state-vscdb.jsonl`) check at `after_line: 0`, dated from the composer.

use agent_tail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, Timestamp, read_state_vscdb_jsonl};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
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
        "claude-desktop" => Harness::ClaudeDesktop,
        "codex" => Harness::Codex,
        "cursor" => Harness::Cursor,
        "pi" => Harness::Pi,
        "opencode" => Harness::OpenCode,
        "gemini" => Harness::Gemini,
        "copilot" => Harness::Copilot,
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
        if input.join("state-vscdb.jsonl").is_file() && !input.join("session.jsonl").is_file() {
            check_database_snapshot(&scenario, &input);
            continue;
        }
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

        // Optional helper transcripts and `.meta.json` live under input/subagents; a
        // checkpoint can feed them before it checks the parent's state.
        let helpers = helper_files(&input);
        let mut helper_offsets: HashMap<String, u64> = HashMap::new();
        let mut helper_lines: HashMap<String, usize> = HashMap::new();

        let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
        let mut offset = 0u64;
        let mut line_no = 0u64;
        let mut seen = vec![false; checkpoints.len()];
        let mut last_time = Timestamp::EPOCH;
        for raw in bytes.split_inclusive(|&b| b == b'\n') {
            let entry: Option<Value> = serde_json::from_slice(raw.trim_ascii()).ok();
            let time = entry.as_ref().and_then(entry_time).unwrap_or(last_time);
            last_time = time;
            let ctx = ChunkCtx { offset, mtime: time, path: file };
            reducer.feed(raw, ctx).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            offset += raw.len() as u64;
            line_no += 1;

            for (index, checkpoint) in checkpoints.iter().enumerate() {
                if checkpoint["after_line"].as_u64() != Some(line_no) {
                    continue;
                }
                if let Some(key) = checkpoint["helper"].as_str() {
                    let text = helpers
                        .transcripts
                        .get(key)
                        .unwrap_or_else(|| panic!("{}: no helper {key}", scenario.display()));
                    let lines: Vec<&[u8]> = text.as_bytes().split_inclusive(|&b| b == b'\n').collect();
                    let want = checkpoint["helper_line"].as_u64().map_or(lines.len(), |line| line as usize);
                    let fed = helper_lines.entry(key.into()).or_default();
                    let id = format!("subagents/{key}.jsonl");
                    for raw in &lines[*fed..want.min(lines.len())] {
                        let entry: Option<Value> = serde_json::from_slice(raw.trim_ascii()).ok();
                        let time = entry.as_ref().and_then(entry_time).unwrap_or(last_time);
                        let helper_offset = helper_offsets.entry(key.into()).or_default();
                        let ctx = ChunkCtx { offset: *helper_offset, mtime: time, path: file };
                        reducer
                            .feed_helper(&id, raw, ctx)
                            .unwrap_or_else(|e| panic!("{} helper {key}: {e}", scenario.display()));
                        *helper_offset += raw.len() as u64;
                    }
                    *fed = want.min(lines.len());
                }
                if let Some(key) = checkpoint["meta"].as_str() {
                    let bytes = helpers
                        .metas
                        .get(key)
                        .unwrap_or_else(|| panic!("{}: no helper metadata {key}", scenario.display()));
                    reducer.feed_helper_meta(&format!("subagents/{key}.jsonl"), bytes);
                }
                seen[index] = true;
                let now = time + std::time::Duration::from_secs_f64(checkpoint["now_offset_s"].as_f64().unwrap());
                let state = serde_json::to_value(reducer.state(now)).unwrap();
                for key in [
                    "activity",
                    "certainty",
                    "reason",
                    "permission",
                    "sandbox",
                    "title",
                    "cost_usd",
                    "out_tokens",
                    "usage",
                    "recent",
                    "background",
                ] {
                    let Some(want) = checkpoint.get(key) else { continue };
                    let got = if key == "title" {
                        state.get("title").or_else(|| state.get("meta").and_then(|meta| meta.get("title")))
                    } else {
                        state.get(key)
                    };
                    assert_eq!(got, Some(want), "{} after line {line_no}: {key}", scenario.display());
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
/// A record's own time: an RFC 3339 field for file harnesses, or an OpenCode row's
/// `time_created`/`time_updated` in Unix milliseconds.
fn entry_time(entry: &Value) -> Option<Timestamp> {
    if let Some(time) = entry["timestamp"].as_str().and_then(Timestamp::parse_rfc3339) {
        return Some(time);
    }
    if let Some(time) = entry["startTime"].as_str().and_then(Timestamp::parse_rfc3339) {
        return Some(time);
    }
    let row = entry.get("row").unwrap_or(entry);
    row.get("time_created")
        .or_else(|| row.get("time_updated"))
        .and_then(Value::as_f64)
        .filter(|&millis| millis > 0.0)
        .map(|millis| Timestamp::from_secs_f64(millis / 1000.0))
}

struct HelperFiles {
    transcripts: BTreeMap<String, String>,
    metas: BTreeMap<String, Vec<u8>>,
}

/// Helper files from `input/subagents/`, keyed by the `<agent>` in `agent-<agent>.jsonl`.
fn helper_files(input: &Path) -> HelperFiles {
    let mut transcripts = BTreeMap::new();
    let mut metas = BTreeMap::new();
    let dir = input.join("subagents");
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue };
        if let Some(key) = name.strip_suffix(".meta.json") {
            if let Ok(bytes) = fs::read(&path) {
                metas.insert(key.to_string(), bytes);
            }
        } else if let Some(key) = name.strip_suffix(".jsonl")
            && let Ok(text) = fs::read_to_string(&path)
        {
            transcripts.insert(key.to_string(), text);
        }
    }
    HelperFiles { transcripts, metas }
}

/// Cursor database-only fixtures have no transcript: `input/state-vscdb.jsonl` is the
/// donated snapshot, and `expected/status.jsonl` asserts what the database reader says.
fn check_database_snapshot(scenario: &Path, input: &Path) {
    let path = input.join("state-vscdb.jsonl");
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let state = read_state_vscdb_jsonl(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let composer = state.composers.first().unwrap_or_else(|| panic!("{}: no composers", path.display()));
    let at = composer.updated_at().unwrap_or(Timestamp::EPOCH);
    let mut reducer = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
    reducer.set_composer_title(composer.name.as_deref());
    reducer.set_composer_status(composer.activity().map(|activity| (activity, at)));

    let checkpoints: Vec<Value> = fs::read_to_string(scenario.join("expected/status.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut seen = vec![false; checkpoints.len()];
    for (index, checkpoint) in checkpoints.iter().enumerate() {
        if checkpoint["after_line"].as_u64() != Some(0) {
            continue;
        }
        seen[index] = true;
        let now = at + std::time::Duration::from_secs_f64(checkpoint["now_offset_s"].as_f64().unwrap());
        let state = serde_json::to_value(reducer.state(now)).unwrap();
        for key in [
            "activity",
            "certainty",
            "reason",
            "permission",
            "sandbox",
            "title",
            "cost_usd",
            "out_tokens",
            "usage",
            "recent",
            "background",
        ] {
            let Some(want) = checkpoint.get(key) else { continue };
            let got = if key == "title" {
                state.get("title").or_else(|| state.get("meta").and_then(|meta| meta.get("title")))
            } else {
                state.get(key)
            };
            assert_eq!(got, Some(want), "{} after database row: {key}", scenario.display());
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
