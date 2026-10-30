//! The committed fixture corpus and the language-neutral schemas in `spec/schema/` must
//! agree. This validates every committed `expected/events.jsonl` and `expected/status.jsonl`
//! line, every event parsed from the scenario inputs (including harnesses with no committed
//! events file yet), and the reducer's `SessionState`.

use agent_tail::{
    ChunkCtx, Harness, ParseMode, ReducerOptions, SessionReducer, Timestamp, parse_lines, read_state_vscdb_jsonl,
};
use jsonschema::Validator;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn validator(name: &str) -> Validator {
    let path = repo_root().join("spec/schema").join(name);
    let schema: Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn validate(validator: &Validator, value: &Value, label: &str) {
    if let Err(error) = validator.validate(value) {
        panic!("{label}: {error}\ninstance: {}", serde_json::to_string_pretty(value).unwrap());
    }
}

fn json_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{}: {line}: {e}", path.display())))
        .collect()
}

fn find_expected(root: &Path, name: &str, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_expected(&path, name, found);
        } else if path.file_name().is_some_and(|file| file == name) {
            found.push(path);
        }
    }
}

fn scenarios(root: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.join("expected/status.jsonl").is_file() {
                found.push(path);
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

fn event_with(kind: &str, fields: Value) -> Value {
    let mut event = json!({
        "id": "fixture:0:1:0:1:0",
        "harness": "codex",
        "source": 0,
        "offset": 0,
        "length": 1,
        "line": 1,
        "record_ids": [],
        "at": null,
        "type": kind,
    });
    event.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    event
}

#[test]
fn committed_expected_events_and_statuses_validate() {
    let event_schema = validator("event.schema.json");
    let status_schema = validator("status.schema.json");
    let fixtures = repo_root().join("fixtures");

    let mut event_files = Vec::new();
    find_expected(&fixtures, "events.jsonl", &mut event_files);
    event_files.sort();
    assert!(!event_files.is_empty(), "no expected/events.jsonl under {}", fixtures.display());
    for path in event_files {
        for (index, event) in json_lines(&path).into_iter().enumerate() {
            validate(&event_schema, &event, &format!("{}:{}", path.display(), index + 1));
        }
    }

    let mut status_files = Vec::new();
    find_expected(&fixtures, "status.jsonl", &mut status_files);
    status_files.sort();
    assert!(!status_files.is_empty(), "no expected/status.jsonl under {}", fixtures.display());
    for path in status_files {
        for (index, checkpoint) in json_lines(&path).into_iter().enumerate() {
            validate(&status_schema, &checkpoint, &format!("{}:{}", path.display(), index + 1));
        }
    }
}

#[test]
fn every_scenario_parses_to_valid_events_and_state() {
    let event_schema = validator("event.schema.json");
    let state_schema = validator("session-state.schema.json");
    let fixtures = repo_root().join("fixtures");
    let mut found = Vec::new();
    scenarios(&fixtures, &mut found);
    found.sort();
    assert!(!found.is_empty(), "no scenarios under {}", fixtures.display());

    for scenario in found {
        let harness = harness_for(&scenario, &fixtures);
        let path = scenario.join("input/session.jsonl");
        if !path.is_file() {
            let database = scenario.join("input/state-vscdb.jsonl");
            if database.is_file() {
                let bytes = fs::read(&database).unwrap_or_else(|e| panic!("{}: {e}", database.display()));
                let state = read_state_vscdb_jsonl(&bytes).unwrap_or_else(|e| panic!("{}: {e}", database.display()));
                assert!(!state.composers.is_empty(), "{}: no composers", database.display());
                continue;
            }
        }
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for (index, event) in parse_lines(harness, &bytes, ParseMode::Full).enumerate() {
            let value = serde_json::to_value(&event).unwrap();
            validate(&event_schema, &value, &format!("{}: event {index}", path.display()));
        }

        let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
        reducer.feed(&bytes, ChunkCtx { offset: 0, mtime: Timestamp::EPOCH, path: &path }).unwrap();
        let state = serde_json::to_value(reducer.state(Timestamp::from_secs_f64(1_792_756_800.0))).unwrap();
        validate(&state_schema, &state, &format!("{}: final state", path.display()));
    }
}

#[test]
fn event_schema_accepts_every_v0_1_event_kind() {
    let event_schema = validator("event.schema.json");
    let empty_usage = json!({
        "input_tokens": null,
        "output_tokens": null,
        "cache_read_input_tokens": null,
        "cache_creation_input_tokens": null,
    });
    let samples = [
        event_with(
            "user_prompt",
            json!({"text":"go","raw":"go","wrappers":[{"name":"user_query","value":"go"}],"starts_turn":true}),
        ),
        event_with("assistant_text", json!({"text":"done"})),
        event_with("reasoning", json!({"signature":null})),
        event_with(
            "tool_call",
            json!({"name":"Bash","call_id":null,"kind":"shell","input":{"command":"true"},"asks_user":false,
                   "activity_line":{"kind":"command","command":"true"}}),
        ),
        event_with("tool_result", json!({"call_id":null,"is_error":false,"size":0})),
        event_with("approval_requested", json!({"call_id":null,"tool_name":null,"request_kind":"commands"})),
        event_with("approval_completed", json!({"call_id":null,"approved":false,"result_kind":"denied"})),
        event_with("turn_start", json!({"turn_id":null})),
        event_with("turn_end", json!({"reason":"end_turn","error":null,"background_agents":null})),
        event_with("usage", json!({"usage":empty_usage,"reported":empty_usage,"message_id":null,"cumulative":false})),
        event_with("permission_mode", json!({"mode":"unknown"})),
        event_with("sandbox_policy", json!({"policy":"unknown"})),
        event_with("model_change", json!({"model":"test"})),
        event_with("title", json!({"title":"A title","title_source":"generated"})),
        event_with("cost", json!({"usd":0.0})),
        event_with("helper_spawn", json!({"agent_id":null,"task_id":null,"output_file":null,"tool_use_id":null})),
        event_with("helper_end", json!({"task_id":null,"tool_use_id":null,"output_file":null,"status":"completed"})),
        event_with(
            "wakeup_scheduled",
            json!({"kind":"loop","due_at":null,"reason":null,"job_id":null,"recurring":false,
                   "certainty":"firm","source_call_id":null}),
        ),
        event_with("wakeup_cleared", json!({"kind":"cron","job_id":"job-1","source_call_id":null})),
        event_with("compaction", json!({"trigger":null,"pre_tokens":null,"post_tokens":null})),
        event_with(
            "session_info",
            json!({"cwd":null,"branch":null,"session_id":null,"entrypoint":null,"approvals_reviewer":null}),
        ),
        event_with("meta", json!({"record_type":"attachment","subtype":null,"record":{"type":"attachment"}})),
        event_with(
            "unknown",
            json!({"record_type":"new_thing","reason":"unknown record type","record":{"type":"new_thing"}}),
        ),
    ];
    for sample in samples {
        validate(&event_schema, &sample, sample["type"].as_str().unwrap());
    }

    let mut extra = event_with("assistant_text", json!({"text": "done"}));
    extra["not_in_the_contract"] = json!(true);
    assert!(!event_schema.is_valid(&extra), "an unknown event field must fail the schema");

    let mut sparse = event_with("tool_call", json!({"name": "Bash", "kind": "shell", "input": {}, "asks_user": false}));
    sparse.as_object_mut().unwrap().remove("at");
    assert!(event_schema.is_valid(&sparse), "an absent optional field equals null");
}
