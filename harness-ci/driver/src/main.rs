//! Run a real coding-agent CLI under a PTY against the mock model server, sample its screen
//! and `agenttail`'s state every 100 ms, and score one against the other.
//!
//! The CLI is isolated in a temp `HOME` and temp workspace. Claude Code's onboarding is
//! seeded so the PTY starts at the prompt. Transcripts and the mock's request log are kept
//! in the run directory as artifacts.

use agenttail::{
    Activity, Certainty, ChunkCtx, Harness, Reason, ReducerOptions, SessionReducer, SessionState, Timestamp,
};
use mock_server::{MockState, router, scenario::Scenario as MockScenario};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

const COLS: u16 = 120;
const ROWS: u16 = 40;
const SAMPLE_EVERY: Duration = Duration::from_millis(100);
const DUMMY_KEY: &str = "dummy-key";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScreenState {
    Starting,
    Idle,
    Working,
    PermissionPrompt,
    AskUserQuestion,
    Error,
    Unknown,
}

impl ScreenState {
    fn id(self) -> &'static str {
        match self {
            ScreenState::Starting => "starting",
            ScreenState::Idle => "idle",
            ScreenState::Working => "working",
            ScreenState::PermissionPrompt => "permission_prompt",
            ScreenState::AskUserQuestion => "ask_user_question",
            ScreenState::Error => "error",
            ScreenState::Unknown => "unknown",
        }
    }
}

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

#[derive(Debug, Serialize)]
struct LibraryPoint {
    activity: Activity,
    certainty: Certainty,
    reason: Reason,
    since: Option<Timestamp>,
    last_write: Option<Timestamp>,
}

#[derive(Debug, Serialize)]
struct Sample {
    elapsed_ms: u64,
    screen: &'static str,
    library: Option<LibraryPoint>,
}

#[derive(Debug, Default, Serialize)]
struct Score {
    scenario: String,
    description: Option<String>,
    harness: String,
    samples: usize,
    screen_counts: BTreeMap<String, usize>,
    /// screen -> library activity -> samples
    confusion: BTreeMap<String, BTreeMap<String, usize>>,
    failures: Vec<String>,
    mock_requests: u64,
    remaining_mock_steps: usize,
    transcript: Option<String>,
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
        let new_state = detect_screen(&screen);
        if new_state != screen_state {
            screen_state = new_state;
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
                library: library.as_ref().map(library_point),
            });
            last_sample = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let _ = child.kill();
    let transcript = tail.as_ref().map(|tail| tail.path.clone());
    let score = score(&scenario, harness, &samples, &mock, transcript.as_deref());

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

/// The screen detectors for each harness. Every string here comes from a real captured
/// screen; see the tests at the bottom.
fn detect_screen(screen: &str) -> ScreenState {
    let text = normalize(screen);
    if text.contains("do you want to proceed?") || text.contains("esc to cancel · tab to amend") {
        return ScreenState::PermissionPrompt;
    }
    if text.contains("api error") || text.contains("authentication failed") || text.contains("request failed") {
        return ScreenState::Error;
    }
    if text.contains("esc to interrupt") || text.contains("retrying in") || text.contains("processing…") {
        return ScreenState::Working;
    }
    if text.contains("? for shortcuts") || text.contains("bypass permissions on") || text.contains("auto mode on") {
        return ScreenState::Idle;
    }
    if text.contains("welcome to claude code") {
        return ScreenState::Starting;
    }
    ScreenState::Unknown
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn library_point(state: &SessionState) -> LibraryPoint {
    LibraryPoint {
        activity: state.activity,
        certainty: state.certainty,
        reason: state.reason,
        since: state.since,
        last_write: state.last_write,
    }
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

fn score(
    scenario: &DriverScenario,
    harness: Harness,
    samples: &[Sample],
    mock: &MockState,
    transcript: Option<&Path>,
) -> Score {
    let mut score = Score {
        scenario: scenario.name.clone(),
        description: scenario.description.clone(),
        harness: harness.id().into(),
        samples: samples.len(),
        mock_requests: mock.requests(),
        remaining_mock_steps: mock.remaining(),
        transcript: transcript.map(|path| path.display().to_string()),
        ..Score::default()
    };
    let mut failures = BTreeSet::new();
    let stall = ReducerOptions::default().stall_after;
    let stall_ms = stall.as_millis() as u64;

    for sample in samples {
        *score.screen_counts.entry(sample.screen.to_string()).or_default() += 1;
        if let Some(library) = &sample.library {
            *score
                .confusion
                .entry(sample.screen.to_string())
                .or_default()
                .entry(activity_id(library.activity).to_string())
                .or_default() += 1;
            if library.activity == Activity::AwaitingApproval {
                if scenario.no_approval {
                    failures.insert("AwaitingApproval in a mode where no approval is possible".into());
                }
                if sample.screen != ScreenState::PermissionPrompt.id() {
                    failures.insert(format!("AwaitingApproval while the screen is {}", sample.screen));
                }
            }
        }
    }

    // A permission prompt that stays on screen longer than the stall threshold plus slack
    // without the library saying AwaitingApproval is a missed prompt.
    let mut run_start: Option<u64> = None;
    let mut run_last = 0u64;
    let mut saw_awaiting = false;
    for sample in samples {
        let is_prompt = sample.screen == ScreenState::PermissionPrompt.id();
        if is_prompt {
            run_start.get_or_insert(sample.elapsed_ms);
            run_last = sample.elapsed_ms;
            saw_awaiting |= sample
                .library
                .as_ref()
                .is_some_and(|l| matches!(l.activity, Activity::AwaitingApproval | Activity::AwaitingAnswer));
        } else if let Some(start) = run_start.take()
            && run_last.saturating_sub(start) > stall_ms + 1_000
            && !saw_awaiting
        {
            failures
                .insert(format!("permission prompt on screen for {} ms without AwaitingApproval", run_last - start));
        }
    }
    if let Some(start) = run_start
        && run_last.saturating_sub(start) > stall_ms + 1_000
        && !saw_awaiting
    {
        failures.insert(format!("permission prompt on screen for {} ms without AwaitingApproval", run_last - start));
    }

    score.failures = failures.into_iter().collect();
    score
}

fn activity_id(activity: Activity) -> &'static str {
    match activity {
        Activity::AwaitingAnswer => "awaiting_answer",
        Activity::AwaitingApproval => "awaiting_approval",
        Activity::RunningTool => "running_tool",
        Activity::Thinking => "thinking",
        Activity::BackgroundWork => "background_work",
        Activity::Done => "done",
        Activity::Interrupted => "interrupted",
        Activity::Errored => "errored",
        Activity::Idle => "idle",
        Activity::Unknown => "unknown",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_captured_permission_prompt() {
        // Captured from Claude Code 2.1.310, default mode, a `touch` command.
        let screen = r#"
            ╭──────────────────────────────────────────────────────────────╮
            │ Bash command                                                 │
            │   Create a marker file                                       │
            │   touch permission-marker.txt                                │
            │ Do you want to proceed?                                      │
            │ ❯ 1. Yes                                                     │
            │   2. Yes, and always allow access …                          │
            │   3. No                                                      │
            │ Esc to cancel · Tab to amend                                 │
            ╰──────────────────────────────────────────────────────────────╯
        "#;
        assert_eq!(detect_screen(screen), ScreenState::PermissionPrompt);
    }

    #[test]
    fn detects_the_captured_working_screen() {
        let screen = r#"
            ❯ run the command
            ✻ Composing…
            ❯
            esc to interrupt · ← for agents
        "#;
        assert_eq!(detect_screen(screen), ScreenState::Working);
    }

    #[test]
    fn detects_the_captured_idle_screen() {
        let screen = r#"
            ⏺ The marker file was created.
            ✻ Worked for 0s · done 9:43 PM

            ❯
            ? for shortcuts · ← for agents
        "#;
        assert_eq!(detect_screen(screen), ScreenState::Idle);
    }

    #[test]
    fn detects_the_captured_bypass_idle_screen() {
        // Captured from Claude Code 2.1.310 with --dangerously-skip-permissions.
        let screen = r#"
            ▗ ▗   ▖ ▖  Claude Code v2.1.310
                       Sonnet 4.5 · API Usage Billing
             ▘▘   ▝▝   /private/tmp/work
            ────────────────────────────────────────────────────────────────
            ❯
            ────────────────────────────────────────────────────────────────
              ⏵⏵ bypass permissions on (shift+tab to cycle)
        "#;
        assert_eq!(detect_screen(screen), ScreenState::Idle);
    }

    #[test]
    fn errors_beat_the_spinner() {
        let screen = r#"
            ❯ run the command
            ✻ API error · Retrying in 1s · attempt 1/10
            esc to interrupt · ← for agents
        "#;
        assert_eq!(detect_screen(screen), ScreenState::Error);
    }
}
