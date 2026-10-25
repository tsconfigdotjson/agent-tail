# Pi session format

This note is based on read-only surveys, a scripted offline run, and harness CI for Pi
**1.4.0** on Linux, against the local Anthropic-compatible mock. The committed fixtures were
captured with `agent-tail fixture capture` from Pi 1.4.0 sessions and scanned by
`agent-tail fixture scan`. Everything below is observed on 1.4.0 unless marked *inference*.

## Paths and layout

```
~/.pi/agent/sessions/<encoded-cwd>/<timestamp>_<session-id>.jsonl
```

The encoded folder is `--` + the working folder with its leading separator removed and
`/`, `\` and `:` replaced by `-`, + `--`. Example:

```
cwd    /home/dev/work/my-app
folder --home-dev-work-my-app--
```

A session id is normally a UUID; `pi --session-id` can supply another id. The file name is
`<ISO-ish timestamp>_<session-id>.jsonl`.

Overrides, in Pi's precedence order:

| variable / option | effect |
|---|---|
| `PI_CODING_AGENT_DIR` | the config directory (default `~/.pi/agent`); sessions go below its `sessions/` |
| `PI_CODING_AGENT_SESSION_DIR` | replaces the whole session directory; files can sit directly in it, with no `<encoded-cwd>/` folder |
| `--session-dir <dir>` | CLI option with the highest precedence; same flat layout as the variable |

`PI_CODING_AGENT_DIR` and `PI_CODING_AGENT_SESSION_DIR` are documented in Pi's
[Environment Variables](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/environment-variables.md)
and were exercised offline. Pi resolves a relative value against its current working
directory; the tracker deliberately ignores relative values, as it does for Claude Code and
Codex. The tracker's env roots are ordered so `PI_CODING_AGENT_SESSION_DIR` wins over
`PI_CODING_AGENT_DIR`, matching Pi.

A session with `--no-session` is in memory and writes nothing, so the tracker cannot see it.
`/fork`, `/clone` and `/new` create ordinary session files; a forked header may carry
`parentSession`.

There is no built-in helper/subagent transcript in 1.4.0. Pi's README says it skips
sub-agents; extensions can add agent tools, but no helper file shape was observed. The layout
therefore has no `helpers_in` and no `BackgroundWork` signal from Pi.

<a id="record-catalogue"></a>
## Session entries

Each line is one JSON object. Entries form an `id`/`parentId` tree; Pi appends the active
branch and the latest entry is the current leaf. The header is the first line:

```json
{"type":"session","version":3,"id":"uuid","timestamp":"2026-10-25T01:19:17.855Z","cwd":"/work/app"}
```

| entry type | notable fields |
|---|---|
| `session` | `version` (file format, v3 here), `id`, `timestamp`, `cwd`, optional `parentSession` |
| `message` | `message.role` (`system`, `user`, `assistant`, `toolResult`, `bashExecution`, or extension `custom`), `message.timestamp` (Unix ms) |
| `model_change` | `provider`, `modelId` |
| `thinking_level_change` | `thinkingLevel` |
| `session_info` | `name`: the user-visible session title |
| `usage` | `kind` (e.g. `cache_warm`), `provider`, `model`, `usage` |
| `compaction` | `summary`, `firstKeptEntryId`, `tokensBefore`, optional `usage`, `systemMessage`, `details`, `fromHook` |
| `context_edit` | `targetId`, `replacement` (null omits the target from future model context) |
| `branch_summary` | `fromId`, `summary`, optional `usage` |
| `label` | `targetId`, `label` |
| `custom` | `customType`, `data` (extension state, not LLM context) |
| `custom_message` | `customType`, `content`, `display`, `details` |

Message content blocks are `text` (`text`), `thinking` (`thinking`, `thinkingSignature`), and
`toolCall` (`id`, `name`, `arguments`). User content is a string or `text`/`image` blocks.
Tool results are separate `message` entries with role `toolResult`, `toolCallId`, `toolName`,
`content`, `isError`, optional `usage`, `details`, `nestedCalls`.

An assistant message is the complete provider response, with `stopReason`, `usage` and an
optional `errorMessage`. `stopReason` values observed or documented:
`pending` (streaming only, not persisted), `stop`, `toolUse`, `length`, `error`, `aborted`,
and `deferred`.

Usage is per model call: `input`, `output`, `cacheRead`, `cacheWrite`, `totalTokens`, and a
`cost.total` in USD. The session total is their sum. This differs from Claude Code's
cumulative `cost-state`.

## Status signals

- **Prompt:** a `message` with role `user`. The opening prompt's first line is the fallback
  title; `session_info.name` is the user's title and wins. An empty name clears it and falls
  back to the opening prompt. Firm `Thinking` / `prompt_sent`.
- **Tool call:** an assistant message with `stopReason: toolUse` and one or more `toolCall`
  blocks. Pending calls are matched by `toolCall.id` to later `toolResult.toolCallId`, so
  parallel calls stay pending until every result arrives. Firm `RunningTool` /
  `tool_call_pending`.
- **No approval prompt.** Pi does not ask before tool calls; its built-in `bash`, `read`,
  `edit`, `write`, `grep`, `find` and `ls` tools run with the OS user's permissions. There is
  no permission-mode record and no approval event. The adapter marks Pi sessions
  `no_approval`, so a pending tool stays firm `RunningTool` however long it runs; it never
  becomes inferred `AwaitingApproval`. `SessionState.permission` stays `None`.
- **Ask-user tools:** none are built in and none were observed. Extension tools could ask,
  but no names were seen, so `spec/discovery/pi.toml` keeps the list empty rather than
  guessing.
- **Tool result:** a role `toolResult` message with the matching id. With every call
  answered, the model is `Thinking` / `tool_result_in`; with another call still pending it
  remains `RunningTool`.
- **Turn end:** assistant `stopReason: stop` (normal), or `length` (output limit) is firm
  `Done` / `turn_ended`. `aborted` is firm `Interrupted` / `interrupted`. `error` is firm
  `Errored` / `errored`, carrying `errorMessage`. `deferred` was documented but not observed;
  it is left as `Meta`, so it does not claim a finished turn.
- **Compaction:** a `compaction` entry becomes `Compaction`. Pi does not record whether it
  was manual, threshold or overflow, so `trigger` remains `None`.
- **Model changes:** `model_change` and each assistant message name the model; the last
  `model_change.modelId` is exposed as `ModelChange`.
- **Title:** `session_info.name`, or the opening prompt's first line.
- **Direct shell commands:** role `bashExecution` entries are the user's `!` command output,
  not prompts or model tool calls; they become `Meta`.
- **Custom/extension entries:** `custom`, `custom_message`, `label`, `context_edit` and
  `branch_summary` become `Meta` (branch/compaction usage is counted when the entry is on
  the active branch).

No state is inferred from timing except the existing `idle_after` rule: a Pi session whose
file was not written for 15 minutes is `Idle` like every other harness. There is no Pi
approval-waiting inference because the harness has no approval prompt.

## Version and harness CI

Pinned known-good: **1.4.0**. npm `latest` at survey time: **1.4.0**. `pi --version` prints
just the version (`1.4.0`).

`harness-ci` has a Pi path that runs the real npm CLI against the local mock model server in
`--mode json`, tails the session file with `agent-tail`, and fails on a wrong firm status or an
unexpected event shape. It runs for pinned 1.4.0 and npm latest, on Linux and macOS, so a
new release fails loudly instead of being skipped. Scenarios cover plain text, one tool,
parallel tool calls, a failing tool result, and a terminal API error. The fixtures also pin
the aborted stop reason. The Pi path is transcript/status based, not PTY-screen scored;
Pi's TUI screen reader is not part of this path. See `harness-ci/README.md`.

## Branches

The session file is an `id`/`parentId` tree, and Pi appends the active branch; the last
entry in the file is the leaf. `SessionReducer` reduces only that leaf's parent chain for
status, usage, cost, recent activity and title. An abandoned branch's tool lines, usage,
title changes and turn ends therefore do not leak into a session's state.

Pi's `branch()` only moves its in-memory leaf pointer. A switch-back becomes visible in the
file as the next appended entry whose `parentId` is the older leaf. The reducer then
rebuilds from the entries it has already parsed, retaining the restored ancestors and
dropping the abandoned ones. Normal appends stay incremental; only a changed leaf triggers
the replay. Pi's retained entries are the lean-parsed values already used for reduction,
not a second raw copy of the file.

`parse_lines` remains lossless: it emits events from every record in the file, including
abandoned branches, with `Event::off_chain: true` on those records. `SessionReducer`
ignores off-chain events, so `feed` and `feed_events(parse_lines(...))` produce the same
state. A linear Pi session has no off-chain events and behaves as before.

Compaction is just another entry on the active chain. Its `firstKeptEntryId` controls what
Pi sends to the model after compaction; it does not prune the session's status/usage chain,
so usage from active-chain ancestors before the compaction boundary remains counted, as
does the compaction entry's own usage. An off-chain compaction contributes nothing.

## Open questions and limits

- **`deferred`** stop reasons have no observed session. They are kept as `Meta` rather than
  guessed as done or running.
- **Extension ask tools** are not known. If a Pi extension adds an ask-the-user tool, the
  spec should gain its exact name with a real capture.
- **Older file versions:** Pi migrates v1/v2 sessions to v3 on load. The adapter reads the
  entry names shared by current files; an unmigrated old file was not captured.
- **No `.jsonl.zst` or compression** was seen; Pi writes plain JSONL.
