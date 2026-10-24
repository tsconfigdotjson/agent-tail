//! Run a real coding-agent CLI under a PTY against the mock model server, sample its screen
//! and `agenttail`'s state every 100 ms, and score one against the other.
//!
//! The CLI is isolated in a temp `HOME` and temp workspace. Claude Code's onboarding is
//! seeded so the PTY starts at the prompt. Transcripts and the mock's request log are kept
//! in the run directory as artifacts.

mod score;
mod screen;

use agenttail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, SessionState, Timestamp};
use mock_server::{MockState, router, scenario::Scenario as MockScenario};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use score::{Sample, Score};
use screen::ScreenState;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

const COLS: u16 = 120;
const ROWS: u16 = 40;
const SAMPLE_EVERY: Duration = Duration::from_millis(100);
const DUMMY_KEY: &str = "dummy-key";

#[derive(Debug, Deserialize)]
struct DriverScenario {
    name: String,
    harness: String,
    #[serde(default)]
    description: Option<String>,
    /// Path to the mock-server scenario, relative to this file.
    mock: PathBuf,
    /// Defaults per harness. The first element is the executable.
    #[serde(default)]
    command: Vec<String>,
    /// Typed once, the first time the screen is idle.
    #[serde(default)]
    prompt: Option<String>,
    /// Keys to send when the screen enters a state.
    #[serde(default)]
    keys: Vec<KeyAction>,
    #[serde(default = "default_run_for_ms")]
    run_for_ms: u64,
    #[serde(default)]
    no_approval: bool,
    #[serde(default, deserialize_with = "score::deserialize_expected")]
    expected_activity: Vec<score::ExpectedOne>,
    #[serde(default)]
    min_screen_ms: BTreeMap<String, u64>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct KeyAction {
    on: ScreenState,
    send: String,
    /// Send only once (the default), or every time the state is seen.
    #[serde(default)]
    repeat: bool,
    #[serde(default)]
    after_ms: u64,
    #[serde(default = "default_occurrence")]
    occurrence: u32,
}

fn default_occurrence() -> u32 {
    1
}

fn default_run_for_ms() -> u64 {
    12_000
}

struct Tail {
    path: PathBuf,
    reducer: SessionReducer,
    offset: u64,
}

struct Args {
    scenario: PathBuf,
    out: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut scenario = None;
    let mut out = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenario" | "-s" => scenario = Some(PathBuf::from(args.next().ok_or("--scenario needs a path")?)),
            "--out" | "-o" => out = Some(PathBuf::from(args.next().ok_or("--out needs a path")?)),
            "--help" | "-h" => {
                println!("usage: harness-driver --scenario <file> [--out <dir>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args { scenario: scenario.ok_or("--scenario is required")?, out })
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("harness-driver: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(args).await {
        eprintln!("harness-driver: {error}");
        std::process::exit(2);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let scenario_path =
        std::fs::canonicalize(&args.scenario).map_err(|e| format!("scenario {}: {e}", args.scenario.display()))?;
    let scenario_text = std::fs::read_to_string(&scenario_path).map_err(|e| e.to_string())?;
    let scenario: DriverScenario =
        serde_json::from_str(&scenario_text).map_err(|e| format!("parse {}: {e}", scenario_path.display()))?;
    let harness: Harness = scenario.harness.parse().map_err(|e| format!("harness: {e}"))?;
    if harness != Harness::ClaudeCode {
        return Err(format!("{} harness is not wired up yet (Claude Code first)", harness.name()));
    }
    let mock_path = scenario_path.parent().unwrap_or(Path::new(".")).join(&scenario.mock);
    let temp = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let home = std::fs::canonicalize(&home).map_err(|e| e.to_string())?;
    let work = std::fs::canonicalize(&work).map_err(|e| e.to_string())?;
    let mock_text = std::fs::read_to_string(&mock_path).map_err(|e| format!("read {}: {e}", mock_path.display()))?;
    let mock_text = substitute_workspace(&mock_text, &work);
    let mock_scenario = MockScenario::parse(&mock_text).map_err(|e| format!("parse {}: {e}", mock_path.display()))?;

    let out = args.out.unwrap_or_else(|| {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("harness-ci")
            .join("runs")
            .join(format!("{}-{stamp}", scenario.name))
    });
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let mock_log = out.join("mock-requests.jsonl");

    let state = MockState::new(mock_scenario).with_log(mock_log);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("bind mock server: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router(server_state)).await {
            eprintln!("mock server: {error}");
        }
    });

    let run_state = state.clone();
    let scenario_name = scenario.name.clone();
    let run_out = out.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        run_pty(scenario, harness, addr.to_string(), run_state, run_out, &home, &work)
    })
    .await
    .map_err(|e| format!("driver task: {e}"))?;
    server.abort();

    let score = outcome?;
    let score_path = out.join("score.json");
    std::fs::write(&score_path, serde_json::to_string_pretty(&score).expect("score serializes"))
        .map_err(|e| format!("write {}: {e}", score_path.display()))?;
    println!("{}: {} samples, {} failure(s)", scenario_name, score.samples, score.failures.len());
    println!("run: {}", out.display());
    for failure in &score.failures {
        println!("  FAIL: {failure}");
    }
    if score.failures.is_empty() { Ok(()) } else { Err("scenario had hard failures (see score.json)".into()) }
}

fn run_pty(
    scenario: DriverScenario,
    harness: Harness,
    addr: String,
    mock: std::sync::Arc<MockState>,
    out: PathBuf,
    home: &Path,
    work: &Path,
) -> Result<Score, String> {
    seed_claude(home, work)?;
    seed_files(work, &scenario.files)?;

    let command = if scenario.command.is_empty() { default_command(harness) } else { scenario.command.clone() };
    let mut builder = CommandBuilder::new(&command[0]);
    for arg in &command[1..] {
        builder.arg(arg);
    }
    builder.cwd(work);
    builder.env("HOME", home);
    builder.env("CLAUDE_CONFIG_DIR", home.join(".claude"));
    builder.env("ANTHROPIC_BASE_URL", format!("http://{addr}"));
    builder.env("ANTHROPIC_API_KEY", DUMMY_KEY);
    builder.env("DISABLE_AUTOUPDATER", "1");
    builder.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    builder.env("DISABLE_TELEMETRY", "1");
    builder.env("DISABLE_ERROR_REPORTING", "1");
    builder.env("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1");
    builder.env("TERM", "xterm-256color");
    builder.env("COLUMNS", COLS.to_string());
    builder.env("LINES", ROWS.to_string());
    for (key, value) in &scenario.env {
        builder.env(key, value);
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| format!("open pty: {e}"))?;
    let mut child = pair.slave.spawn_command(builder).map_err(|e| format!("spawn {}: {e}", command[0]))?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| format!("pty reader: {e}"))?;
    let mut writer = pair.master.take_writer().map_err(|e| format!("pty writer: {e}"))?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    let mut tail: Option<Tail> = None;
    let mut samples = Vec::new();
    let mut final_screen = String::new();
    let mut keys_sent = vec![false; scenario.keys.len()];
    let mut keys_clock = KeyClock::new(ScreenState::Starting);
    let mut prompt_sent = false;
    let mut screen_state = ScreenState::Starting;
    let mut last_sample = Instant::now();
    let start = Instant::now();
    let run_for = Duration::from_millis(scenario.run_for_ms);

    while start.elapsed() < run_for {
        while let Ok(bytes) = rx.try_recv() {
            parser.process(&bytes);
        }
        let screen = parser.screen().contents();
        let reading = screen::read_screen(harness, &screen);
        if reading.state != screen_state {
            screen_state = reading.state;
        }
        final_screen = screen;
        let now_ms = start.elapsed().as_millis() as u64;
        let (key_state, occurrence, key_elapsed_ms) = keys_clock.tick(reading.state, now_ms);

        let prompt_pending = scenario.prompt.is_some() && !prompt_sent;
        if !prompt_pending {
            for index in
                due_keys(&scenario.keys, &keys_sent, key_state, occurrence, Duration::from_millis(key_elapsed_ms))
            {
                write_keys(&mut writer, scenario.keys[index].send.as_bytes())?;
                keys_sent[index] = true;
                eprintln!(
                    "driver: sent {:?} on {} occurrence {occurrence}",
                    scenario.keys[index].send,
                    scenario.keys[index].on.id()
                );
            }
        }

        if screen_state == ScreenState::Idle
            && !prompt_sent
            && let Some(prompt) = &scenario.prompt
        {
            write_keys(&mut writer, prompt.as_bytes())?;
            write_keys(&mut writer, b"\r")?;
            prompt_sent = true;
            keys_clock.reset(ScreenState::Idle, now_ms);
            eprintln!("driver: prompt sent");
        }

        if last_sample.elapsed() >= SAMPLE_EVERY {
            let library = library_state(&mut tail, harness, home, Timestamp::from(SystemTime::now()));
            samples.push(Sample {
                elapsed_ms: start.elapsed().as_millis() as u64,
                screen: screen_state.id(),
                background_agents: reading.background_agents,
                library: library.as_ref().map(score::library_point),
            });
            last_sample = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let _ = child.kill();
    let transcript = tail.as_ref().map(|tail| tail.path.clone());
    let transcript_text = transcript.as_ref().and_then(|path| std::fs::read_to_string(path).ok());
    let helpers = collect_helpers(home);
    copy_helpers(&helpers, &out)?;
    let expectations: Vec<score::Expectation> =
        scenario.expected_activity.iter().map(score::ExpectedOne::expectation).collect();
    let score = score::score(&score::ScoreRequest {
        scenario: &scenario.name,
        description: scenario.description.clone(),
        harness: harness.id(),
        no_approval: scenario.no_approval,
        expected: &expectations,
        samples: &samples,
        mock_requests: mock.requests(),
        remaining_mock_steps: mock.remaining(),
        transcript: transcript.as_deref().map(|path| path.display().to_string()),
        min_screen_ms: &scenario.min_screen_ms,
        transcript_jsonl: transcript_text.as_deref(),
        helpers: &helpers,
    });

    let timeline = out.join("timeline.jsonl");
    let mut timeline_text = String::new();
    for sample in &samples {
        timeline_text.push_str(&serde_json::to_string(sample).expect("sample serializes"));
        timeline_text.push('\n');
    }
    std::fs::write(&timeline, timeline_text).map_err(|e| format!("write {}: {e}", timeline.display()))?;
    std::fs::write(out.join("screen.txt"), final_screen).map_err(|e| e.to_string())?;
    if let Some(path) = &transcript
        && let Ok(bytes) = std::fs::read(path)
    {
        std::fs::write(out.join("transcript.jsonl"), bytes).map_err(|e| e.to_string())?;
    }
    Ok(score)
}

const KEY_STABLE_MS: u64 = 300;

struct KeyClock {
    state: ScreenState,
    since_ms: u64,
    counts: BTreeMap<ScreenState, u32>,
    gap: Option<(ScreenState, u64)>,
}

impl KeyClock {
    fn new(state: ScreenState) -> Self {
        Self { state, since_ms: 0, counts: BTreeMap::new(), gap: None }
    }

    fn reset(&mut self, state: ScreenState, now_ms: u64) {
        *self = Self { state, since_ms: now_ms, counts: BTreeMap::new(), gap: None };
    }

    fn tick(&mut self, raw: ScreenState, now_ms: u64) -> (ScreenState, u32, u64) {
        if raw == self.state {
            self.gap = None;
        } else {
            match self.gap {
                Some((pending, at)) if pending == raw && now_ms.saturating_sub(at) >= KEY_STABLE_MS => {
                    self.state = raw;
                    self.since_ms = at;
                    self.gap = None;
                    *self.counts.entry(raw).or_default() += 1;
                }
                Some((pending, _)) if pending == raw => {}
                _ => self.gap = Some((raw, now_ms)),
            }
        }
        let occurrence = self.counts.get(&self.state).copied().unwrap_or(0);
        (self.state, occurrence, now_ms.saturating_sub(self.since_ms))
    }
}

fn due_keys(keys: &[KeyAction], sent: &[bool], screen: ScreenState, occurrence: u32, elapsed: Duration) -> Vec<usize> {
    keys.iter()
        .enumerate()
        .filter(|(index, action)| {
            action.on == screen
                && (action.occurrence == 0 || action.occurrence == occurrence)
                && elapsed >= Duration::from_millis(action.after_ms)
                && (action.repeat || !sent[*index])
        })
        .map(|(index, _)| index)
        .collect()
}

fn substitute_workspace(scenario: &str, workspace: &Path) -> String {
    let encoded = serde_json::to_string(&workspace.display().to_string()).expect("a path encodes as a json string");
    let encoded = &encoded[1..encoded.len() - 1];
    scenario.replace("{{workspace}}", encoded)
}

fn seed_files(work: &Path, files: &BTreeMap<String, String>) -> Result<(), String> {
    for (rel, contents) in files {
        let relative = Path::new(rel);
        if relative.is_absolute() || relative.components().any(|component| component == std::path::Component::ParentDir)
        {
            return Err(format!("scenario file {rel:?} escapes the workspace"));
        }
        let path = work.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn write_keys(writer: &mut Box<dyn Write + Send>, bytes: &[u8]) -> Result<(), String> {
    writer.write_all(bytes).map_err(|e| format!("write pty: {e}"))?;
    writer.flush().map_err(|e| format!("flush pty: {e}"))
}

fn default_command(harness: Harness) -> Vec<String> {
    match harness {
        Harness::ClaudeCode => vec!["claude".into(), "--bare".into(), "--model".into(), "claude-sonnet-4-5".into()],
        _ => vec![],
    }
}

fn seed_claude(home: &Path, work: &Path) -> Result<(), String> {
    let config_dir = home.join(".claude");
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    let work = std::fs::canonicalize(work).unwrap_or_else(|_| work.to_path_buf());
    let config = serde_json::json!({
        "hasCompletedOnboarding": true,
        "lastOnboardingVersion": "2.1.310",
        "numStartups": 10,
        "theme": "dark",
        "bypassPermissionsModeAccepted": true,
        "customApiKeyResponses": { "approved": [DUMMY_KEY], "rejected": [] },
        "projects": {
            work.to_string_lossy(): {
                "hasTrustDialogAccepted": true,
                "allowedTools": [],
                "mcpServers": {}
            }
        }
    });
    std::fs::write(config_dir.join(".claude.json"), serde_json::to_vec_pretty(&config).expect("config serializes"))
        .map_err(|e| format!("seed .claude.json: {e}"))?;
    std::fs::write(config_dir.join("settings.json"), br#"{"skipDangerousModePermissionPrompt": true}"#)
        .map_err(|e| format!("seed settings.json: {e}"))
}

fn library_state(tail: &mut Option<Tail>, _harness: Harness, home: &Path, now: Timestamp) -> Option<SessionState> {
    let path = find_claude_transcript(home)?;
    let metadata = std::fs::metadata(&path).ok()?;
    let len = metadata.len();
    let mtime = metadata.modified().map(Timestamp::from).unwrap_or(now);
    let replaced = tail.as_ref().is_some_and(|tail| tail.path != path || len < tail.offset);
    if tail.is_none() || replaced {
        let mut reducer = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
        reducer.set_path(&path);
        *tail = Some(Tail { path: path.clone(), reducer, offset: 0 });
    }
    let tail = tail.as_mut().expect("tail exists");
    if len > tail.offset {
        let mut file = std::fs::File::open(&path).ok()?;
        file.seek(SeekFrom::Start(tail.offset)).ok()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let ctx = ChunkCtx { offset: tail.offset, mtime, path: &tail.path };
        if tail.reducer.feed(&bytes, ctx).is_ok() {
            tail.offset = len;
        } else {
            // The file was rewritten in a way the reducer couldn't follow: start over.
            let mut reducer = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
            reducer.set_path(&path);
            let ctx = ChunkCtx { offset: 0, mtime, path: &path };
            let all = std::fs::read(&path).ok()?;
            reducer.feed(&all, ctx).ok()?;
            *tail = Tail { path: path.clone(), reducer, offset: all.len() as u64 };
        }
    } else {
        tail.reducer.transcript_written(mtime);
    }
    Some(tail.reducer.state(now))
}

fn collect_helpers(home: &Path) -> Vec<score::ObservedHelper> {
    let mut found = Vec::new();
    walk_helpers(&home.join(".claude").join("projects"), 0, &mut found);
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

fn walk_helpers(dir: &Path, depth: usize, found: &mut Vec<score::ObservedHelper>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        if path.file_name().is_some_and(|name| name == "subagents") {
            read_subagents(&path, found);
        } else {
            walk_helpers(&path, depth + 1, found);
        }
    }
}

fn read_subagents(dir: &Path, found: &mut Vec<score::ObservedHelper>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut metas = BTreeMap::new();
    let mut jsonls = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue };
        if let Some(id) = name.strip_suffix(".meta.json")
            && safe_helper_id(id)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            metas.insert(id.to_string(), text);
        } else if let Some(id) = name.strip_suffix(".jsonl")
            && safe_helper_id(id)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            jsonls.insert(id.to_string(), text);
        }
    }
    let mut ids = BTreeSet::new();
    ids.extend(metas.keys().cloned());
    ids.extend(jsonls.keys().cloned());
    for id in ids {
        found.push(score::ObservedHelper { meta_json: metas.remove(&id), jsonl: jsonls.remove(&id), id });
    }
}

fn safe_helper_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && id != "." && id != ".."
}

fn copy_helpers(helpers: &[score::ObservedHelper], out: &Path) -> Result<(), String> {
    if helpers.is_empty() {
        return Ok(());
    }
    let dir = out.join("subagents");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for helper in helpers {
        if !safe_helper_id(&helper.id) {
            continue;
        }
        if let Some(jsonl) = &helper.jsonl {
            let path = dir.join(format!("{}.jsonl", helper.id));
            std::fs::write(&path, jsonl).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        if let Some(meta) = &helper.meta_json {
            let path = dir.join(format!("{}.meta.json", helper.id));
            std::fs::write(&path, meta).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn find_claude_transcript(home: &Path) -> Option<PathBuf> {
    let root = home.join(".claude").join("projects");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    walk(&root, 0, &mut best);
    best.map(|(_, path)| path)
}

fn walk(dir: &Path, depth: usize, best: &mut Option<(SystemTime, PathBuf)>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            if path.file_name().is_some_and(|name| name == "subagents") {
                continue;
            }
            walk(&path, depth + 1, best);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(time, _)| modified > *time) {
                *best = Some((modified, path));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(name: &str) -> DriverScenario {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    #[test]
    fn the_green_scenarios_parse_without_expected_activity() {
        let plain = scenario("claude_plain_qa.json");
        assert!(plain.expected_activity.is_empty());
        assert!(!plain.no_approval);
        let approval = scenario("claude_bash_approval.json");
        assert_eq!(approval.keys.len(), 1);
        assert_eq!(approval.keys[0].on, ScreenState::PermissionPrompt);
        assert_eq!(approval.keys[0].occurrence, 1);
        let pending = scenario("claude_bash_approval_pending.json");
        assert!(pending.keys.is_empty());
        let bypass = scenario("claude_bash_bypass.json");
        assert!(bypass.no_approval);
        assert!(bypass.expected_activity.is_empty());
    }

    #[test]
    fn expected_activity_parses_on_a_driver_scenario() {
        let raw = r#"{
            "name": "ends",
            "harness": "claude_code",
            "mock": "mock.json",
            "expected_activity": ["done", {"activity": "interrupted", "when": "ever", "screen": "idle"}]
        }"#;
        let scenario: DriverScenario = serde_json::from_str(raw).unwrap();
        assert_eq!(scenario.expected_activity.len(), 2);
        assert_eq!(scenario.expected_activity[0].expectation().activity, "done");
        assert_eq!(scenario.expected_activity[1].expectation().when, score::ExpectWhen::Ever);
        assert_eq!(scenario.expected_activity[1].expectation().screen.as_deref(), Some("idle"));
    }

    fn key(on: ScreenState, occurrence: u32, after_ms: u64, repeat: bool) -> KeyAction {
        KeyAction { on, send: "x".into(), repeat, after_ms, occurrence }
    }

    #[test]
    fn a_short_flicker_does_not_open_the_next_permission_prompt() {
        let mut clock = KeyClock::new(ScreenState::Starting);
        assert_eq!(clock.tick(ScreenState::Starting, 0).1, 0);
        assert_eq!(clock.tick(ScreenState::PermissionPrompt, 1_000).0, ScreenState::Starting);
        let (state, occurrence, elapsed) = clock.tick(ScreenState::PermissionPrompt, 1_300);
        assert_eq!(state, ScreenState::PermissionPrompt);
        assert_eq!(occurrence, 1);
        assert_eq!(elapsed, 300);
        let keys = vec![
            key(ScreenState::PermissionPrompt, 1, 1_500, false),
            key(ScreenState::PermissionPrompt, 2, 1_500, false),
        ];
        let sent = vec![false, false];
        assert!(due_keys(&keys, &sent, state, occurrence, Duration::from_millis(elapsed)).is_empty());
        let (state, occurrence, elapsed) = clock.tick(ScreenState::PermissionPrompt, 2_500);
        assert_eq!(due_keys(&keys, &sent, state, occurrence, Duration::from_millis(elapsed)), vec![0]);

        assert_eq!(clock.tick(ScreenState::Working, 2_600).0, ScreenState::PermissionPrompt);
        let (state, occurrence, _) = clock.tick(ScreenState::PermissionPrompt, 2_700);
        assert_eq!((state, occurrence), (ScreenState::PermissionPrompt, 1));

        clock.tick(ScreenState::Working, 3_000);
        let (state, occurrence, elapsed) = clock.tick(ScreenState::Working, 3_300);
        assert_eq!((state, occurrence), (ScreenState::Working, 1));
        assert_eq!(elapsed, 300);
        clock.tick(ScreenState::PermissionPrompt, 4_000);
        let (state, occurrence, elapsed) = clock.tick(ScreenState::PermissionPrompt, 4_300);
        assert_eq!((state, occurrence), (ScreenState::PermissionPrompt, 2));
        assert_eq!(elapsed, 300);
        assert_eq!(due_keys(&keys, &sent, state, occurrence, Duration::from_millis(1_500)), vec![1]);
    }

    #[test]
    fn the_second_permission_prompt_gets_its_own_key() {
        let keys = vec![
            key(ScreenState::PermissionPrompt, 1, 1_500, false),
            key(ScreenState::PermissionPrompt, 2, 1_500, false),
        ];
        let sent = vec![false, false];
        let first = due_keys(&keys, &sent, ScreenState::PermissionPrompt, 1, Duration::from_millis(1_500));
        assert_eq!(first, vec![0]);
        let early = due_keys(&keys, &sent, ScreenState::PermissionPrompt, 1, Duration::from_millis(1_499));
        assert!(early.is_empty());
        let mut sent = vec![true, false];
        let still_first = due_keys(&keys, &sent, ScreenState::PermissionPrompt, 1, Duration::from_millis(5_000));
        assert!(still_first.is_empty());
        let second = due_keys(&keys, &sent, ScreenState::PermissionPrompt, 2, Duration::from_millis(1_500));
        assert_eq!(second, vec![1]);
        sent[1] = true;
        assert!(due_keys(&keys, &sent, ScreenState::PermissionPrompt, 2, Duration::from_millis(9_000)).is_empty());
        let any = vec![key(ScreenState::Working, 0, 1_000, false)];
        assert!(due_keys(&any, &[false], ScreenState::Working, 1, Duration::from_millis(999)).is_empty());
        assert_eq!(due_keys(&any, &[false], ScreenState::Working, 2, Duration::from_millis(1_000)), vec![0]);
        assert!(due_keys(&any, &[true], ScreenState::Working, 3, Duration::from_millis(5_000)).is_empty());
        let repeat = vec![key(ScreenState::AskUserQuestion, 1, 0, true)];
        let sent = vec![true];
        assert_eq!(due_keys(&repeat, &sent, ScreenState::AskUserQuestion, 1, Duration::ZERO), vec![0]);
    }

    #[test]
    fn the_a3_scenarios_parse() {
        let denied = scenario("claude_bash_approval_denied.json");
        assert_eq!(denied.keys.len(), 2);
        assert_eq!(denied.keys[0].occurrence, 1);
        assert_eq!(denied.keys[1].occurrence, 2);
        assert_eq!(denied.keys[0].send, "\r");
        assert!(denied.keys[1].send.contains('\u{1b}'));
        let denied_end = denied.expected_activity[0].expectation();
        assert_eq!(denied_end.activity, "done");
        assert_eq!(denied_end.certainty.as_deref(), Some("firm"));
        assert_eq!(denied_end.screen.as_deref(), Some("idle"));

        let edits = scenario("claude_accept_edits.json");
        assert!(edits.command.iter().any(|arg| arg == "acceptEdits"));
        assert_eq!(edits.keys.len(), 2);
        assert_eq!(edits.expected_activity[0].expectation().activity, "done");

        let plan = scenario("claude_plan_exit.json");
        assert!(plan.command.iter().any(|arg| arg == "plan"));
        assert_eq!(plan.files.get("note.txt").map(String::as_str), Some("seed\n"));
        let exit = plan.expected_activity[0].expectation();
        assert_eq!(exit.activity, "awaiting_answer");
        assert_eq!(exit.certainty.as_deref(), Some("firm"));
        assert_eq!(exit.when, score::ExpectWhen::End);
        assert_eq!(exit.screen.as_deref(), Some("ask_user_question"));

        let ask = scenario("claude_ask_user.json");
        assert_eq!(ask.keys[0].on, ScreenState::AskUserQuestion);
        assert_eq!(ask.keys[0].after_ms, 30_000);
        assert_eq!(ask.min_screen_ms.get("ask_user_question"), Some(&25_000));
        let waiting = ask.expected_activity[0].expectation();
        assert_eq!(waiting.activity, "awaiting_answer");
        assert_eq!(waiting.certainty.as_deref(), Some("firm"));
        assert_eq!(waiting.when, score::ExpectWhen::Ever);
        assert_eq!(ask.expected_activity[1].expectation().activity, "done");

        for name in [
            "claude_bash_approval_denied.json",
            "claude_accept_edits.json",
            "claude_plan_exit.json",
            "claude_ask_user.json",
        ] {
            let driver = scenario(name);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&driver.mock);
            let text =
                std::fs::read_to_string(&mock_path).unwrap_or_else(|e| panic!("read {}: {e}", mock_path.display()));
            let text = substitute_workspace(&text, Path::new("/tmp/a3 work\""));
            mock_server::scenario::Scenario::parse(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", mock_path.display()));
        }
    }

    #[test]
    fn workspace_substitution_stays_inside_json_strings() {
        let raw = r#"{"file_path":"{{workspace}}/note.txt","untouched":"{{workspace}}"}"#;
        let got = substitute_workspace(raw, Path::new("/tmp/a3 work\""));
        let value: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(value["file_path"], "/tmp/a3 work\"/note.txt");
    }

    #[test]
    fn resetting_the_key_clock_forgets_startup_screens() {
        let mut clock = KeyClock::new(ScreenState::Starting);
        clock.tick(ScreenState::Working, 1_000);
        let (state, occurrence, _) = clock.tick(ScreenState::Working, 1_400);
        assert_eq!((state, occurrence), (ScreenState::Working, 1));
        clock.reset(ScreenState::Idle, 5_000);
        assert_eq!(clock.tick(ScreenState::Idle, 5_100).1, 0);
        clock.tick(ScreenState::Working, 6_000);
        let (state, occurrence, elapsed) = clock.tick(ScreenState::Working, 6_400);
        assert_eq!((state, occurrence), (ScreenState::Working, 1));
        assert_eq!(elapsed, 400);
    }

    #[test]
    fn the_interrupt_background_and_error_scenarios_parse() {
        let thinking = scenario("claude_interrupt_thinking.json");
        assert_eq!(thinking.keys[0].on, ScreenState::Working);
        assert_eq!(thinking.keys[0].send, "\u{1b}");
        assert_eq!(thinking.keys[0].after_ms, 2_500);
        assert_eq!(thinking.keys[0].occurrence, 0);
        let thinking_end = thinking.expected_activity[0].expectation();
        assert_eq!(thinking_end.activity, "interrupted");
        assert_eq!(thinking_end.certainty.as_deref(), Some("firm"));
        assert_eq!(thinking_end.when, score::ExpectWhen::End);
        assert_eq!(thinking_end.screen.as_deref(), Some("idle"));

        let tool = scenario("claude_interrupt_tool.json");
        assert!(tool.no_approval);
        assert!(tool.command.iter().any(|arg| arg == "--dangerously-skip-permissions"));
        assert_eq!(tool.keys[0].send, "\u{1b}");
        assert_eq!(tool.keys[0].occurrence, 0);
        assert_eq!(tool.expected_activity[0].expectation().activity, "interrupted");

        let background = scenario("claude_background_agent.json");
        assert!(background.command.iter().all(|arg| arg != "--bare"));
        assert!(background.no_approval);
        let activities: Vec<_> = background.expected_activity.iter().map(|item| item.expectation().activity).collect();
        assert_eq!(activities, vec!["background_work".to_string(), "done".to_string()]);
        let waiting = background.expected_activity[0].expectation();
        assert_eq!(waiting.certainty.as_deref(), Some("firm"));
        assert_eq!(waiting.when, score::ExpectWhen::Ever);
        assert_eq!(waiting.screen.as_deref(), Some("working"));
        let finished = background.expected_activity[1].expectation();
        assert_eq!(finished.activity, "done");
        assert_eq!(finished.certainty.as_deref(), Some("firm"));
        assert_eq!(finished.screen.as_deref(), Some("idle"));

        let overloaded = scenario("claude_api_error_529.json");
        assert_eq!(overloaded.expected_activity[0].expectation().activity, "errored");
        assert_eq!(overloaded.expected_activity[0].expectation().certainty.as_deref(), Some("firm"));
        assert_eq!(overloaded.env.get("CLAUDE_CODE_MAX_RETRIES").map(String::as_str), Some("0"));
        let broken = scenario("claude_api_error_stream.json");
        assert_eq!(broken.expected_activity[0].expectation().activity, "errored");
        assert_eq!(broken.env.get("CLAUDE_CODE_MAX_RETRIES").map(String::as_str), Some("0"));
        assert_eq!(broken.expected_activity[0].expectation().when, score::ExpectWhen::End);

        for name in [
            "claude_interrupt_thinking.json",
            "claude_interrupt_tool.json",
            "claude_background_agent.json",
            "claude_api_error_529.json",
            "claude_api_error_stream.json",
        ] {
            let driver = scenario(name);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&driver.mock);
            let text =
                std::fs::read_to_string(&mock_path).unwrap_or_else(|e| panic!("read {}: {e}", mock_path.display()));
            let text = substitute_workspace(&text, Path::new("/tmp/a3 work\""));
            let mock = mock_server::scenario::Scenario::parse(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", mock_path.display()));
            if name == "claude_background_agent.json" {
                assert_eq!(mock.dispatch, mock_server::scenario::Dispatch::FirstMatch);
                assert!(mock.steps.len() >= 5, "{}", mock.steps.len());
            }
            if name == "claude_api_error_529.json" {
                assert_eq!(mock.steps.len(), 8);
                assert!(mock.steps.iter().all(|step| {
                    matches!(step.reply.fault, Some(mock_server::scenario::Fault::Http { status: 529, .. }))
                }));
            }
            if name == "claude_api_error_stream.json" {
                assert_eq!(mock.steps.len(), 8);
                assert!(
                    mock.steps
                        .iter()
                        .all(|step| matches!(step.reply.fault, Some(mock_server::scenario::Fault::BrokenStream)))
                );
            }
        }
    }

    #[test]
    fn helper_transcripts_are_collected_and_copied() {
        let home = tempfile::tempdir().unwrap();
        let sub = home.path().join(".claude/projects/work/sess/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-outer.jsonl"), "{\"type\":\"assistant\"}\n").unwrap();
        std::fs::write(sub.join("agent-inner.meta.json"), "{\"spawnDepth\":2}\n").unwrap();
        std::fs::write(sub.join("../not-a-helper.jsonl"), "nope").unwrap();
        std::fs::write(sub.join("../x.jsonl"), "nope").unwrap();
        let helpers = collect_helpers(home.path());
        let ids: Vec<_> = helpers.iter().map(|helper| helper.id.as_str()).collect();
        assert_eq!(ids, vec!["agent-inner", "agent-outer"]);
        assert!(helpers[1].jsonl.as_ref().unwrap().contains("assistant"));
        assert!(helpers[0].meta_json.as_ref().unwrap().contains("spawnDepth"));
        let out = tempfile::tempdir().unwrap();
        copy_helpers(&helpers, out.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("subagents/agent-outer.jsonl")).unwrap(),
            "{\"type\":\"assistant\"}\n"
        );
        assert!(out.path().join("subagents/agent-inner.meta.json").is_file());
    }

    #[test]
    fn seeded_files_stay_inside_the_workspace() {
        let work = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        files.insert("note.txt".into(), "seed\n".into());
        files.insert("nested/plan.md".into(), "plan\n".into());
        seed_files(work.path(), &files).unwrap();
        assert_eq!(std::fs::read_to_string(work.path().join("note.txt")).unwrap(), "seed\n");
        assert_eq!(std::fs::read_to_string(work.path().join("nested/plan.md")).unwrap(), "plan\n");
        files.insert("../outside.txt".into(), "no".into());
        assert!(seed_files(work.path(), &files).is_err());
        files.remove("../outside.txt");
        files.insert("/tmp/outside.txt".into(), "no".into());
        assert!(seed_files(work.path(), &files).is_err());
    }
}
