//! The first read of a huge Claude Code transcript: how long until a
//! session that has run all day has a state, in 1 MB chunks.
//!
//! `cargo bench -p agent-tail --bench first_read [-- <MB>]` (default 500). The transcript is
//! synthetic and generated as it's read, so the numbers are parsing, not the disk. Run it
//! under `/usr/bin/time -l` (macOS) or `-v` (Linux) for peak memory.

use agent_tail::{ParseMode, READ_CHUNK, ReducerOptions, SessionReducer, Timestamp, read_from};
use std::io::Read;
use std::path::Path;
use std::time::Instant;

/// A Claude Code transcript of `turns` turns, made as it's read. Each turn is a prompt, a
/// Bash call and its 256 KB output, a Read and its 64 KB file, and a 2 KB answer: tool
/// output is most of a real transcript's bytes.
struct Synthetic {
    turns: usize,
    turn: usize,
    buf: Vec<u8>,
    at: usize,
    big: String,
    medium: String,
}

impl Synthetic {
    fn new(turns: usize) -> Self {
        let line = "test result: ok. 42 passed; \"quoted\" \\ é ✓\n";
        let text = |n: usize| serde_json::to_string(&line.repeat(n / line.len())).unwrap();
        Synthetic { turns, turn: 0, buf: Vec::new(), at: 0, big: text(256 << 10), medium: text(64 << 10) }
    }

    fn fill(&mut self) {
        let (n, ts) = (self.turn, "2026-10-23T10:00:00.000Z");
        let head = format!(r#""timestamp":"{ts}","cwd":"/w/app","gitBranch":"main","sessionId":"s1""#);
        self.buf.clear();
        self.at = 0;
        let lines = [
            format!(r#"{{"type":"user",{head},"message":{{"role":"user","content":"Prompt {n}: run the tests"}}}}"#),
            format!(
                r#"{{"type":"assistant",{head},"message":{{"id":"m{n}a","stop_reason":"tool_use","usage":{{"output_tokens":40}},"content":[{{"type":"tool_use","id":"t{n}a","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user",{head},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t{n}a","content":{}}}]}}}}"#,
                self.big
            ),
            format!(
                r#"{{"type":"assistant",{head},"message":{{"id":"m{n}b","stop_reason":"tool_use","usage":{{"output_tokens":30}},"content":[{{"type":"tool_use","id":"t{n}b","name":"Read","input":{{"file_path":"/w/app/src/main.rs"}}}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user",{head},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t{n}b","content":{}}}]}}}}"#,
                self.medium
            ),
            format!(
                r#"{{"type":"assistant",{head},"message":{{"id":"m{n}c","stop_reason":"end_turn","usage":{{"output_tokens":500}},"content":[{{"type":"text","text":"{}"}}]}}}}"#,
                "All tests pass. ".repeat(128)
            ),
        ];
        for l in lines {
            self.buf.extend_from_slice(l.as_bytes());
            self.buf.push(b'\n');
        }
        self.turn += 1;
    }

    fn turn_len() -> usize {
        let mut s = Synthetic::new(1);
        s.fill();
        s.buf.len()
    }
}

impl Read for Synthetic {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.buf.len() {
            if self.turn == self.turns {
                return Ok(0);
            }
            self.fill();
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn main() {
    let mb: usize = std::env::args().skip(1).find_map(|a| a.parse().ok()).unwrap_or(500);
    let turns = (mb << 20) / Synthetic::turn_len();
    let now = Timestamp::parse_rfc3339("2026-10-23T10:05:00Z").unwrap();
    let path = Path::new("/h/.claude/projects/-w-app/s1.jsonl");

    // What making the bytes costs, to take off.
    let start = Instant::now();
    let bytes = std::io::copy(&mut Synthetic::new(turns), &mut std::io::sink()).unwrap() as usize;
    let making = start.elapsed();

    for mode in [ParseMode::Lean, ParseMode::Full] {
        let mut s = SessionReducer::new(agent_tail::Harness::ClaudeCode, ReducerOptions { mode, ..Default::default() });
        let start = Instant::now();
        read_from(&mut Synthetic::new(turns), &mut s, READ_CHUNK, now, path);
        let took = start.elapsed().saturating_sub(making);
        let st = s.state(now);
        assert_eq!(s.consumed(), bytes as u64);
        assert_eq!(st.diagnostics.lines, turns as u64 * 6);
        println!(
            "{mode:?}: {:.0} MB ({turns} turns) in {:.2} s = {:.0} MB/s; {:?}, {} lines, {} cut, {} tokens",
            bytes as f64 / 1048576.0,
            took.as_secs_f64(),
            bytes as f64 / 1048576.0 / took.as_secs_f64(),
            st.activity,
            st.diagnostics.lines,
            st.diagnostics.cut,
            st.out_tokens
        );
    }
    println!("(generating the bytes took {:.2} s, not counted)", making.as_secs_f64());
}
