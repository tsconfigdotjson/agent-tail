//! `agenttail`: the library on the command line, for debugging what a dashboard shows.
//!
//!   agenttail ls [--hours N] [--agents claude_code,codex,cursor]
//!       One poll of this machine's sessions, printed as JSON lines, most urgent first.
//!   agenttail watch [--hours N] [--agents …]
//!       Polls every second and prints each change of a session's activity, with its reason.
//!   agenttail events [--agent ID] <path>
//!       Prints one transcript's normalized events, one JSON object per line.

use agenttail::{Filter, Harness, ParseMode, SessionKey, SessionState, Timestamp, Tracker, parse_lines};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const USAGE: &str = "usage: agenttail ls [--hours N] [--agents ID,…]
       agenttail watch [--hours N] [--agents ID,…]
       agenttail events [--agent ID] <path>

  --hours N     sessions written in the last N hours (default 24)
  --agents IDS  only these harnesses: claude_code, codex, cursor (default: all)
  --agent ID    the harness for an events path when it can't be told from the path
  events <path> prints normalized events as JSON lines";

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
        format!("can't tell which harness wrote {}; pass --agent claude_code|codex|cursor", path.display())
    })?;
    for event in parse_lines(harness, &bytes, ParseMode::Full) {
        println!("{}", serde_json::to_string(&event).map_err(|e| e.to_string())?);
    }
    Ok(())
}

/// Tell a harness from the path conventions each one uses. A file name or path segment
/// with the harness's name is enough for fixture dumps; real paths match their roots first,
/// so a project folder named `codex` under `.claude` is still a Claude transcript.
fn harness_for(path: &std::path::Path) -> Option<Harness> {
    let text = path.to_string_lossy().to_ascii_lowercase().replace('\\', "/");
    let file = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if text.contains("/.cursor/") || text.contains("/agent-transcripts/") {
        Some(Harness::Cursor)
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
    } else {
        None
    }
}

/// A last resort for files outside a harness's usual folders: the record shapes are
/// distinctive enough for a debugging dump. An explicit `--agent` still wins.
fn harness_from_bytes(bytes: &[u8]) -> Option<Harness> {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(64 << 10)]);
    if text.contains(r#""type":"session_meta""#) || text.contains(r#""type": "session_meta""#) {
        Some(Harness::Codex)
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
            ("fixtures/legacy/claude-code-2.1.jsonl", Harness::ClaudeCode),
            ("fixtures/legacy/codex-0.160-exec.jsonl", Harness::Codex),
            ("fixtures/legacy/cursor-synthetic.jsonl", Harness::Cursor),
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
    }
}
