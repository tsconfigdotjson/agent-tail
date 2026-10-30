//! The language-neutral conformance commands: `events <input-dir>` and
//! `status <input-dir> --checkpoints <file>`.
//!
//! Both read a fixture-style input directory (`session.jsonl` plus optional
//! `subagents/`, or a database-only `state-vscdb.jsonl`), and both write canonical JSON:
//! object keys sorted at every depth, and, when comparing, a missing optional field is the
//! same as an explicit `null`.

use crate::{harness_for, harness_from_bytes};
use agent_tail::{
    ChunkCtx, Harness, ParseMode, ReducerOptions, SessionReducer, Timestamp, parse_lines, read_state_vscdb_jsonl,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// `agent-tail events [--agent ID] <input-dir|transcript>`
pub fn events(args: &[String]) -> Result<(), String> {
    let (path, agent) = path_and_agent(args, "events")?;
    let input = Input::locate(&path)?;
    if let Some(database) = &input.database {
        return Err(format!("{}: a state.vscdb snapshot has no transcript events", database.display()));
    }
    let bytes = std::fs::read(&input.main).map_err(|e| format!("{}: {e}", input.main.display()))?;
    let harness =
        agent.or_else(|| harness_for(&input.main)).or_else(|| harness_from_bytes(&bytes)).ok_or_else(|| {
            format!(
                "can't tell which harness wrote {}; pass --agent claude_code|claude_desktop|codex|cursor|pi|opencode|gemini|copilot",
                input.main.display()
            )
        })?;
    for event in parse_lines(harness, &bytes, ParseMode::Full) {
        let value = serde_json::to_value(&event).map_err(|e| e.to_string())?;
        println!("{}", serde_json::to_string(&canonical(&value)).map_err(|e| e.to_string())?);
    }
    Ok(())
}

/// `agent-tail status [--agent ID] <input-dir|transcript> --checkpoints <file>`
///
/// Prints one canonical JSON object per checkpoint, in checkpoint order, then exits with
/// [`ExitCode::FAILURE`] when a checkpoint's declared expected fields do not match. The
/// checkpoint file is both the timing script and the expected result; a checkpoint with only
/// `after_line` and `now_offset_s` is input-only and is always accepted.
pub fn status(args: &[String]) -> Result<ExitCode, String> {
    let mut checkpoints_path = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--checkpoints" => {
                let value = iter.next().ok_or("--checkpoints needs a value")?;
                checkpoints_path = Some(PathBuf::from(value));
            }
            other => rest.push((*other).to_string()),
        }
    }
    let checkpoints_path = checkpoints_path.ok_or("status needs --checkpoints <file>")?;
    let (path, agent) = path_and_agent(&rest, "status")?;
    let input = Input::locate(&path)?;
    let checkpoints = read_checkpoints(&checkpoints_path)?;
    let matched = if let Some(database) = &input.database {
        let bytes = std::fs::read(database).map_err(|e| format!("{}: {e}", database.display()))?;
        let state = read_state_vscdb_jsonl(&bytes).map_err(|e| e.to_string())?;
        run_database_checkpoints(&input, &state, &checkpoints)?
    } else {
        let bytes = std::fs::read(&input.main).map_err(|e| format!("{}: {e}", input.main.display()))?;
        let harness =
            agent.or_else(|| harness_for(&input.main)).or_else(|| harness_from_bytes(&bytes)).ok_or_else(|| {
                format!(
                    "can't tell which harness wrote {}; pass --agent claude_code|claude_desktop|codex|cursor|pi|opencode|gemini|copilot",
                    input.main.display()
                )
            })?;
        run_checkpoints(harness, &input, &checkpoints)?
    };
    Ok(if matched { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn path_and_agent(args: &[String], command: &str) -> Result<(PathBuf, Option<Harness>), String> {
    let mut agent = None;
    let mut path = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--agent" => {
                let id = iter.next().ok_or("--agent needs a value")?;
                agent = Some(id.parse::<Harness>().map_err(|e| e.to_string())?);
            }
            other if other.starts_with('-') => return Err(format!("no option \"{other}\"")),
            other if path.is_some() => return Err(format!("{command} takes one input path, not \"{other}\"")),
            other => path = Some(PathBuf::from(other)),
        }
    }
    Ok((path.ok_or_else(|| format!("{command} needs an input directory or transcript"))?, agent))
}

/// A fixture-style input: one main transcript, or a database snapshot, and the directory
/// its helpers live under.
#[derive(Debug)]
struct Input {
    main: PathBuf,
    database: Option<PathBuf>,
    root: PathBuf,
}

impl Input {
    fn locate(path: &Path) -> Result<Input, String> {
        if path.is_file() {
            let database = path.file_name().is_some_and(|name| name == "state-vscdb.jsonl");
            return Ok(Input {
                main: path.to_path_buf(),
                database: database.then(|| path.to_path_buf()),
                root: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            });
        }
        if !path.is_dir() {
            return Err(format!("{}: not a file or directory", path.display()));
        }
        // Accept a scenario directory (`input/session.jsonl`) as well as the input directory
        // itself (`session.jsonl`). A database-only fixture has `input/state-vscdb.jsonl`.
        let dir = if path.join("input/session.jsonl").is_file() || path.join("input/state-vscdb.jsonl").is_file() {
            path.join("input")
        } else {
            path.to_path_buf()
        };
        let main = dir.join("session.jsonl");
        if main.is_file() {
            return Ok(Input { main, database: None, root: dir });
        }
        let database = dir.join("state-vscdb.jsonl");
        if database.is_file() {
            return Ok(Input { main: database.clone(), database: Some(database), root: dir });
        }
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let file = entry.path();
            if file.is_file() && file.extension().is_some_and(|ext| ext == "jsonl") {
                found.push(file);
            }
        }
        found.sort();
        match found.as_slice() {
            [one] => Ok(Input { main: one.clone(), database: None, root: dir }),
            [] => Err(format!(
                "{}: no session.jsonl, state-vscdb.jsonl (or single .jsonl) in the input directory",
                dir.display()
            )),
            many => Err(format!(
                "{}: expected one main transcript, found {}: {}",
                dir.display(),
                many.len(),
                many.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy()).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

fn read_checkpoints(path: &Path) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut checkpoints = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
        if !value.is_object() {
            return Err(format!("{}:{}: a checkpoint must be a JSON object", path.display(), index + 1));
        }
        let after = value["after_line"]
            .as_u64()
            .ok_or_else(|| format!("{}:{}: checkpoint needs an integer after_line", path.display(), index + 1))?;
        let _ = after;
        if !value["now_offset_s"].as_f64().is_some_and(f64::is_finite) {
            return Err(format!("{}:{}: checkpoint needs a finite now_offset_s", path.display(), index + 1));
        }
        checkpoints.push(value);
    }
    if checkpoints.is_empty() {
        return Err(format!("{}: no checkpoints", path.display()));
    }
    Ok(checkpoints)
}

/// The status fields a checkpoint can assert. Always emitted, `null` when absent, so a port
/// can diff its own canonical output against the same file.
const STATUS_FIELDS: [&str; 13] = [
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
    "background",
    "pending_wakeups",
];

fn run_checkpoints(harness: Harness, input: &Input, checkpoints: &[Value]) -> Result<bool, String> {
    let bytes = std::fs::read(&input.main).map_err(|e| format!("{}: {e}", input.main.display()))?;
    let helpers = helper_files(&input.root);
    let mut helper_offsets: BTreeMap<String, u64> = BTreeMap::new();
    let mut helper_lines: BTreeMap<String, usize> = BTreeMap::new();
    let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
    let mut seen = vec![false; checkpoints.len()];
    let mut matched = true;
    let mut last_time = Timestamp::EPOCH;

    // A checkpoint can fire before any line (after_line 0), which is also how an empty or
    // not-yet-written transcript is tested.
    matched &= check_at(
        0,
        Timestamp::EPOCH,
        &mut reducer,
        checkpoints,
        &mut seen,
        &helpers,
        &mut helper_lines,
        &mut helper_offsets,
        input.main.as_path(),
        &mut last_time,
    )?;

    let mut offset = 0u64;
    let mut line_no = 0u64;
    for raw in bytes.split_inclusive(|&b| b == b'\n') {
        let entry: Option<Value> = serde_json::from_slice(raw.trim_ascii()).ok();
        let time = entry.as_ref().and_then(entry_time).unwrap_or(last_time);
        last_time = time;
        let ctx = ChunkCtx { offset, mtime: time, path: &input.main };
        reducer.feed(raw, ctx).map_err(|e| format!("{}: {e}", input.main.display()))?;
        offset += raw.len() as u64;
        line_no += 1;
        matched &= check_at(
            line_no,
            time,
            &mut reducer,
            checkpoints,
            &mut seen,
            &helpers,
            &mut helper_lines,
            &mut helper_offsets,
            input.main.as_path(),
            &mut last_time,
        )?;
    }

    let missing: Vec<_> = checkpoints
        .iter()
        .enumerate()
        .filter(|(index, _)| !seen[*index])
        .map(|(_, checkpoint)| checkpoint["after_line"].as_u64().unwrap_or_default())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{}: checkpoint after_line {} was never reached",
            input.main.display(),
            missing.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(matched)
}

#[allow(clippy::too_many_arguments)]
fn check_at(
    line_no: u64,
    time: Timestamp,
    reducer: &mut SessionReducer,
    checkpoints: &[Value],
    seen: &mut [bool],
    helpers: &HelperFiles,
    helper_lines: &mut BTreeMap<String, usize>,
    helper_offsets: &mut BTreeMap<String, u64>,
    main_path: &Path,
    last_time: &mut Timestamp,
) -> Result<bool, String> {
    let mut matched = true;
    for (index, checkpoint) in checkpoints.iter().enumerate() {
        if checkpoint["after_line"].as_u64() != Some(line_no) {
            continue;
        }
        if let Some(key) = checkpoint["helper"].as_str() {
            let text =
                helpers.transcripts.get(key).ok_or_else(|| format!("no helper transcript subagents/{key}.jsonl"))?;
            let lines: Vec<&[u8]> = text.as_bytes().split_inclusive(|&b| b == b'\n').collect();
            let want = checkpoint["helper_line"].as_u64().map_or(lines.len(), |line| line as usize);
            let fed = helper_lines.entry(key.into()).or_default();
            if want < *fed {
                return Err(format!("after_line {line_no}: helper {key} helper_line {want} went backwards from {fed}"));
            }
            let id = format!("subagents/{key}.jsonl");
            for raw in &lines[*fed..want.min(lines.len())] {
                let entry: Option<Value> = serde_json::from_slice(raw.trim_ascii()).ok();
                let helper_time = entry.as_ref().and_then(entry_time).unwrap_or(*last_time);
                let offset = helper_offsets.entry(key.into()).or_default();
                let ctx = ChunkCtx { offset: *offset, mtime: helper_time, path: main_path };
                reducer.feed_helper(&id, raw, ctx).map_err(|e| format!("helper {key}: {e}"))?;
                *offset += raw.len() as u64;
            }
            *fed = want.min(lines.len());
        }
        if let Some(key) = checkpoint["meta"].as_str() {
            let bytes =
                helpers.metas.get(key).ok_or_else(|| format!("no helper metadata subagents/{key}.meta.json"))?;
            reducer.feed_helper_meta(&format!("subagents/{key}.jsonl"), bytes);
        }
        seen[index] = true;
        let offset = checkpoint["now_offset_s"].as_f64().unwrap_or_default();
        if offset < 0.0 {
            return Err(format!("after_line {line_no}: now_offset_s must not be negative"));
        }
        let now = time + Duration::from_secs_f64(offset);
        let state = serde_json::to_value(reducer.state(now)).map_err(|e| e.to_string())?;
        let actual = status_object(&state, checkpoint);
        println!("{}", serde_json::to_string(&canonical(&actual)).map_err(|e| e.to_string())?);
        for field in STATUS_FIELDS {
            let Some(want) = checkpoint.get(field) else { continue };
            let got = actual.get(field).unwrap_or(&Value::Null);
            if !equivalent(want, got) {
                eprintln!(
                    "{}: after_line {line_no}: {field}: expected {}, got {}",
                    main_path.display(),
                    serde_json::to_string(&canonical(want)).unwrap_or_default(),
                    serde_json::to_string(&canonical(got)).unwrap_or_default()
                );
                matched = false;
            }
        }
    }
    Ok(matched)
}

/// A record's own time: an RFC 3339 field for file harnesses, or an OpenCode row's
/// `time_created`/`time_updated` in Unix milliseconds.
fn entry_time(entry: &Value) -> Option<Timestamp> {
    if let Some(time) = entry["timestamp"].as_str().and_then(Timestamp::parse_rfc3339) {
        return Some(time);
    }
    let row = entry.get("row").unwrap_or(entry);
    row.get("time_created")
        .or_else(|| row.get("time_updated"))
        .and_then(Value::as_f64)
        .filter(|&millis| millis > 0.0)
        .map(|millis| Timestamp::from_secs_f64(millis / 1000.0))
}

fn run_database_checkpoints(
    input: &Input,
    database: &agent_tail::CursorState,
    checkpoints: &[Value],
) -> Result<bool, String> {
    let composer =
        database.composers.first().ok_or_else(|| format!("{}: no composerData rows", input.main.display()))?;
    let at = composer.updated_at().unwrap_or(Timestamp::EPOCH);
    let mut reducer = SessionReducer::new(Harness::Cursor, ReducerOptions::default());
    reducer.set_composer_title(composer.name.as_deref());
    reducer.set_composer_status(composer.activity().map(|activity| (activity, at)));

    let mut seen = vec![false; checkpoints.len()];
    let mut matched = true;
    for (index, checkpoint) in checkpoints.iter().enumerate() {
        if checkpoint["after_line"].as_u64() != Some(0) {
            continue;
        }
        seen[index] = true;
        let offset = checkpoint["now_offset_s"].as_f64().unwrap_or_default();
        if offset < 0.0 {
            return Err(format!("after database snapshot: now_offset_s must not be negative"));
        }
        let now = at + Duration::from_secs_f64(offset);
        let state = serde_json::to_value(reducer.state(now)).map_err(|e| e.to_string())?;
        let actual = status_object(&state, checkpoint);
        println!("{}", serde_json::to_string(&canonical(&actual)).map_err(|e| e.to_string())?);
        for field in STATUS_FIELDS {
            let Some(want) = checkpoint.get(field) else { continue };
            let got = actual.get(field).unwrap_or(&Value::Null);
            if !equivalent(want, got) {
                eprintln!(
                    "{}: after database snapshot: {field}: expected {}, got {}",
                    input.main.display(),
                    serde_json::to_string(&canonical(want)).unwrap_or_default(),
                    serde_json::to_string(&canonical(got)).unwrap_or_default()
                );
                matched = false;
            }
        }
    }
    let missing: Vec<_> = checkpoints
        .iter()
        .enumerate()
        .filter(|(index, _)| !seen[*index])
        .map(|(_, checkpoint)| checkpoint["after_line"].as_u64().unwrap_or_default())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{}: checkpoint after_line {} was never reached",
            input.main.display(),
            missing.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(matched)
}

fn status_object(state: &Value, checkpoint: &Value) -> Value {
    let mut object = Map::new();
    object.insert("after_line".into(), checkpoint["after_line"].clone());
    object.insert("now_offset_s".into(), checkpoint["now_offset_s"].clone());
    for field in [
        "activity",
        "certainty",
        "reason",
        "permission",
        "sandbox",
        "error",
        "cost_usd",
        "out_tokens",
        "usage",
        "recent",
        "background",
        "pending_wakeups",
    ] {
        object.insert(field.into(), state.get(field).cloned().unwrap_or(Value::Null));
    }
    let title = state
        .get("meta")
        .and_then(|meta| meta.get("title"))
        .or_else(|| state.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map_or(Value::Null, |title| json!(title));
    object.insert("title".into(), title);
    Value::Object(object)
}

struct HelperFiles {
    transcripts: BTreeMap<String, String>,
    metas: BTreeMap<String, Vec<u8>>,
}

fn helper_files(input: &Path) -> HelperFiles {
    let mut transcripts = BTreeMap::new();
    let mut metas = BTreeMap::new();
    let dir = input.join("subagents");
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue };
        if let Some(key) = name.strip_suffix(".meta.json") {
            if let Ok(bytes) = std::fs::read(&path) {
                metas.insert(key.to_string(), bytes);
            }
        } else if let Some(key) = name.strip_suffix(".jsonl")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            transcripts.insert(key.to_string(), text);
        }
    }
    HelperFiles { transcripts, metas }
}

/// Recursively sort every object's keys. Arrays keep their order. Numbers are left as
/// serde_json wrote them, except that negative zero is folded to zero so `-0.0` and `0.0`
/// cannot differ across implementations.
pub fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object.iter().collect();
            let mut out = Map::new();
            for (key, child) in sorted {
                out.insert(key.clone(), canonical(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Canonical equality: object key order does not matter, and a missing key equals `null`.
pub fn equivalent(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<_> = left.keys().chain(right.keys()).collect();
            keys.into_iter()
                .all(|key| equivalent(left.get(key).unwrap_or(&Value::Null), right.get(key).unwrap_or(&Value::Null)))
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(a, b)| equivalent(a, b))
        }
        (Value::Number(left), Value::Number(right)) => {
            left == right || left.as_f64().zip(right.as_f64()).is_some_and(|(left, right)| left == right)
        }
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_at_every_depth() {
        let value = json!({"z": 1, "a": {"y": [ {"b": 2, "a": 1} ]}});
        let text = serde_json::to_string(&canonical(&value)).unwrap();
        assert_eq!(text, r#"{"a":{"y":[{"a":1,"b":2}]},"z":1}"#);
        assert_eq!(text, serde_json::to_string(&canonical(&value)).unwrap(), "deterministic");
    }

    #[test]
    fn canonical_event_and_state_timestamps_round_trip_milliseconds() {
        let input = concat!(
            r#"{"type":"user","timestamp":"2026-10-23T10:00:00.123Z","cwd":"/w/app","message":{"content":"go"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-10-23T10:00:00.987Z","message":{"id":"m1","stop_reason":"end_turn","usage":{"output_tokens":1},"content":[{"type":"text","text":"done"}]}}"#,
            "\n",
        );
        let wanted = [
            Timestamp::parse_rfc3339("2026-10-23T10:00:00.123Z").unwrap(),
            Timestamp::parse_rfc3339("2026-10-23T10:00:00.987Z").unwrap(),
        ];
        let events: Vec<_> = parse_lines(Harness::ClaudeCode, input.as_bytes(), ParseMode::Full).collect();
        for at in wanted {
            let event =
                events.iter().find(|event| event.at == Some(at)).unwrap_or_else(|| panic!("no event at {at:?}"));
            let value = serde_json::to_value(event).unwrap();
            assert_eq!(value["at"].as_f64(), Some(at.as_secs_f64()));
            let text = serde_json::to_string(&canonical(&value)).unwrap();
            let at_text = serde_json::to_string(&canonical(&value)["at"]).unwrap();
            assert!(!at_text.contains(['e', 'E']), "no exponent drift: {at_text}");
            let back: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(back["at"].as_f64(), Some(at.as_secs_f64()), "canonical JSON round trip");
        }

        let mut reducer = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        let path = Path::new("/h/.claude/projects/-w-app/s.jsonl");
        let written = wanted[1];
        reducer.feed(input.as_bytes(), ChunkCtx { offset: 0, mtime: written, path }).unwrap();
        let state = serde_json::to_value(reducer.state(written)).unwrap();
        for field in ["last_at", "since", "last_write"] {
            assert_eq!(state[field].as_f64(), Some(written.as_secs_f64()), "{field}");
            let text = serde_json::to_string(&canonical(&state[field])).unwrap();
            assert!(!text.contains(['e', 'E']), "{field} has no exponent drift: {text}");
            assert_eq!(text, serde_json::to_string(&written.as_secs_f64()).unwrap());
        }
        let text = serde_json::to_string(&canonical(&state)).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        for field in ["last_at", "since", "last_write"] {
            assert_eq!(back[field].as_f64(), Some(written.as_secs_f64()), "{field} round trip");
        }

        let checkpoint = json!({"after_line": 2, "now_offset_s": 0.123});
        let status = status_object(&state, &checkpoint);
        let text = serde_json::to_string(&canonical(&status)).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["now_offset_s"].as_f64(), Some(0.123), "status checkpoint times keep milliseconds");
        let offset_text = serde_json::to_string(&back["now_offset_s"]).unwrap();
        assert!(!offset_text.contains(['e', 'E']), "no exponent drift: {offset_text}");
    }

    #[test]
    fn absent_optional_fields_equal_null_but_false_does_not() {
        assert!(equivalent(&json!({"a": 1}), &json!({"a": 1, "b": null})));
        assert!(equivalent(&json!({"sidechain": null}), &json!({})));
        assert!(!equivalent(&json!({"sidechain": false}), &json!({"sidechain": null})));
        assert!(!equivalent(&json!([1, 2]), &json!([2, 1])), "arrays keep their order");
        assert!(equivalent(&json!(-0.0), &json!(0)));
        assert!(equivalent(&json!(1), &json!(1.0)), "JSON numbers compare numerically");
    }

    #[test]
    fn a_status_object_always_has_the_conformance_fields() {
        let state = json!({"activity":"done","certainty":"firm","reason":"turn_ended","permission":null,
                           "sandbox":null,"meta":{"title":"t"}});
        let checkpoint = json!({"after_line": 4, "now_offset_s": 30.0});
        let actual = status_object(&state, &checkpoint);
        for field in STATUS_FIELDS {
            assert!(actual.get(field).is_some(), "{field}");
        }
        assert_eq!(actual["title"], "t");
        assert_eq!(actual["error"], Value::Null);
        let untitled = status_object(&json!({"meta":{"title":""}}), &checkpoint);
        assert_eq!(untitled["title"], Value::Null, "no title is null, not an empty string");
    }

    #[test]
    fn locates_the_input_directory_and_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("scenario/input");
        std::fs::create_dir_all(input.join("subagents")).unwrap();
        std::fs::write(input.join("session.jsonl"), "{}\n").unwrap();
        std::fs::write(input.join("subagents/agent-a1.jsonl"), "{}\n").unwrap();
        let located = Input::locate(&dir.path().join("scenario")).unwrap();
        assert_eq!(located.main, input.join("session.jsonl"));
        let helpers = helper_files(&located.root);
        assert!(helpers.transcripts.contains_key("agent-a1"));
    }

    #[test]
    fn rejects_an_ambiguous_input_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.path().join("b.jsonl"), "{}\n").unwrap();
        let error = Input::locate(dir.path()).unwrap_err();
        assert!(error.contains("expected one main transcript"), "{error}");
    }

    #[test]
    fn malformed_checkpoints_are_named_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.jsonl");
        std::fs::write(&path, "{\"after_line\":1,\"now_offset_s\":1}\nnot json\n").unwrap();
        let error = read_checkpoints(&path).unwrap_err();
        assert!(error.contains(":2:"), "{error}");
    }
}
