# agenttail

**What every coding agent on this machine is doing right now**: thinking, running a tool,
waiting for your answer, perhaps waiting for an approval, done, or idle. Read from the
sessions Claude Code, Codex, Cursor, Pi, OpenCode, Gemini CLI and GitHub Copilot CLI
already write, so there's nothing to install into the agents and nothing to configure.

Status: **pre-release, private.** The sans-IO core, the `fs` tracker, the CLI and the
real-harness rig are in place. Permission modes, firm turn ends, interrupts, errors and
background helpers are implemented; harness CI covers Claude Code, Codex, OpenCode, Pi,
Gemini CLI and Copilot CLI, while Cursor still needs a donated live-approval capture.
StatusWall adoption is next.
See [PLAN.md](PLAN.md) and [issue #1](https://github.com/tsconfigdotjson/agenttail/issues/1).

## Status is the headline

For each session you get a `SessionState`:

| `activity` | when (today's rules) | `certainty` |
|---|---|---|
| `awaiting_answer` | the last entry calls an ask-the-user tool (`AskUserQuestion`, `ExitPlanMode`, `request_user_input`, Cursor's ask tools, OpenCode's `question`/`plan_exit`) | firm |
| `awaiting_approval` | a persisted permission request is pending (Copilot's `permission.requested`), or a tool call has had no result for 20 s **and the mode can ask a human**: perhaps a permission prompt | firm (Copilot) / **inferred** |
| `running_tool` | a tool call is waiting for its result, including under bypass/`dontAsk`/`never`, an SDK entrypoint or an AI approval reviewer | firm |
| `thinking` | a prompt was sent, a tool's result came back, or Cursor's composer status is `generating` | firm |
| `done` | the turn ended normally (Claude `end_turn`/`stop_hook_summary`/`turn_duration`/`agents_killed`, or a rejected tool use; Codex `task_complete`/`turn_complete`; Cursor `turn_ended` or `composerData.status: completed`; OpenCode's `step-finish` reason `stop`; Gemini's final text message; Copilot's final assistant message/`session.shutdown`) | firm |
| `interrupted` | the user interrupted or aborted (Claude's interrupt text, Codex `turn_aborted`, Cursor's `turn_ended` or `composerData.status: aborted`, OpenCode's aborted `step-finish`, Copilot `abort`) | firm |
| `errored` | the harness recorded an error ending the turn (Claude's synthetic API row, Codex `task_complete.error`, Cursor's error status and message, OpenCode's failed `step-finish`/assistant error, Gemini's `type: error`) | firm |
| `background_work` | the turn ended, but a subagent or background task is still writing (Claude's `pendingBackgroundAgentCount` makes this firm; a helper's own transcript makes it inferred) | firm / **inferred** |
| `idle` | nothing written for 15 min | firm |
| `unknown` | nothing that moves the state has been read yet: an empty or metadata-only session. A dashboard can map this to its idle look (StatusWall adoption is #16) | firm |

Each state also carries the harness's last `permission` mode (Claude Code and Codex) and
Codex `sandbox` policy, the `background` helpers still running, and, for `errored`, the
harness's `error` kind/message. For Cursor, the `sqlite` feature (on by default) reads the
IDE's `state.vscdb` from a private snapshot: `composerData.name` becomes the title, and
`composerData.status` can firm up thinking/done/interrupted where the transcript is older.
For OpenCode the same feature reads a private snapshot of `opencode.db`; its permission
prompts are ephemeral, so a pending tool stays `AwaitingApproval` only as an inference.

Each state also says *why* (`reason`, one value per rule), and carries the project, branch,
title, cost (where the harness records it), token usage (`input`, `output`, cache read and
cache write, each `None` when the harness doesn't record it), the last few activity lines
(`› fix the build`, `$ cargo test`, `Edit routing.ts`), and diagnostics (lines skipped, lines
cut). The thresholds and the number of activity lines are options.

Under status is the event layer: `parse_lines(harness, bytes, mode)` yields normalized
`Event`s (`UserPrompt`, `ToolCall`/`ToolResult`, `TurnEnd`, `Usage`, `PermissionMode`,
`Title`, `HelperSpawn`/`HelperEnd`, `Compaction`, `Meta`, `Unknown`, …) with byte/line
provenance, the harness's own record ids, and deterministic ids. The reducer folds the same
events, so a status and the event dump cannot disagree. Claude Code's sidechain rows are
still emitted, with `Event::sidechain` set; a parent reducer ignores them while
`feed_helper` applies them to the helper's own state. Pi's abandoned branches are likewise
emitted with `Event::off_chain`, which the reducer ignores; only the active leaf's parent
chain moves a Pi session. Unknown records and malformed lines are kept as `Unknown` rather
than dropped.

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
agenttail ls [--hours N] [--agents claude_code,codex,cursor,pi,opencode,gemini,copilot]  # one poll, JSON lines
agenttail watch [--hours N] [--agents …]                                  # each change, with its reason
agenttail events [--agent ID] <input-dir|path>                            # canonical JSON events
agenttail status <input-dir> --checkpoints <file>                         # status conformance
agenttail survey [--drift]                                                # shape vs spec/baseline
agenttail support-matrix [--check]                                        # regenerate support-matrix.md
agenttail fixture redact|capture|scan …                        # offline fixture tooling
```

`fixture capture` picks a local session, copies its transcript and subagents, redacts home
paths/usernames/hosts/emails and secrets, shows a value-free diff/report, and writes the
versioned fixture directory. `fixture redact` is deterministic and structure-preserving;
`fixture scan` is what the pre-commit hook and CI use. Everything is offline. See
[fixtures/README.md](fixtures/README.md) and [docs/conformance.md](docs/conformance.md) for
the language-neutral `events`/`status` contract, canonical JSON and `survey --drift`. The
frozen v0.1 shapes live in [`spec/schema/`](spec/schema/) and
[`spec/status.md`](spec/status.md); committed expected events and statuses are validated
against them by the test suite.

## What it reads, and what it touches

- **Reads**, read-only: `~/.claude/projects/**.jsonl` (Claude Code; `CLAUDE_CONFIG_DIR`
  moves it),
  `~/.codex/sessions/YYYY/MM/DD/*.jsonl` and `~/.codex/session_index.jsonl` (Codex;
  `CODEX_HOME` moves both; compressed `.jsonl.zst` history is skipped),
  `~/.cursor/projects/*/agent-transcripts/**.jsonl` and Cursor's
  `User/globalStorage/state.vscdb` (Cursor),
  `~/.pi/agent/sessions/<encoded-cwd>/*.jsonl` (Pi; `PI_CODING_AGENT_DIR` and
  `PI_CODING_AGENT_SESSION_DIR` move it), and
  `~/.local/share/opencode/opencode.db` (OpenCode; `XDG_DATA_HOME` moves it and `OPENCODE_DB`
  names the file), `~/.gemini/tmp/<project>/chats/*.jsonl` (Gemini CLI; `GEMINI_CLI_HOME`
  moves it), and `~/.copilot/session-state/<id>/events.jsonl` (Copilot CLI; `COPILOT_HOME`
  moves it). Claude Code subagent transcripts and their sibling `.meta.json` are read too. Only the bytes appended since the last poll are read; a session's first read goes 1 MB
  at a time. Cursor's and OpenCode's databases are never opened in place: they are copied with
  `-wal`/`-shm` into a private temporary folder, opened `mode=ro`, and that folder is deleted
  afterwards. Cursor's copy is throttled to once every 30 s; OpenCode's is taken when its
  stamp changes, because its rows are rewritten as a turn progresses.
- **Does not read** `~/.claude/sessions/<pid>.json`: the A3 registry run showed `waiting`
  also covers dialogs and a SIGKILL leaves a stale file, so it is evidence, not a source
  (see DECISIONS.md). No pid liveness probe is made.
- **Touches nothing** in the agents: no hooks, config changes or subprocesses, and **no
  network**. Symlinks are not followed. The only writes are private, read-only database
  snapshots (Cursor's `state.vscdb` and OpenCode's `opencode.db`) in the system temp folder,
  removed before the read returns.
- **Keeps** a small state per session in memory (never the raw transcript), plus the start
  of a line still being written. Pi additionally keeps the lean-parsed entries it needs to
  replay the active branch when a leaf changes; normal appends do not re-read the file.

## Harnesses

| harness | where | verified against |
|---|---|---|
| Claude Code | `~/.claude/projects/<project>/<id>.jsonl` | a scrubbed 2.1 recording |
| Codex CLI | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | scrubbed 0.160.1 recordings |
| Cursor agent | `~/.cursor/projects/<project>/agent-transcripts/<id>/<id>.jsonl`, plus the IDE's `state.vscdb` | a handmade transcript and synthetic SQLite rows in the shape seen on a real Mac; live approval waits still need a donated capture |
| Pi | `~/.pi/agent/sessions/<encoded-cwd>/<timestamp>_<id>.jsonl` | Pi 1.4.0 (pinned and npm latest at the time); `PI_CODING_AGENT_DIR` and `PI_CODING_AGENT_SESSION_DIR` overrides; no approval prompt, so a pending tool is firm `RunningTool`; only the active leaf's parent chain moves status/usage/title, with abandoned branches flagged `off_chain` in the event dump |
| OpenCode | `~/.local/share/opencode/opencode.db` (`XDG_DATA_HOME`, `OPENCODE_DB`) | a live 1.19.4 `opencode run` against the harness mock, with exported/redacted row fixtures; approvals remain inferred because they are not persisted |
| Gemini CLI | `~/.gemini/tmp/<project>/chats/session-*.jsonl` (`GEMINI_CLI_HOME`) | live 0.65.0 runs against the harness mock; tool calls are written only after completion, so no pending approval is on disk |
| Copilot CLI | `~/.copilot/session-state/<id>/events.jsonl` (`COPILOT_HOME`) | live 1.0.103 runs through its BYOK mock provider; `permission.requested`/`permission.completed` make approval waits firm, the prompt-mode unattended denial is captured, and hand-made fixtures cover the interactive user-approve/user-deny completion shapes |

## Building

Rust 1.88 or later (edition 2024). `cargo test --all-features`; `cargo bench -p agenttail
--bench first_read` reads a synthetic 500 MB Claude Code transcript.

The first-read performance gate builds that bench and runs it under GNU `/usr/bin/time`;
it fails if the `Lean` read takes 3.0 s or more, or peak RSS reaches 50 MB:

```sh
PATH="$HOME/.cargo/bin:$PATH" harness-ci/ci/first-read-budget.sh
```

On Multibeast's Linux box (2 vCPU, rustc 1.88.0, 2026-10-25) it measured **0.76-1.95 s** for
Lean, **1.29-3.75 s** for Full (reported, not budgeted), and **9.4-9.9 MB** peak RSS. The
script runs the built bench executable directly, because `/usr/bin/time -v cargo bench`
measures cargo's RSS rather than the bench child's. See
[harness-ci/README.md](harness-ci/README.md#a8-first-read-performance-budget-2026-10-25).

See [DECISIONS.md](DECISIONS.md) for what was decided and why, and
[QUESTIONS.md](QUESTIONS.md) for what's open.
