//! `agenttail`: the library on the command line, for debugging what a dashboard shows.
//!
//!   agenttail ls [--hours N] [--agents claude_code,codex,cursor]
//!       One poll of this machine's sessions, printed as JSON lines, most urgent first.
//!   agenttail watch [--hours N] [--agents …]
//!       Polls every second and prints each change of a session's activity, with its reason.

use agenttail::{Filter, Harness, SessionKey, SessionState, Timestamp, Tracker};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const USAGE: &str = "usage: agenttail ls [--hours N] [--agents ID,…]
       agenttail watch [--hours N] [--agents ID,…]

  --hours N     sessions written in the last N hours (default 24)
  --agents IDS  only these harnesses: claude_code, codex, cursor (default: all)";

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
    let filter = filter(rest)?;
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME isn't set")?;
    let mut tracker = Tracker::from_env(&home, filter);
    match command.as_str() {
        "ls" => {
            for (key, state) in tracker.poll(now()) {
                println!("{}", session(&key, &state));
            }
            Ok(())
        }
        "watch" => watch(&mut tracker),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("no command \"{other}\"")),
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
