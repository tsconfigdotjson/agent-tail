# OpenCode

OpenCode is the coding agent from `anomalyco/opencode` (the `opencode-ai` npm package). This
lane follows that current CLI, not the archived 2025 `opencode-ai/opencode` Go implementation.
This document records what 1.19.4 actually writes, verified with the real CLI against the
harness mock on Linux on 2026-10-25. Facts that come only from source are marked as such.

## Paths and layout

OpenCode keeps sessions in **SQLite**, not append-only JSONL:

```
$XDG_DATA_HOME/opencode/opencode.db              # default: ~/.local/share/opencode/opencode.db
$XDG_DATA_HOME/opencode/opencode-<channel>.db    # channel builds
$OPENCODE_DB                                     # explicit override
```

- The data root is `~/.local/share/opencode` on every OS (the XDG default, including macOS
  and Windows with `%USERPROFILE%`). `XDG_DATA_HOME` moves it; `opencode/` is appended.
- `OPENCODE_DB` names the database. `:memory:` means no persisted database; an absolute path
  wins; a relative path is resolved below the data root
  (`packages/core/src/database/database.ts` in 1.19.4).
- The database is opened from a private copy with its `-wal`/`-shm` sidecars, read-only, and
  the copy is removed afterwards. Rows are rewritten in place as a turn progresses, so the
  tracker treats the database stamp as the change signal rather than an append offset.

`opencode run` is fully non-interactive and can be pointed at a local OpenAI-compatible
provider through `OPENCODE_CONFIG_CONTENT` (the provider's `npm` is
`@ai-sdk/openai-compatible`). `OPENCODE_DISABLE_MODELS_FETCH=1`, `OPENCODE_PURE=1`,
`OPENCODE_DISABLE_AUTOUPDATE=1`, `OPENCODE_DISABLE_AUTOCOMPACT=1` and
`OPENCODE_DISABLE_PROJECT_CONFIG=1` keep a run offline and isolated. The harness driver also
sets `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` under its
temporary home.

## Database catalogue (1.19.4)

The observed runs used the **legacy `message`/`part` tables**. The v2 `session_message` table
exists in the same database but stayed empty for every `opencode run` in the spike.

| table | rows that matter |
|---|---|
| `session` | `id`, `parent_id`, `directory` (absolute cwd), `title`, `version`, cost and token totals, `time_created`/`time_updated` (Unix milliseconds), `permission` ruleset, `agent`, `model`, `time_compacting` |
| `message` | `id`, `session_id`, `time_created`/`time_updated`, and `data` JSON with `role` (`user`/`assistant`), model ids, token usage and, on the final assistant row, `finish` (`stop`, `tool-calls`, …) and `time.completed` |
| `part` | `id`, `message_id`, `session_id`, `time_created`/`time_updated`, and `data` JSON. The types seen were `text`, `reasoning`, `tool`, `step-start` and `step-finish`; `compaction`, `file`, `patch`, `snapshot` and `subtask` are also defined |
| `session_message` | v2 projection; empty in the 1.19.4 runs, kept as `Unknown` rather than silently ignored if a future version uses it |

A tool part carries its mutable state:

```json
{"type":"tool","tool":"bash","callID":"call_mock_0_0",
 "state":{"status":"running","input":{"command":"sleep 8","description":"wait"},
          "time":{"start":1792894054773}}}
```

`status` moves `pending` → `running` → `completed` or `error`. A completed state carries the
input, output and start/end times; an error carries an error object. A `step-finish` part
carries `reason`: `tool-calls` ends a model step but not the turn; `stop` ends the turn.

A user's text is not on the user message row. It is a `text` part belonging to that message.
For `opencode run`, a single argument that contains spaces is stored with surrounding JSON
quotes (`run "say hello"` becomes `"say hello"`); passing the words as separate argv entries
avoids that CLI quirk.

## Status signals

- `session.directory` (and assistant `path.cwd`) is the project; the last one read wins.
- `session.title` is the title. `opencode run` asks the model for a title on a separate
  request whose system prompt starts `You are a title generator.` The mock answers that
  request without consuming a scenario step.
- A user `text` part starts a turn (`Thinking`).
- A `tool` part with state `pending`/`running` is `RunningTool` while it is fresh. The tool
  name `question` and `plan_exit` ask the user (`AwaitingAnswer`, firm) because their part
  stays pending while the question is up.
- A completed/error tool part is a tool result. A `step-finish` with `reason: "stop"` is
  `Done`; `error`/`failed` is `Errored`; `aborted`/`interrupted`/`cancelled` is
  `Interrupted`.
- **Approval prompts are not persisted.** `permission.asked` / `permission.v2.asked` are
  ephemeral events in the permission service, not rows. Only the pending/running tool part is
  on disk. After the 20 s stall threshold the status is therefore `AwaitingApproval` with
  **inferred** certainty, and a `question` tool is the only firm "needs you" signal. The
  session's `permission` column is a ruleset (`allow`/`ask`/`deny` by action and pattern), not
  a mode string, so the reader does not invent a permission mode.
- Subagent sessions have a non-null `session.parent_id`. The tracker rebuilds each top-level
  session from its rows and feeds direct child sessions as helpers, so a child still working
  after the parent's turn ends is `BackgroundWork`. Nested grandchildren are not followed yet.

## Harness CI

The harness driver installs the real npm CLI, starts it under a PTY with a temporary home, and
points its `test/test-model` provider at the in-process mock's OpenAI **Chat Completions**
endpoint (`/v1/chat/completions`). The driver waits for the database to update, samples the
library state every 100 ms, and exports the database rows to `transcripts/<session-id>.jsonl`
for scoring and survey. Scenarios: `opencode_plain_qa` (text turn, firm done) and
`opencode_tool` (a two-second bash call sampled as `RunningTool`, then firm done). The CI
matrix runs a pinned **1.19.4** and npm **latest**; a latest failure is a failing job, not a
silent skip, and the pinned job tells a regression from an upstream change.

```sh
cd harness-ci
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -p harness-driver -- --scenario driver/scenarios/opencode_plain_qa.json
cargo run -p harness-driver -- --scenario driver/scenarios/opencode_tool.json
```

The offline fixtures under `fixtures/opencode/1.19.4/linux/` are exported, redacted rows:
`plain-qa` (a complete text turn), `tool` (a completed bash call and the following answer),
`tool-running` (a live snapshot during `sleep 8`, including the 20 s stall checkpoint),
`question` (a pending `question` part is firm `AwaitingAnswer`) and `interrupted` (a
`step-finish` reason `aborted` is firm `Interrupted`, not a completed partial answer). The
fixture row stream is the same `{"table": ..., "row": ...}` shape the reader emits.

## Open questions

- The v2 `session_message` projection and event log are unread; 1.19.4 did not write them for
  `opencode run`. A latest-version drift that switches to them should fail the harness job and
  produce a new fixture, rather than being guessed at.
- A real interactive TUI approval (not `--auto` / non-interactive `run`) has not been captured.
  The pending/running tool part is the on-disk evidence, but no approval row exists to make it
  firm.
