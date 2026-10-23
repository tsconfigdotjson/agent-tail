//! Big transcript lines, made small before they're parsed (tvdash #139). Most of a
//! transcript's bytes are tool output (a file read, a test run, a file written) that no
//! adapter looks at past its first line, yet parsing it into a `Value` unescapes and copies
//! every byte. Cutting each long string short first, with a quick scan, leaves the same
//! session. Used in [`ParseMode::Lean`](crate::ParseMode::Lean) only.

use std::borrow::Cow;

/// Lines up to this long are parsed as they are.
const LEAN_ABOVE: usize = 16 << 10;
/// What's kept of a longer string: far more than any card shows of one (a line of a prompt,
/// a command or a title), and more than enough to hold an apply_patch's first file name.
pub(crate) const KEEP: usize = 4 << 10;
/// Strings that are JSON themselves, which an adapter parses again: kept whole (Codex's
/// tool call arguments).
const WHOLE: [&[u8]; 1] = [b"\"arguments\""];

/// `line`, with each JSON string over `KEEP` bytes cut to its first `KEEP` or fewer (whole
/// characters and escapes), except those `WHOLE` names. Lines up to `LEAN_ABOVE`, and
/// anything that isn't well-formed, come back as they are: borrowed, so a caller can tell
/// whether anything was cut.
pub(crate) fn lean(line: &[u8]) -> Cow<'_, [u8]> {
    if line.len() <= LEAN_ABOVE {
        return Cow::Borrowed(line);
    }
    let mut out = Vec::new();
    let (mut at, mut copied) = (0, 0);
    while let Some(open) = memchr::memchr(b'"', &line[at..]).map(|i| at + i) {
        // The closing quote: the first that isn't escaped.
        let mut i = open + 1;
        let close = loop {
            match memchr::memchr2(b'"', b'\\', &line[i..]) {
                Some(j) if line[i + j] == b'\\' => i += j + 2,
                Some(j) => break i + j,
                None => return Cow::Borrowed(line),
            }
        };
        if close - open - 1 > KEEP && !names_whole(&line[..open]) {
            let cut = boundary(&line[open + 1..close], KEEP);
            out.extend_from_slice(&line[copied..open + 1 + cut]);
            out.push(b'"');
            copied = close + 1;
        }
        at = close + 1;
    }
    if copied == 0 {
        return Cow::Borrowed(line);
    }
    out.extend_from_slice(&line[copied..]);
    Cow::Owned(out)
}

/// Whether the string after `before` is the value of a key `WHOLE` names.
fn names_whole(before: &[u8]) -> bool {
    let before = before.trim_ascii_end();
    let Some(before) = before.strip_suffix(b":") else { return false };
    let before = before.trim_ascii_end();
    WHOLE.iter().any(|key| before.ends_with(key))
}

/// The most of `s` (a JSON string's inside) up to `max` bytes that ends between two whole
/// characters: not inside an escape (a surrogate pair is one), nor a UTF-8 sequence.
fn boundary(s: &[u8], max: usize) -> usize {
    let (mut i, mut last) = (0, 0);
    while i < s.len() {
        let len = match s[i] {
            b'\\' if s.get(i + 1) == Some(&b'u') => {
                let high = s.get(i + 2..i + 6).is_some_and(|hex| {
                    matches!(hex[0].to_ascii_lowercase(), b'd')
                        && matches!(hex[1].to_ascii_lowercase(), b'8'..=b'9' | b'a'..=b'b')
                });
                if high && s.get(i + 6..i + 8) == Some(b"\\u") { 12 } else { 6 }
            }
            b'\\' => 2,
            b if b < 0x80 => 1,
            b if b >= 0xF0 => 4,
            b if b >= 0xE0 => 3,
            _ => 2,
        };
        if i + len > max {
            break;
        }
        i += len;
        last = i;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn parsed(line: &[u8]) -> Value {
        serde_json::from_slice(&lean(line)).expect("still JSON")
    }

    #[test]
    fn short_lines_and_short_strings_are_left_alone() {
        let line = json!({"type": "user", "message": {"content": "hi"}}).to_string();
        assert!(matches!(lean(line.as_bytes()), Cow::Borrowed(_)));
        let many = json!({"content": vec!["x".repeat(100); 400]}).to_string();
        assert!(many.len() > LEAN_ABOVE);
        assert!(matches!(lean(many.as_bytes()), Cow::Borrowed(_)), "no one string is long");
    }

    #[test]
    fn long_strings_keep_their_start_and_the_rest_of_the_entry() {
        let output = format!("first line\n{}", "test case ... ok\n\"quoted\" \\ é ✓ 😀\n".repeat(5_000));
        let entry = json!({"type": "user", "timestamp": "2026-10-19T10:00:00Z",
                           "message": {"content": [{"type": "tool_result", "content": output}]}, "after": "kept"});
        let lean = parsed(entry.to_string().as_bytes());
        let kept = lean["message"]["content"][0]["content"].as_str().unwrap();
        // KEEP counts the bytes as written, escapes and all.
        assert!(kept.len() <= KEEP && kept.len() > KEEP / 2, "{}", kept.len());
        assert!(output.starts_with(kept));
        assert_eq!(lean["message"]["content"][0]["type"], "tool_result");
        assert_eq!((&lean["timestamp"], &lean["after"]), (&entry["timestamp"], &entry["after"]));
    }

    /// `s` as a JSON string with every character past ASCII written as `\u` escapes (as
    /// some writers do), astral ones as surrogate pairs.
    fn ascii_json(s: &str) -> String {
        let mut out = String::from("\"");
        let json = serde_json::to_string(s).unwrap();
        for c in json[1..json.len() - 1].chars() {
            if c.is_ascii() {
                out.push(c);
            } else {
                c.encode_utf16(&mut [0; 2]).iter().for_each(|u| out.push_str(&format!("\\u{u:04x}")));
            }
        }
        out + "\""
    }

    #[test]
    fn cuts_between_whole_characters_and_escapes() {
        for unit in ["é", "✓", "😀", "\\", "\"", "\n", "\u{1}", "\u{2028}"] {
            for pad in 0..12 {
                let text = format!("{}{}", "a".repeat(pad), unit.repeat(LEAN_ABOVE));
                for body in [serde_json::to_string(&text).unwrap(), ascii_json(&text)] {
                    let line = format!("{{\"content\":{body}}}");
                    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["content"], text.as_str());
                    let kept = parsed(line.as_bytes());
                    let kept = kept["content"].as_str().unwrap();
                    assert!(text.starts_with(kept), "{unit:?} after {pad}");
                    // A `\u0001` is six bytes written, one read.
                    assert!(kept.len() > KEEP / 8, "{unit:?} after {pad}: {}", kept.len());
                }
            }
        }
    }

    #[test]
    fn arguments_stay_whole() {
        let arguments = json!({"cmd": format!("cat <<EOF\n{}\nEOF", "x".repeat(LEAN_ABOVE))}).to_string();
        let entry = json!({"type": "response_item", "payload": {"type": "function_call", "name": "exec_command", "arguments": arguments, "output": "y".repeat(LEAN_ABOVE)}});
        let lean = parsed(entry.to_string().as_bytes());
        assert_eq!(lean["payload"]["arguments"], entry["payload"]["arguments"]);
        assert_eq!(lean["payload"]["output"].as_str().unwrap().len(), KEEP);
    }

    #[test]
    fn broken_lines_come_back_as_they_are() {
        let line = format!("{{\"content\":\"{}", "x".repeat(LEAN_ABOVE));
        assert!(matches!(lean(line.as_bytes()), Cow::Borrowed(_)));
    }
}
