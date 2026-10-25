//! Fixture capture and redaction: turn a transcript into a reviewable, deterministic
//! fixture without sending its content anywhere.
//!
//! Redaction works on JSONL records when they parse and on plain text otherwise. A record's
//! field order, array order and line count are kept. Values are rewritten in this order:
//! secret-valued keys, private keys/auth headers, home paths, URL/SSH hosts, emails,
//! secret patterns and high-entropy tokens. `--lorem` replaces prose values with lorem of
//! the same character length (whitespace is kept), after the sensitive-value checks.
//!
//! Every run writes a report with replacement counts and a byte-offset remap per line. The
//! report never contains an original secret or home path; it names only the placeholder.

use agenttail::{Harness, HelperOwner, Role};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How to redact.
#[derive(Clone, Debug, Default)]
pub struct RedactOptions {
    /// Replace prose values with lorem of the same length.
    pub lorem: bool,
}

/// One placeholder substituted for a sensitive value.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub line: u64,
    pub kind: String,
    pub placeholder: String,
}

/// Where a line's bytes moved because redaction changed its length.
#[derive(Clone, Debug, Serialize)]
pub struct OffsetRemap {
    pub line: u64,
    pub before_start: u64,
    pub before_end: u64,
    pub after_start: u64,
    pub after_end: u64,
    pub delta: i64,
}

/// What redaction did to one input. Counts are by category; no original value is recorded.
#[derive(Clone, Debug, Serialize)]
pub struct RedactReport {
    pub format: u32,
    /// The fixture-relative name only; never the source path, so the report can be committed.
    pub file: String,
    pub lorem: bool,
    pub lines: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub replacements: BTreeMap<String, u64>,
    pub offsets: Vec<OffsetRemap>,
    pub findings: Vec<Finding>,
    pub malformed_lines: Vec<u64>,
}

impl RedactReport {
    pub fn count(&self, kind: &str) -> u64 {
        self.replacements.get(kind).copied().unwrap_or(0)
    }

    pub fn sensitive(&self) -> u64 {
        ["secret_key", "secret_pattern", "secret_entropy", "home_path", "user"]
            .iter()
            .map(|kind| self.count(kind))
            .sum()
    }
}

/// A redacted file and its report.
pub struct Redaction {
    pub bytes: Vec<u8>,
    pub report: RedactReport,
}

/// Redact one file's bytes. `input` is a label for the report (usually a path).
pub fn redact_bytes(input: &[u8], options: &RedactOptions, input_label: &str) -> Redaction {
    let mut redactor = Redactor::new(options);
    let mut out = Vec::with_capacity(input.len());
    let mut offsets = Vec::new();
    let mut before_start = 0u64;
    let mut after_start = 0u64;
    let mut line_no = 0u64;
    let mut at = 0usize;
    while at < input.len() {
        let end = input[at..].iter().position(|&b| b == b'\n').map_or(input.len(), |i| at + i + 1);
        let raw = &input[at..end];
        let (body, ending) = split_ending(raw);
        line_no += 1;

        let redacted = match std::str::from_utf8(body) {
            Ok(text) if !text.trim().is_empty() => match parse_json_values(text) {
                Some(values) => {
                    let mut redacted = Vec::new();
                    for (index, value) in values.into_iter().enumerate() {
                        if index > 0 {
                            redacted.push(b' ');
                        }
                        let value = redactor.value(value, None, line_no);
                        redacted.extend(serde_json::to_vec(&value).expect("JSON values serialize"));
                    }
                    redacted
                }
                None => {
                    redactor.malformed_lines.push(line_no);
                    redactor.text(text, None, line_no, false).into_bytes()
                }
            },
            Ok(_) => body.to_vec(),
            Err(_) => {
                redactor.malformed_lines.push(line_no);
                redactor.text(&String::from_utf8_lossy(body), None, line_no, false).into_bytes()
            }
        };

        out.extend_from_slice(&redacted);
        out.extend_from_slice(ending);
        let before_end = before_start + raw.len() as u64;
        let after_end = out.len() as u64;
        offsets.push(OffsetRemap {
            line: line_no,
            before_start,
            before_end,
            after_start,
            after_end,
            delta: after_end as i64 - before_end as i64,
        });
        before_start = before_end;
        after_start = after_end;
        at = end;
    }

    let report = RedactReport {
        format: 1,
        file: input_label.into(),
        lorem: options.lorem,
        lines: line_no,
        bytes_before: input.len() as u64,
        bytes_after: out.len() as u64,
        replacements: redactor.replacements,
        offsets,
        findings: redactor.findings,
        malformed_lines: redactor.malformed_lines,
    };
    let _ = after_start;
    Redaction { bytes: out, report }
}

/// Every JSON value on one physical line, with only whitespace between them. The event
/// parser also accepts several objects on one line; redaction must not fall back to a
/// context-free text pass just because a harness wrote two.
fn parse_json_values(text: &str) -> Option<Vec<Value>> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let mut values = Vec::new();
    loop {
        let before = stream.byte_offset();
        match stream.next() {
            None => {
                if text[before..].trim().is_empty() {
                    break;
                }
                return None;
            }
            Some(Ok(value)) => values.push(value),
            Some(Err(_)) => return None,
        }
    }
    (!values.is_empty()).then_some(values)
}

fn split_ending(raw: &[u8]) -> (&[u8], &[u8]) {
    if let Some(body) = raw.strip_suffix(b"\r\n") {
        (body, &raw[body.len()..])
    } else if let Some(body) = raw.strip_suffix(b"\n") {
        (body, &raw[body.len()..])
    } else {
        (raw, &[])
    }
}

struct Redactor {
    options: RedactOptions,
    replacements: BTreeMap<String, u64>,
    findings: Vec<Finding>,
    malformed_lines: Vec<u64>,
    /// Token value -> placeholder. Keeping the map stable means the same secret in two
    /// records gets the same placeholder, while the report still never names the value.
    secrets: BTreeMap<String, String>,
    path_keys: BTreeMap<String, String>,
}

impl Redactor {
    fn new(options: &RedactOptions) -> Self {
        Redactor {
            options: options.clone(),
            replacements: BTreeMap::new(),
            findings: Vec::new(),
            malformed_lines: Vec::new(),
            secrets: BTreeMap::new(),
            path_keys: BTreeMap::new(),
        }
    }

    fn bump(&mut self, kind: &str, line: u64, placeholder: &str) {
        *self.replacements.entry(kind.into()).or_default() += 1;
        self.findings.push(Finding { line, kind: kind.into(), placeholder: placeholder.into() });
    }

    fn secret(&mut self, value: &str, kind: &str, line: u64) -> String {
        if let Some(placeholder) = self.secrets.get(value) {
            let placeholder = placeholder.clone();
            self.bump(kind, line, &placeholder);
            return placeholder;
        }
        let placeholder = format!("<secret-{:04}>", self.secrets.len() + 1);
        self.secrets.insert(value.into(), placeholder.clone());
        self.bump(kind, line, &placeholder);
        placeholder
    }

    fn value(&mut self, value: Value, key: Option<&str>, line: u64) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(&text, key, line, false)),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.value(item, key, line)).collect()),
            Value::Object(map) => {
                let mut out = Map::new();
                for (item_key, item_value) in map {
                    let new_key = self.key(&item_key, line);
                    let new_value = self.value(item_value, Some(&item_key), line);
                    let mut unique = new_key.clone();
                    let mut n = 2;
                    while out.contains_key(&unique) {
                        unique = format!("{new_key}#{n}");
                        n += 1;
                    }
                    out.insert(unique, new_value);
                }
                Value::Object(out)
            }
            other => other,
        }
    }

    fn key(&mut self, key: &str, line: u64) -> String {
        if looks_like_absolute_path(key) {
            if let Some(placeholder) = self.path_keys.get(key) {
                let placeholder = placeholder.clone();
                self.bump("path_key", line, &placeholder);
                return placeholder;
            }
            let placeholder = format!("<path-{:04}>", self.path_keys.len() + 1);
            self.path_keys.insert(key.into(), placeholder.clone());
            self.bump("path_key", line, &placeholder);
            return placeholder;
        }
        self.text(key, Some("<key>"), line, true)
    }

    fn text(&mut self, text: &str, key: Option<&str>, line: u64, is_key: bool) -> String {
        if !is_key && key.is_some_and(is_secret_key) {
            if is_placeholder(text) {
                return text.into();
            }
            return self.secret(text, "secret_key", line);
        }
        if !is_key && (text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----")) {
            return self.secret(text, "secret_pattern", line);
        }
        if !is_key && key.is_some_and(is_user_key) && !text.trim().is_empty() {
            if is_placeholder(text) {
                return text.into();
            }
            self.bump("user", line, "<user>");
            return "<user>".into();
        }
        if !is_key && key.is_some_and(is_host_key) && !text.trim().is_empty() && !text.starts_with('<') {
            self.bump("host", line, "<host>");
            return "<host>".into();
        }

        let original = text;
        let mut out = text.to_string();
        out = self.auth_headers(&out, line);
        out = self.home_paths(&out, line);
        out = self.urls(&out, line);
        out = self.emails(&out, line);
        out = self.ipv4(&out, line);
        let entropy = !is_key && !key.is_some_and(is_path_or_id_key);
        out = self.secrets_in(&out, line, entropy);

        if self.options.lorem
            && !is_key
            && key.is_some_and(is_prose_key)
            && out == original
            && !original.trim().is_empty()
        {
            let lorem = lorem_like(original);
            *self.replacements.entry("lorem".into()).or_default() += 1;
            return lorem;
        }
        out
    }

    fn auth_headers(&mut self, text: &str, line: u64) -> String {
        let lower = text.to_ascii_lowercase();
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < text.len() {
            let next = ["bearer ", "basic "]
                .iter()
                .filter_map(|scheme| lower[at..].find(scheme).map(|i| (at + i, scheme.len())))
                .min_by_key(|(i, _)| *i);
            let Some((start, scheme_len)) = next else {
                out.push_str(&text[at..]);
                break;
            };
            out.push_str(&text[at..start + scheme_len]);
            let token_start = start + scheme_len;
            let mut token_end = token_start;
            while token_end < text.len() && is_token_byte(text.as_bytes()[token_end]) {
                token_end += 1;
            }
            let token = &text[token_start..token_end];
            if token.is_empty() || token.starts_with('<') {
                out.push_str(token);
            } else {
                let placeholder = self.secret(token, "secret_pattern", line);
                out.push_str(&placeholder);
            }
            at = token_end;
        }
        out
    }

    fn home_paths(&mut self, text: &str, line: u64) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < text.len() {
            if let Some(len) = root_home(&text[at..]) {
                out.push_str("/home/<user>");
                self.bump("home_path", line, "<user>");
                at += len;
                continue;
            }
            let Some(prefix_len) = home_prefix(&text[at..]) else {
                let ch = text[at..].chars().next().expect("at is a character boundary");
                out.push(ch);
                at += ch.len_utf8();
                continue;
            };
            out.push_str(&text[at..at + prefix_len]);
            let name_start = at + prefix_len;
            let mut name_end = name_start;
            while name_end < text.len() {
                let b = text.as_bytes()[name_end];
                if b == b'/'
                    || b == b'\\'
                    || b.is_ascii_whitespace()
                    || matches!(b, b'"' | b'\'' | b'<' | b'>' | b'|' | b',' | b';' | b')' | b']' | b'}')
                {
                    break;
                }
                name_end += 1;
            }
            let name = &text[name_start..name_end];
            if name.is_empty() || name.starts_with('<') || name == "user" {
                out.push_str(name);
            } else {
                out.push_str("<user>");
                self.bump("home_path", line, "<user>");
            }
            at = name_end;
        }
        out
    }

    fn urls(&mut self, text: &str, line: u64) -> String {
        let lower = text.to_ascii_lowercase();
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < text.len() {
            let next = ["http://", "https://", "ssh://", "ftp://", "file://"]
                .iter()
                .filter_map(|scheme| lower[at..].find(scheme).map(|i| at + i))
                .min();
            let Some(start) = next else {
                out.push_str(&text[at..]);
                break;
            };
            let scheme_end = start + lower[start..].find("://").unwrap() + 3;
            let mut authority_end = scheme_end;
            while authority_end < text.len() {
                let b = text.as_bytes()[authority_end];
                if b.is_ascii_whitespace()
                    || matches!(b, b'/' | b'?' | b'#' | b'"' | b'\'' | b')' | b']' | b'}' | b'<' | b'>' | b',')
                {
                    break;
                }
                authority_end += 1;
            }
            out.push_str(&text[at..scheme_end]);
            let authority = &text[scheme_end..authority_end];
            out.push_str(&self.authority(authority, line));
            at = authority_end;
        }
        out
    }

    fn authority(&mut self, authority: &str, line: u64) -> String {
        if authority.is_empty() {
            return String::new();
        }
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };
        let mut out = String::new();
        if let Some(userinfo) = userinfo {
            if let Some((user, password)) = userinfo.split_once(':') {
                if user.starts_with('<') {
                    out.push_str(user);
                } else {
                    out.push_str("<user>");
                    self.bump("host", line, "<user>");
                }
                out.push(':');
                if password.starts_with('<') {
                    out.push_str(password);
                } else {
                    let placeholder = self.secret(password, "secret_pattern", line);
                    out.push_str(&placeholder);
                }
            } else if userinfo.starts_with('<') {
                out.push_str(userinfo);
            } else {
                out.push_str("<user>");
                self.bump("host", line, "<user>");
            }
            out.push('@');
        }
        let (host, port) = split_host_port(hostport);
        if host.is_empty() || host.starts_with('<') || host == "localhost" || host == "127.0.0.1" || host == "[::1]" {
            out.push_str(host);
        } else {
            if host.starts_with('[') {
                out.push_str("[<host>]");
            } else {
                out.push_str("<host>");
            }
            self.bump("host", line, "<host>");
        }
        out.push_str(port);
        out
    }

    fn emails(&mut self, text: &str, line: u64) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < bytes.len() {
            if bytes[at] != b'@' {
                let ch = text[at..].chars().next().expect("character boundary");
                out.push(ch);
                at += ch.len_utf8();
                continue;
            }
            let mut start = at;
            while start > 0 && is_email_local(bytes[start - 1]) {
                start -= 1;
            }
            let mut end = at + 1;
            while end < bytes.len() && is_email_domain(bytes[end]) {
                end += 1;
            }
            let local = &text[start..at];
            let domain = &text[at + 1..end];
            if local.is_empty()
                || local.starts_with('<')
                || !domain.contains('.')
                || domain.starts_with('.')
                || domain.ends_with('.')
            {
                let ch = text[at..].chars().next().expect("character boundary");
                out.push(ch);
                at += ch.len_utf8();
                continue;
            }
            // The local part and domain were copied character by character; drop them.
            let already = out.len().saturating_sub(local.len());
            out.truncate(already);
            out.push_str("<email>");
            self.bump("email", line, "<email>");
            at = end;
        }
        out
    }

    fn ipv4(&mut self, text: &str, line: u64) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < bytes.len() {
            if bytes[at].is_ascii_digit() && (at == 0 || !bytes[at - 1].is_ascii_digit()) {
                let mut end = at;
                let mut groups = 0;
                loop {
                    let start = end;
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                    if end == start || end - start > 3 {
                        break;
                    }
                    groups += 1;
                    if groups == 4 {
                        break;
                    }
                    if end < bytes.len() && bytes[end] == b'.' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                let candidate = &text[at..end];
                let mut parts = candidate.split('.');
                let valid = groups == 4
                    && parts.clone().count() == 4
                    && parts.all(|part| part.len() <= 3 && part.parse::<u16>().is_ok());
                if valid && (end == bytes.len() || !matches!(bytes[end], b'.' | b'/')) {
                    out.push_str("<host>");
                    self.bump("host", line, "<host>");
                    at = end;
                    continue;
                }
            }
            let ch = text[at..].chars().next().expect("character boundary");
            out.push(ch);
            at += ch.len_utf8();
        }
        out
    }

    fn secrets_in(&mut self, text: &str, line: u64, entropy: bool) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < bytes.len() {
            if !is_token_byte(bytes[at]) {
                let ch = text[at..].chars().next().expect("character boundary");
                out.push(ch);
                at += ch.len_utf8();
                continue;
            }
            let mut end = at;
            while end < bytes.len() && is_token_byte(bytes[end]) {
                end += 1;
            }
            let raw = &text[at..end];
            let (trim_start, trim_end) = trim_token(raw);
            let start = at + trim_start;
            let end = at + trim_end;
            if start > at {
                out.push_str(&text[at..start]);
            }
            let token = &text[start..end];
            if token.is_empty() || token.starts_with('<') {
                out.push_str(token);
            } else if let Some(kind) = token_secret_kind(token, entropy) {
                let placeholder = self.secret(token, kind, line);
                out.push_str(&placeholder);
            } else {
                out.push_str(token);
            }
            if end < at + raw.len() {
                out.push_str(&text[end..at + raw.len()]);
            }
            at += raw.len();
        }
        out
    }
}

fn is_placeholder(text: &str) -> bool {
    text.starts_with('<') && text.ends_with('>')
}

fn home_prefix(text: &str) -> Option<usize> {
    for prefix in ["/home/", "/Users/", "/users/", "/var/home/", "/usr/home/"] {
        if text.starts_with(prefix) {
            return Some(prefix.len());
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 9
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
        && bytes[3..9].eq_ignore_ascii_case(b"Users\\")
    {
        return Some(9);
    }
    if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"\\Users\\") {
        return Some(8);
    }
    None
}

/// The root account's home has no username segment to replace: rewrite the whole `/root`
/// prefix so it still reads as an absolute path.
fn root_home(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if !bytes.starts_with(b"/root") {
        return None;
    }
    let next = bytes.get(5).copied();
    next.is_none_or(|b| {
        b == b'/'
            || b == b'\\'
            || b.is_ascii_whitespace()
            || matches!(b, b'"' | b'\'' | b',' | b';' | b')' | b']' | b'}')
    })
    .then_some(5)
}

fn split_host_port(hostport: &str) -> (&str, &str) {
    if hostport.starts_with('[')
        && let Some(end) = hostport.find(']')
    {
        return (&hostport[..=end], &hostport[end + 1..]);
    }
    if let Some(colon) = hostport.rfind(':')
        && hostport[colon + 1..].bytes().all(|b| b.is_ascii_digit())
    {
        return (&hostport[..colon], &hostport[colon..]);
    }
    (hostport, "")
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'=' | b'.')
}

fn trim_token(token: &str) -> (usize, usize) {
    let bytes = token.as_bytes();
    let mut start = 0;
    while start < bytes.len() && matches!(bytes[start], b'.' | b'-' | b'=') {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && matches!(bytes[end - 1], b'.' | b'-' | b'=') {
        end -= 1;
    }
    (start, end)
}

fn token_secret_kind(token: &str, entropy: bool) -> Option<&'static str> {
    const PREFIXES: &[(&str, usize)] = &[
        ("sk-ant-", 20),
        ("sk-", 20),
        ("ghp_", 20),
        ("gho_", 20),
        ("ghs_", 20),
        ("ghu_", 20),
        ("ghr_", 20),
        ("github_pat_", 20),
        ("glpat-", 20),
        ("xoxb-", 10),
        ("xoxp-", 10),
        ("xoxa-", 10),
        ("xoxr-", 10),
        ("xoxs-", 10),
        ("npm_", 20),
        ("pypi-", 20),
        ("hf_", 20),
        ("AKIA", 16),
        ("ASIA", 16),
        ("AIza", 20),
        ("ya29.", 20),
        ("sk_live_", 12),
        ("rk_live_", 12),
        ("SG.", 20),
    ];
    for (prefix, min) in PREFIXES {
        if let Some(at) = token.find(prefix)
            && token.len() >= at + prefix.len() + min
        {
            return Some("secret_pattern");
        }
    }
    if let Some(at) = token.find("eyJ")
        && token[at..].matches('.').count() >= 2
        && token.len() - at >= 32
    {
        return Some("secret_pattern");
    }
    if is_kebab_identifier(token) {
        return None;
    }
    if entropy && token.len() >= 24 && !is_uuid_like(token) && !is_path_like(token) && entropy_bits(token) >= 3.5 {
        let has_lower = token.bytes().any(|b| b.is_ascii_lowercase());
        let has_upper = token.bytes().any(|b| b.is_ascii_uppercase());
        let has_digit = token.bytes().any(|b| b.is_ascii_digit());
        let has_symbol = token.bytes().any(|b| !b.is_ascii_alphanumeric());
        let classes = u8::from(has_lower) + u8::from(has_upper) + u8::from(has_digit) + u8::from(has_symbol);
        if classes >= 2 && (has_digit || has_symbol || (has_lower && has_upper && token.len() >= 32)) {
            return Some("secret_entropy");
        }
    }
    None
}

fn entropy_bits(text: &str) -> f64 {
    let mut counts = [0usize; 256];
    for b in text.bytes() {
        counts[b as usize] += 1;
    }
    let len = text.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Relative paths and fixture ids look random to an entropy check. They are not secrets;
/// a high-entropy token that also contains uppercase, `+` or `=` is still checked.
fn is_path_like(text: &str) -> bool {
    text.contains('/')
        && text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'/' | b'-' | b'_' | b'.'))
}

/// Lower-case words joined by hyphens are names (event types, decision kinds, command
/// names), not random secrets. A token with any upper-case letter, digit or other symbol
/// still goes through the entropy check.
fn is_kebab_identifier(text: &str) -> bool {
    if !text.contains('-') {
        return false;
    }
    text.split('-').all(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

fn is_uuid_like(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn is_email_local(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
}

fn is_secret_key(key: &str) -> bool {
    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    if matches!(
        normalized.as_str(),
        "token"
            | "tokensecret"
            | "authtoken"
            | "sessiontoken"
            | "bearertoken"
            | "apikey"
            | "apikeyvalue"
            | "secret"
            | "secretkey"
            | "clientsecret"
            | "password"
            | "passwd"
            | "passphrase"
            | "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "privatekey"
            | "accesskey"
            | "signingkey"
            | "encryptionkey"
            | "credential"
            | "credentials"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
    ) {
        return true;
    }
    (normalized.ends_with("apikey")
        || normalized.ends_with("password")
        || normalized.ends_with("secret")
        || normalized.ends_with("credential"))
        && !matches!(
            normalized.as_str(),
            "tokencount"
                | "inputtokens"
                | "outputtokens"
                | "totaltokens"
                | "reasoningtokens"
                | "cachedinputtokens"
                | "maxtokens"
        )
}

fn is_user_key(key: &str) -> bool {
    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    matches!(
        normalized.as_str(),
        "user" | "username" | "login" | "loginname" | "account" | "accountname" | "owner" | "author" | "committer"
    )
}

fn is_host_key(key: &str) -> bool {
    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    matches!(normalized.as_str(), "host" | "hostname" | "server" | "serverhost" | "domain")
}

fn is_path_or_id_key(key: &str) -> bool {
    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    matches!(
        normalized.as_str(),
        "cwd"
            | "path"
            | "file"
            | "filepath"
            | "outputfile"
            | "outputfilepath"
            | "repositoryurl"
            | "url"
            | "uri"
            | "transcriptpath"
            | "projectpath"
            | "messagingsocketpath"
            | "id"
            | "uuid"
            | "sessionid"
            | "messageid"
            | "callid"
            | "tooluseid"
            | "threadid"
            | "turnid"
            | "agentid"
            | "taskid"
            | "requestid"
            | "windowid"
            | "hash"
            | "commit"
            | "commithash"
            | "digest"
            | "shasum"
            | "instructions"
            | "revision"
            | "record_ids"
            | "recordids"
            | "ids"
    ) || key.ends_with("Id")
        || key.ends_with("_id")
        || key.to_ascii_lowercase().ends_with("hash")
        || key.to_ascii_lowercase().ends_with("digest")
}

fn is_prose_key(key: &str) -> bool {
    let normalized: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect();
    matches!(
        normalized.as_str(),
        "text"
            | "content"
            | "message"
            | "prompt"
            | "description"
            | "title"
            | "lastprompt"
            | "summary"
            | "thinking"
            | "stdout"
            | "stderr"
            | "output"
            | "aggregatedoutput"
            | "formattedoutput"
            | "command"
            | "arguments"
            | "query"
            | "pattern"
            | "body"
            | "aititle"
            | "threadname"
            | "name"
    )
}

fn looks_like_absolute_path(text: &str) -> bool {
    text.starts_with('/')
        || text.starts_with('~')
        || (text.len() >= 3
            && text.as_bytes()[0].is_ascii_alphabetic()
            && text.as_bytes()[1] == b':'
            && matches!(text.as_bytes()[2], b'\\' | b'/'))
}

fn lorem_like(text: &str) -> String {
    const LOREM: &[u8] =
        b"lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ";
    let mut out = String::with_capacity(text.len());
    let mut at = 0usize;
    for ch in text.chars() {
        if ch.is_whitespace() {
            out.push(ch);
        } else {
            out.push(LOREM[at % LOREM.len()] as char);
            at += 1;
        }
    }
    out
}

pub fn write_report(path: &Path, report: &RedactReport) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// Redact every regular file under `input` into the same relative path under `out`.
/// The combined report is written to `<out>/redaction.json`; no source path is recorded in
/// it, so the report can be committed with a fixture.
pub fn redact_tree(
    input: &Path,
    out: &Path,
    options: &RedactOptions,
    report: Option<&Path>,
) -> Result<Vec<Redaction>, String> {
    if !input.is_dir() {
        return Err(format!("{}: not a directory", input.display()));
    }
    let mut redactions = Vec::new();
    for file in files_under(input)? {
        let rel = file.strip_prefix(input).unwrap_or(&file);
        let bytes = fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let redaction = redact_bytes(&bytes, options, &rel.display().to_string());
        let dest = out.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::write(&dest, &redaction.bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
        print_summary(&format!("{} -> {}", rel.display(), dest.display()), &redaction.report);
        redactions.push(redaction);
    }
    let report_path = report.map_or_else(|| out.join("redaction.json"), Path::to_path_buf);
    let reports: Vec<_> = redactions.iter().map(|redaction| redaction.report.clone()).collect();
    let text = serde_json::to_string_pretty(&report_json(&reports)).map_err(|e| e.to_string())?;
    fs::write(&report_path, text + "\n").map_err(|e| format!("{}: {e}", report_path.display()))?;
    Ok(redactions)
}

/// A short, value-free summary of a redaction for a human.
pub fn summary(label: &str, report: &RedactReport) -> String {
    let changed = report
        .offsets
        .iter()
        .filter(|offset| offset.before_end - offset.before_start != offset.after_end - offset.after_start)
        .count();
    let sensitive = report.sensitive();
    let mut counts: Vec<_> = report
        .replacements
        .iter()
        .filter(|(kind, _)| kind.as_str() != "lorem")
        .map(|(kind, count)| format!("{kind}={count}"))
        .collect();
    if report.lorem {
        counts.push(format!("lorem={}", report.count("lorem")));
    }
    format!(
        "redact: {label}: {} line(s), {} -> {} bytes, {changed} line(s) resized, {sensitive} sensitive value(s){}",
        report.lines,
        report.bytes_before,
        report.bytes_after,
        if counts.is_empty() { String::new() } else { format!(" ({})", counts.join(", ")) }
    )
}

/// Print a short, value-free summary of a redaction to a human.
pub fn print_summary(label: &str, report: &RedactReport) {
    println!("{}", summary(label, report));
}

/// Recursively scan paths for what `fixture redact` would change: secrets and home paths.
/// Emails and hosts are reported too, but not treated as failures.
pub fn scan(paths: &[PathBuf]) -> Result<usize, String> {
    let mut failures = 0usize;
    let mut warnings = 0usize;
    for path in paths {
        for file in files_under(path)? {
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            // Reports intentionally contain placeholder examples; their source is checked
            // before they are written. Everything else under fixtures/ is scanned, docs too.
            if name == "redaction.json" || name.ends_with(".report.json") {
                continue;
            }
            let bytes = fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let redaction = redact_bytes(&bytes, &RedactOptions::default(), &file.display().to_string());
            let report = redaction.report;
            for finding in &report.findings {
                if matches!(
                    finding.kind.as_str(),
                    "secret_key" | "secret_pattern" | "secret_entropy" | "home_path" | "user"
                ) {
                    failures += 1;
                    println!("{}:{}: {}", file.display(), finding.line, finding.kind);
                } else if matches!(finding.kind.as_str(), "email" | "host") {
                    warnings += 1;
                }
            }
        }
    }
    if warnings > 0 {
        eprintln!("scan: {warnings} email/host value(s) found; not counted as failures");
    }
    if failures > 0 {
        return Err(format!("{failures} secret or home-path finding(s) under fixtures/"));
    }
    println!("scan: no secrets or home paths");
    Ok(warnings)
}

fn files_under(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(out);
    }
    let entries = fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", path.display()))?;
        let child = entry.path();
        let ty = entry.file_type().map_err(|e| format!("{}: {e}", child.display()))?;
        if ty.is_symlink() {
            continue;
        } else if ty.is_dir() {
            out.extend(files_under(&child)?);
        } else if ty.is_file() {
            out.push(child);
        }
    }
    out.sort();
    Ok(out)
}

/// A session that `fixture capture` can pick.
#[derive(Clone, Debug)]
pub struct SessionCandidate {
    pub harness: Harness,
    pub path: PathBuf,
    pub id: String,
}

pub fn sessions(home: &Path) -> Result<Vec<SessionCandidate>, String> {
    let mut out = Vec::new();
    for harness in Harness::ALL {
        // OpenCode keeps sessions in SQLite, not files. The harness driver exports its rows
        // as JSONL for CI; `fixture capture` doesn't copy a live database yet.
        if harness == Harness::OpenCode {
            continue;
        }
        for root in harness_roots(harness, home) {
            if root.is_dir() {
                walk_sessions(harness, &root, &root, 0, &mut out)?;
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

/// Every root a harness may have below `home`. Most have one; Claude Desktop has consumer
/// and 3P app-data roots for each OS.
fn harness_roots(harness: Harness, home: &Path) -> Vec<PathBuf> {
    match harness {
        Harness::ClaudeCode => vec![
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map_or_else(|| home.join(".claude/projects"), |dir| dir.join("projects")),
        ],
        Harness::Codex => vec![
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map_or_else(|| home.join(".codex/sessions"), |dir| dir.join("sessions")),
        ],
        Harness::Cursor => vec![home.join(".cursor/projects")],
        Harness::ClaudeDesktop => {
            harness.discovery().roots.for_os(std::env::consts::OS).into_iter().map(|root| home.join(root)).collect()
        }
        Harness::Pi => vec![
            std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| {
                    std::env::var_os("PI_CODING_AGENT_DIR")
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .map(|dir| dir.join("sessions"))
                })
                .unwrap_or_else(|| home.join(".pi/agent/sessions")),
        ],
        Harness::OpenCode => vec![home.join(".local/share/opencode")],
        Harness::Gemini => vec![
            std::env::var_os("GEMINI_CLI_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map_or_else(|| home.join(".gemini/tmp"), |dir| dir.join(".gemini/tmp")),
        ],
        Harness::Copilot => vec![
            std::env::var_os("COPILOT_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map_or_else(|| home.join(".copilot/session-state"), |dir| dir.join("session-state")),
        ],
        _ => vec![home.join(harness.layout().root)],
    }
}

fn walk_sessions(
    harness: Harness,
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<SessionCandidate>,
) -> Result<(), String> {
    let layout = harness.layout();
    if depth > layout.walk_depth() {
        return Ok(());
    }
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| format!("{}: {e}", path.display()))?;
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            walk_sessions(harness, root, &path, depth + 1, out)?;
        } else if ty.is_file()
            && path.extension().is_some_and(|ext| ext == layout.session_suffix)
            && let Ok(rel) = path.strip_prefix(root)
            && matches!(layout.role(rel), Some(Role::Session))
        {
            out.push(SessionCandidate { harness, id: session_id(harness, &path), path });
        }
    }
    Ok(())
}

fn session_id(harness: Harness, path: &Path) -> String {
    match harness {
        Harness::Cursor => path
            .parent()
            .and_then(Path::file_name)
            .map_or_else(|| file_stem(path), |name| name.to_string_lossy().into_owned()),
        // Claude Desktop: <org>/local_<id>/audit.jsonl, or dispatch's local_ditto_<id>.
        Harness::ClaudeDesktop => path
            .parent()
            .and_then(Path::file_name)
            .map_or_else(|| file_stem(path), |name| name.to_string_lossy().into_owned()),
        // Pi's file is <timestamp>_<session-id>.jsonl; the timestamp has no underscore, so
        // the first one separates a custom id that may contain more.
        Harness::Pi => file_stem(path).split_once('_').map_or_else(|| file_stem(path), |(_, id)| id.into()),
        Harness::Copilot => path
            .parent()
            .and_then(Path::file_name)
            .map_or_else(|| file_stem(path), |name| name.to_string_lossy().into_owned()),
        _ => file_stem(path),
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map_or_else(|| path.display().to_string(), |stem| stem.to_string_lossy().into_owned())
}

/// The files that make up a session: its main transcript and any helper transcripts.
pub fn session_files(candidate: &SessionCandidate) -> Vec<PathBuf> {
    let mut files = vec![candidate.path.clone()];
    let layout = candidate.harness.layout();
    let Some(helpers) = layout.helpers_in else { return files };
    let helper_dir = match layout.helper_owner {
        HelperOwner::SessionFolder => candidate.path.parent().map(|parent| parent.join(helpers)),
        HelperOwner::SessionFileStem => {
            let mut owner = candidate.path.clone();
            owner.set_extension("");
            Some(owner.join(helpers))
        }
        _ => None,
    };
    if let Some(dir) = helper_dir
        && let Ok(entries) = fs::read_dir(dir)
    {
        let mut found: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "jsonl")
                    || path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".meta.json"))
            })
            .collect();
        found.sort();
        files.extend(found);
    }
    files
}

/// Where a session file goes inside a fixture's `input/`.
pub fn input_relative(candidate: &SessionCandidate, file: &Path) -> PathBuf {
    if file == candidate.path {
        return PathBuf::from("session.jsonl");
    }
    let layout = candidate.harness.layout();
    let Some(helpers) = layout.helpers_in else {
        return Path::new("subagents").join(file.file_name().unwrap_or_default());
    };
    let helper_dir = match layout.helper_owner {
        HelperOwner::SessionFolder => candidate.path.parent().map(|parent| parent.join(helpers)),
        HelperOwner::SessionFileStem => {
            let mut owner = candidate.path.clone();
            owner.set_extension("");
            Some(owner.join(helpers))
        }
        _ => None,
    };
    if let Some(dir) = helper_dir.as_deref()
        && let Ok(rel) = file.strip_prefix(dir)
    {
        return Path::new("subagents").join(rel);
    }
    Path::new("subagents").join(file.file_name().unwrap_or_default())
}

pub fn version_of(candidate: &SessionCandidate) -> String {
    let Ok(text) = fs::read_to_string(&candidate.path) else { return "unknown".into() };
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
        let version = match candidate.harness {
            Harness::ClaudeCode => record.get("version").and_then(Value::as_str),
            Harness::Codex => {
                record.pointer("/payload/cli_version").or_else(|| record.get("cli_version")).and_then(Value::as_str)
            }
            Harness::Cursor => record.get("version").and_then(Value::as_str),
            // Pi's session header version is the file-format version, not the CLI version.
            Harness::Pi => None,
            Harness::OpenCode => record.pointer("/row/version").and_then(Value::as_str),
            Harness::Copilot => record.pointer("/data/copilotVersion").and_then(Value::as_str),
            _ => None,
        };
        if let Some(version) = version {
            return version.into();
        }
    }
    "unknown".into()
}

pub fn utc_date() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs() as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn sanitize_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "captured".into() } else { out.chars().take(48).collect() }
}

pub fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub fn report_json(reports: &[RedactReport]) -> Value {
    json!({
        "format": 1,
        "files": reports,
        "replacements": reports.iter().fold(BTreeMap::<String, u64>::new(), |mut all, report| {
            for (kind, count) in &report.replacements {
                *all.entry(kind.clone()).or_default() += count;
            }
            all
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(text: &str) -> Redaction {
        redact_bytes(text.as_bytes(), &RedactOptions::default(), "test.jsonl")
    }

    fn redact_lorem(text: &str) -> Redaction {
        redact_bytes(text.as_bytes(), &RedactOptions { lorem: true }, "test.jsonl")
    }

    fn value_of(redaction: &Redaction) -> Value {
        serde_json::from_slice(&redaction.bytes).unwrap()
    }

    #[test]
    fn keeps_field_order_and_line_count() {
        let input = "{\"z\":1,\"a\":\"/home/alice/project\",\"m\":[1,2]}\n{\"b\":2,\"a\":1}\n";
        let got = redact(input);
        let text = String::from_utf8(got.bytes).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("{\"z\":1,\"a\":\"/home/<user>/project\",\"m\":[1,2]}"), "{text}");
        assert!(text.contains("{\"b\":2,\"a\":1}"), "{text}");
        assert_eq!(got.report.lines, 2);
        assert_eq!(got.report.offsets.len(), 2);
    }

    #[test]
    fn home_paths_usernames_and_hosts_become_placeholders() {
        let input = concat!(
            r#"{"cwd":"/home/alice/work/app","win":"C:\\Users\\bob\\app","mac":"/Users/carol/app"}"#,
            "\n",
            r#"{"url":"https://alice:hunter2@example.com:8443/x","email":"alice@real.example","ssh":"git@github.com:org/repo"}"#,
            "\n",
        );
        let got = redact(input);
        let text = String::from_utf8(got.bytes).unwrap();
        let values: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(values[0]["cwd"], "/home/<user>/work/app");
        assert_eq!(values[0]["win"], "C:\\Users\\<user>\\app");
        assert_eq!(values[0]["mac"], "/Users/<user>/app");
        assert_eq!(values[1]["url"], "https://<user>:<secret-0001>@<host>:8443/x");
        assert!(!text.contains("hunter2"));
        assert_eq!(values[1]["email"], "<email>");
        assert_eq!(values[1]["ssh"], "<email>:org/repo");
        assert!(got.report.count("home_path") >= 3);
        assert!(got.report.count("host") >= 2);
        assert!(got.report.count("email") >= 2);
    }

    #[test]
    fn root_home_any_windows_drive_and_username_fields_are_redacted() {
        let input = concat!(
            r#"{"user":"alice","username":"bob","root":"/root/.ssh/id_ed25519","win":"D:\\Users\\carol\\app","var":"/var/home/dave/app"}"#,
            "\n",
            r#"{"email":"alice@example.com","logical":"a long ZxQwErTyUiOpAsDfGhJkLzXcVbNmQwErTyBlob"}"#,
            "\n",
        );
        let got = redact(input);
        let values: Vec<Value> =
            String::from_utf8(got.bytes).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(values[0]["user"], "<user>");
        assert_eq!(values[0]["username"], "<user>");
        assert_eq!(values[0]["root"], "/home/<user>/.ssh/id_ed25519");
        assert_eq!(values[0]["win"], "D:\\Users\\<user>\\app");
        assert_eq!(values[0]["var"], "/var/home/<user>/app");
        assert_eq!(values[1]["email"], "<email>");
        assert!(values[1]["logical"].as_str().unwrap().contains("<secret-"));
        assert!(got.report.count("user") >= 2);
        assert!(got.report.count("home_path") >= 3);
        assert!(got.report.count("email") >= 1);
        assert!(got.report.count("secret_entropy") >= 1);
    }

    #[test]
    fn two_json_objects_on_one_line_are_both_redacted() {
        let got = redact("{\"token\":\"abc\"}{\"cwd\":\"/home/alice/x\"}\n");
        let text = String::from_utf8(got.bytes).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains("abc"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        let values: Vec<Value> = text
            .split("} {")
            .enumerate()
            .map(|(i, part)| {
                let part = match i {
                    0 => format!("{part}}}"),
                    _ => format!("{{{part}"),
                };
                serde_json::from_str(&part).unwrap()
            })
            .collect();
        assert_eq!(values[0]["token"], "<secret-0001>");
        assert_eq!(values[1]["cwd"], "/home/<user>/x");
        assert!(got.report.malformed_lines.is_empty());
    }

    #[test]
    fn secret_prefixes_and_entropy_are_found_inside_env_style_text() {
        let got = redact(
            "{\"note\":\"OPENAI_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345\",\"blob\":\"ZxQwErTyUiOpAsDfGhJkLzXcVbNmQwErTyBlob\"}\n",
        );
        let values = Value::Array(
            String::from_utf8(got.bytes).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect(),
        );
        assert!(values[0]["note"].as_str().unwrap().contains("<secret-"));
        assert!(values[0]["blob"].as_str().unwrap().contains("<secret-"));
        assert!(got.report.count("secret_pattern") >= 1);
        assert!(got.report.count("secret_entropy") >= 1);
    }

    #[test]
    fn secret_keys_patterns_and_entropy_are_redacted_without_looking_like_usage() {
        let input = concat!(
            r#"{"api_key":"abc","token":"abcd","safe_token_count":12,"usage":{"input_tokens":10,"output_tokens":4},"note":"sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345"}"#,
            "\n",
            r#"{"body":"random A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0","headers":{"authorization":"Bearer abcdefghijklmnopqrstuvwxyz"}}"#,
            "\n",
        );
        let got = redact(input);
        let values: Vec<Value> =
            String::from_utf8(got.bytes).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(values[0]["api_key"], "<secret-0001>");
        assert_eq!(values[0]["token"], "<secret-0002>");
        assert_eq!(values[0]["safe_token_count"], 12);
        assert_eq!(values[0]["usage"]["input_tokens"], 10);
        assert!(values[0]["note"].as_str().unwrap().starts_with("<secret-"));
        assert!(values[1]["body"].as_str().unwrap().contains("<secret-"));
        assert!(values[1]["headers"]["authorization"].as_str().unwrap().starts_with("<secret-"));
        assert_eq!(got.report.count("secret_key"), 3);
        assert!(got.report.count("secret_pattern") >= 1);
        assert!(got.report.count("secret_entropy") >= 1);
    }

    #[test]
    fn kebab_case_decision_kinds_are_not_secrets() {
        let kind = "denied-interactively-by-user";
        let got = redact(&format!("{{\"result\":{{\"kind\":\"{kind}\"}}}}\n"));
        assert_eq!(value_of(&got)["result"]["kind"], kind);
        assert_eq!(got.report.count("secret_entropy"), 0);

        let prose = format!("the result kind is {kind}\n");
        let got = redact(&prose);
        assert_eq!(String::from_utf8(got.bytes).unwrap(), prose);
        assert_eq!(got.report.count("secret_entropy"), 0);
    }

    #[test]
    fn lorem_has_the_same_length_and_keeps_whitespace() {
        let input = "{\"text\":\"Hello, world!\\nSecond line here.\",\"path\":\"/home/alice/x\"}\n";
        let got = redact_lorem(input);
        let values = value_of(&got);
        let text = values["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), "Hello, world!\nSecond line here.".chars().count());
        assert!(text.contains('\n'));
        assert!(!text.contains("Hello"));
        assert_eq!(values["path"], "/home/<user>/x");
        assert_eq!(got.report.count("lorem"), 1);
    }

    #[test]
    fn report_offsets_track_resized_lines_and_never_name_the_secret() {
        let input = "{\"a\":\"/home/alice/x\"}\n{\"token\":\"supersecretvalue\"}\n";
        let got = redact(input);
        assert_eq!(got.report.offsets[0].before_start, 0);
        assert_eq!(got.report.offsets[0].after_start, 0);
        assert_eq!(got.report.offsets[1].before_start, got.report.offsets[0].before_end);
        assert_eq!(got.report.offsets[1].after_start, got.report.offsets[0].after_end);
        let report = serde_json::to_string(&got.report).unwrap();
        assert!(!report.contains("supersecretvalue"));
        assert!(!report.contains("/home/alice"));
        assert!(report.contains("<secret-0001>"));
    }

    #[test]
    fn malformed_text_is_still_redacted_and_counted() {
        let got = redact("secret /home/alice/x token sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345");
        let text = String::from_utf8(got.bytes).unwrap();
        assert!(!text.contains("alice"));
        assert!(!text.contains("sk-ant-"));
        assert_eq!(got.report.malformed_lines, [1]);
    }

    #[test]
    fn is_deterministic() {
        let input = "{\"token\":\"a-secret\",\"path\":\"/Users/sam/x\",\"text\":\"hi\"}\n";
        let a = redact(input);
        let b = redact(input);
        assert_eq!(a.bytes, b.bytes);
        assert_eq!(serde_json::to_string(&a.report).unwrap(), serde_json::to_string(&b.report).unwrap());
    }

    #[test]
    fn scan_fails_on_real_values_but_passes_placeholders() {
        let dir = std::env::temp_dir().join(format!("agenttail-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("bad.jsonl"),
            "{\"token\":\"sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345\",\"cwd\":\"/home/alice/x\"}\n",
        )
        .unwrap();
        assert!(scan(std::slice::from_ref(&dir)).is_err());
        fs::write(
            dir.join("good.jsonl"),
            "{\"token\":\"<secret-0001>\",\"cwd\":\"/home/<user>/x\",\"email\":\"<email>\"}\n",
        )
        .unwrap();
        fs::remove_file(dir.join("bad.jsonl")).unwrap();
        assert!(scan(std::slice::from_ref(&dir)).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn utc_date_has_the_expected_shape() {
        let date = utc_date();
        assert_eq!(date.len(), 10);
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
    }

    #[test]
    fn sanitizes_scenario_names() {
        assert_eq!(sanitize_component("A plain QA!"), "a-plain-qa");
        assert_eq!(sanitize_component("..."), "captured");
    }

    #[test]
    fn claude_desktop_session_ids_are_the_local_folder() {
        let path = Path::new("/h/.config/Claude/local-agent-mode-sessions/u/o/local_abc/audit.jsonl");
        assert_eq!(session_id(Harness::ClaudeDesktop, path), "local_abc");
        let dispatch = Path::new("/h/.config/Claude/local-agent-mode-sessions/u/o/agent/local_ditto_def/audit.jsonl");
        assert_eq!(session_id(Harness::ClaudeDesktop, dispatch), "local_ditto_def");
    }

    #[test]
    fn pi_session_ids_keep_custom_underscores() {
        let path = Path::new("/h/.pi/agent/sessions/--w-app--/2026-10-25T01-00-00-000Z_my_session_1.jsonl");
        assert_eq!(session_id(Harness::Pi, path), "my_session_1");
        let uuid = Path::new("/h/.pi/agent/sessions/--w-app--/2026-10-25T01-00-00-000Z_01a170a4.jsonl");
        assert_eq!(session_id(Harness::Pi, uuid), "01a170a4");
    }
}
