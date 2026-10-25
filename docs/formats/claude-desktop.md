# Claude Desktop: local agent mode

Claude Desktop has two transcript surfaces. The Code tab writes an ordinary Claude Code
transcript; local agent mode (Chat / Cowork / agent sessions) writes an `audit.jsonl` stream
plus a sidecar. `agent-tail` reads the latter through the `claude_desktop` adapter and leaves
Code-tab transcripts to the [Claude Code adapter](claude-code.md).

Support status is **ported-unverified**: the adapter, discovery spec, and handmade fixtures
exist, and the scheduled Xvfb lane can score an ephemeral live Desktop turn when
`OPENROUTER_API_KEY` is configured, but no redacted Desktop capture is committed yet.

## Two surfaces, two readers

| Surface | Artifacts | Read by `agent-tail` |
|---|---|---|
| Code tab | Desktop metadata under app-data `claude-code-sessions/`; the transcript is a Claude Code transcript at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` | The Claude Code adapter once the transcript appears there. Desktop metadata is not read. |
| Cowork / agent / Chat (local agent mode) | app-data `local-agent-mode-sessions/…/local_<id>.json` plus `local_<id>/audit.jsonl` or `<first-8-hex>/audit.jsonl` (HMAC-chained) | The `claude_desktop` adapter and discovery spec. Dispatch sessions at `<org>/agent/local_ditto_<id>/audit.jsonl` are read as their own sessions. |

The Code tab must not be re-read through `claude_desktop`, and local agent mode must not be
sent to the Claude Code project walker.

## App-data roots

The discovery file is [`spec/discovery/claude-desktop.toml`](../../spec/discovery/claude-desktop.toml).
`Tracker` scans every platform root for the OS it runs on. The Linux consumer path is the
single `roots.default` for tooling that shows one root; it is not a claim that it is the only
root.

| Build | macOS | Linux | Windows |
|---|---|---|---|
| Consumer | `~/Library/Application Support/Claude/local-agent-mode-sessions/` | `~/.config/Claude/local-agent-mode-sessions/` | `%APPDATA%\Claude\local-agent-mode-sessions\` (unverified) |
| 3P / gateway | `~/Library/Application Support/Claude-3p/local-agent-mode-sessions/` | `~/.config/Claude-3p/local-agent-mode-sessions/` | `%LOCALAPPDATA%\Claude-3p\local-agent-mode-sessions\` |

The 3P roots are vendor-documented. The consumer paths still need verification on a real
install. `%APPDATA%` and `%LOCALAPPDATA%` can be redirected; the spec uses home-relative
defaults and does not yet handle a redirected profile.

## Session layout

```text
<root>/<user>/<org>/local_<uuid>/audit.jsonl
<root>/<user>/<org>/local_<uuid>.json                 # sidecar metadata
<root>/<user>/<org>/<first-8-hex>/audit.jsonl         # short session folder
<root>/<user>/<org>/agent/local_ditto_<uuid>/audit.jsonl
<root>/<user>/<org>/agent/local_ditto_<uuid>.json     # dispatch metadata
```

The discovery layout requires `audit.jsonl` inside a folder whose name starts with `local_`
or is the first eight hex characters of the session id, allows five folders below the root
(so dispatch's extra `agent/` level fits), and does not walk other files as sessions. The
short folder is not a different session shape: recent Desktop builds choose
`<first-8-hex>` for a new session, while the sidecar stays `local_<uuid>.json` in the parent
folder. A missing or malformed sidecar does not fail the session; the audit log still parses
with its own `system/init` facts.

Dispatch sessions (`local_ditto_*`) are delegated background agents. They are not helpers of
an interactive session here: each has its own audit log and is reported as its own session.
Linking a dispatch run back to the interactive session that spawned it is not implemented.

## Sidecar: `local_<id>.json`

The sidecar carries facts the audit stream does not repeat. The fields this adapter uses are:

| Field | Use |
|---|---|
| `sessionId`, `cliSessionId` | session identity |
| `cwd` | project name (last path component) |
| `title` | display title, source `other` |
| `model` | model-change event |
| `permissionMode` | permission mode when present |
| `lastActivityAt`, `createdAt` | session facts only; not used as message clocks |

Other fields seen in public artifacts (`sessionType`, `initialMessage`, `hostLoopMode`,
`memoryEnabled`, `skillsEnabled`, `pluginsEnabled`, `slashCommands`) are not consumed yet.
The tracker reads the sidecar when it appears or changes, so a title or cwd written after the
first poll is picked up.

## What the adapter reads

`audit.jsonl` is the Claude Agent SDK's stream-json shape. The shared Claude record reader
(`crates/agent-tail/src/adapters/claude.rs`) already knows the conversation records, so the
Desktop adapter reuses it rather than copying it. Desktop-specific handling:

| Record | Mapping |
|---|---|
| `system` / `subtype: init` | shared reader parses `cwd`, `session_id`, `permissionMode`; this adapter adds `model` as a model change and keeps the record as `meta` |
| `user` | prompt, or `tool_result` blocks (including the SDK's snake-case root `tool_use_result` payload) |
| `assistant` | text, reasoning, tool calls; `stop_reason: end_turn` is a firm turn end |
| `result` | turn end / errors / interruption, session `total_cost_usd`, and session-total usage; `terminal_reason: aborted_streaming`/`aborted_tools` is `interrupted` |
| `rate_limit_event`, `tool_use_summary` | known `meta`, never `unknown` |
| anything else | `unknown`, kept whole, never dropped |

The SDK message records carry `uuid` and `session_id` but no timestamp. Status times therefore
fall back to `audit.jsonl`'s mtime, exactly as a Claude Code line without a timestamp does. A
`timestamp` is used when one is present; the fixtures do not invent one.

The sidecar's `lastActivityAt` is not used to move `since`/`last_at`: a metadata write can be
later than the last message, and making it the turn clock would turn a finished session back
into work.

## Status

The shared status rules apply unchanged:

- a prompt or tool result is `thinking`;
- an accepted `end_turn` / `result` is `done`;
- a `result` with `is_error`, `errors`, or an SDK error subtype is `errored`;
- an aborted `result` is `interrupted`;
- no writes for 15 minutes is `idle`;
- a pending tool is `running_tool` while the recorded mode cannot ask a human
  (`bypassPermissions`, `dontAsk`), and `awaiting_approval` after the shared 20 second stall
  **inference** when it can.

Unlike Copilot CLI, the audit stream has no persisted `permission.requested` /
`permission.completed` pair, so a pending tool cannot be firm `awaiting_approval` from a
permission record. The `tool-stall` fixture pins the honest result: firm `running_tool` at
0 seconds and inferred `awaiting_approval` at 25 seconds. If an audit ever contains an
ask-user tool, it is currently treated as an ordinary pending tool; the discovery spec keeps
`asks_user` empty rather than guessing.

## Fixtures and tests

Under `fixtures/claude-desktop/handmade/any/`:

| Fixture | What it pins |
|---|---|
| `plain-qa` | init, prompt, assistant `end_turn`, result frame; working → done → idle after 15 minutes |
| `tool-turn` | pending `Bash` call is firm `running_tool`; result returns to thinking; result frame closes it done |
| `tool-stall` | default mode: pending call is firm running at 0 seconds and inferred `awaiting_approval` at 25 seconds; bypass mode is covered by a unit test |
| `error` | a terminal `result` with `is_error` and `errors[]` is firm errored with the message kept |
| `interrupted` | a `result` with `terminal_reason: aborted_streaming` is firm interrupted even though the subtype is `success` |

They are **handmade**, synthesized from the public Claude Agent SDK record shape, not captured
from a real Desktop install. The adapter also has unit tests for the sidecar, error/abort
results, no-prompt modes, and the shared record parser.

What is still missing: a committed redacted artifact from the scheduled live lane, a
reproducible scored Desktop turn from that artifact, and verification of the consumer/3P
roots on macOS, Linux, and Windows. The #108 lane checked local disk and artifacts for an
already-captured audit log and found none; it did not start an auth-dependent Desktop chase.
