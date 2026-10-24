//! Cursor: `~/.cursor/projects/<project>/agent-transcripts/<id>/<id>.jsonl`, one entry per
//! line: messages, {"role": "user" | "assistant", "message": {"content": [{"type": "text",
//! "text"} | {"type": "tool_use", "name", "input"}, …]}}, and the end of each turn,
//! {"type": "turn_ended", "status": "success" | "error"}. Subagents' transcripts are in
//! `<id>/subagents/<subagent id>.jsonl`.
//!
//! Seen on a real Mac (October 2026) but undocumented, so read defensively: anything else
//! is skipped. Entries have no times of their own: a prompt is dated by the `<timestamp>`
//! block it starts with (`Tuesday, Jun 2, 2026, 11:20 AM (UTC+8)`, to the minute; tvdash
//! #192), and anything else by the transcript's mtime when it was read.
//! `<project>` is the workspace's path with every `/` made `-` (so `my-app` and `my/app` look
//! alike): the project's name is found in the transcript where it can be (a path in a tool
//! call or a prompt that is in the workspace), and until then is the folder's last part.

use super::Adapter;
use crate::harness::{HelperOwner, Layout};
use crate::session::{Last, Session, command, file_name, first_line, tool};
use crate::state::ActivityLine;
use crate::time::{days_from_civil, valid_date};
use serde_json::Value;
use std::path::Path;

/// Tools that stop and ask the user something, lowercased with `_` left out.
const ASKS_USER: [&str; 4] = ["askquestion", "askuserquestion", "askfollowupquestion", "requestuserinput"];
/// Where the project folders' sessions are.
const TRANSCRIPTS: &str = "agent-transcripts";
/// The longest string looked through for the workspace's path.
const MAX_PATH: usize = 1024;

pub(crate) const LAYOUT: Layout = Layout {
    root: ".cursor/projects",
    // <project>/agent-transcripts/<id>/<id>.jsonl.
    max_depth: Some(4),
    sessions_in: Some(TRANSCRIPTS),
    helpers_in: Some("subagents"),
    helper_owner: HelperOwner::SessionFolder,
    day_folders: false,
};

pub(crate) struct Cursor;

impl Adapter for Cursor {
    fn start(&self, s: &mut Session, path: &Path) {
        // The folder before the last agent-transcripts: local paths and remote ones alike.
        let parts: Vec<_> = path.components().map(|c| c.as_os_str().to_string_lossy()).collect();
        if let Some(i) = parts.iter().rposition(|p| p == TRANSCRIPTS).filter(|&i| i > 0) {
            s.folder = parts[i - 1].to_string();
            s.project = s.folder.rsplit('-').find(|p| !p.is_empty()).unwrap_or(&s.folder).to_string();
        }
    }

    fn apply(&self, s: &mut Session, e: &Value) {
        let t = s.mtime;
        if e["type"] == "turn_ended" {
            let status = e["status"].as_str().unwrap_or_default();
            let error = e["error"].as_str().unwrap_or_default();
            if status == "aborted" || (status == "error" && user_stopped(error)) {
                s.set(Last::Interrupted, t);
            } else if status == "error" {
                s.set_error(if error.is_empty() { "error" } else { error }, t);
            } else {
                s.set(Last::EndTurn, t);
            }
            return;
        }
        let role = e["role"].as_str().or(e["message"]["role"].as_str());
        let content = &e["message"]["content"];
        match role {
            Some("user") => user(s, content, t),
            Some("assistant") => assistant(s, content, t),
            _ => {}
        }
    }
}

/// Cursor writes a user stop as `turn_ended` with `status: "error"` and a phrase such as
/// "User aborted request"; a genuine failure carries an error message instead.
fn user_stopped(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    ["abort", "cancel", "interrupt", "stopped by user", "user stopped"].iter().any(|s| error.contains(s))
}

fn user(s: &mut Session, content: &Value, t: f64) {
    let items: Vec<&Value> = match content {
        Value::Array(items) => items.iter().collect(),
        text @ Value::String(_) => vec![text],
        _ => return,
    };
    if items.iter().any(|c| c["type"] == "tool_result") {
        s.set(Last::ToolResult, t);
        return;
    }
    let texts: Vec<&str> = items.iter().filter_map(|c| c.as_str().or(c["text"].as_str())).collect();
    // Sent when its <timestamp> says, though no later than the file was written.
    let t = texts.iter().find_map(|text| sent_at(text)).map_or(t, |sent| sent.min(t));
    for text in &texts {
        find_project(s, text.split(|c: char| c.is_whitespace() || "\"'`()<>,;".contains(c)));
    }
    let typed = texts.iter().find_map(|t| typed(t)).unwrap_or_default();
    if let Some(first) = s.prompt(typed, t)
        && s.title.is_empty()
    {
        // Cursor's titles aren't in the transcript; the opening prompt is the best summary.
        s.title = first;
    }
}

/// What the user typed, from a prompt's text: inside <user_query> when it's there, and
/// otherwise the text unless it's all context (a tag block, like <attached_files>).
fn typed(text: &str) -> Option<&str> {
    if let Some((_, rest)) = text.split_once("<user_query>") {
        return Some(rest.split_once("</user_query>").map_or(rest, |(q, _)| q));
    }
    (!text.trim_start().starts_with('<')).then_some(text)
}

/// When a prompt was sent, in seconds since the epoch, from the <timestamp> block that
/// starts `text`: None if there's none, or it doesn't read as `parse_sent` expects.
fn sent_at(text: &str) -> Option<f64> {
    let rest = text.trim_start().strip_prefix("<timestamp>")?;
    parse_sent(rest.split_once("</timestamp>")?.0)
}

/// Seconds since the epoch for a time as Cursor writes it, `Tuesday, Jun 2, 2026, 11:20 AM
/// (UTC+8)`: a word before the date (the weekday, not checked), an English month by name or
/// 3 letters, before or after the day, the 4-digit year, a 12- or 24-hour time with or
/// without seconds, and the UTC offset. Anything else is None rather than a guess.
fn parse_sent(s: &str) -> Option<f64> {
    let (local, zone) = s.trim().strip_suffix(')')?.rsplit_once('(')?;
    let offset = utc_offset(zone.trim())?;
    let mut words = local.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()).peekable();
    words.next_if(|w| month(w).is_none() && digits(w, 1, 2).is_none());
    let (a, b) = (words.next()?, words.next()?);
    let (m, d) = match month(a) {
        Some(m) => (m, digits(b, 1, 2)?),
        None => (month(b)?, digits(a, 1, 2)?),
    };
    let y = digits(words.next()?, 4, 4)?;
    let mut clock = words.next()?.to_ascii_lowercase();
    let mut half = words.next().map(str::to_ascii_lowercase);
    if words.next().is_some() {
        return None;
    }
    if half.is_none()
        && let Some(i) = clock.find(['a', 'p'])
    {
        half = Some(clock.split_off(i));
    }
    let mut hms = clock.split(':');
    let (h, min) = (digits(hms.next()?, 1, 2)?, digits(hms.next()?, 2, 2)?);
    let sec = hms.next().map_or(Some(0), |s| digits(s, 2, 2))?;
    if hms.next().is_some() {
        return None;
    }
    let h = match half.as_deref() {
        None => h,
        Some(_) if !(1..=12).contains(&h) => return None,
        Some("am" | "a.m.") => h % 12,
        Some("pm" | "p.m.") => h % 12 + 12,
        Some(_) => return None,
    };
    if !valid_date(i64::from(y), m, d) || h > 23 || min > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(i64::from(y), i64::from(m), i64::from(d));
    Some((days * 86400 + i64::from(h * 3600 + min * 60 + sec) - offset) as f64)
}

/// `word` as a number, if it's `min` to `max` ASCII digits and nothing else.
fn digits(word: &str, min: usize, max: usize) -> Option<u32> {
    ((min..=max).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_digit())).then(|| word.parse().ok())?
}

/// An English month's number, by its name or first 3 letters (or Sept).
fn month(word: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let word = word.trim_end_matches('.').to_ascii_lowercase();
    let i = MONTHS.iter().position(|m| {
        word.len() >= 3 && (*m == word || (m.starts_with(&word) && (word.len() == 3 || word == "sept")))
    })?;
    Some(i as u32 + 1)
}

/// Seconds east of UTC for `UTC`, `UTC+8`, `UTC-07:00`, `UTC+0530` (or GMT…).
fn utc_offset(zone: &str) -> Option<i64> {
    let rest = zone.strip_prefix("UTC").or(zone.strip_prefix("GMT"))?;
    if rest.is_empty() {
        return Some(0);
    }
    let (sign, hm) = match rest.strip_prefix('+') {
        Some(hm) => (1, hm),
        // A hyphen, or the minus sign some locales write.
        None => (-1, rest.strip_prefix('-').or(rest.strip_prefix('\u{2212}'))?),
    };
    let (h, m) = match hm.split_once(':') {
        Some((h, m)) => (digits(h, 1, 2)?, digits(m, 2, 2)?),
        None if hm.len() == 4 => (digits(hm.get(..2)?, 2, 2)?, digits(hm.get(2..)?, 2, 2)?),
        None => (digits(hm, 1, 2)?, 0),
    };
    (h <= 14 && m < 60).then_some(sign * i64::from(h * 3600 + m * 60))
}

fn assistant(s: &mut Session, content: &Value, t: f64) {
    for c in content.as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("tool_use") => {
                let name = c["name"].as_str().unwrap_or_default();
                let parsed;
                let input = match c["input"].as_str() {
                    Some(json) => {
                        parsed = serde_json::from_str(json).unwrap_or(Value::Null);
                        &parsed
                    }
                    None => &c["input"],
                };
                find_project(s, strings(input));
                let key: String =
                    name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
                s.tool_call(
                    ASKS_USER.contains(&key.as_str()),
                    (!name.is_empty()).then(|| describe_tool(name, input)),
                    t,
                );
            }
            // Cursor doesn't record tools' results: the model writing again means they're in.
            Some("text" | "thinking" | "reasoning") => s.set(Last::ToolResult, t),
            _ => {}
        }
    }
}

/// A tool call's string arguments, and those in lists.
fn strings(input: &Value) -> impl Iterator<Item = &str> {
    let values = input.as_object().into_iter().flat_map(|o| o.values());
    values.flat_map(|v| v.as_str().into_iter().chain(v.as_array().into_iter().flatten().filter_map(Value::as_str)))
}

/// Name the project after the workspace folder, if one of `words` is a path in it.
fn find_project<'a>(s: &mut Session, words: impl Iterator<Item = &'a str>) {
    if s.folder.is_empty() || s.seen.contains("project") {
        return;
    }
    let folder = key(&s.folder);
    for word in words.filter(|w| w.starts_with('/') && w.len() <= MAX_PATH) {
        if let Some(name) = workspace_in(word, &folder) {
            s.project = name.into();
            s.seen.insert("project".into());
            return;
        }
    }
}

/// The last folder of the part of `path` that names the workspace whose project folder is
/// `folder` (`key`ed): `/Users/lee/w/vibe-os/src/main.rs` in `Users-lee-w-vibe-os` is
/// `vibe-os`.
fn workspace_in<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    if folder.is_empty() || !key(path).starts_with(folder) {
        return None;
    }
    let mut ends = path.match_indices('/').map(|(i, _)| i).chain([path.len()]);
    let workspace = &path[..ends.find(|&i| key(&path[..i]) == folder)?];
    workspace.rsplit('/').next().filter(|n| !n.is_empty())
}

/// `s` as Cursor's folder names might write it, to compare: every run of characters other
/// than letters and digits made one `-`, none at the ends, lowercase.
fn key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

fn describe_tool(name: &str, input: &Value) -> ActivityLine {
    if let Some(c) = input["command"].as_str() {
        return command(c);
    }
    let keys = [
        "file_path",
        "target_file",
        "path",
        "target_directory",
        "glob_pattern",
        "pattern",
        "query",
        "search_term",
        "url",
        "description",
        "prompt",
    ];
    for key in keys {
        if let Some(v) = input[key].as_str().filter(|v| !v.is_empty()) {
            let v = if key.contains("file") || key.contains("path") || key.contains("directory") {
                file_name(v)
            } else {
                first_line(v).to_string()
            };
            return tool(name, v);
        }
    }
    ActivityLine::Tool { name: name.into(), detail: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusClass as Status;
    use crate::harness::Harness;
    use crate::reducer::LOG_LINES;
    use crate::testutil::{NOW, log, status};
    use crate::time::{civil_from_days, parse_rfc3339};
    use serde_json::json;

    const STALL_AFTER: f64 = 20.0;

    /// A session after `events`, its transcript written `NOW` (Cursor's entries take the
    /// write's time).
    fn session(events: &[Value]) -> Session {
        let mut s = Session::new(Harness::Cursor, LOG_LINES);
        s.mtime = NOW;
        events.iter().for_each(|e| s.apply(e));
        s
    }

    /// A session in the project folder `folder`, after `events`.
    fn in_folder(folder: &str, events: &[Value]) -> Session {
        let path = format!("/h/.cursor/projects/{folder}/agent-transcripts/a1/a1.jsonl");
        let mut s = Session::at(Harness::Cursor, LOG_LINES, Path::new(&path));
        s.mtime = NOW;
        events.iter().for_each(|e| s.apply(e));
        s
    }

    fn prompt(text: &str) -> Value {
        json!({"role": "user", "message": {"content": [{"type": "text", "text": text}]}})
    }

    fn tool_call(name: &str, input: Value) -> Value {
        json!({"role": "assistant", "message": {"content": [{"type": "tool_use", "name": name, "input": input}]}})
    }

    fn said(text: &str) -> Value {
        json!({"role": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
    }

    fn turn_ended(status: &str) -> Value {
        json!({"type": "turn_ended", "status": status})
    }

    #[test]
    fn follows_a_session() {
        let mut s = session(&[prompt("<user_query>\nFix routing\nplease\n</user_query>")]);
        assert_eq!((s.title.as_str(), status(&s)), ("Fix routing", (Status::Working, "thinking")));
        s.apply(&tool_call("Shell", json!({"command": "rg -n foo .\nmore"})));
        assert_eq!((status(&s), log(&s).last().unwrap().as_str()), ((Status::Working, "working"), "$ rg -n foo ."));
        s.apply(&said("Found it."));
        assert_eq!(status(&s), (Status::Working, "thinking"), "the model writing again: the tool is done");
        s.apply(&prompt("<user_query>and another</user_query>"));
        assert_eq!(s.title, "Fix routing", "title is the opening prompt");
        s.apply(&turn_ended("success"));
        assert_eq!(status(&s), (Status::Done, "done"));
        assert_eq!(log(&s), ["› Fix routing", "$ rg -n foo .", "› and another"]);
        assert_eq!((s.harness.id(), s.cost, s.out_tokens), ("cursor", None, 0), "Cursor reports no spend");
    }

    #[test]
    fn every_turn_end_is_told_apart() {
        for (event, last) in [
            (turn_ended("success"), Last::EndTurn),
            (turn_ended(""), Last::EndTurn),
            (turn_ended("aborted"), Last::Interrupted),
            (json!({"type": "turn_ended", "status": "error", "error": "User aborted request"}), Last::Interrupted),
            (json!({"type": "turn_ended", "status": "error", "error": "Connection failed"}), Last::Errored),
            (json!({"type": "turn_ended"}), Last::EndTurn),
        ] {
            let s = session(&[prompt("go"), tool_call("Shell", json!({"command": "make"})), event]);
            assert_eq!(status(&s), (Status::Done, "done"));
            assert_eq!(s.last, last);
        }
        let s = session(&[json!({"type": "turn_ended", "status": "error", "error": "Connection failed"})]);
        assert_eq!(s.error.as_deref(), Some("Connection failed"));
    }

    #[test]
    fn waits_on_the_user() {
        for name in ["AskQuestion", "ask_question", "AskUserQuestion", "request_user_input"] {
            let s = session(&[prompt("go"), tool_call(name, json!({"question": "Which one?"}))]);
            assert_eq!(status(&s), (Status::NeedsYou, "needs your answer"), "{name}");
        }
        let mut s = session(&[prompt("go"), tool_call("Shell", json!({"command": "rm -rf build"}))]);
        // Cursor's entries are as old as the file's last write.
        s.last_at -= STALL_AFTER * 2.0;
        s.mtime -= STALL_AFTER * 2.0;
        assert_eq!(
            status(&s),
            (Status::NeedsYou, "may need approval"),
            "a tool call with nothing after it for a while"
        );
    }

    #[test]
    fn finds_what_the_user_typed() {
        assert_eq!(typed("<user_query>\n  Fix it\n</user_query>"), Some("\n  Fix it\n"));
        assert_eq!(typed("<user_info>x</user_info>\n<user_query>Fix it"), Some("Fix it"), "an unclosed query");
        assert_eq!(typed("Fix it"), Some("Fix it"));
        assert_eq!(typed("  <attached_files>a.rs</attached_files>"), None, "context alone");
        let s = session(&[prompt("<attached_files>a.rs</attached_files>")]);
        assert_eq!((status(&s).1, s.log.len(), s.title.as_str()), ("thinking", 0, ""), "still a turn starting");
        let s = session(&[json!({"role": "user", "message": {"content": "plain text"}})]);
        assert_eq!(log(&s), ["› plain text"], "content as a string");
        let s = session(&[json!({"message": {"role": "user", "content": [{"type": "text", "text": "role inside"}]}})]);
        assert_eq!(log(&s), ["› role inside"]);
    }

    /// `secs` as Cursor writes a time, in UTC: `Friday, Oct 16, 2026, 9:41 AM (UTC)` (the
    /// weekday isn't checked, so it's always Friday here).
    fn cursor_time(secs: i64) -> String {
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        let (y, m, d) = civil_from_days(secs.div_euclid(86400));
        let (h, min) = (secs.rem_euclid(86400) / 3600, secs.rem_euclid(3600) / 60);
        let (h12, half) = (if h % 12 == 0 { 12 } else { h % 12 }, if h < 12 { "AM" } else { "PM" });
        format!("Friday, {} {d}, {y}, {h12}:{min:02} {half} (UTC)", MONTHS[m as usize - 1])
    }

    #[test]
    fn dates_prompts_from_their_timestamp() {
        let at = |s: &str| parse_rfc3339(s).unwrap();
        let mut s = in_folder("w-app", &[]);
        s.mtime = at("2026-10-16T17:00:00Z");
        s.apply(&prompt("<timestamp>Friday, Oct 16, 2026, 9:41 AM (UTC-7)</timestamp>\n<user_query>go</user_query>"));
        assert_eq!((s.last_at, log(&s)), (at("2026-10-16T16:41:00Z"), vec!["› go".to_string()]));
        // The block as an item of its own, or alone with nothing typed (a turn all the same).
        let alone = json!({"role": "user", "message": {"content": [{"type": "text", "text": "<timestamp>Friday, Oct 16, 2026, 9:50 AM (UTC-7)</timestamp>"}]}});
        s.apply(&alone);
        assert_eq!((s.last_at, s.last, s.log.len()), (at("2026-10-16T16:50:00Z"), Last::Prompt, 1));
        let items = json!({"role": "user", "message": {"content": [
            {"type": "text", "text": "  <timestamp>Friday, Oct 16, 2026, 9:55 AM (UTC-7)</timestamp>"},
            {"type": "text", "text": "<user_query>more</user_query>"}]}});
        s.apply(&items);
        assert_eq!(s.last_at, at("2026-10-16T16:55:00Z"));
        // What the model does next is dated by the file's mtime, as before.
        s.apply(&said("ok"));
        assert_eq!(s.last_at, s.mtime);
        // No later than the file was written; no block, or one that doesn't read: the mtime.
        for text in [
            "<timestamp>Friday, Oct 16, 2026, 11:00 AM (UTC-7)</timestamp><user_query>x</user_query>",
            "<user_query>x</user_query>",
            "<timestamp>16/10/2026 09:41</timestamp><user_query>x</user_query>",
            "<user_query>x <timestamp>Friday, Oct 16, 2026, 9:41 AM (UTC-7)</timestamp></user_query>",
            "<timestamp>Friday, Oct 16, 2026, 9:41 AM (UTC-7)<user_query>x</user_query>",
        ] {
            s.apply(&prompt(text));
            assert_eq!(s.last_at, s.mtime, "{text}");
        }
        // A time in the future: no later than the write.
        let s = session(&[prompt("<timestamp>Friday, Oct 16, 2099, 9:41 AM (UTC-7)</timestamp>go")]);
        assert_eq!(s.last_at, NOW);
        // And the state counts from the prompt.
        let mut s = in_folder("w-app", &[]);
        s.mtime = NOW;
        s.apply(&prompt(&format!(
            "<timestamp>{}</timestamp><user_query>go</user_query>",
            cursor_time(NOW as i64 - 600)
        )));
        let st = s.state(crate::Timestamp::from_secs_f64(NOW), &Default::default(), Default::default());
        assert_eq!(st.since, Some(crate::Timestamp::from_secs_f64(NOW - 600.0)));
    }

    #[test]
    fn reads_cursors_times() {
        let at = |s: &str| Some(parse_rfc3339(s).unwrap());
        // As seen in Cursor's transcripts.
        assert_eq!(parse_sent("Tuesday, Jun 2, 2026, 11:20 AM (UTC+8)"), at("2026-06-02T03:20:00Z"));
        assert_eq!(parse_sent("Friday, Sep 18, 2026, 10:48 AM (UTC+1)"), at("2026-09-18T09:48:00Z"));
        // Around noon and midnight, and a day over in UTC.
        assert_eq!(parse_sent("Friday, Oct 16, 2026, 12:05 PM (UTC)"), at("2026-10-16T12:05:00Z"));
        assert_eq!(parse_sent("Friday, Oct 16, 2026, 12:05 AM (UTC)"), at("2026-10-16T00:05:00Z"));
        assert_eq!(parse_sent("Friday, Oct 16, 2026, 11:30 PM (UTC-7)"), at("2026-10-17T06:30:00Z"));
        // Other ways it might be written: a narrow space before PM (as Intl writes it),
        // lowercase or no space, seconds, 24 hours, full or dotted months, the day first,
        // no weekday, offsets with minutes, GMT, a minus sign, padding.
        let same = at("2026-10-16T16:41:00Z");
        for s in [
            "Friday, Oct 16, 2026, 9:41\u{202f}AM (UTC-7)",
            "Friday, Oct 16, 2026, 9:41 am (UTC-7)",
            "Friday, Oct 16, 2026, 9:41AM (UTC-7)",
            "Friday, Oct 16, 2026, 9:41 a.m. (UTC-7)",
            "Friday, Oct 16, 2026, 09:41 (UTC-07:00)",
            "Friday, October 16, 2026, 9:41 AM (UTC-0700)",
            "Fri, Oct. 16, 2026, 9:41 AM (GMT-7)",
            "Friday, 16 Oct 2026, 9:41 AM (UTC\u{2212}7)",
            "Oct 16, 2026, 9:41 AM (UTC-7)",
            "\n  Friday, Oct 16, 2026, 9:41 AM (UTC-7)\n",
        ] {
            assert_eq!(parse_sent(s), same, "{s:?}");
        }
        assert_eq!(parse_sent("Friday, Oct 16, 2026, 9:41:30 AM (UTC-7)"), at("2026-10-16T16:41:30Z"));
        assert_eq!(parse_sent("Sunday, Sept 6, 2026, 23:59 (UTC+5:30)"), at("2026-09-06T18:29:00Z"));
        assert_eq!(parse_sent("Thursday, May 7, 2026, 1:00 PM (UTC+14)"), at("2026-05-06T23:00:00Z"));
        assert_eq!(parse_sent("Thursday, Feb 29, 2024, 1:00 PM (UTC)"), at("2024-02-29T13:00:00Z"), "a leap day");
        // Anything else isn't guessed at.
        for s in [
            "",
            "Friday, Oct 16, 2026, 9:41 AM",
            "Friday, Oct 16, 2026, 9:41 AM (PDT)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+15)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+5:60)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+123)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+a€)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+5:30:00)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC 7)",
            "Friday, Oct 16, 2026, 9:41 AM (UTC+5:3)",
            "Friday, Oct 16, 26, 9:41 AM (UTC)",
            "Friday, Oct 16, -2026, 9:41 AM (UTC)",
            "Friday, Oct 16, 02026, 9:41 AM (UTC)",
            "Friday, Oct +16, 2026, 9:41 AM (UTC)",
            "Friday, Oct 16, 2026, 9:+41 AM (UTC)",
            "Friday, Oct 16, 2026, 9:4 AM (UTC)",
            "Friday, Oct 16, 2026, 9:41:5 AM (UTC)",
            "Friday, Oct 16, 2026, 123:41 (UTC)",
            "Friday, Oct 16, 2026, 13:41 PM (UTC)",
            "Friday, Oct 16, 2026, 0:41 AM (UTC)",
            "Friday, Oct 16, 2026, 9:41 XM (UTC)",
            "Friday, Oct 16, 2026, 24:00 (UTC)",
            "Friday, Oct 16, 2026, 9:60 (UTC)",
            "Friday, Oct 16, 2026, 9:41:60 (UTC)",
            "Friday, Oct 16, 2026, 9 AM (UTC)",
            "Friday, Oct 16, 2026, 9:41:00:00 (UTC)",
            "Friday, Oct 32, 2026, 9:41 (UTC)",
            "Friday, Oct 0, 2026, 9:41 (UTC)",
            "Friday, Feb 29, 2026, 9:41 (UTC)",
            "Friday, Okt 16, 2026, 9:41 (UTC)",
            "Friday, Octo 16, 2026, 9:41 (UTC)",
            "Friday, Oct 16th, 2026, 9:41 (UTC)",
            "Friday, Oct 16, 9:41 (UTC)",
            "Friday, Oct 16, 2026, 9:41 AM extra (UTC)",
            "Friday Saturday, Oct 16, 2026, 9:41 (UTC)",
            "2026-10-16T16:41:00Z",
        ] {
            assert_eq!(parse_sent(s), None, "{s:?}");
        }
        assert_eq!(sent_at("<timestamp>Oct 16, 2026, 9:41 (UTC)</timestamp>"), at("2026-10-16T09:41:00Z"));
        assert_eq!(sent_at("<timestamp>Oct 16, 2026, 9:41 (UTC)"), None, "unclosed");
        assert_eq!(sent_at("x<timestamp>Oct 16, 2026, 9:41 (UTC)</timestamp>"), None, "not at the start");
    }

    #[test]
    fn names_the_project_after_the_workspace() {
        // Until the transcript tells, the folder's last part.
        assert_eq!(in_folder("Users-sam-Work-demo-app", &[]).project, "app");
        assert_eq!(in_folder("Users-sam-Work-shade", &[prompt("hi")]).project, "shade");
        // A path in the workspace, in a tool call or a prompt, tells.
        let read = tool_call("Read", json!({"path": "/Users/sam/Work/demo-app/src/main.rs"}));
        assert_eq!(in_folder("Users-sam-Work-demo-app", std::slice::from_ref(&read)).project, "demo-app");
        let info = prompt("<user_info>Workspace Path: /Users/sam/Work/demo-app</user_info>");
        assert_eq!(in_folder("Users-sam-Work-demo-app", &[info]).project, "demo-app");
        let listed = tool_call("Grep", json!({"pattern": "x", "paths": ["/Users/sam/Work/demo-app/src"]}));
        assert_eq!(in_folder("Users-sam-Work-demo-app", &[listed]).project, "demo-app");
        // Dots, spaces and case, however Cursor writes them.
        let dotted = tool_call("Read", json!({"path": "/Users/sam/.vibe-worktrees/My App/README.md"}));
        assert_eq!(in_folder("Users-sam-vibe-worktrees-My-App", std::slice::from_ref(&dotted)).project, "My App");
        assert_eq!(in_folder("users-sam--vibe-worktrees-my-app", &[dotted]).project, "My App");
        // Paths elsewhere don't, nor does a folder whose name only starts the same.
        let elsewhere = tool_call("Read", json!({"path": "/Users/sam/Work/demo-app-two/a.rs"}));
        assert_eq!(in_folder("Users-sam-Work-demo-app", &[elsewhere]).project, "app");
        let outside = tool_call("Read", json!({"path": "/etc/hosts"}));
        assert_eq!(in_folder("Users-sam-Work-demo-app", &[outside, read.clone()]).project, "demo-app");
        // The first that tells wins.
        let deeper = tool_call("Read", json!({"path": "/Users/sam/Work/demo/app/x"}));
        assert_eq!(in_folder("Users-sam-Work-demo-app", &[read, deeper]).project, "demo-app");
        // Not in a Cursor project folder at all: nothing to go on.
        let s = Session::at(Harness::Cursor, LOG_LINES, Path::new("/somewhere/s.jsonl"));
        assert_eq!((s.project.as_str(), s.folder.as_str()), ("", ""));
        assert_eq!(workspace_in("/", "users"), None);
        assert_eq!(workspace_in("/Users", ""), None);
    }

    #[test]
    fn skips_what_it_doesnt_know() {
        let odd = [
            json!({"type": "something_new", "data": [1, 2]}),
            json!({"role": "system", "message": {"content": [{"type": "text", "text": "rules"}]}}),
            json!({"role": "assistant", "message": {"content": "not a list"}}),
            json!({"role": "assistant", "message": {"content": [{"type": "image"}, 42, null, {"type": "tool_use"}]}}),
            json!({"role": "user", "message": {"content": 7}}),
            json!({"role": "user"}),
            json!([1, 2, 3]),
            json!("a string"),
            json!(null),
        ];
        let s = session(&odd);
        assert!(s.log.is_empty() && s.title.is_empty());
        assert_eq!(status(&s), (Status::Working, "working"), "a tool call, even nameless, is work");
        let tool_result = json!({"role": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}});
        let s = session(&[prompt("go"), tool_call("Shell", json!({"command": "x"})), tool_result]);
        assert_eq!(
            (status(&s).1, s.log.len()),
            ("thinking", 2),
            "a tool's result, if Cursor ever records one, isn't a prompt"
        );
    }

    #[test]
    fn describes_tools() {
        let d = |name, input| describe_tool(name, &input).to_string();
        assert_eq!(d("Shell", json!({"command": "git status\nmore"})), "$ git status");
        assert_eq!(d("Read", json!({"path": "/a/b/main.rs"})), "Read main.rs");
        assert_eq!(d("read_file", json!({"target_file": "src/lib.rs"})), "read_file lib.rs");
        assert_eq!(d("list_dir", json!({"target_directory": "/a/src/"})), "list_dir src");
        assert_eq!(d("Glob", json!({"glob_pattern": "**/*.rs"})), "Glob **/*.rs");
        assert_eq!(d("codebase_search", json!({"query": "where\nelse"})), "codebase_search where");
        assert_eq!(d("Task", json!({"description": "Find tests", "prompt": "x"})), "Task Find tests");
        assert_eq!(d("TodoWrite", json!({"todos": []})), "TodoWrite");
        assert_eq!(d("Mystery", Value::Null), "Mystery");
        let s = session(&[tool_call("Shell", json!("{\"command\": \"make\"}"))]);
        assert_eq!(log(&s), ["$ make"], "input given as JSON text");
    }

    /// A synthetic transcript in the shape seen on a real Mac (none of anyone's content):
    /// two turns, the second ending in an error.
    const SYNTHETIC: &str = include_str!("../../../../fixtures/legacy/cursor-synthetic.jsonl");

    #[test]
    fn reads_a_whole_session() {
        let events: Vec<Value> = SYNTHETIC.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let s = in_folder("Users-sam-Work-demo-app", &events);
        assert_eq!((s.project.as_str(), s.title.as_str()), ("demo-app", "Add a health check to the API"));
        assert_eq!(
            log(&s),
            [
                "Task Find the existing tests",
                "StrReplace routes.ts",
                "$ npm test",
                "› Now deploy it",
                "$ npm run deploy"
            ]
        );
        assert_eq!(status(&s), (Status::Done, "done"));
        // Stopped mid-turn, it's at work.
        let shell = events.iter().position(|e| e["message"]["content"][0]["name"] == "Shell").unwrap();
        let s = in_folder("Users-sam-Work-demo-app", &events[..=shell]);
        assert_eq!(status(&s), (Status::Working, "working"));
        // Each prompt is dated by its timestamp, the opening one and the one after a turn.
        let at = |s: &str| parse_rfc3339(s).unwrap();
        let s = in_folder("Users-sam-Work-demo-app", &events[..1]);
        assert_eq!((s.last_at, s.last), (at("2026-10-16T16:05:00Z"), Last::Prompt));
        let again = events.iter().rposition(|e| e["role"] == "user").unwrap();
        let s = in_folder("Users-sam-Work-demo-app", &events[..=again]);
        assert_eq!((s.last_at, s.last), (at("2026-10-16T16:41:00Z"), Last::Prompt));
    }
}
