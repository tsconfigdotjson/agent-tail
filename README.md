# agent-tail

[![Harness CI](https://github.com/tsconfigdotjson/agent-tail/actions/workflows/harness-ci.yml/badge.svg)](https://github.com/tsconfigdotjson/agent-tail/actions/workflows/harness-ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**What every coding agent on your machine is doing right now.**

`agent-tail` reads the session transcripts that coding agents already write and turns them
into one small, current piece of state per session: thinking, running a tool, waiting for
your answer or approval, done, interrupted, errored, idle, or unknown. It is a Rust library
with a small CLI, and it works without installing hooks or changing any agent configuration.

It supports:

- Claude Code
- Claude Desktop local agent mode
- Codex
- Cursor
- Pi
- OpenCode
- Gemini CLI
- GitHub Copilot CLI

`agent-tail` is read-only and local. It makes no network calls, starts no subprocesses, and
does not write to the agents' own directories.

## Status is the headline

Each session has an `activity`, a `certainty`, and a machine-readable `reason` that explains
which rule decided it.

| `activity` | meaning |
|---|---|
| `awaiting_answer` | the last entry asks the user a question or asks for plan approval |
| `awaiting_approval` | a tool call may be waiting for a permission prompt |
| `running_tool` | a tool call is pending and cannot be waiting for a human |
| `thinking` | a prompt or tool result started model work |
| `background_work` | the turn ended, but a subagent or background task is still running |
| `done` | the turn ended normally |
| `interrupted` | the user interrupted the turn |
| `errored` | the harness recorded an error ending the turn |
| `idle` | nothing has been written for the configured idle window |
| `unknown` | no entry that moves the state has been read yet |

`certainty` is `firm` when the transcript says so and `inferred` when the activity depends
on a timing or shape heuristic. A pending tool in bypass/never-ask mode is firmly
`running_tool`; an unknown mode can only produce an inferred `awaiting_approval` after the
stall threshold.

The state also carries the project, branch, title, model, permission/sandbox information,
token usage, recorded cost, the last few activity lines, running helpers, pending scheduled
wakeups, and diagnostics.
A usage/rate-limit stop is firm `errored` with `stop_reason: "usage_limit"` and, when the
harness records one, `resets_at`; a new turn clears both.
The library never reads the clock: callers pass `now`, which makes status timelines
deterministic and testable.

## Quick start

Build from source with Rust 1.88 or newer (edition 2024). The crates are not published to
crates.io yet; `publish = false` is intentional for now.

```sh
git clone https://github.com/tsconfigdotjson/agent-tail.git
cd agent-tail
cargo build --release
./target/release/agent-tail ls
```

Install the CLI from this checkout if you want it on your `PATH`:

```sh
cargo install --path crates/agent-tail-cli
agent-tail ls
```

One poll prints JSON lines, most urgent first. Watch for changes with their reasons:

```sh
agent-tail ls --hours 2
agent-tail watch --hours 2
agent-tail ls --agents claude_code,codex,cursor
```

All commands are offline:

```text
agent-tail ls [--hours N] [--agents ID,…]
agent-tail watch [--hours N] [--agents ID,…]
agent-tail events [--agent ID] <input-dir|transcript>
agent-tail status [--agent ID] <input-dir|transcript> --checkpoints <file>
agent-tail survey [--drift] [PATH…]
agent-tail support-matrix [--check]
agent-tail fixture redact|capture|scan …
```

## Use it as a library

The core is sans-IO: bytes in, state out. Feed chunks as they arrive and ask for the state at
a time you choose.

```rust
use agent_tail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, Timestamp};
use std::path::Path;

let mut session = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
let written = Timestamp::parse_rfc3339("2026-10-23T10:00:00Z").unwrap();
session.feed(
    transcript_bytes,
    ChunkCtx { offset: 0, mtime: written, path: Path::new("session.jsonl") },
)?;
let state = session.state(written + std::time::Duration::from_secs(5));
println!("{:?} because {:?} ({:?})", state.activity, state.reason, state.certainty);
```

With the default `fs` feature, a `Tracker` finds and tails every supported harness below a
home folder:

```rust
use agent_tail::{Filter, Timestamp, Tracker};
use std::time::{Duration, SystemTime};

let mut tracker = Tracker::new(home, Filter::within(Duration::from_secs(3600)));
for (key, state) in tracker.poll(Timestamp::from(SystemTime::now())) {
    println!("{} {:?}", key.path.display(), state.activity);
}
```

Harness layouts, ask-user tool names, permission vocabulary, timestamp rules, and observed
quirks live in `spec/discovery/*.toml` and are embedded at build time. The event layer is
also available through `parse_lines`; the reducer folds the same events, so a status and an
event dump cannot disagree.

## Support matrix

The generated [support matrix](docs/support-matrix.md) records exactly what has been verified,
against which harness versions, and from which kind of evidence: CI-verified,
donated-verified, ported-unverified, spike, or unsupported.

| Harness | Current evidence |
|---|---|
| Claude Code | CI-verified |
| Codex | CI-verified |
| Pi | CI-verified |
| OpenCode | CI-verified |
| Gemini CLI | CI-verified |
| GitHub Copilot CLI | CI-verified |
| Cursor | donated-verified |
| Claude Desktop local agent mode | ported-unverified; no real capture yet |

Code-tab transcripts from Claude Desktop stay on the Claude Code adapter. Format notes for
each harness are under [docs/formats/](docs/formats/).

## What it reads, and what it touches

- **Reads, read-only:** the session files below each harness's normal home, for example
  `~/.claude/projects`, `~/.codex/sessions`, `~/.cursor/projects`, `~/.pi/agent/sessions`,
  `~/.gemini/tmp`, `~/.copilot/session-state`, and the OpenCode database under
  `~/.local/share/opencode`. Claude Code subagent transcripts and their `.meta.json` files
  are read too. Environment overrides such as `CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `PI_CODING_AGENT_DIR`, `GEMINI_CLI_HOME`, `COPILOT_HOME`, `XDG_DATA_HOME`, and
  `OPENCODE_DB` are honoured where the harness documents them.
- **Reads incrementally:** only bytes appended since the previous poll are read. A session's
  first read is chunked and memory-bounded.
- **Database snapshots:** Cursor's `state.vscdb` and OpenCode's `opencode.db` are never opened
  in place. They are copied with their `-wal`/`-shm` sidecars into a private temporary folder,
  opened read-only, and removed before the read returns.
- **Does not read** process/liveness registries as a source of truth. They are evidence only.
- **Touches nothing** in the agents: no hooks, config changes, or subprocesses. **No
  network.** Symlinks are not followed. The only writes are the temporary database snapshots
  described above.
- **Keeps** a small amount of state per session in memory, never the raw transcript, plus the
  start of a line that is still being written.

## Fixtures, conformance, and drift

The repository ships a language-neutral fixture corpus and JSON Schemas so ports and
downstream tools can check the same behavior:

- [Fixtures](fixtures/README.md): capture, redaction, checkpoints, and the committed corpus.
- [Conformance](docs/conformance.md): the `events` and `status` runner contract, canonical JSON,
  and the shape survey.
- [Spec](spec/status.md): the frozen v0.1 status model and checkpoint protocol.

All fixture captures are redacted. The pre-commit hook and Harness CI run the same offline
scanner and fail on secrets or real home paths.

### Contribute a fixture

A real, redacted capture is the most useful contribution for an unverified harness or a new
version.

```sh
# Capture a local session and redact it into the versioned fixture layout.
agent-tail fixture capture --session <path-or-id> --scenario <name> --out fixtures

# Check the result before committing.
agent-tail fixture scan fixtures

# Regenerate the expected event dump after an intentional input change.
agent-tail events fixtures/<harness>/<version>/<os>/<scenario> \
  > fixtures/<harness>/<version>/<os>/<scenario>/expected/events.jsonl
```

Add or update `meta.toml`, run `agent-tail status` against the scenario's checkpoints, and
include the generated `redaction.json` report. See [fixtures/README.md](fixtures/README.md)
for the full layout. If you cannot capture a session, a handmade fixture from a public format
description is still useful, but it will be marked `ported-unverified` until a real artifact
arrives.

## Development

```sh
cargo test --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo bench -p agent-tail --bench first_read
```

The real-harness CI lives in [harness-ci/](harness-ci/README.md). It runs the actual CLIs
against a local mock model server, under a PTY where needed, and scores `agent-tail`'s status
against what the CLI shows. The first-read budget gate builds the benchmark and checks a
500 MB synthetic transcript.

## License

MIT. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Security reports are covered by
[SECURITY.md](SECURITY.md).
