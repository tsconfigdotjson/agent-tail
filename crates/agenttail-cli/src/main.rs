//! `agenttail`: the library on the command line, for debugging what a dashboard shows.
//!
//!   agenttail ls [--hours N] [--agents claude_code,codex,cursor,pi]
//!       One poll of this machine's sessions, printed as JSON lines, most urgent first.
//!   agenttail watch [--hours N] [--agents …]
//!       Polls every second and prints each change of a session's activity, with its reason.
//!   agenttail events [--agent ID] <path>
//!       Prints one transcript's normalized events, one JSON object per line.
//!   agenttail fixture redact|capture|scan …
//!       Makes, redacts and checks offline fixtures (see `fixture --help`).

mod fixture;

use agenttail::{Filter, Harness, ParseMode, SessionKey, SessionState, Timestamp, Tracker, parse_lines};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const USAGE: &str = "usage: agenttail ls [--hours N] [--agents ID,…]
       agenttail watch [--hours N] [--agents ID,…]
       agenttail events [--agent ID] <path>
       agenttail fixture redact [--lorem] [--in-place] [--report FILE] <input> [output]
       agenttail fixture capture [--session PATH|ID] [--scenario NAME] [--out DIR]
       agenttail fixture scan [PATH…]

  --hours N     sessions written in the last N hours (default 24)
  --agents IDS  only these harnesses: claude_code, codex, cursor, pi (default: all)
  --agent ID    the harness for an events path when it can't be told from the path
  events <path> prints normalized events as JSON lines
  fixture       redact|capture|scan manage fixtures without network access";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agenttail: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or("no command")?;
    match command.as_str() {
        "ls" | "watch" => {
            let filter = filter(rest)?;
            let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME isn't set")?;
            let mut tracker = Tracker::from_env(&home, filter);
            if command == "ls" {
                for (key, state) in tracker.poll(now()) {
                    println!("{}", session(&key, &state));
                }
                Ok(())
            } else {
                watch(&mut tracker)
            }
        }
        "events" => events(rest),
        "fixture" => fixture(rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("no command \"{other}\"")),
    }
}

/// `agenttail events [--agent ID] <path>`: a transcript's events as JSON lines.
fn events(args: &[String]) -> Result<(), String> {
    let mut agent = None;
    let mut path = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent" => {
                let id = args.next().ok_or("--agent needs a value")?;
                agent = Some(id.parse::<Harness>().map_err(|e| e.to_string())?);
            }
            other if other.starts_with('-') => return Err(format!("no option \"{other}\"")),
            other if path.is_some() => return Err(format!("events takes one path, not \"{other}\"")),
            other => path = Some(PathBuf::from(other)),
        }
    }
    let path = path.ok_or("events needs a path")?;
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let harness = agent.or_else(|| harness_for(&path)).or_else(|| harness_from_bytes(&bytes)).ok_or_else(|| {
        format!("can't tell which harness wrote {}; pass --agent claude_code|codex|cursor|pi", path.display())
    })?;
    for event in parse_lines(harness, &bytes, ParseMode::Full) {
        println!("{}", serde_json::to_string(&event).map_err(|e| e.to_string())?);
    }
    Ok(())
}

/// `agenttail fixture …`: offline fixture capture, redaction and scanning.
fn fixture(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or("fixture needs a command: redact, capture or scan")?;
    match command.as_str() {
        "redact" => fixture_redact(rest),
        "capture" => fixture_capture(rest),
        "scan" => fixture_scan(rest),
        other => Err(format!("no fixture command \"{other}\" (redact, capture, scan)")),
    }
}

fn fixture_redact(args: &[String]) -> Result<(), String> {
    let mut options = fixture::RedactOptions::default();
    let mut report = None;
    let mut in_place = false;
    let mut positional = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lorem" => options.lorem = true,
            "--in-place" => in_place = true,
            "--report" => report = Some(PathBuf::from(args.next().ok_or("--report needs a value")?)),
            other if other.starts_with('-') => return Err(format!("no option \"{other}\"")),
            other => positional.push(PathBuf::from(other)),
        }
    }
    let input = positional.first().ok_or("fixture redact needs an input path")?.clone();
    if positional.len() > 2 {
        return Err("fixture redact takes an input and at most one output".into());
    }
    if input.is_dir() {
        let output = if in_place {
            input.clone()
        } else {
            positional
                .get(1)
                .cloned()
                .ok_or("fixture redact on a directory needs an output directory (or --in-place)")?
        };
        if output == input && report.is_some() {
            return Err(
                "fixture redact --in-place writes its report under the directory; don't also pass --report".into()
            );
        }
        fixture::redact_tree(&input, &output, &options, report.as_deref())?;
        println!("redact: wrote {} fixture file(s) under {}", count_files(&output)?, output.display());
        return Ok(());
    }
    let bytes = std::fs::read(&input).map_err(|e| format!("{}: {e}", input.display()))?;
    let label =
        input.file_name().map_or_else(|| input.display().to_string(), |name| name.to_string_lossy().into_owned());
    let redaction = fixture::redact_bytes(&bytes, &options, &label);
    if in_place || positional.len() == 2 {
        let output = if in_place { input.clone() } else { positional[1].clone() };
        if output != input
            && let Some(parent) = output.parent()
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&output, &redaction.bytes).map_err(|e| format!("{}: {e}", output.display()))?;
        let default_report =
            output.with_file_name(format!("{}.report.json", output.file_name().unwrap_or_default().to_string_lossy()));
        fixture::write_report(report.as_deref().unwrap_or(&default_report), &redaction.report)?;
        fixture::print_summary(&format!("{} -> {}", input.display(), output.display()), &redaction.report);
    } else {
        std::io::stdout().write_all(&redaction.bytes).map_err(|e| format!("stdout: {e}"))?;
        if let Some(path) = &report {
            fixture::write_report(path, &redaction.report)?;
        }
        eprintln!("{}", fixture::summary(&input.display().to_string(), &redaction.report));
    }
    Ok(())
}

fn fixture_capture(args: &[String]) -> Result<(), String> {
    let mut options = fixture::RedactOptions::default();
    let mut session = None;
    let mut scenario = None;
    let mut out = PathBuf::from("fixtures");
    let mut version = None;
    let mut os = None;
    let mut agent = None;
    let mut capture = "donated".to_string();
    let mut force = false;
    let mut positional = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lorem" => options.lorem = true,
            "--force" => force = true,
            "--session" => session = Some(args.next().ok_or("--session needs a value")?.to_string()),
            "--scenario" => scenario = Some(args.next().ok_or("--scenario needs a value")?.to_string()),
            "--out" => out = PathBuf::from(args.next().ok_or("--out needs a value")?),
            "--version" => version = Some(args.next().ok_or("--version needs a value")?.to_string()),
            "--os" => os = Some(args.next().ok_or("--os needs a value")?.to_string()),
            "--agent" => {
                let id = args.next().ok_or("--agent needs a value")?;
                agent = Some(id.parse::<Harness>().map_err(|e| e.to_string())?);
            }
            "--capture" => capture = args.next().ok_or("--capture needs a value")?.to_string(),
            other if other.starts_with('-') => return Err(format!("no option \"{other}\"")),
            other => positional.push(other.to_string()),
        }
    }
    if session.is_none() && positional.len() == 1 {
        session = Some(positional[0].clone());
    } else if !positional.is_empty() {
        return Err("fixture capture takes at most one session path or id".into());
    }
    if !matches!(capture.as_str(), "donated" | "handmade" | "ci-scripted") {
        return Err(format!("--capture {capture}: use donated, handmade or ci-scripted"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME isn't set")?;
    let candidates = fixture::sessions(&home)?;
    let candidate = pick_session(&candidates, session.as_deref(), agent)?;
    let version = version.unwrap_or_else(|| fixture::version_of(&candidate));
    let os = os.unwrap_or_else(|| std::env::consts::OS.to_string());
    let scenario = fixture::sanitize_component(scenario.as_deref().unwrap_or(&format!("captured-{}", candidate.id)));
    let target = out.join(candidate.harness.id().replace('_', "-")).join(&version).join(&os).join(&scenario);
    if target.exists() && std::fs::read_dir(&target).is_ok_and(|mut entries| entries.next().is_some()) && !force {
        return Err(format!("{} already exists; pass --force to replace it", target.display()));
    }
    if target.exists() && force {
        std::fs::remove_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    }

    let files = fixture::session_files(&candidate);
    let mut reports = Vec::new();
    for file in &files {
        let rel = fixture::input_relative(&candidate, file);
        let before = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let redaction = fixture::redact_bytes(&before, &options, &rel.display().to_string());
        let dest = target.join("input").join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&dest, &redaction.bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
        fixture::print_summary(&format!("{} -> {}", rel.display(), dest.display()), &redaction.report);
        let changed = changed_lines(&before, &redaction.bytes);
        println!("diff: {}: {changed}/{} line(s) changed", rel.display(), redaction.report.lines);
        reports.push(redaction.report);
    }
    let report = fixture::report_json(&reports);
    let report_text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(target.join("redaction.json"), report_text + "\n")
        .map_err(|e| format!("{}: {e}", target.join("redaction.json").display()))?;
    let description = format!(
        "Captured offline from a {} session ({}) and redacted by `agenttail fixture capture`.",
        candidate.harness.name(),
        candidate.id
    );
    let meta = [
        format!("harness = {}", fixture::toml_string(candidate.harness.id())),
        format!("version = {}", fixture::toml_string(&version)),
        format!("os = {}", fixture::toml_string(&os)),
        format!("capture = {}", fixture::toml_string(&capture)),
        format!("date = {}", fixture::toml_string(&fixture::utc_date())),
        format!("description = {}", fixture::toml_string(&description)),
        "redaction = \"redaction.json\"".to_string(),
        String::new(),
    ]
    .join("\n");
    std::fs::write(target.join("meta.toml"), meta)
        .map_err(|e| format!("{}: {e}", target.join("meta.toml").display()))?;
    println!("capture: wrote {} ({} input file(s))", target.display(), files.len());
    println!("capture: review the diff above and redaction.json before committing");
    Ok(())
}

fn pick_session(
    candidates: &[fixture::SessionCandidate],
    wanted: Option<&str>,
    agent: Option<Harness>,
) -> Result<fixture::SessionCandidate, String> {
    if let Some(wanted) = wanted {
        let as_path = Path::new(wanted);
        if as_path.is_file() {
            if let Some(candidate) = candidates.iter().find(|candidate| candidate.path == as_path) {
                return Ok(candidate.clone());
            }
            let harness = agent.or_else(|| harness_for(as_path)).ok_or_else(|| {
                format!("can't tell which harness wrote {wanted}; pass --agent claude_code|codex|cursor|pi")
            })?;
            return Ok(fixture::SessionCandidate {
                harness,
                path: as_path.to_path_buf(),
                id: fixture::sanitize_component(wanted),
            });
        }
        let found: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.id == wanted || candidate.path.to_string_lossy().contains(wanted))
            .cloned()
            .collect();
        return match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(format!("no session \"{wanted}\"; run `agenttail fixture capture` to pick the newest")),
            many => Err(format!(
                "session \"{wanted}\" matches {} files; pass a full path:\n{}",
                many.len(),
                many.iter().map(|candidate| format!("  {}", candidate.path.display())).collect::<Vec<_>>().join("\n")
            )),
        };
    }
    candidates
        .iter()
        .filter(|candidate| agent.is_none_or(|agent| candidate.harness == agent))
        .max_by_key(|candidate| {
            std::fs::metadata(&candidate.path).and_then(|meta| meta.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
        })
        .cloned()
        .ok_or_else(|| "no sessions found under HOME".into())
}

fn fixture_scan(args: &[String]) -> Result<(), String> {
    let mut paths: Vec<PathBuf> = args.iter().filter(|arg| !arg.starts_with('-')).map(PathBuf::from).collect();
    if let Some(option) = args.iter().find(|arg| arg.starts_with('-')) {
        return Err(format!("no option \"{option}\""));
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("fixtures"));
    }
    fixture::scan(&paths)?;
    Ok(())
}

fn changed_lines(before: &[u8], after: &[u8]) -> usize {
    let before = before.split_inclusive(|&b| b == b'\n');
    let after = after.split_inclusive(|&b| b == b'\n');
    before.zip(after).filter(|(a, b)| a != b).count()
}

fn count_files(dir: &Path) -> Result<usize, String> {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            if entry.path().is_dir() {
                stack.push(entry.path());
            } else {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Tell a harness from the path conventions each one uses. A file name or path segment
/// with the harness's name is enough for fixture dumps; real paths match their roots first,
/// so a project folder named `codex` under `.claude` is still a Claude transcript.
fn harness_for(path: &std::path::Path) -> Option<Harness> {
    let text = path.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    let file = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if text.contains("/.cursor/") || text.contains("/agent-transcripts/") {
        Some(Harness::Cursor)
    } else if text.contains("/.pi/") || text.contains("/pi/agent/sessions/") {
        Some(Harness::Pi)
    } else if text.contains("/.codex/") || text.contains("/sessions/") && file.starts_with("rollout-") {
        Some(Harness::Codex)
    } else if text.contains("/.claude/") {
        Some(Harness::ClaudeCode)
    } else if file.contains("cursor") {
        Some(Harness::Cursor)
    } else if text.contains("codex") {
        Some(Harness::Codex)
    } else if text.contains("claude") {
        Some(Harness::ClaudeCode)
    } else if file.starts_with("pi-") {
        Some(Harness::Pi)
    } else {
        None
    }
}

/// A last resort for files outside a harness's usual folders: the record shapes are
/// distinctive enough for a debugging dump. An explicit `--agent` still wins.
fn first_record_is_pi(bytes: &[u8]) -> bool {
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    let Ok(record) = serde_json::from_slice::<Value>(first) else { return false };
    record["type"] == "session" && record["version"].is_u64() && record["id"].is_string() && record["cwd"].is_string()
}

fn harness_from_bytes(bytes: &[u8]) -> Option<Harness> {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(64 << 10)]);
    if text.contains(r#""type":"session_meta""#) || text.contains(r#""type": "session_meta""#) {
        Some(Harness::Codex)
    } else if first_record_is_pi(bytes) {
        Some(Harness::Pi)
    } else if text.contains(r#""type":"turn_ended""#) || text.contains(r#""type": "turn_ended""#) {
        Some(Harness::Cursor)
    } else if text.contains(r#""type":"permission-mode""#)
        || text.contains(r#""type":"assistant""#)
        || text.contains(r#""type":"user""#)
        || text.contains(r#""type":"system""#)
    {
        Some(Harness::ClaudeCode)
    } else if text.contains(r#""role":"user""#) || text.contains(r#""role":"assistant""#) {
        Some(Harness::Cursor)
    } else {
        None
    }
}

/// The filter `--hours` and `--agents` ask for.
fn filter(args: &[String]) -> Result<Filter, String> {
    let mut filter = Filter::within(Duration::from_secs(24 * 3600));
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--hours" => {
                let hours: f64 = value.parse().map_err(|_| format!("--hours {value}: not a number"))?;
                filter.window = Duration::try_from_secs_f64(hours * 3600.0)
                    .map_err(|_| format!("--hours {value}: it should be 0 or more"))?;
            }
            "--agents" => {
                let ids = value.split(',').filter(|id| !id.is_empty()).map(str::parse::<Harness>);
                filter.harnesses = Some(ids.collect::<Result<_, _>>().map_err(|e| e.to_string())?);
            }
            other => return Err(format!("no option \"{other}\"")),
        }
    }
    Ok(filter)
}

fn now() -> Timestamp {
    Timestamp::from(SystemTime::now())
}

/// A session as one JSON object: its state, with the transcript's path.
fn session(key: &SessionKey, state: &SessionState) -> Value {
    let mut v = serde_json::to_value(state).expect("states serialize");
    v["path"] = json!(key.path);
    v
}

/// Poll every second; print each session's activity when it changes, and when it leaves.
fn watch(tracker: &mut Tracker) -> Result<(), String> {
    let mut last: HashMap<PathBuf, Value> = HashMap::new();
    loop {
        let now = now();
        let mut seen = HashMap::new();
        for (key, st) in tracker.poll(now) {
            let what = json!({"activity": st.activity, "certainty": st.certainty, "reason": st.reason});
            if last.get(&key.path) != Some(&what) {
                let mut line = json!({"at": now, "harness": key.harness, "path": key.path, "project": st.meta.project});
                line.as_object_mut().unwrap().extend(what.as_object().unwrap().clone());
                println!("{line}");
            }
            seen.insert(key.path, what);
        }
        for path in last.keys().filter(|p| !seen.contains_key(*p)) {
            println!("{}", json!({"at": now, "path": path, "gone": true}));
        }
        last = seen;
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn real_paths_win_over_harness_names_in_project_folders() {
        for (path, want) in [
            ("/h/.claude/projects/-home-lee-codex/s.jsonl", Harness::ClaudeCode),
            ("/h/.claude/projects/-home-lee-cursor/s.jsonl", Harness::ClaudeCode),
            ("/h/.codex/sessions/2026/10/24/rollout-x.jsonl", Harness::Codex),
            ("/h/.codex/sessions/2026/10/24/claude-copy.jsonl", Harness::Codex),
            ("/h/.cursor/projects/w-claude/agent-transcripts/a/a.jsonl", Harness::Cursor),
            ("/h/.pi/agent/sessions/--w-app--/2026-10-25T01-00-00-000Z_s.jsonl", Harness::Pi),
            ("fixtures/legacy/claude-code-2.1.jsonl", Harness::ClaudeCode),
            ("fixtures/legacy/codex-0.160-exec.jsonl", Harness::Codex),
            ("fixtures/legacy/cursor-synthetic.jsonl", Harness::Cursor),
            ("fixtures/legacy/pi-1.4-session.jsonl", Harness::Pi),
        ] {
            assert_eq!(harness_for(Path::new(path)), Some(want), "{path}");
        }
    }

    #[test]
    fn record_shapes_do_not_mistake_text_for_a_cursor_record() {
        let claude = br#"{"type":"user","message":{"content":"the turn_ended record is next"}}"#;
        assert_eq!(harness_from_bytes(claude), Some(Harness::ClaudeCode));
        let cursor = br#"{"type":"turn_ended","status":"success"}"#;
        assert_eq!(harness_from_bytes(cursor), Some(Harness::Cursor));
        let codex = br#"{"type":"session_meta","payload":{}}"#;
        assert_eq!(harness_from_bytes(codex), Some(Harness::Codex));
        let pi = br#"{"type":"session","version":3,"id":"s","cwd":"/w"}"#;
        assert_eq!(harness_from_bytes(pi), Some(Harness::Pi));
        let reordered = br#"{"cwd": "/w", "id": "s", "version": 3, "type": "session"}"#;
        assert_eq!(harness_from_bytes(reordered), Some(Harness::Pi));
        let not_pi = br#"{"type":"session","id":"s","cwd":"/w"}"#;
        assert_eq!(harness_from_bytes(not_pi), None, "a missing format version is not enough");
    }
}
