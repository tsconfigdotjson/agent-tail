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
use std::collections::BTreeMap;
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
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct KeyAction {
    on: ScreenState,
    send: String,
    /// Send only once (the default), or every time the state is seen.
    #[serde(default)]
    repeat: bool,
    /// Wait this long after entering the state before sending.
    #[serde(default)]
    after_ms: u64,
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
    let mock_scenario = MockScenario::from_path(&mock_path)?;

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
    let outcome = tokio::task::spawn_blocking(move || run_pty(scenario, harness, addr.to_string(), run_state, run_out))
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
) -> Result<Score, String> {
    let temp = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    seed_claude(&home, &work)?;

    let command = if scenario.command.is_empty() { default_command(harness) } else { scenario.command.clone() };
    let mut builder = CommandBuilder::new(&command[0]);
    for arg in &command[1..] {
        builder.arg(arg);
    }
    builder.cwd(&work);
    builder.env("HOME", &home);
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
    let mut prompt_sent = false;
    let mut screen_state = ScreenState::Starting;
    let mut state_since = Instant::now();
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
            state_since = Instant::now();
        }
        final_screen = screen;

        if screen_state == ScreenState::Idle
            && !prompt_sent
            && let Some(prompt) = &scenario.prompt
        {
            write_keys(&mut writer, prompt.as_bytes())?;
            write_keys(&mut writer, b"\r")?;
            prompt_sent = true;
            eprintln!("driver: prompt sent");
        }

        for (i, action) in scenario.keys.iter().enumerate() {
            let due = action.on == screen_state && state_since.elapsed() >= Duration::from_millis(action.after_ms);
            if due && (action.repeat || !keys_sent[i]) {
                write_keys(&mut writer, action.send.as_bytes())?;
                keys_sent[i] = true;
                eprintln!("driver: sent {:?} on {}", action.send, action.on.id());
            }
        }

        if last_sample.elapsed() >= SAMPLE_EVERY {
            let library = library_state(&mut tail, harness, &home, Timestamp::from(SystemTime::now()));
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
}
