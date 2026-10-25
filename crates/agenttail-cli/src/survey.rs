//! `agenttail survey [--drift]`: the Phase 0 shape survey, in the CLI.
//!
//! The survey records identifiers of structure only: record types, field paths, content-block
//! types, discriminator values and tool names. Ids, timestamps and free text are never
//! written, so a value-only change cannot look like drift.
//!
//! With `--drift` the shape is compared to `spec/baseline/<harness>.json`. New shapes and new
//! discriminator values are drift. Values a baseline has but the current corpus did not show
//! are reported but do not fail by default: the baseline is a union of observations, and a
//! local or fixture-sized survey is not expected to cover all of it. `--strict-removals`
//! makes those fail too, which is what a complete nightly matrix should use.

use crate::{harness_for, harness_from_bytes};
use agenttail::Harness;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

type ValueSets = BTreeMap<String, BTreeSet<Option<String>>>;

const PAYLOAD_KEYS: &[&str] = &[
    "content",
    "context",
    "data",
    "description",
    "details",
    "input",
    "input_schema",
    "lastPrompt",
    "aiTitle",
    "messages",
    "properties",
    "prompt",
    "rendered",
    "replacement",
    "shape",
    "snapshot",
    "summary",
    "system",
    "text",
    "thinking",
    "tools",
    "toolUseResult",
    "toolsAdded",
    "toolsRemoved",
    "parameters",
    "sections",
    "url",
];

const CLAUDE_ENVELOPE: &[&str] = &[
    "uuid",
    "parentUuid",
    "timestamp",
    "sessionId",
    "session_id",
    "cwd",
    "gitBranch",
    "version",
    "entrypoint",
    "userType",
    "isSidechain",
    "slug",
    "agentId",
    "sessionKind",
];

const CLAUDE_FIELD_TYPES: &[&str] = &[
    "ai-title",
    "api-request",
    "assistant",
    "cost-state",
    "fork-context-ref",
    "last-prompt",
    "mode",
    "permission-mode",
    "queue-operation",
    "user",
];

const CLAUDE_SYSTEM_FIELDS: &[&str] =
    &["compact_boundary", "local_command", "scheduled_task_fire", "stop_hook_summary", "turn_duration"];

#[derive(Default)]
struct Shape {
    harness: Option<Harness>,
    record_types: BTreeSet<String>,
    record_groups: BTreeMap<String, BTreeSet<String>>,
    record_payloads: BTreeMap<String, BTreeSet<Option<String>>>,
    field_paths: ValueSets,
    content_types: ValueSets,
    enums: ValueSets,
    block_fields: ValueSets,
    system_subtypes: BTreeSet<String>,
    attachment_types: BTreeSet<String>,
    tool_names_seen: BTreeSet<String>,
    roles: BTreeSet<String>,
    user_wrapper_tags: BTreeSet<String>,
    record_shapes: BTreeSet<Vec<String>>,
    versions: BTreeSet<String>,
    files: BTreeMap<String, u64>,
}

impl Shape {
    fn new(harness: Harness) -> Shape {
        Shape { harness: Some(harness), ..Shape::default() }
    }

    fn field(&mut self, label: &str, value: impl Into<String>) {
        self.field_paths.entry(label.to_string()).or_default().insert(Some(value.into()));
    }

    fn content(&mut self, label: &str, value: impl Into<String>) {
        self.content_types.entry(label.to_string()).or_default().insert(Some(value.into()));
    }

    fn enum_value(&mut self, label: &str, value: Option<&str>) {
        self.enums.entry(label.to_string()).or_default().insert(value.map(str::to_string));
    }

    fn to_value(&self) -> Value {
        let mut object = Map::new();
        if let Some(harness) = self.harness {
            object.insert("harness".into(), json!(harness.id().replace('_', "-")));
        }
        if !self.record_payloads.is_empty() {
            object.insert("record_types".into(), value_sets_to_json(&self.record_payloads));
        } else if !self.record_groups.is_empty() {
            let groups = self
                .record_groups
                .iter()
                .map(|(label, values)| (label.clone(), Value::Array(values.iter().map(|value| json!(value)).collect())))
                .collect::<BTreeMap<_, _>>();
            object.insert("record_types".into(), value_sets_to_json_from_strings(&groups));
        } else {
            object.insert("record_types".into(), Value::Array(self.record_types.iter().map(|t| json!(t)).collect()));
        }
        for (key, sets) in [
            ("field_paths", &self.field_paths),
            ("content_types", &self.content_types),
            ("enums", &self.enums),
            ("block_fields", &self.block_fields),
        ] {
            if !sets.is_empty() {
                object.insert(key.into(), value_sets_to_json(sets));
            }
        }
        for (key, values) in [
            ("system_subtypes", &self.system_subtypes),
            ("attachment_types", &self.attachment_types),
            ("tool_names_seen", &self.tool_names_seen),
            ("roles", &self.roles),
            ("user_wrapper_tags", &self.user_wrapper_tags),
        ] {
            if !values.is_empty() {
                object.insert(key.into(), Value::Array(values.iter().map(|value| json!(value)).collect()));
            }
        }
        if !self.record_shapes.is_empty() {
            object.insert(
                "record_shapes".into(),
                Value::Array(
                    self.record_shapes
                        .iter()
                        .map(|shape| Value::Array(shape.iter().map(|key| json!(key)).collect()))
                        .collect(),
                ),
            );
        }
        if !self.versions.is_empty() {
            object.insert("versions".into(), Value::Array(self.versions.iter().map(|v| json!(v)).collect()));
        }
        if !self.files.is_empty() {
            object.insert(
                "files".into(),
                Value::Object(self.files.iter().map(|(ext, count)| (ext.clone(), Value::from(*count))).collect()),
            );
        }
        Value::Object(object)
    }
}

fn value_sets_to_json(sets: &ValueSets) -> Value {
    Value::Object(
        sets.iter()
            .map(|(label, values)| {
                let values =
                    values.iter().map(|value| value.as_ref().map_or(Value::Null, |value| json!(value))).collect();
                (label.clone(), Value::Array(values))
            })
            .collect(),
    )
}

fn value_sets_to_json_from_strings(sets: &BTreeMap<String, Value>) -> Value {
    Value::Object(sets.iter().map(|(label, values)| (label.clone(), values.clone())).collect())
}

/// `agenttail survey [--drift] [--strict-removals] [--strict-fields] [--baseline DIR]
/// [--agent ID] [--out FILE] [--home DIR] [PATH…]`
pub fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut drift = false;
    let mut strict_removals = false;
    let mut strict_fields = false;
    let mut baseline_dir = None;
    let mut agent = None;
    let mut out = None;
    let mut home = None;
    let mut paths = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--drift" => drift = true,
            "--strict-removals" => strict_removals = true,
            "--strict-fields" => strict_fields = true,
            "--baseline" => baseline_dir = Some(PathBuf::from(iter.next().ok_or("--baseline needs a value")?)),
            "--out" => out = Some(PathBuf::from(iter.next().ok_or("--out needs a value")?)),
            "--home" => home = Some(PathBuf::from(iter.next().ok_or("--home needs a value")?)),
            "--agent" => {
                let id = iter.next().ok_or("--agent needs a value")?;
                agent = Some(id.parse::<Harness>().map_err(|e| e.to_string())?);
            }
            other if other.starts_with('-') => return Err(format!("no option \"{other}\"")),
            other => paths.push(PathBuf::from(other)),
        }
    }
    let home = home.or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if home.is_none() && paths.is_empty() {
        return Err("survey needs HOME or at least one path".into());
    }

    let mut groups: BTreeMap<Harness, InputSet> = BTreeMap::new();
    if paths.is_empty() {
        let home = home.as_deref().expect("checked");
        for harness in Harness::ALL {
            if agent.is_some_and(|wanted| wanted != harness) {
                continue;
            }
            let set = default_inputs(harness, home)?;
            if set.has_transcripts() {
                groups.insert(harness, set);
            }
        }
        if groups.is_empty() {
            return Err(format!("survey: no transcripts under {}", home.display()));
        }
    } else {
        for path in paths {
            let harness = match agent {
                Some(agent) => agent,
                None => detect_harness(&path)?,
            };
            let set = explicit_inputs(&path)?;
            if !set.has_transcripts() {
                return Err(format!("survey: no .jsonl transcripts under {}", path.display()));
            }
            groups.entry(harness).or_default().merge(set);
        }
    }

    let mut shapes = BTreeMap::new();
    for (harness, input) in groups {
        shapes.insert(harness, scan(harness, &input)?);
    }
    let output = if shapes.len() == 1 {
        shapes.values().next().expect("one").clone()
    } else {
        Value::Object(shapes.iter().map(|(harness, shape)| (harness.id().to_string(), shape.clone())).collect())
    };
    let text = serde_json::to_string_pretty(&canonical(&output)).map_err(|e| e.to_string())? + "\n";
    if let Some(path) = &out {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("survey: wrote {}", path.display());
    } else {
        print!("{text}");
    }

    if !drift {
        return Ok(ExitCode::SUCCESS);
    }
    let baseline_dir = match baseline_dir {
        Some(path) => path,
        None => find_baseline_dir().ok_or("survey --drift: pass --baseline <spec/baseline>")?,
    };
    let mut drifted = false;
    for (harness, shape) in &shapes {
        let file = baseline_dir.join(format!("{}.json", harness.id().replace('_', "-")));
        let baseline: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?)
                .map_err(|e| format!("{}: {e}", file.display()))?;
        let diff = diff_shapes(&baseline, shape, strict_fields);
        if has_drift(&diff, strict_removals) {
            drifted = true;
        }
        eprintln!("{}", render_report(*harness, &baseline, shape, &diff, strict_removals));
    }
    Ok(if drifted { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn find_baseline_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("spec/baseline");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn detect_harness(path: &Path) -> Result<Harness, String> {
    if let Some(harness) = harness_for(path) {
        return Ok(harness);
    }
    let first = if path.is_file() {
        path.to_path_buf()
    } else {
        let mut found = Vec::new();
        collect_files(path, &mut found, false);
        found.into_iter().find(|file| file.extension().is_some_and(|ext| ext == "jsonl")).ok_or_else(|| {
            format!("survey: no transcripts under {}; pass --agent claude_code|codex|cursor|pi", path.display())
        })?
    };
    let bytes = std::fs::read(&first).map_err(|e| format!("{}: {e}", first.display()))?;
    harness_from_bytes(&bytes).ok_or_else(|| {
        format!("survey: can't tell which harness wrote {}; pass --agent claude_code|codex|cursor|pi", first.display())
    })
}

#[derive(Default)]
struct InputSet {
    transcripts: BTreeSet<PathBuf>,
    metas: BTreeSet<PathBuf>,
    registry: BTreeSet<PathBuf>,
    session_index: Option<PathBuf>,
    jsonl_count: u64,
    zst_count: u64,
}

impl InputSet {
    fn has_transcripts(&self) -> bool {
        !self.transcripts.is_empty()
    }

    fn merge(&mut self, other: InputSet) {
        self.transcripts.extend(other.transcripts);
        self.metas.extend(other.metas);
        self.registry.extend(other.registry);
        if self.session_index.is_none() {
            self.session_index = other.session_index;
        }
        self.jsonl_count += other.jsonl_count;
        self.zst_count += other.zst_count;
    }
}

fn default_inputs(harness: Harness, home: &Path) -> Result<InputSet, String> {
    let mut set = InputSet::default();
    let discovery = harness.discovery();
    let mut roots: Vec<PathBuf> =
        discovery.roots.for_os(std::env::consts::OS).into_iter().map(|root| home.join(root)).collect();
    for override_ in discovery.roots.env {
        let Some(value) = std::env::var_os(override_.var) else { continue };
        let dir = PathBuf::from(value);
        if !override_.absolute || dir.is_absolute() {
            roots = vec![if override_.append.is_empty() { dir } else { dir.join(override_.append) }];
        }
    }
    for root in roots {
        let mut files = Vec::new();
        collect_layout_files(harness, &root, &root, &mut files)?;
        for file in files {
            let name = file.to_string_lossy();
            if name.ends_with(".meta.json") {
                set.metas.insert(file);
            } else if file.extension().is_some_and(|ext| ext == "jsonl") {
                set.jsonl_count += 1;
                set.transcripts.insert(file);
            }
        }
        if harness == Harness::ClaudeCode
            && let Some(parent) = root.parent()
        {
            let sessions = parent.join("sessions");
            if let Ok(entries) = std::fs::read_dir(&sessions) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|ext| ext == "json") {
                        set.registry.insert(entry.path());
                    }
                }
            }
        }
        if harness == Harness::Codex
            && let Some(parent) = root.parent()
        {
            let index = parent.join("session_index.jsonl");
            if index.is_file() {
                set.session_index = Some(index);
            }
        }
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.to_string_lossy();
                if name.ends_with(".jsonl.zst") {
                    set.zst_count += 1;
                }
            }
        }
    }
    Ok(set)
}

fn collect_layout_files(harness: Harness, root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if harness.layout().walk_depth() != usize::MAX && rel.components().count() >= harness.layout().walk_depth()
            {
                continue;
            }
            collect_layout_files(harness, root, &path, out)?;
        } else if path.file_name().is_some_and(|name| name == "session_index.jsonl") {
            continue;
        } else if path.to_string_lossy().ends_with(".meta.json") {
            if path.to_string_lossy().replace('\\', "/").contains("/subagents/") {
                out.push(path);
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if harness.layout().role(rel).is_some() {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn explicit_inputs(path: &Path) -> Result<InputSet, String> {
    let mut set = InputSet::default();
    if path.is_file() {
        add_explicit_file(&mut set, path);
        return Ok(set);
    }
    let base = if path.join("input").is_dir() { path.join("input") } else { path.to_path_buf() };
    let mut files = Vec::new();
    collect_files(&base, &mut files, true);
    for file in files {
        add_explicit_file(&mut set, &file);
    }
    if (path.join("input").is_dir() && path.join("input/session_index.jsonl").is_file())
        || path.join("session_index.jsonl").is_file()
    {
        set.session_index = Some(if path.join("input").is_dir() {
            path.join("input/session_index.jsonl")
        } else {
            path.join("session_index.jsonl")
        });
    }
    Ok(set)
}

fn add_explicit_file(set: &mut InputSet, file: &Path) {
    let name = file.to_string_lossy();
    if file.file_name().is_some_and(|name| name == "state-vscdb.jsonl") {
        // Cursor's SQLite fixture is not a transcript; its shape is committed Phase 0
        // evidence and is not re-surveyed yet.
        return;
    }
    if name.ends_with(".meta.json") {
        set.metas.insert(file.to_path_buf());
    } else if let Some(name) = file.file_name().and_then(|name| name.to_str())
        && name == "session_index.jsonl"
    {
        set.session_index = Some(file.to_path_buf());
    } else if file.extension().is_some_and(|ext| ext == "jsonl") {
        set.jsonl_count += 1;
        set.transcripts.insert(file.to_path_buf());
    } else if name.ends_with(".jsonl.zst") {
        set.zst_count += 1;
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, skip_expected: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            if matches!(name, ".git" | "target" | "runs" | "node_modules") {
                continue;
            }
            if skip_expected && name == "expected" {
                continue;
            }
            collect_files(&path, out, skip_expected);
        } else {
            out.push(path);
        }
    }
}

fn scan(harness: Harness, input: &InputSet) -> Result<Value, String> {
    let mut shape = Shape::new(harness);
    shape.files.insert(".jsonl".into(), input.jsonl_count);
    if input.zst_count > 0 || harness == Harness::Codex {
        shape.files.insert(".jsonl.zst".into(), input.zst_count);
    }
    match harness {
        Harness::ClaudeCode => scan_claude(&mut shape, &input.transcripts, &input.metas, &input.registry)?,
        Harness::Codex => scan_codex(&mut shape, &input.transcripts, input.session_index.as_deref())?,
        Harness::Cursor => scan_cursor(&mut shape, &input.transcripts)?,
        Harness::Pi => scan_pi(&mut shape, &input.transcripts)?,
        Harness::OpenCode => {
            return Err(
                "survey: OpenCode sessions live in opencode.db; transcript shape survey is not supported yet".into()
            );
        }
        _ => {}
    }
    Ok(shape.to_value())
}

/// Read one transcript a line at a time, so a survey of a year of sessions does not hold
/// every parsed record in memory at once.
fn for_each_record(path: &Path, mut visit: impl FnMut(&Value)) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && value.is_object()
        {
            visit(&value);
        }
    }
    Ok(())
}

fn is_helper_path(path: &Path) -> bool {
    path.to_string_lossy().replace('\\', "/").contains("/subagents/")
        || path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("agent-"))
}

fn scan_claude(
    shape: &mut Shape,
    files: &BTreeSet<PathBuf>,
    metas: &BTreeSet<PathBuf>,
    registry: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    let skip: BTreeSet<&str> = CLAUDE_ENVELOPE.iter().copied().chain(std::iter::once("type")).collect();
    let mut envelope = BTreeSet::new();
    envelope.extend(CLAUDE_ENVELOPE.iter().map(|key| key.to_string()));
    for path in files {
        let group = if is_helper_path(path) { "subagent" } else { "main" };
        for_each_record(path, |record| {
            let record_type = record["type"].as_str().unwrap_or("<none>");
            shape.record_groups.entry(group.into()).or_default().insert(record_type.into());
            if let Some(version) = record["version"].as_str() {
                shape.versions.insert(version.into());
            }
            if record_type == "system"
                && let Some(subtype) = record["subtype"].as_str()
            {
                shape.system_subtypes.insert(subtype.into());
            }
            if record_type == "attachment"
                && let Some(attachment_type) = record["attachment"]["type"].as_str()
            {
                shape.attachment_types.insert(attachment_type.into());
            }
            if record_type == "system" {
                if let Some(subtype) = record["subtype"].as_str()
                    && CLAUDE_SYSTEM_FIELDS.contains(&subtype)
                {
                    let mut system_skip = skip.clone();
                    system_skip.insert("subtype");
                    collect_fields(record, &format!("system/{subtype}"), &mut shape.field_paths, &system_skip, false);
                }
            } else if CLAUDE_FIELD_TYPES.contains(&record_type) {
                collect_fields(record, record_type, &mut shape.field_paths, &skip, record_type == "assistant");
            }
            record_claude_enums(shape, record);
            claude_content(shape, record);
            if record_type == "assistant"
                && let Some(blocks) = record["message"]["content"].as_array()
            {
                for block in blocks {
                    if block["type"] == "tool_use" {
                        if let Some(name) = block["name"].as_str() {
                            shape.tool_names_seen.insert(claude_tool_name(name));
                        }
                        collect_fields(block, "tool_use", &mut shape.field_paths, &BTreeSet::new(), false);
                    } else if block["type"] == "thinking" {
                        collect_fields(block, "thinking", &mut shape.field_paths, &BTreeSet::new(), false);
                    } else if block["type"] == "tool_result" {
                        collect_fields(block, "tool_result", &mut shape.field_paths, &BTreeSet::new(), false);
                    }
                }
            }
            if record_type == "user"
                && let Some(blocks) = record["message"]["content"].as_array()
            {
                for block in blocks {
                    if block["type"] == "tool_result" {
                        collect_fields(block, "tool_result", &mut shape.field_paths, &BTreeSet::new(), false);
                    }
                }
                if let Some(result) = record.get("toolUseResult").and_then(Value::as_object) {
                    let agent_result = result.contains_key("agentId")
                        || result.contains_key("canContinueAgent")
                        || result.contains_key("outputFile");
                    if agent_result {
                        for key in result.keys() {
                            shape.field("Agent toolUseResult", key.clone());
                        }
                    }
                }
            }
        })?;
    }
    shape.field_paths.entry("envelope".into()).or_default().extend(envelope.into_iter().map(Some));
    for path in metas {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(object) = value.as_object() {
            for key in object.keys() {
                shape.field("subagent meta.json", key.clone());
            }
            if let Some(agent_type) = object.get("agentType").and_then(Value::as_str) {
                shape.enum_value("meta.agentType", Some(agent_type));
            }
        }
    }
    for path in registry {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(object) = value.as_object() {
            for key in object.keys() {
                shape.field("sessions/<pid>.json", key.clone());
            }
            if let Some(status) = object.get("status").and_then(Value::as_str) {
                shape.enum_value("sessions.status", Some(status));
            }
        }
    }
    Ok(())
}

fn claude_content(shape: &mut Shape, record: &Value) {
    let record_type = record["type"].as_str().unwrap_or_default();
    let Some(content) = record["message"]["content"].as_array() else { return };
    for block in content {
        let Some(block_type) = block["type"].as_str() else { continue };
        match record_type {
            "assistant" | "user" | "api-request-blob" => shape.content(record_type, block_type),
            _ => {}
        }
        if record_type == "user"
            && block_type == "tool_result"
            && let Some(nested) = block["content"].as_array()
        {
            for item in nested {
                if let Some(item_type) = item["type"].as_str() {
                    shape.content("tool_result.content", item_type);
                }
            }
        }
    }
}

fn record_claude_enums(shape: &mut Shape, record: &Value) {
    let record_type = record["type"].as_str().unwrap_or_default();
    let paths: &[(&str, &[&str])] = &[
        ("assistant.error", &["error"]),
        ("compactMetadata.trigger", &["compactMetadata", "trigger"]),
        ("entrypoint", &["entrypoint"]),
        ("message.stop_reason", &["message", "stop_reason"]),
        ("mode", &["mode"]),
        ("origin.kind", &["origin", "kind"]),
        ("permissionDecision.decision", &["permissionDecision", "decision"]),
        ("permissionDecision.reasonType", &["permissionDecision", "reasonType"]),
        ("permissionDecision.source", &["permissionDecision", "source"]),
        ("permissionMode", &["permissionMode"]),
        ("promptSource", &["promptSource"]),
        ("queue-operation.operation", &["operation"]),
        ("system.level", &["level"]),
        ("toolDenialKind", &["toolDenialKind"]),
        ("toolUseResult.status", &["toolUseResult", "status"]),
    ];
    for (label, path) in paths {
        if let Some(value) = path_value(record, path) {
            shape.enums.entry((*label).into()).or_default().insert(enum_string(value));
        }
    }
    if record_type == "user"
        && let Some(status) = task_notification_status(record)
    {
        shape.enum_value("task-notification <status>", Some(&status));
    }
}

fn claude_tool_name(name: &str) -> String {
    if name.starts_with("mcp__") { "mcp__*".into() } else { name.into() }
}

fn task_notification_status(record: &Value) -> Option<String> {
    let content = &record["message"]["content"];
    let text = if let Some(text) = content.as_str() {
        text.to_string()
    } else {
        content.as_array()?.iter().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n")
    };
    let start = text.find("<status>")? + "<status>".len();
    let end = text[start..].find("</status>")? + start;
    Some(text[start..end].trim().to_string())
}

fn scan_codex(shape: &mut Shape, files: &BTreeSet<PathBuf>, session_index: Option<&Path>) -> Result<(), String> {
    let envelope = ["timestamp", "type", "payload"];
    for path in files {
        for_each_record(path, |record| {
            for key in envelope {
                shape.field("envelope", key);
            }
            let record_type = record["type"].as_str().unwrap_or("<none>");
            let payload = &record["payload"];
            let payload_type = payload["type"].as_str();
            shape.record_payloads.entry(record_type.into()).or_default().insert(payload_type.map(str::to_string));
            let label = payload_type.map_or_else(|| record_type.to_string(), |kind| format!("{record_type}/{kind}"));
            // The label already carries the payload discriminator; `payload.type` itself is
            // not a separate field in the Phase 0 baseline.
            collect_fields(payload, &label, &mut shape.field_paths, &BTreeSet::from(["type"]), true);
            if let Some(info) = payload.get("info").and_then(Value::as_object) {
                for usage in ["total_token_usage", "last_token_usage"] {
                    if let Some(object) = info.get(usage).and_then(Value::as_object) {
                        for key in object.keys() {
                            shape.field("token_usage", key.clone());
                        }
                    }
                }
            }
            if record_type == "response_item"
                && payload_type == Some("message")
                && let Some(content) = payload["content"].as_array()
            {
                for block in content {
                    if let Some(block_type) = block["type"].as_str() {
                        shape.content("response_item/message", block_type);
                    }
                }
            }
            if record_type == "response_item"
                && matches!(payload_type, Some("function_call" | "custom_tool_call"))
                && let Some(name) = payload["name"].as_str()
            {
                shape.tool_names_seen.insert(name.into());
            }
            for (label, path) in [
                ("approval_policy", &["approval_policy"][..]),
                ("sandbox_policy.type", &["sandbox_policy", "type"][..]),
                ("message.role", &["role"][..]),
                ("custom_tool_call.status", &["status"][..]),
                ("model_provider", &["model_provider"][..]),
                ("source", &["source"][..]),
            ] {
                if let Some(value) = path_value(payload, path) {
                    let applies = match label {
                        "approval_policy" | "sandbox_policy.type" => record_type == "turn_context",
                        "message.role" => record_type == "response_item" && payload_type == Some("message"),
                        "custom_tool_call.status" => {
                            record_type == "response_item" && payload_type == Some("custom_tool_call")
                        }
                        "model_provider" | "source" => record_type == "session_meta",
                        _ => true,
                    };
                    if applies {
                        shape.enums.entry(label.into()).or_default().insert(enum_string(value));
                    }
                }
            }
            if record_type == "session_meta"
                && let Some(version) = payload["cli_version"].as_str()
            {
                shape.versions.insert(version.into());
            }
        })?;
    }
    if let Some(path) = session_index
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let mut fields = BTreeSet::new();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
            if let Value::Object(object) = value {
                for key in object.keys() {
                    fields.insert(key.clone());
                }
            }
        }
        for key in fields {
            shape.field("session_index.jsonl", key);
        }
    }
    Ok(())
}

fn scan_pi(shape: &mut Shape, files: &BTreeSet<PathBuf>) -> Result<(), String> {
    for path in files {
        for_each_record(path, |record| {
            let record_type = record["type"].as_str().unwrap_or("<none>");
            shape.record_types.insert(record_type.into());
            collect_fields(record, record_type, &mut shape.field_paths, &BTreeSet::new(), true);
            if record_type == "message" {
                if let Some(role) = record["message"]["role"].as_str() {
                    shape.enum_value("message.role", Some(role));
                    if let Some(content) = record["message"]["content"].as_array() {
                        for block in content {
                            if let Some(block_type) = block["type"].as_str() {
                                shape.content(&format!("message.{role}"), block_type);
                            }
                            if block["type"] == "toolCall"
                                && let Some(name) = block["name"].as_str()
                            {
                                shape.tool_names_seen.insert(name.into());
                            }
                        }
                    }
                }
                if let Some(stop_reason) = record["message"]["stopReason"].as_str() {
                    shape.enum_value("message.stopReason", Some(stop_reason));
                }
                if let Some(tool) = record["message"]["toolName"].as_str() {
                    shape.tool_names_seen.insert(tool.into());
                }
            }
        })?;
    }
    Ok(())
}

fn scan_cursor(shape: &mut Shape, files: &BTreeSet<PathBuf>) -> Result<(), String> {
    for path in files {
        for_each_record(path, |record| {
            let Some(object) = record.as_object() else { return };
            let mut keys: Vec<String> = object.keys().cloned().collect();
            keys.sort();
            shape.record_shapes.insert(keys);
            if let Some(record_type) = record["type"].as_str() {
                shape.record_types.insert(record_type.into());
            }
            if let Some(role) = record["role"].as_str() {
                shape.roles.insert(role.into());
            }
            if let Some(content) = record["message"]["content"].as_array() {
                let role = record["role"].as_str().unwrap_or("<none>");
                for block in content {
                    if let Some(block_type) = block["type"].as_str() {
                        shape.content(role, block_type);
                        let mut fields: Vec<String> =
                            block.as_object().map(|object| object.keys().cloned().collect()).unwrap_or_default();
                        fields.sort();
                        shape.block_fields.entry(block_type.into()).or_default().extend(fields.into_iter().map(Some));
                    }
                    if block["type"] == "tool_use"
                        && let Some(name) = block["name"].as_str()
                    {
                        shape.tool_names_seen.insert(name.into());
                    }
                    if block["type"] == "text"
                        && role == "user"
                        && let Some(text) = block["text"].as_str()
                    {
                        for tag in wrapper_tags(text) {
                            shape.user_wrapper_tags.insert(tag);
                        }
                    }
                }
            }
            for (label, path) in [("turn_ended.status", &["status"][..]), ("turn_ended.error", &["error"][..])] {
                if let Some(value) = path_value(record, path) {
                    shape.enums.entry(label.into()).or_default().insert(enum_string(value));
                }
            }
        })?;
    }
    Ok(())
}

/// Cursor wraps a user prompt in leading `<timestamp>`/`<user_query>` blocks. Only the
/// leading tag blocks count: a mention of `<user>` inside the prompt is prose, not a wrapper.
fn wrapper_tags(text: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = text.trim_start();
    while let Some(after) = rest.strip_prefix('<') {
        let Some(end) = after.find('>') else { break };
        let tag = &after[..end];
        if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':')) {
            break;
        }
        tags.push(tag.to_string());
        rest = after[end + 1..].trim_start();
    }
    tags.sort();
    tags.dedup();
    tags
}

fn path_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn enum_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn collect_fields(value: &Value, label: &str, out: &mut ValueSets, skip_top: &BTreeSet<&str>, mark_arrays: bool) {
    let mut fields = BTreeSet::new();
    walk_fields(value, "", &mut fields, skip_top, true, mark_arrays);
    out.entry(label.into()).or_default().extend(fields.into_iter().map(Some));
}

fn walk_fields(
    value: &Value,
    prefix: &str,
    out: &mut BTreeSet<String>,
    skip_top: &BTreeSet<&str>,
    root: bool,
    mark_arrays: bool,
) {
    if let Some(object) = value.as_object() {
        for (key, child) in object {
            if !safe_key(key) || (root && skip_top.contains(key.as_str())) {
                continue;
            }
            let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            out.insert(path.clone());
            if !PAYLOAD_KEYS.contains(&key.as_str()) {
                walk_fields(child, &path, out, skip_top, false, mark_arrays);
            }
        }
    } else if let Some(items) = value.as_array() {
        if mark_arrays && !prefix.is_empty() {
            out.insert(format!("{prefix}[]"));
        }
        let last = prefix.rsplit('.').next().unwrap_or(prefix);
        if !PAYLOAD_KEYS.contains(&last) {
            for item in items {
                walk_fields(item, &format!("{prefix}[]"), out, skip_top, false, mark_arrays);
            }
        }
    }
}

fn safe_key(key: &str) -> bool {
    !key.contains(['/', '\\', '\n'])
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object.iter().collect();
            Value::Object(sorted.into_iter().map(|(key, value)| (key.clone(), canonical(value))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

#[derive(Debug, Default)]
struct Diff {
    added: BTreeMap<String, Vec<String>>,
    removed: BTreeMap<String, Vec<String>>,
    added_fields: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    removed_fields: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

fn diff_shapes(baseline: &Value, current: &Value, strict_fields: bool) -> Diff {
    let mut diff = Diff::default();
    for key in ["record_types", "field_paths", "content_types", "enums", "block_fields"] {
        let (Some(old), Some(new)) = (baseline.get(key), current.get(key)) else { continue };
        match (old, new) {
            (Value::Object(old), Value::Object(new)) => {
                for (label, values) in new {
                    let old_values = old.get(label).and_then(Value::as_array).cloned().unwrap_or_default();
                    let new_values = values.as_array().cloned().unwrap_or_default();
                    let (added, removed) = if key == "field_paths" && !strict_fields {
                        field_path_diff(&new_values, &old_values)
                    } else {
                        set_diff(&new_values, &old_values)
                    };
                    if !added.is_empty() {
                        diff.added_fields.entry(key.into()).or_default().insert(label.clone(), added);
                    }
                    if !removed.is_empty() {
                        diff.removed_fields.entry(key.into()).or_default().insert(label.clone(), removed);
                    }
                }
            }
            (Value::Array(old), Value::Array(new)) => {
                let (added, removed) = set_diff(new, old);
                if !added.is_empty() {
                    diff.added.entry(key.into()).or_default().extend(added);
                }
                if !removed.is_empty() {
                    diff.removed.entry(key.into()).or_default().extend(removed);
                }
            }
            _ => {}
        }
    }
    for key in ["attachment_types", "system_subtypes", "tool_names_seen", "roles", "user_wrapper_tags", "layout"] {
        let (Some(old), Some(new)) = (baseline.get(key), current.get(key)) else { continue };
        let (Some(old), Some(new)) = (old.as_array(), new.as_array()) else { continue };
        let (added, removed) = set_diff(new, old);
        if !added.is_empty() {
            diff.added.entry(key.into()).or_default().extend(added);
        }
        if !removed.is_empty() {
            diff.removed.entry(key.into()).or_default().extend(removed);
        }
    }
    if let (Some(old), Some(new)) = (
        baseline.get("record_shapes").and_then(Value::as_array),
        current.get("record_shapes").and_then(Value::as_array),
    ) {
        let key = |value: &Value| serde_json::to_string(&canonical(value)).unwrap_or_default();
        let old: BTreeSet<_> = old.iter().map(key).collect();
        let new: BTreeSet<_> = new.iter().map(key).collect();
        let added: Vec<_> = new.difference(&old).cloned().collect();
        let removed: Vec<_> = old.difference(&new).cloned().collect();
        if !added.is_empty() {
            diff.added.entry("record_shapes".into()).or_default().extend(added);
        }
        if !removed.is_empty() {
            diff.removed.entry("record_shapes".into()).or_default().extend(removed);
        }
    }
    if let (Some(old), Some(new)) = (baseline.get("files"), current.get("files")) {
        let (Some(old), Some(new)) = (old.as_object(), new.as_object()) else { return diff };
        let added: Vec<_> = new.keys().filter(|key| !old.contains_key(*key)).cloned().collect();
        let removed: Vec<_> = old.keys().filter(|key| !new.contains_key(*key)).cloned().collect();
        if !added.is_empty() {
            diff.added.entry("files".into()).or_default().extend(added);
        }
        if !removed.is_empty() {
            diff.removed.entry("files".into()).or_default().extend(removed);
        }
    }
    diff
}

/// Field-path drift is conservative by default: a new path counts only when none of its
/// ancestors is already in the baseline. The Phase 0 baselines are curated (they store
/// `message.usage` without every member, or `message.sections` as one leaf), so a generic
/// walk that recursed one level further would otherwise report false drift on every local
/// survey. `--strict-fields` compares the full generated tree instead.
fn field_path_diff(new: &[Value], old: &[Value]) -> (Vec<String>, Vec<String>) {
    let key = |value: &Value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let old: BTreeSet<_> = old.iter().map(key).collect();
    let new: BTreeSet<_> = new.iter().map(key).collect();
    let added = new
        .iter()
        .filter(|path| !old.contains(*path) && !is_known_container(path, &old) && !has_ancestor(path, &old))
        .cloned()
        .collect();
    let removed = old.difference(&new).cloned().collect();
    (added, removed)
}

fn is_known_container(path: &str, old: &BTreeSet<String>) -> bool {
    let dot = format!("{path}.");
    let array = format!("{path}[]");
    old.iter().any(|known| known.starts_with(&dot) || known.starts_with(&array))
}

fn has_ancestor(path: &str, old: &BTreeSet<String>) -> bool {
    let mut at = 0;
    while let Some(found) = path[at..].find(['.', '[']) {
        let end = at + found;
        if old.contains(&path[..end]) {
            return true;
        }
        at = end + 1;
    }
    false
}

fn set_diff(new: &[Value], old: &[Value]) -> (Vec<String>, Vec<String>) {
    let key = |value: &Value| match value {
        Value::Null => "null".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let old: BTreeSet<_> = old.iter().map(key).collect();
    let new: BTreeSet<_> = new.iter().map(key).collect();
    (new.difference(&old).cloned().collect(), old.difference(&new).cloned().collect())
}

fn has_drift(diff: &Diff, strict_removals: bool) -> bool {
    !diff.added.is_empty()
        || !diff.added_fields.is_empty()
        || (strict_removals && (!diff.removed.is_empty() || !diff.removed_fields.is_empty()))
}

fn render_report(harness: Harness, baseline: &Value, current: &Value, diff: &Diff, strict_removals: bool) -> String {
    let mut lines = vec![
        format!("# Harness shape drift: {}", harness.id().replace('_', "-")),
        String::new(),
        format!("- Baseline harness: `{}`", baseline.get("harness").and_then(Value::as_str).unwrap_or("?")),
        format!("- Current records: `{}`", current.get("record_types").map_or(0, Value::array_len_or_object_len)),
        format!("- Shape changed: **{}**", if has_drift(diff, strict_removals) { "yes" } else { "no" }),
        String::new(),
        "The survey compares record types, field paths, content-block types and small".into(),
        "vocabularies. Ids, timestamps and free-text values are deliberately not surveyed, so".into(),
        "value-only changes are not drift.".into(),
        String::new(),
    ];
    let mut any = false;
    for key in [
        "record_types",
        "attachment_types",
        "system_subtypes",
        "tool_names_seen",
        "roles",
        "user_wrapper_tags",
        "record_shapes",
        "files",
    ] {
        if let Some(values) = diff.added.get(key) {
            any = true;
            lines.extend(section(&format!("Added {key}"), values));
        }
        if strict_removals && let Some(values) = diff.removed.get(key) {
            any = true;
            lines.extend(section(&format!("Removed {key}"), values));
        }
    }
    for key in ["field_paths", "content_types", "enums", "block_fields"] {
        if let Some(labels) = diff.added_fields.get(key) {
            for (label, values) in labels {
                any = true;
                lines.extend(section(&format!("Added {} in `{label}`", key.replace('_', " ")), values));
            }
        }
        if strict_removals && let Some(labels) = diff.removed_fields.get(key) {
            for (label, values) in labels {
                any = true;
                lines.extend(section(&format!("Removed {} in `{label}`", key.replace('_', " ")), values));
            }
        }
    }
    if !any {
        lines.push("No shape changes.".into());
        lines.push(String::new());
    }
    if !strict_removals && (!diff.removed.is_empty() || !diff.removed_fields.is_empty()) {
        lines.push("Some baseline observations were not seen in this survey. They are not drift".into());
        lines.push("unless `--strict-removals` is passed, because a local survey is not a".into());
        lines.push("complete corpus.".into());
        lines.push(String::new());
    }
    lines.join("\n")
}

trait ArrayLen {
    fn array_len_or_object_len(&self) -> usize;
}

impl ArrayLen for Value {
    fn array_len_or_object_len(&self) -> usize {
        self.as_array().map_or_else(|| self.as_object().map_or(0, Map::len), Vec::len)
    }
}

fn section(title: &str, values: &[String]) -> Vec<String> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("### {title}"), String::new()];
    lines.extend(values.iter().map(|value| format!("- `{value}`")));
    lines.push(String::new());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn transcript(dir: &Path, name: &str, records: &[Value]) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, records.iter().map(|record| format!("{record}\n")).collect::<String>()).unwrap();
        path
    }

    fn shape_for(harness: Harness, paths: &[PathBuf]) -> Value {
        let mut input = InputSet::default();
        for path in paths {
            add_explicit_file(&mut input, path);
        }
        scan(harness, &input).unwrap()
    }

    #[test]
    fn value_only_changes_are_not_drift() {
        let temp = TempDir::new().unwrap();
        let base = json!({"type":"user","uuid":"11111111-1111-1111-1111-111111111111",
            "timestamp":"2026-01-01T00:00:00Z","cwd":"/home/a","message":{"role":"user","content":"hello"}});
        let changed = json!({"type":"user","uuid":"99999999-9999-9999-9999-999999999999",
            "timestamp":"2027-02-02T02:02:02Z","cwd":"/home/b","message":{"role":"user","content":"different"}});
        let old = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "old/session.jsonl", &[base])]);
        let new = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "new/session.jsonl", &[changed])]);
        let diff = diff_shapes(&old, &new, false);
        assert!(!has_drift(&diff, true), "{diff:?}");
        assert!(render_report(Harness::ClaudeCode, &old, &new, &diff, true).contains("No shape changes."));
    }

    #[test]
    fn a_new_field_path_is_drift() {
        let temp = TempDir::new().unwrap();
        let old = json!({"type":"user","message":{"role":"user","content":"hi"}});
        let new = json!({"type":"user","message":{"role":"user","content":"hi"},"turnPosition":{"promptIndex":1}});
        let old = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "old/session.jsonl", &[old])]);
        let new = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "new/session.jsonl", &[new])]);
        let diff = diff_shapes(&old, &new, false);
        assert!(has_drift(&diff, false));
        assert!(diff.added_fields["field_paths"]["user"].contains(&"turnPosition".into()));
    }

    #[test]
    fn a_new_discriminator_value_is_drift() {
        let temp = TempDir::new().unwrap();
        let old = json!({"type":"user","permissionMode":"default","message":{"content":"hi"}});
        let new = json!({"type":"user","permissionMode":"auto","message":{"content":"hi"}});
        let old = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "old/session.jsonl", &[old])]);
        let new = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "new/session.jsonl", &[new])]);
        let diff = diff_shapes(&old, &new, false);
        assert!(has_drift(&diff, false));
        assert!(diff.added_fields["enums"]["permissionMode"].contains(&"auto".into()));
    }

    #[test]
    fn nested_field_additions_need_strict_fields() {
        let temp = TempDir::new().unwrap();
        let old = json!({"type":"assistant","message":{"usage":{"output_tokens":1},"content":[]}});
        let new = json!({"type":"assistant","message":{"usage":{"output_tokens":1,"new_counter":2},"content":[]}});
        let old = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "old/session.jsonl", &[old])]);
        let new = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "new/session.jsonl", &[new])]);
        assert!(!has_drift(&diff_shapes(&old, &new, false), false));
        let diff = diff_shapes(&old, &new, true);
        assert!(diff.added_fields["field_paths"]["assistant"].contains(&"message.usage.new_counter".into()));
    }

    #[test]
    fn removed_observations_only_fail_strictly() {
        let temp = TempDir::new().unwrap();
        let old = json!({"type":"assistant","message":{"id":"m","content":[]}});
        let new = json!({"type":"assistant","message":{"content":[]}});
        let old = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "old/session.jsonl", &[old])]);
        let new = shape_for(Harness::ClaudeCode, &[transcript(temp.path(), "new/session.jsonl", &[new])]);
        let diff = diff_shapes(&old, &new, false);
        assert!(!has_drift(&diff, false));
        assert!(has_drift(&diff, true));
        assert!(render_report(Harness::ClaudeCode, &old, &new, &diff, false).contains("not seen in this survey"));
    }

    #[test]
    fn scan_is_deterministic_and_keeps_ids_out_of_the_shape() {
        let temp = TempDir::new().unwrap();
        let records = vec![
            json!({"type":"user","uuid":"secret-id","timestamp":"2026-01-01T00:00:00Z",
                   "message":{"role":"user","content":"secret prose"}}),
            json!({"type":"assistant","uuid":"other-id","message":{"id":"m1","content":[{"type":"text","text":"secret"}]}}),
        ];
        let path = transcript(temp.path(), "session.jsonl", &records);
        let first = shape_for(Harness::ClaudeCode, std::slice::from_ref(&path));
        let second = shape_for(Harness::ClaudeCode, &[path]);
        assert_eq!(first, second);
        let text = first.to_string();
        assert!(!text.contains("secret-id"), "{text}");
        assert!(!text.contains("secret prose"), "{text}");
        assert!(!text.contains("2026-01-01"), "{text}");
    }

    #[test]
    fn codex_compares_envelope_and_payload_discriminators() {
        let temp = TempDir::new().unwrap();
        let old = json!({"timestamp":"2026-01-01T00:00:00Z","type":"turn_context",
                         "payload":{"approval_policy":"on-request","sandbox_policy":{"type":"workspace-write"},"model":"gpt-5"}});
        let new = json!({"timestamp":"2026-01-01T00:00:00Z","type":"turn_context",
                         "payload":{"approval_policy":"never","sandbox_policy":{"type":"danger-full-access"},"model":"gpt-5"}});
        let old = shape_for(Harness::Codex, &[transcript(temp.path(), "old/session.jsonl", &[old])]);
        let new = shape_for(Harness::Codex, &[transcript(temp.path(), "new/session.jsonl", &[new])]);
        let diff = diff_shapes(&old, &new, false);
        assert!(diff.added_fields["enums"]["approval_policy"].contains(&"never".into()));
        assert!(diff.added_fields["enums"]["sandbox_policy.type"].contains(&"danger-full-access".into()));
    }

    #[test]
    fn cursor_records_discriminator_shapes() {
        let temp = TempDir::new().unwrap();
        let records = vec![
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}),
            json!({"type":"turn_ended","status":"success"}),
        ];
        let shape = shape_for(Harness::Cursor, &[transcript(temp.path(), "session.jsonl", &records)]);
        assert_eq!(shape["record_types"], json!(["turn_ended"]));
        assert_eq!(shape["roles"], json!(["assistant"]));
        assert_eq!(shape["block_fields"]["text"], json!(["text", "type"]));
        assert!(shape["record_shapes"].as_array().unwrap().contains(&json!(["message", "role"])));
    }

    #[test]
    fn pi_content_types_are_keyed_by_role() {
        let temp = TempDir::new().unwrap();
        let records = vec![json!({"type":"message","message":{"role":"assistant","stopReason":"stop",
            "content":[{"type":"text","text":"hi"},{"type":"toolCall","name":"bash"}]}})];
        let shape = shape_for(Harness::Pi, &[transcript(temp.path(), "session.jsonl", &records)]);
        assert_eq!(shape["content_types"]["message.assistant"], json!(["text", "toolCall"]));
        assert_eq!(shape["enums"]["message.stopReason"], json!(["stop"]));
        assert_eq!(shape["tool_names_seen"], json!(["bash"]));
    }

    #[test]
    fn explicit_fixture_root_skips_expected_output() {
        let temp = TempDir::new().unwrap();
        let scenario = temp.path().join("scenario");
        transcript(&scenario.join("input"), "session.jsonl", &[json!({"type":"session","version":3})]);
        transcript(&scenario.join("expected"), "events.jsonl", &[json!({"type":"assistant_text"})]);
        let input = explicit_inputs(&scenario).unwrap();
        assert_eq!(input.transcripts.len(), 1);
    }
}
