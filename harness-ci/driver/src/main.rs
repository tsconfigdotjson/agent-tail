//! Run a real coding-agent CLI under a PTY against the mock model server, sample its screen
//! and `agent-tail`'s state every 100 ms, and score one against the other.
//!
//! The CLI is isolated in a temp `HOME` and temp workspace. Claude Code's onboarding and
//! Codex's provider config are seeded so the PTY starts at the prompt or composer.
//! Transcripts and the mock's request log are kept in the run directory as artifacts.

mod score;
mod screen;

use agent_tail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, SessionState, Timestamp, read_opencode_db};
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
/// CLI startup is not part of the scenario's observation window. A prompt-driven phase gets
/// this long to reach the composer; once the prompt is typed, `run_for_ms` starts.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

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
    #[serde(default)]
    settings: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    phases: Vec<Phase>,
    #[serde(default)]
    exit: ExitMode,
    #[serde(default)]
    expect_registry: Option<score::RegistryExpect>,
    #[serde(default)]
    expect_observations: Option<score::ObserveExpect>,
}

#[derive(Debug, Clone, Deserialize)]
struct Phase {
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    keys: Vec<KeyAction>,
    #[serde(default)]
    run_for_ms: Option<u64>,
    #[serde(default)]
    exit: Option<ExitMode>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExitMode {
    #[default]
    Kill,
    Graceful,
}

struct PhaseSpec {
    command: Vec<String>,
    prompt: Option<String>,
    keys: Vec<KeyAction>,
    run_for_ms: u64,
    exit: ExitMode,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
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
    if !matches!(
        harness,
        Harness::ClaudeCode | Harness::Codex | Harness::Pi | Harness::OpenCode | Harness::Gemini | Harness::Copilot
    ) {
        return Err(format!("{} harness is not wired up yet", harness.name()));
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
        if matches!(harness, Harness::Pi | Harness::Gemini | Harness::Copilot) {
            run_headless_json(scenario, harness, addr.to_string(), run_state, run_out, &home, &work)
        } else {
            run_pty(scenario, harness, addr.to_string(), run_state, run_out, &home, &work)
        }
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

/// Pi, Gemini CLI and Copilot CLI can run a one-shot prompt without a TUI. They still
/// exercise the real CLI, the mock provider, the session writer and the same reducer;
/// samples are transcript-based and the screen is `unknown`, so their driver scenarios
/// assert activities without screen strings.
fn run_headless_json(
    scenario: DriverScenario,
    harness: Harness,
    addr: String,
    mock: std::sync::Arc<MockState>,
    out: PathBuf,
    home: &Path,
    work: &Path,
) -> Result<Score, String> {
    seed_harness(home, work, harness, &addr, &scenario.settings)?;
    seed_files(work, &scenario.files)?;
    let phases = materialize_phases(&scenario);
    let origin = Instant::now();
    let mut tail: Option<Tail> = None;
    let mut samples = Vec::new();
    let mut session_id = None;
    for (index, phase) in phases.iter().enumerate() {
        let mut command = resolve_command(&phase.command, harness, session_id.as_deref())?;
        if let Some(prompt) = &phase.prompt {
            command.push(prompt.clone());
        }
        let events = std::fs::File::create(out.join(format!("{}-events-{index}.jsonl", harness.id())))
            .map_err(|e| format!("create {} events: {e}", harness.id()))?;
        let stderr = std::fs::File::create(out.join(format!("{}-stderr-{index}.log", harness.id())))
            .map_err(|e| format!("create {} stderr: {e}", harness.id()))?;
        let mut builder = std::process::Command::new(&command[0]);
        builder.args(&command[1..]);
        builder.current_dir(work);
        builder.env("HOME", home);
        match harness {
            Harness::Pi => {
                builder.env("PI_CODING_AGENT_DIR", home.join(".pi").join("agent"));
                builder.env("PI_CODING_AGENT_SESSION_DIR", home.join(".pi").join("agent").join("sessions"));
                builder.env("PI_OFFLINE", "1");
                builder.env("PI_SKIP_VERSION_CHECK", "1");
                builder.env("PI_TELEMETRY", "0");
            }
            Harness::Gemini => {
                builder.env("GEMINI_CLI_HOME", home);
                builder.env("GEMINI_API_KEY", DUMMY_KEY);
                builder.env("GOOGLE_GEMINI_BASE_URL", format!("http://{addr}"));
                builder.env("CI", "true");
                builder.env("GEMINI_CLI_TRUST_WORKSPACE", "true");
                builder.env("GEMINI_CLI_NO_RELAUNCH", "true");
                builder.env("GEMINI_TELEMETRY_ENABLED", "false");
            }
            Harness::Copilot => {
                builder.env("COPILOT_HOME", home.join(".copilot"));
                builder.env("COPILOT_OFFLINE", "true");
                builder.env("COPILOT_PROVIDER_BASE_URL", format!("http://{addr}/v1"));
                builder.env("COPILOT_PROVIDER_TYPE", "openai");
                builder.env("COPILOT_PROVIDER_API_KEY", DUMMY_KEY);
                builder.env("COPILOT_PROVIDER_MODEL_ID", "test-model");
                builder.env("COPILOT_MODEL", "test-model");
                builder.env("CI", "true");
                builder.env("NO_COLOR", "1");
            }
            _ => {}
        }
        builder.env("TERM", "xterm-256color");
        for (key, value) in &phase.env {
            builder.env(key, value);
        }
        builder.stdout(events).stderr(stderr);
        let mut child = builder.spawn().map_err(|e| format!("spawn {}: {e}", command[0]))?;
        let start = Instant::now();
        let run_for = Duration::from_millis(phase.run_for_ms);
        let mut last_sample = Instant::now() - SAMPLE_EVERY;
        loop {
            let exited = child.try_wait().map_err(|e| format!("wait {}: {e}", command[0]))?.is_some();
            if last_sample.elapsed() >= SAMPLE_EVERY {
                let library = library_state(&mut tail, harness, home, Timestamp::from(SystemTime::now()));
                samples.push(Sample {
                    elapsed_ms: origin.elapsed().as_millis() as u64,
                    screen: ScreenState::Unknown.id(),
                    background_agents: false,
                    library: library.as_ref().map(score::library_point),
                });
                last_sample = Instant::now();
            }
            if exited || start.elapsed() >= run_for {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if child.try_wait().map_err(|e| e.to_string())?.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_millis(250));
        let now = Timestamp::from(SystemTime::now());
        let library = library_state(&mut tail, harness, home, now);
        samples.push(Sample {
            elapsed_ms: origin.elapsed().as_millis() as u64,
            screen: ScreenState::Unknown.id(),
            background_agents: false,
            library: library.as_ref().map(score::library_point),
        });
        if let Some(id) = read_session_id(home, harness) {
            session_id = Some(id);
        }
    }

    let transcript = tail.as_ref().map(|tail| tail.path.clone());
    let dumps = collect_transcript_dumps(home, harness);
    copy_transcripts(&dumps, &out)?;
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
        transcripts: &dumps,
        helpers: &[],
        registry: &[],
        registry_fields: &[],
        registry_files_after_exit: None,
        expect_registry: None,
        expect_observations: scenario.expect_observations.as_ref(),
    });
    let timeline = out.join("timeline.jsonl");
    let mut timeline_text = String::new();
    for sample in &samples {
        timeline_text.push_str(&serde_json::to_string(sample).expect("sample serializes"));
        timeline_text.push('\n');
    }
    std::fs::write(&timeline, timeline_text).map_err(|e| format!("write {}: {e}", timeline.display()))?;
    std::fs::write(out.join("screen.txt"), "json mode: no PTY screen\n").map_err(|e| e.to_string())?;
    if let Some(path) = &transcript
        && let Ok(bytes) = std::fs::read(path)
    {
        std::fs::write(out.join("transcript.jsonl"), bytes).map_err(|e| e.to_string())?;
    }
    Ok(score)
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
    seed_harness(home, work, harness, &addr, &scenario.settings)?;
    seed_files(work, &scenario.files)?;

    let phases = materialize_phases(&scenario);
    let origin = Instant::now();
    let mut tail: Option<Tail> = None;
    let mut samples = Vec::new();
    let mut registry = Vec::new();
    let mut registry_fields = BTreeSet::new();
    let mut final_screen = String::new();
    let mut session_id = None;
    for phase in &phases {
        let command = resolve_command(&phase.command, harness, session_id.as_deref())?;
        drive_phase(DrivePhase {
            phase,
            command: &command,
            harness,
            addr: &addr,
            home,
            work,
            origin,
            tail: &mut tail,
            samples: &mut samples,
            registry: &mut registry,
            registry_fields: &mut registry_fields,
            final_screen: &mut final_screen,
        })?;
        std::thread::sleep(Duration::from_millis(200));
        if let Some(id) = read_session_id(home, harness) {
            session_id = Some(id);
        }
    }

    std::thread::sleep(Duration::from_millis(300));
    let files_after = read_registry(home).len();
    let saw_registry = registry.iter().any(|sample| sample.status.is_some());
    let registry_files_after_exit = if saw_registry || files_after > 0 { Some(files_after) } else { None };
    kill_registry_pids(home);

    let transcript = tail.as_ref().map(|tail| tail.path.clone());
    let dumps = collect_transcript_dumps(home, harness);
    copy_transcripts(&dumps, &out)?;
    let helpers = collect_helpers(home);
    copy_helpers(&helpers, &out)?;
    let expectations: Vec<score::Expectation> =
        scenario.expected_activity.iter().map(score::ExpectedOne::expectation).collect();
    let registry_fields: Vec<String> = registry_fields.into_iter().collect();
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
        transcripts: &dumps,
        helpers: &helpers,
        registry: &registry,
        registry_fields: &registry_fields,
        registry_files_after_exit,
        expect_registry: scenario.expect_registry.as_ref(),
        expect_observations: scenario.expect_observations.as_ref(),
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

/// The first deadline for a phase: startup allowance for prompt-driven scenarios, the
/// scenario window otherwise. A prompt that appears resets the deadline to `run_for` later.
fn initial_deadline(start: Instant, run_for: Duration, has_prompt: bool) -> Instant {
    start + if has_prompt { STARTUP_TIMEOUT } else { run_for }
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

fn materialize_phases(scenario: &DriverScenario) -> Vec<PhaseSpec> {
    if scenario.phases.is_empty() {
        return vec![PhaseSpec {
            command: scenario.command.clone(),
            prompt: scenario.prompt.clone(),
            keys: scenario.keys.clone(),
            run_for_ms: scenario.run_for_ms,
            exit: scenario.exit,
            env: scenario.env.clone(),
        }];
    }
    scenario
        .phases
        .iter()
        .map(|phase| {
            let mut env = scenario.env.clone();
            env.extend(phase.env.clone());
            PhaseSpec {
                command: if phase.command.is_empty() { scenario.command.clone() } else { phase.command.clone() },
                prompt: phase.prompt.clone(),
                keys: phase.keys.clone(),
                run_for_ms: phase.run_for_ms.unwrap_or(scenario.run_for_ms),
                exit: phase.exit.unwrap_or(scenario.exit),
                env,
            }
        })
        .collect()
}

fn resolve_command(command: &[String], harness: Harness, session_id: Option<&str>) -> Result<Vec<String>, String> {
    let command = if command.is_empty() { default_command(harness) } else { command.to_vec() };
    let mut resolved = Vec::with_capacity(command.len());
    for arg in command {
        if arg.contains("{{session_id}}") {
            let id = session_id.ok_or("{{session_id}} needs a session from the previous phase")?;
            resolved.push(arg.replace("{{session_id}}", id));
        } else {
            resolved.push(arg);
        }
    }
    if resolved.is_empty() {
        return Err("scenario command is empty".into());
    }
    Ok(resolved)
}

struct DrivePhase<'a> {
    phase: &'a PhaseSpec,
    command: &'a [String],
    harness: Harness,
    addr: &'a str,
    home: &'a Path,
    work: &'a Path,
    origin: Instant,
    tail: &'a mut Option<Tail>,
    samples: &'a mut Vec<Sample>,
    registry: &'a mut Vec<score::RegistrySample>,
    registry_fields: &'a mut BTreeSet<String>,
    final_screen: &'a mut String,
}

fn drive_phase(drive: DrivePhase<'_>) -> Result<(), String> {
    let DrivePhase {
        phase,
        command,
        harness,
        addr,
        home,
        work,
        origin,
        tail,
        samples,
        registry,
        registry_fields,
        final_screen,
    } = drive;
    let mut builder = CommandBuilder::new(&command[0]);
    for arg in &command[1..] {
        builder.arg(arg);
    }
    builder.cwd(work);
    builder.env("HOME", home);
    match harness {
        Harness::ClaudeCode => {
            builder.env("CLAUDE_CONFIG_DIR", home.join(".claude"));
            builder.env("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            builder.env("ANTHROPIC_API_KEY", DUMMY_KEY);
            builder.env("DISABLE_AUTOUPDATER", "1");
            builder.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
            builder.env("DISABLE_TELEMETRY", "1");
            builder.env("DISABLE_ERROR_REPORTING", "1");
            builder.env("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1");
        }
        Harness::Codex => {
            builder.env("CODEX_HOME", home.join(".codex"));
            builder.env("MOCK_API_KEY", DUMMY_KEY);
        }
        Harness::Pi => {
            builder.env("PI_CODING_AGENT_DIR", home.join(".pi").join("agent"));
            builder.env("PI_CODING_AGENT_SESSION_DIR", home.join(".pi").join("agent").join("sessions"));
            builder.env("PI_OFFLINE", "1");
            builder.env("PI_SKIP_VERSION_CHECK", "1");
            builder.env("PI_TELEMETRY", "0");
        }
        Harness::OpenCode => {
            builder.env("XDG_CONFIG_HOME", home.join(".config"));
            builder.env("XDG_DATA_HOME", home.join(".local/share"));
            builder.env("XDG_STATE_HOME", home.join(".local/state"));
            builder.env("XDG_CACHE_HOME", home.join(".cache"));
            builder.env("OPENCODE_TEST_HOME", home);
            builder.env("OPENCODE_CONFIG_CONTENT", opencode_config(addr));
            builder.env("OPENCODE_DISABLE_PROJECT_CONFIG", "1");
            builder.env("OPENCODE_PURE", "1");
            builder.env("OPENCODE_DISABLE_AUTOUPDATE", "1");
            builder.env("OPENCODE_DISABLE_AUTOCOMPACT", "1");
            builder.env("OPENCODE_DISABLE_MODELS_FETCH", "1");
            builder.env("OPENCODE_AUTH_CONTENT", "{}");
        }
        _ => {}
    }
    builder.env("TERM", "xterm-256color");
    builder.env("COLUMNS", COLS.to_string());
    builder.env("LINES", ROWS.to_string());
    for (key, value) in &phase.env {
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
    let mut keys_sent = vec![false; phase.keys.len()];
    let mut keys_clock = KeyClock::new(ScreenState::Starting);
    let mut prompt_sent = false;
    let mut screen_state = ScreenState::Starting;
    let mut last_sample = Instant::now() - SAMPLE_EVERY;
    let phase_start = Instant::now();
    keys_clock.reset(ScreenState::Starting, origin.elapsed().as_millis() as u64);
    let run_for = Duration::from_millis(phase.run_for_ms);
    // A newer CLI can spend longer on startup than an old one did. Keep that out of the
    // scenario window so a slow launch is not scored as a transcript regression.
    let mut deadline = initial_deadline(phase_start, run_for, phase.prompt.is_some());

    while Instant::now() < deadline {
        while let Ok(bytes) = rx.try_recv() {
            parser.process(&bytes);
        }
        let screen = parser.screen().contents();
        let reading = screen::read_screen(harness, &screen);
        if reading.state != screen_state {
            screen_state = reading.state;
        }
        *final_screen = screen;
        let now_ms = origin.elapsed().as_millis() as u64;
        let (key_state, occurrence, key_elapsed_ms) = keys_clock.tick(reading.state, now_ms);
        let prompt_pending = phase.prompt.is_some() && !prompt_sent;
        if !prompt_pending {
            for index in due_keys(&phase.keys, &keys_sent, key_state, occurrence, Duration::from_millis(key_elapsed_ms))
            {
                write_keys(&mut writer, phase.keys[index].send.as_bytes())?;
                keys_sent[index] = true;
                eprintln!(
                    "driver: sent {:?} on {} occurrence {occurrence}",
                    phase.keys[index].send,
                    phase.keys[index].on.id()
                );
            }
        }
        if screen_state == ScreenState::Idle
            && !prompt_sent
            && let Some(prompt) = &phase.prompt
        {
            write_keys(&mut writer, prompt.as_bytes())?;
            write_keys(&mut writer, b"\r")?;
            prompt_sent = true;
            keys_clock.reset(ScreenState::Idle, now_ms);
            deadline = Instant::now() + run_for;
            eprintln!("driver: prompt sent");
        }
        if last_sample.elapsed() >= SAMPLE_EVERY {
            sample_once(
                harness,
                home,
                &mut SampleSink { tail, samples, registry, registry_fields },
                screen_state,
                reading.background_agents,
                now_ms,
            );
            last_sample = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if phase.exit == ExitMode::Graceful {
        let _ = write_keys(&mut writer, b"/exit\r");
        if !wait_for_child(&mut child, Duration::from_secs(6), &rx, &mut parser) {
            let _ = write_keys(&mut writer, b"\x04");
            let _ = wait_for_child(&mut child, Duration::from_secs(3), &rx, &mut parser);
        }
        let now_ms = origin.elapsed().as_millis() as u64;
        let reading = screen::read_screen(harness, &parser.screen().contents());
        sample_once(
            harness,
            home,
            &mut SampleSink { tail, samples, registry, registry_fields },
            reading.state,
            reading.background_agents,
            now_ms,
        );
    }
    let pid = child.process_id();
    let _ = child.kill();
    stop_pid(pid);
    Ok(())
}

fn wait_for_child(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    limit: Duration,
    rx: &mpsc::Receiver<Vec<u8>>,
    parser: &mut vt100::Parser,
) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        while let Ok(bytes) = rx.try_recv() {
            parser.process(&bytes);
        }
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

fn stop_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    if pid == std::process::id() {
        return;
    }
    if let Some(group) = process_group(pid)
        && group != 0
        && process_group(std::process::id()) != Some(group)
    {
        let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{group}")]).status();
    }
    let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
}

fn process_group(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.split_once(')')?.1;
    rest.split_whitespace().nth(2)?.parse().ok()
}

struct SampleSink<'a> {
    tail: &'a mut Option<Tail>,
    samples: &'a mut Vec<Sample>,
    registry: &'a mut Vec<score::RegistrySample>,
    registry_fields: &'a mut BTreeSet<String>,
}

fn sample_once(
    harness: Harness,
    home: &Path,
    sink: &mut SampleSink<'_>,
    screen: ScreenState,
    background_agents: bool,
    now_ms: u64,
) {
    let library = library_state(sink.tail, harness, home, Timestamp::from(SystemTime::now()));
    sink.samples.push(Sample {
        elapsed_ms: now_ms,
        screen: screen.id(),
        background_agents,
        library: library.as_ref().map(score::library_point),
    });
    for snap in read_registry(home) {
        sink.registry_fields.extend(snap.keys.iter().cloned());
        sink.registry.push(score::RegistrySample {
            elapsed_ms: now_ms,
            screen: screen.id().to_string(),
            pid: snap.pid,
            session_id: snap.session_id,
            status: snap.status,
            waiting_for: snap.waiting_for,
        });
    }
}

struct RegistrySnap {
    pid: Option<u64>,
    session_id: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    keys: BTreeSet<String>,
}

fn read_registry(home: &Path) -> Vec<RegistrySnap> {
    let dir = home.join(".claude").join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut snaps = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let serde_json::Value::Object(map) = value else { continue };
        snaps.push(RegistrySnap {
            pid: map.get("pid").and_then(serde_json::Value::as_u64),
            session_id: map.get("sessionId").and_then(serde_json::Value::as_str).map(str::to_string),
            status: map
                .get("status")
                .and_then(serde_json::Value::as_str)
                .filter(|status| !status.is_empty())
                .map(str::to_string),
            waiting_for: map
                .get("waitingFor")
                .and_then(serde_json::Value::as_str)
                .filter(|waiting| !waiting.is_empty())
                .map(str::to_string),
            keys: map.keys().cloned().collect(),
        });
    }
    snaps.sort_by_key(|snap| snap.pid.unwrap_or(0));
    snaps
}

fn kill_registry_pids(home: &Path) {
    for snap in read_registry(home) {
        let Some(pid) = snap.pid else { continue };
        let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
    }
}

fn read_session_id(home: &Path, harness: Harness) -> Option<String> {
    if harness == Harness::OpenCode {
        return read_opencode_state(home)?.latest().map(|session| session.id.clone());
    }
    let path = find_transcript(home, harness)?;
    let text = std::fs::read_to_string(path).ok()?;
    session_id_in(&text)
}

fn opencode_db(home: &Path) -> PathBuf {
    home.join(".local/share/opencode/opencode.db")
}

fn read_opencode_state(home: &Path) -> Option<agent_tail::OpenCodeState> {
    read_opencode_db(&opencode_db(home)).ok()
}

fn session_id_in(jsonl: &str) -> Option<String> {
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(id) = value.get("sessionId").and_then(serde_json::Value::as_str).filter(|id| !id.is_empty()) {
            return Some(id.to_string());
        }
        if let Some(id) = value
            .get("data")
            .and_then(|data| data.get("sessionId"))
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("session")
            && let Some(id) = value.get("id").and_then(serde_json::Value::as_str).filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
        let Some(payload) = value.get("payload") else { continue };
        if let Some(id) = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }
    }
    None
}

fn collect_transcript_dumps(home: &Path, harness: Harness) -> Vec<score::TranscriptDump> {
    if harness == Harness::OpenCode {
        let Some(state) = read_opencode_state(home) else { return Vec::new() };
        return state
            .sessions
            .into_iter()
            .map(|session| score::TranscriptDump { name: format!("{}.jsonl", session.id), jsonl: session.to_jsonl() })
            .collect();
    }
    let mut found = Vec::new();
    walk_transcripts(&transcript_root(home, harness), 0, harness, &mut found);
    if harness == Harness::Codex {
        let index = home.join(".codex").join("session_index.jsonl");
        if index.is_file() {
            found.push((index.display().to_string(), index));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
        .into_iter()
        .filter_map(|(_, path)| {
            let name =
                if harness == Harness::Codex && path.file_name().is_some_and(|name| name == "session_index.jsonl") {
                    "session_index.jsonl".to_string()
                } else {
                    path.file_name()?.to_str()?.to_string()
                };
            let jsonl = std::fs::read_to_string(&path).ok()?;
            Some(score::TranscriptDump { name, jsonl })
        })
        .collect()
}

fn walk_transcripts(dir: &Path, depth: usize, harness: Harness, found: &mut Vec<(String, PathBuf)>) {
    if depth > transcript_depth(harness) {
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
            walk_transcripts(&path, depth + 1, harness, found);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            found.push((path.display().to_string(), path));
        }
    }
}

fn copy_transcripts(dumps: &[score::TranscriptDump], out: &Path) -> Result<(), String> {
    if dumps.is_empty() {
        return Ok(());
    }
    let dir = out.join("transcripts");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for dump in dumps {
        if dump.name.contains('/') || dump.name.contains('\\') || dump.name == "." || dump.name == ".." {
            continue;
        }
        let path = dir.join(&dump.name);
        std::fs::write(&path, &dump.jsonl).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn default_command(harness: Harness) -> Vec<String> {
    match harness {
        Harness::ClaudeCode => vec!["claude".into(), "--bare".into(), "--model".into(), "claude-sonnet-4-5".into()],
        Harness::Codex => vec!["codex".into(), "--no-daemon".into()],
        Harness::Pi => vec![
            "pi".into(),
            "--mode".into(),
            "json".into(),
            "--model".into(),
            "mock/mock-model".into(),
            "--no-extensions".into(),
            "--no-skills".into(),
            "--no-prompt-templates".into(),
            "--no-themes".into(),
            "--no-context-files".into(),
            "--no-mcp".into(),
        ],
        Harness::OpenCode => vec![
            "opencode".into(),
            "run".into(),
            "--format".into(),
            "json".into(),
            "--auto".into(),
            "-m".into(),
            "test/test-model".into(),
        ],
        Harness::Gemini => vec![
            "gemini".into(),
            "--output-format".into(),
            "json".into(),
            "--approval-mode".into(),
            "yolo".into(),
            "--skip-trust".into(),
            "-p".into(),
        ],
        Harness::Copilot => vec![
            "copilot".into(),
            "--output-format".into(),
            "json".into(),
            "--allow-all-tools".into(),
            "--trust-folder".into(),
            "-p".into(),
        ],
        _ => vec![],
    }
}

/// OpenCode's isolated provider config. `--auto` approves tools that are not explicitly
/// denied; bash/edit are allowed here too so the tool scenario never blocks on a prompt in a
/// non-interactive `run`.
fn opencode_config(addr: &str) -> String {
    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "permission": {"bash": "allow", "edit": "allow"},
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test Model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": {"context": 100_000, "output": 10_000},
                        "cost": {"input": 0, "output": 0},
                        "options": {}
                    }
                },
                "options": {"apiKey": "test-key", "baseURL": format!("http://{addr}/v1")}
            }
        }
    })
    .to_string()
}

fn seed_harness(
    home: &Path,
    work: &Path,
    harness: Harness,
    mock_addr: &str,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    match harness {
        Harness::ClaudeCode => seed_claude(home, work, settings),
        Harness::Codex => seed_codex(home, mock_addr),
        Harness::Pi => seed_pi(home, mock_addr, settings),
        Harness::OpenCode => Ok(()),
        Harness::Gemini => seed_gemini(home, settings),
        Harness::Copilot => Ok(()),
        _ => Err(format!("{} harness is not wired up yet", harness.name())),
    }
}

fn seed_pi(home: &Path, mock_addr: &str, extra: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let config_dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    let models = serde_json::json!({
        "providers": {
            "mock": {
                "name": "Mock",
                "baseUrl": format!("http://{mock_addr}"),
                "api": "anthropic-messages",
                "apiKey": DUMMY_KEY,
                "models": [{
                    "id": "mock-model",
                    "name": "Mock Model",
                    "reasoning": false,
                    "input": ["text"],
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                    "contextWindow": 128000,
                    "maxTokens": 8192
                }]
            }
        }
    });
    std::fs::write(config_dir.join("models.json"), serde_json::to_vec_pretty(&models).expect("models serialize"))
        .map_err(|e| format!("seed Pi models.json: {e}"))?;
    let mut settings = serde_json::json!({"defaultProvider": "mock", "defaultModel": "mock-model"});
    let object = settings.as_object_mut().expect("settings object");
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    std::fs::write(config_dir.join("settings.json"), serde_json::to_vec_pretty(&settings).expect("settings serialize"))
        .map_err(|e| format!("seed Pi settings.json: {e}"))
}

fn seed_gemini(home: &Path, extra: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let config_dir = home.join(".gemini");
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    let mut settings = serde_json::json!({"security": {"auth": {"selectedType": "gemini-api-key"}}});
    let object = settings.as_object_mut().expect("settings object");
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    std::fs::write(config_dir.join("settings.json"), serde_json::to_vec_pretty(&settings).expect("settings serialize"))
        .map_err(|e| format!("seed Gemini settings.json: {e}"))
}

fn seed_codex(home: &Path, mock_addr: &str) -> Result<(), String> {
    let config_dir = home.join(".codex");
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    let config = format!(
        r#"model = "gpt-5.4"
model_provider = "mock"
approval_policy = "on-request"
sandbox_mode = "workspace-write"
check_for_update_on_startup = false

[tui]
disable_paste_burst = true
animations = false

[model_providers.mock]
name = "Mock"
base_url = "http://{mock_addr}/v1"
env_key = "MOCK_API_KEY"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
    );
    std::fs::write(config_dir.join("config.toml"), config).map_err(|e| format!("seed Codex config.toml: {e}"))
}

fn seed_claude(home: &Path, work: &Path, extra: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
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
    let mut settings = serde_json::json!({ "skipDangerousModePermissionPrompt": true });
    let object = settings.as_object_mut().expect("settings object");
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    std::fs::write(config_dir.join("settings.json"), serde_json::to_vec_pretty(&settings).expect("settings serialize"))
        .map_err(|e| format!("seed settings.json: {e}"))
}

fn library_state(tail: &mut Option<Tail>, harness: Harness, home: &Path, now: Timestamp) -> Option<SessionState> {
    if harness == Harness::OpenCode {
        let state = read_opencode_state(home)?;
        let session = state.latest()?;
        let path = opencode_db(home).join(&session.id);
        let mut reducer = SessionReducer::new(Harness::OpenCode, ReducerOptions::default());
        reducer.set_path(&path);
        if let Some(at) = session.updated_at {
            reducer.transcript_written(at);
        }
        session.feed(&mut reducer);
        return Some(reducer.state(now));
    }
    let path = find_transcript(home, harness)?;
    let metadata = std::fs::metadata(&path).ok()?;
    let len = metadata.len();
    let mtime = metadata.modified().map(Timestamp::from).unwrap_or(now);
    let replaced = tail.as_ref().is_some_and(|tail| tail.path != path || len < tail.offset);
    if tail.is_none() || replaced {
        let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
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
            let mut reducer = SessionReducer::new(harness, ReducerOptions::default());
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

fn transcript_root(home: &Path, harness: Harness) -> PathBuf {
    match harness {
        Harness::ClaudeCode => home.join(".claude").join("projects"),
        Harness::Codex => home.join(".codex").join("sessions"),
        Harness::Pi => home.join(".pi").join("agent").join("sessions"),
        Harness::OpenCode => home.join(".local/share/opencode"),
        Harness::Gemini => home.join(".gemini").join("tmp"),
        Harness::Copilot => home.join(".copilot").join("session-state"),
        _ => home.join(".unwired"),
    }
}

fn transcript_depth(harness: Harness) -> usize {
    match harness {
        Harness::Codex => 6,
        _ => 3,
    }
}

fn find_transcript(home: &Path, harness: Harness) -> Option<PathBuf> {
    if harness == Harness::OpenCode {
        let path = opencode_db(home);
        return path.is_file().then_some(path);
    }
    let root = transcript_root(home, harness);
    let mut best: Option<(SystemTime, PathBuf)> = None;
    walk(&root, 0, transcript_depth(harness), &mut best);
    best.map(|(_, path)| path)
}

fn walk(dir: &Path, depth: usize, max_depth: usize, best: &mut Option<(SystemTime, PathBuf)>) {
    if depth > max_depth {
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
            walk(&path, depth + 1, max_depth, best);
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
    fn gemini_and_copilot_scenarios_parse() {
        for (name, harness) in [
            ("gemini_plain_qa.json", "gemini"),
            ("gemini_tool.json", "gemini"),
            ("copilot_plain_qa.json", "copilot"),
            ("copilot_tool.json", "copilot"),
            ("copilot_permission_denied.json", "copilot"),
        ] {
            let scenario = scenario(name);
            assert_eq!(scenario.harness, harness, "{name}");
            assert!(!scenario.expected_activity.is_empty(), "{name}");
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&scenario.mock);
            let text =
                std::fs::read_to_string(&mock_path).unwrap_or_else(|e| panic!("read {}: {e}", mock_path.display()));
            mock_server::scenario::Scenario::parse(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", mock_path.display()));
        }
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
        assert_eq!(thinking.keys[0].after_ms, 8_000);
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
            if name == "claude_interrupt_thinking.json" {
                assert!(matches!(
                    mock.steps[0].reply.fault,
                    Some(mock_server::scenario::Fault::PauseAfterFirstBlock { delay_ms: 20000 })
                ));
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

    #[test]
    fn process_group_reads_this_process_and_skips_a_missing_pid() {
        let group = process_group(std::process::id()).unwrap();
        assert!(group > 0);
        assert_eq!(process_group(u32::MAX), None);
    }

    #[test]
    fn resolve_command_fills_the_session_and_the_default() {
        let command = resolve_command(&[], Harness::ClaudeCode, None).unwrap();
        assert_eq!(command, vec!["claude".to_string(), "--bare".into(), "--model".into(), "claude-sonnet-4-5".into()]);
        let resumed = resolve_command(
            &["claude".into(), "--resume".into(), "{{session_id}}".into()],
            Harness::ClaudeCode,
            Some("abc"),
        )
        .unwrap();
        assert_eq!(resumed, vec!["claude".to_string(), "--resume".into(), "abc".into()]);
        let err = resolve_command(&["{{session_id}}".into()], Harness::ClaudeCode, None).unwrap_err();
        assert!(err.contains("previous phase"), "{err}");
    }

    #[test]
    fn partial_registry_json_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.json"), r#"{"pid":1,"status":"busy","sessionId":"s","waitingFor":""}"#).unwrap();
        std::fs::write(dir.join("2.json"), "{not json").unwrap();
        std::fs::write(dir.join("note.txt"), r#"{"status":"idle"}"#).unwrap();
        let snaps = read_registry(home.path());
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].status.as_deref(), Some("busy"));
        assert_eq!(snaps[0].pid, Some(1));
        assert!(snaps[0].waiting_for.is_none());
        assert!(snaps[0].keys.contains("pid"));
    }

    #[test]
    fn session_id_comes_from_the_first_non_empty_record() {
        let jsonl = "\nnot json\n{\"type\":\"user\"}\n{\"sessionId\":\"abc\",\"type\":\"user\"}\n";
        assert_eq!(session_id_in(jsonl).as_deref(), Some("abc"));
        assert_eq!(session_id_in(r#"{"sessionId":""}"#), None);
    }

    #[test]
    fn run_for_starts_after_the_prompt_with_a_startup_allowance() {
        let start = Instant::now();
        let run_for = Duration::from_millis(12_000);
        assert_eq!(initial_deadline(start, run_for, true), start + STARTUP_TIMEOUT);
        assert_eq!(initial_deadline(start, run_for, false), start + run_for);
    }

    #[test]
    fn materialize_phases_keeps_a_single_phase_scenario() {
        let plain = scenario("claude_plain_qa.json");
        let phases = materialize_phases(&plain);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].prompt.as_deref(), Some("say hello"));
        assert_eq!(phases[0].run_for_ms, 10_000);
        assert_eq!(phases[0].exit, ExitMode::Kill);
        assert!(phases[0].command.is_empty());
    }

    #[test]
    fn the_continuity_scenarios_parse() {
        let manual = scenario("claude_compact_manual.json");
        assert_eq!(manual.keys.len(), 2);
        assert_eq!(manual.keys[0].send, "/compact\r");
        assert_eq!(manual.keys[0].occurrence, 1);
        assert_eq!(manual.keys[1].occurrence, 1);
        assert_eq!(manual.keys[1].after_ms, 4_500);
        assert_eq!(manual.expect_observations.as_ref().unwrap().compaction_triggers, vec!["manual".to_string()]);
        assert_eq!(manual.expect_observations.as_ref().unwrap().compaction_same_file, Some(true));
        assert_eq!(manual.expected_activity[0].expectation().activity, "done");

        let auto = scenario("claude_compact_auto.json");
        assert_eq!(auto.env.get("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE").map(String::as_str), Some("1"));
        assert_eq!(auto.env.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").map(String::as_str), Some("100000"));
        assert!(auto.no_approval);
        assert!(auto.command.iter().any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(auto.command.iter().all(|arg| arg != "--bare"));
        assert_eq!(auto.settings.get("bashOutputMaxChars").and_then(serde_json::Value::as_u64), Some(120_000));
        assert!(auto.keys.is_empty());
        assert_eq!(auto.expect_observations.as_ref().unwrap().compaction_triggers, vec!["auto".to_string()]);

        let mode = scenario("claude_mode_model.json");
        assert!(mode.command.iter().all(|arg| arg != "--bare"));
        assert_eq!(mode.keys[0].send, "\u{1b}[Z");
        assert_eq!(mode.keys[1].send, "/model haiku\r");
        assert_eq!(mode.keys[2].occurrence, 1);
        let expect = mode.expect_observations.as_ref().unwrap();
        assert_eq!(expect.model_switch, Some(true));
        assert_eq!(expect.permission_modes, vec!["default".to_string(), "acceptEdits".to_string()]);

        let continued = scenario("claude_continue.json");
        assert_eq!(continued.exit, ExitMode::Graceful);
        assert_eq!(continued.phases.len(), 2);
        assert!(continued.phases[1].command.iter().any(|arg| arg == "--continue"));
        assert_eq!(continued.expect_observations.as_ref().unwrap().continued_in, Some(false));
        assert_eq!(continued.expect_observations.as_ref().unwrap().same_file_resume, Some(true));
        let phases = materialize_phases(&continued);
        assert_eq!(phases[0].exit, ExitMode::Graceful);
        assert!(phases[0].command.is_empty());
        assert_eq!(phases[1].prompt.as_deref(), Some("what word did I ask you to remember?"));

        let resume = scenario("claude_resume.json");
        assert!(resume.phases[1].command.iter().any(|arg| arg.contains("{{session_id}}")));
        let resolved = resolve_command(&resume.phases[1].command, Harness::ClaudeCode, Some("sess-1")).unwrap();
        assert!(resolved.iter().any(|arg| arg == "sess-1"));

        let fork = scenario("claude_continued_in.json");
        assert_eq!(fork.keys[0].send, "/background\r");
        assert_eq!(fork.exit, ExitMode::Kill);
        assert_eq!(fork.expect_observations.as_ref().unwrap().continued_in, Some(true));
        assert_eq!(fork.expected_activity[0].expectation().activity, "done");
        assert_eq!(fork.expected_activity[0].expectation().when, score::ExpectWhen::Ever);
        assert_eq!(fork.expected_activity[1].expectation().activity, "unknown");

        let long = scenario("claude_long_output.json");
        assert!(long.no_approval);
        assert_eq!(long.expect_observations.as_ref().unwrap().lean_agrees, Some(true));
        assert_eq!(long.expect_observations.as_ref().unwrap().min_tool_result_bytes, Some(20_000));

        let text = scenario("claude_non_ascii.json");
        assert!(text.prompt.as_ref().unwrap().contains('🎉'));
        assert!(text.files.keys().any(|path| path.contains('📝')));
        assert_eq!(text.expect_observations.as_ref().unwrap().non_ascii_path, Some(true));

        let registry = scenario("claude_sessions_registry.json");
        assert_eq!(registry.exit, ExitMode::Graceful);
        assert_eq!(registry.min_screen_ms.get("permission_prompt"), Some(&4_000));
        let registry_expect = registry.expect_registry.as_ref().unwrap();
        assert_eq!(registry_expect.statuses, vec!["busy".to_string(), "idle".to_string(), "waiting".to_string()]);
        assert_eq!(registry_expect.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(registry_expect.waiting_during_screen.as_deref(), Some("permission_prompt"));
        assert_eq!(registry_expect.removed_on_exit, Some(true));
        assert!(registry.expected_activity[0].expectation().screen.is_none());

        for name in [
            "claude_compact_manual.json",
            "claude_compact_auto.json",
            "claude_mode_model.json",
            "claude_continue.json",
            "claude_resume.json",
            "claude_continued_in.json",
            "claude_long_output.json",
            "claude_non_ascii.json",
            "claude_sessions_registry.json",
        ] {
            let driver = scenario(name);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&driver.mock);
            let text =
                std::fs::read_to_string(&mock_path).unwrap_or_else(|e| panic!("read {}: {e}", mock_path.display()));
            let text = substitute_workspace(&text, Path::new("/tmp/a3 work\""));
            let mock = mock_server::scenario::Scenario::parse(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", mock_path.display()));
            if name == "claude_continued_in.json" {
                assert_eq!(mock.steps.len(), 4);
            }
            if name == "claude_non_ascii.json" {
                assert_eq!(mock.title.as_deref(), Some("絵文字タイトル 🎉"));
                let path = mock.steps.iter().find_map(|step| match &step.reply.content[0] {
                    mock_server::scenario::Block::ToolUse { input, .. } => {
                        input.get("file_path").and_then(|value| value.as_str()).map(str::to_string)
                    }
                    _ => None,
                });
                assert_eq!(path.as_deref(), Some("/tmp/a3 work\"/notes/café-📝.txt"));
            }
            if name == "claude_compact_auto.json" {
                assert!(mock.steps.iter().any(|step| step.reply.usage.input_tokens == 20_000));
                assert_eq!(mock.steps.len(), 6);
            }
        }
    }

    #[test]
    fn the_codex_scenarios_parse_and_point_at_valid_mocks() {
        for name in [
            "codex_plain_qa.json",
            "codex_token_count.json",
            "codex_approval_pending.json",
            "codex_approval_never.json",
            "codex_request_user_input.json",
            "codex_parallel_tool_calls.json",
            "codex_turn_aborted.json",
            "codex_task_error.json",
            "codex_resume.json",
        ] {
            let driver = scenario(name);
            assert_eq!(driver.harness, "codex", "{name}");
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&driver.mock);
            let text =
                std::fs::read_to_string(&mock_path).unwrap_or_else(|e| panic!("read {}: {e}", mock_path.display()));
            let text = substitute_workspace(&text, Path::new("/tmp/codex work\""));
            mock_server::scenario::Scenario::parse(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", mock_path.display()));
        }
        let pending = scenario("codex_approval_pending.json");
        assert!(!pending.no_approval);
        assert_eq!(pending.expected_activity[0].expectation().activity, "awaiting_approval");
        assert_eq!(pending.expected_activity[0].expectation().certainty.as_deref(), Some("inferred"));
        let never = scenario("codex_approval_never.json");
        assert!(never.no_approval);
        assert!(never.command.iter().any(|arg| arg == "--no-daemon"));
        assert!(never.command.iter().any(|arg| arg == "never"));
        assert!(never.command.windows(2).any(|args| args == ["-s", "danger-full-access"]));
        let tokens = scenario("codex_token_count.json");
        assert_eq!(tokens.keys[0].on, ScreenState::Idle);
        assert_eq!(tokens.keys[0].occurrence, 0, "fast turns may not leave idle long enough for occurrence 1");
        let ask = scenario("codex_request_user_input.json");
        assert_eq!(ask.prompt.as_deref(), Some("/plan"));
        assert_eq!(ask.keys[1].on, ScreenState::AskUserQuestion);
        let parallel = scenario("codex_parallel_tool_calls.json");
        assert_eq!(parallel.expect_observations.as_ref().unwrap().codex_min_pending_function_calls, Some(3));
        let resume = scenario("codex_resume.json");
        assert!(resume.phases[1].command.iter().any(|arg| arg.contains("{{session_id}}")));
    }

    #[test]
    fn the_pi_scenarios_parse_and_point_at_valid_mocks() {
        for name in ["pi_plain_qa.json", "pi_tool_call.json", "pi_api_error.json"] {
            let driver = scenario(name);
            assert_eq!(driver.harness, "pi", "{name}");
            assert!(driver.no_approval, "{name}");
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&driver.mock);
            let text =
                std::fs::read_to_string(&mock_path).unwrap_or_else(|e| panic!("read {}: {e}", mock_path.display()));
            let text = substitute_workspace(&text, Path::new("/tmp/pi work\""));
            mock_server::scenario::Scenario::parse(&text)
                .unwrap_or_else(|e| panic!("parse {}: {e}", mock_path.display()));
        }
        let tool = scenario("pi_tool_call.json");
        assert_eq!(tool.prompt.as_deref(), Some("use the tool"));
        let activities: Vec<_> = tool.expected_activity.iter().map(|item| item.expectation()).collect();
        assert_eq!(activities[0].activity, "running_tool");
        assert_eq!(activities[0].certainty.as_deref(), Some("firm"));
        assert_eq!(activities[1].activity, "done");
        let error = scenario("pi_api_error.json");
        assert_eq!(error.settings["retry"]["enabled"], serde_json::json!(false));
    }

    #[test]
    fn pi_default_command_and_provider_follow_the_spike() {
        let command = resolve_command(&[], Harness::Pi, None).unwrap();
        assert_eq!(command[0], "pi");
        assert!(command.iter().any(|arg| arg == "--mode"));
        assert!(command.iter().any(|arg| arg == "json"));
        assert!(command.iter().any(|arg| arg == "mock/mock-model"));
        let home = tempfile::tempdir().unwrap();
        seed_pi(home.path(), "127.0.0.1:1234", &serde_json::Map::new()).unwrap();
        let models = std::fs::read_to_string(home.path().join(".pi/agent/models.json")).expect("Pi models.json");
        assert!(models.contains("\"baseUrl\": \"http://127.0.0.1:1234\""), "{models}");
        assert!(models.contains("anthropic-messages"), "{models}");
        assert!(!models.contains("http://127.0.0.1:1234/v1"), "Pi appends the API path itself: {models}");
        let settings = std::fs::read_to_string(home.path().join(".pi/agent/settings.json")).expect("Pi settings.json");
        assert!(settings.contains("mock-model"), "{settings}");
    }

    #[test]
    fn codex_default_command_and_home_follow_the_spike() {
        assert_eq!(
            resolve_command(&[], Harness::Codex, None).unwrap(),
            vec!["codex".to_string(), "--no-daemon".into()]
        );
        let home = tempfile::tempdir().unwrap();
        seed_codex(home.path(), "127.0.0.1:1234").unwrap();
        let config = std::fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""), "{config}");
        assert!(config.contains("disable_paste_burst = true"), "{config}");
        assert!(config.contains("request_max_retries = 0"), "{config}");
    }

    #[test]
    fn opencode_scenarios_parse_and_use_the_chat_mock() {
        for name in ["opencode_plain_qa.json", "opencode_tool.json"] {
            let driver = scenario(name);
            assert_eq!(driver.harness, "opencode", "{name}");
            assert!(driver.prompt.is_none(), "the prompt is in argv for non-interactive run");
            assert!(driver.command.iter().any(|arg| arg == "--format"));
            assert!(driver.command.iter().any(|arg| arg == "json"));
            assert!(driver.command.iter().any(|arg| arg == "--auto"));
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios").join(name);
            let mock_path = path.parent().unwrap().join(&driver.mock);
            let text = std::fs::read_to_string(&mock_path).unwrap();
            let mock = mock_server::scenario::Scenario::parse(&text).unwrap();
            assert!(!mock.steps.is_empty());
        }
        let tool = scenario("opencode_tool.json");
        assert!(tool.expected_activity.iter().any(|expect| expect.expectation().activity == "running_tool"));
        assert!(tool.expected_activity.iter().any(|expect| expect.expectation().activity == "done"));
    }

    #[test]
    fn opencode_default_command_and_config_follow_the_spike() {
        let command = resolve_command(&[], Harness::OpenCode, None).unwrap();
        assert_eq!(&command[..3], ["opencode", "run", "--format"]);
        assert!(command.iter().any(|arg| arg == "test/test-model"));
        let config: serde_json::Value = serde_json::from_str(&opencode_config("127.0.0.1:1234")).unwrap();
        assert_eq!(
            config.pointer("/provider/test/options/baseURL").and_then(|value| value.as_str()),
            Some("http://127.0.0.1:1234/v1")
        );
        assert_eq!(
            config.pointer("/provider/test/npm").and_then(|value| value.as_str()),
            Some("@ai-sdk/openai-compatible")
        );
        assert_eq!(config.pointer("/permission/bash").and_then(|value| value.as_str()), Some("allow"));
        let home = tempfile::tempdir().unwrap();
        seed_harness(home.path(), home.path(), Harness::OpenCode, "127.0.0.1:1234", &serde_json::Map::new()).unwrap();
        assert!(!opencode_db(home.path()).exists(), "the database is created by OpenCode, not seeded");
    }

    #[test]
    fn codex_rollouts_are_found_below_day_folders_and_use_session_meta() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join(".codex/sessions/2026/10/24");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-10-24T00-00-00-abc.jsonl");
        std::fs::write(&rollout, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\",\"session_id\":\"abc\"}}\n")
            .unwrap();
        assert_eq!(find_transcript(home.path(), Harness::Codex).as_deref(), Some(rollout.as_path()));
        assert_eq!(read_session_id(home.path(), Harness::Codex).as_deref(), Some("abc"));
        std::fs::write(
            home.path().join(".codex/session_index.jsonl"),
            "{\"id\":\"abc\",\"thread_name\":\"Codex title\"}\n",
        )
        .unwrap();
        let dumps = collect_transcript_dumps(home.path(), Harness::Codex);
        assert!(dumps.iter().any(|dump| dump.name == "session_index.jsonl"));
        assert!(dumps.iter().any(|dump| dump.name.starts_with("rollout-")));
    }
}
