# agenttail

**What every coding agent on this machine is doing right now**: thinking, running a tool,
waiting for your answer, perhaps waiting for an approval, done, or idle. Read from the
transcripts Claude Code, Codex and Cursor already write, so there's nothing to install into
the agents and nothing to configure.

Status: **pre-release, private.** The sans-IO core, the `fs` tracker, the CLI and the
real-harness rig are in place. Permission modes, firm turn ends, interrupts, errors and
background helpers are implemented; the harness CI matrix and StatusWall adoption are next.
See [PLAN.md](PLAN.md) and [issue #1](https://github.com/tsconfigdotjson/agenttail/issues/1).

## Status is the headline

For each session you get a `SessionState`:

| `activity` | when (today's rules) | `certainty` |
|---|---|---|
| `awaiting_answer` | the last entry calls an ask-the-user tool (`AskUserQuestion`, `ExitPlanMode`, `request_user_input`, Cursor's ask tools) | firm |
| `awaiting_approval` | a tool call has had no result for 20 s **and the mode can ask a human**: perhaps a permission prompt | **inferred** |
| `running_tool` | a tool call is waiting for its result, including under bypass/`dontAsk`/`never`, an SDK entrypoint or an AI approval reviewer | firm |
| `thinking` | a prompt was sent, or a tool's result came back | firm |
| `done` | the turn ended normally (Claude `end_turn`/`turn_duration`/`agents_killed`; Codex `task_complete`/`turn_complete`; Cursor `turn_ended`) | firm |
| `interrupted` | the user interrupted or aborted (Claude's interrupt text, Codex `turn_aborted`, Cursor's abort status/user-stop string) | firm |
| `errored` | the harness recorded an error ending the turn (Claude's synthetic API row, Codex `task_complete.error`, Cursor's error status and message) | firm |
| `background_work` | the turn ended, but a subagent or background task is still writing (Claude's `pendingBackgroundAgentCount` makes this firm; a helper's own transcript makes it inferred) | firm / **inferred** |
| `idle` | nothing written for 15 min, or nothing read yet | firm |

Each state also carries the harness's last `permission` mode (Claude Code and Codex), the
`background` helpers still running, and, for `errored`, the harness's `error` kind/message.

Each state also says *why* (`reason`, one value per rule), and carries the project, branch,
title, cost (where the harness records it), token usage (`input`, `output`, cache read and
cache write, each `None` when the harness doesn't record it), the last few activity lines
(`› fix the build`, `$ cargo test`, `Edit routing.ts`), and diagnostics (lines skipped, lines
cut). The thresholds and the number of activity lines are options.

## Using it

```rust
use agenttail::{ChunkCtx, Harness, ReducerOptions, SessionReducer, Timestamp};
use std::path::Path;

// Sans-IO: bytes in, state out. Feed chunks as they arrive (a file you tail, frames
// from another machine); only complete lines are read, and the rest is held back.
let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
s.feed(bytes, ChunkCtx { offset: 0, mtime, path: Path::new(transcript) })?;
let state = s.state(now); // `now` is yours: the library never reads the clock
println!("{:?} because {:?} ({:?})", state.activity, state.reason, state.certainty);
```

With the `fs` feature (on by default), a `Tracker` finds and tails every harness's
transcripts under a home folder:

```rust
use agenttail::{Filter, Timestamp, Tracker};
use std::time::{Duration, SystemTime};

let mut t = Tracker::new(home, Filter::within(Duration::from_secs(3600)));
for (key, state) in t.poll(Timestamp::from(SystemTime::now())) {
    println!("{} {:?}", key.path.display(), state.activity);
}
```

Each harness's layout is data, from `spec/discovery/*.toml` embedded at build time
(`Harness::layout()`: root, depth, sessions and helpers folders, Codex's day folders), so a
remote shell loop can be generated from it and can't disagree with the local walk. The same
files hold the ask-user names, observed never-prompt tools, mode vocabulary, timestamp rules,
and quirks with their evidence.

The CLI, for looking at what a dashboard would show:

```
agenttail ls [--hours N] [--agents claude_code,codex,cursor]   # one poll, JSON lines
agenttail watch [--hours N] [--agents …]                       # each change, with its reason
```

## What it reads, and what it touches

- **Reads**, read-only: `~/.claude/projects/**.jsonl` (Claude Code; `CLAUDE_CONFIG_DIR`
  moves it),
  `~/.codex/sessions/YYYY/MM/DD/*.jsonl` (Codex; `CODEX_HOME` moves it; compressed
  `.jsonl.zst` history is skipped), `~/.cursor/projects/*/agent-transcripts/**.jsonl` (Cursor). Only the bytes
  appended since the last poll are read; a session's first read goes 1 MB at a time.
- **Does not read** `~/.claude/sessions/<pid>.json`: the A3 registry run showed `waiting`
  also covers dialogs and a SIGKILL leaves a stale file, so it is evidence, not a source
  (see DECISIONS.md). No pid liveness probe is made.
- **Touches nothing**: no writes anywhere, no hooks or config changes in any agent, no
  subprocesses, **no network**. Symlinks are not followed.
- **Keeps** a small state per session in memory (never the transcript), plus the start of
  a line still being written.

## Harnesses

| harness | where | verified against |
|---|---|---|
| Claude Code | `~/.claude/projects/<project>/<id>.jsonl` | a scrubbed 2.1 recording |
| Codex CLI | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | scrubbed 0.160.1 recordings |
| Cursor agent | `~/.cursor/projects/<project>/agent-transcripts/<id>/<id>.jsonl` | a handmade transcript in the shape seen on a real Mac |

## Building

Rust 1.88 or later (edition 2024). `cargo test --all-features`; `cargo bench -p agenttail
--bench first_read` reads a synthetic 500 MB Claude Code transcript.

See [DECISIONS.md](DECISIONS.md) for what was decided and why, and
[QUESTIONS.md](QUESTIONS.md) for what's open.
