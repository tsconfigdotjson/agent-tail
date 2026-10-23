# Cursor agent-transcript format

Survey: read-only, structure only, this Mac, 2026-10-23. It covered 12 session transcripts and
1 subagent transcript across 5 project folders, plus a 5-minute look at a copy of `state.vscdb`
(last modified 2026-10-15; the copy was deleted afterwards). No version string exists in either
source.

## Paths and layout
```
~/.cursor/projects/<workspace path, every non-alnum run made ->/
    agent-transcripts/<id>/<id>.jsonl
    agent-transcripts/<id>/subagents/<subagent id>.jsonl
    agent-tools/  canvases/  mcps/  terminals/        (not transcripts)
~/Library/Application Support/Cursor/User/globalStorage/state.vscdb (+ -wal, -shm)
```

## Transcript record catalogue (main; counts)
| shape | n |
|---|---|
| `{role, message: {content: [...]}}` | 611 (assistant 549, user 62) |
| `{type: "turn_ended", status}` | 49 (status `success`) |
| `{type: "turn_ended", status, error}` | 6 (status `error`) |

Content blocks: assistant `tool_use` `{type, name, input (object)}` 1127 and `text` 192; user
`text` 62. **No `tool_result`, no `thinking`, no ids and no timestamps.** `role` is always top-level,
never `message.role`.

Tool names: `Read, Shell, Grep, StrReplace, CallDynamicTool, GetDynamicTools, Glob, Write,
AwaitShell, ReadLints`; in the subagent also `UpdateCurrentStep`. No ask-user tool was seen.

User text wrappers (tag names only): `<timestamp>` then `<user_query>` (60/62), or `<timestamp>`
alone (3). `<cursor_commands>` was not seen in this corpus.

`turn_ended.error` values (fixed strings): `User aborted request` 4, `user_stopped_generation` 1,
`new_message_submitted` 1. **Every `status: error` was a user stop, not a failure.**

Subagent file: the same shapes, ending with `turn_ended: success` (1/1). The parent transcript
shows **no spawn tool call** for it (*inference: spawned via `CallDynamicTool` or not
recorded*). `state.vscdb` bubbles carry `subagentSpawnTaskToolCallId`, and composerData has
`subagentComposerIds`.

## state.vscdb (quick look)
Tables: `ItemTable`, `cursorDiskKV`, `composerHeaders`. `cursorDiskKV` key prefixes: `agentKv:`
5627, `bubbleId:` 2084, `checkpointId:` 64, `ofsContent:` 55, `codeBlockPartialInlineDiffFates:`
48, `composerData:` 27, `composerRootWriterDiagnostic:` 10, `inlineDiff:` 10, plus many
`composer.content.<sha>` and `composer.autoAccept.*` singletons.

- `composerData:<id>` has 87 top-level fields, including status-like ones: **`status`**
  (`none` 14, `completed` 8, `aborted` 5), **`generatingBubbleIds`**, `isContinuationInProgress`,
  `queueItems`, `unifiedMode` (`agent`|`chat`), `forceMode` (`edit`|`chat`), `subagentComposerIds`,
  `todos`, `contextUsagePercent`, `name`/`subtitle` (a title source), `createdAt`, `lastUpdatedAt`.
- `bubbleId:<composer>:<bubble>` has 87 fields, including `type` (1 = user, 2 = assistant,
  *inference*), `capabilityType`, `toolFormerData` (tool calls; not mapped, so a likely home for
  approval or user-decision state), `toolResults`, `thinking`, `startedAtMs`, `completedAtMs`,
  `turnDurationMs`, `subagentSpawnTaskToolCallId`.
- **Both:** all 12 transcript ids also exist as `composerData`. 15 composers have no
  transcript (older, or `chat`-mode, *inference*). IDE agent chats appear in **both** places.
  `state.vscdb` alone has times and a status.

## Status signals
- **Turn end:** `turn_ended`. Firm. `status: error` with one of the `error` strings above means
  **Interrupted**. No real failure was seen, so Errored has no confirmed shape.
- **Tool pending:** no results are recorded. tvdash treats later assistant text as "results are
  in" (inference, unchanged).
- **Approval pending, generating:** not in transcripts. Candidates are `composerData.status` and
  `generatingBubbleIds`, and `toolFormerData` in bubbles (Phase 4 snapshot reader).
- **Times:** none in transcripts except the prompt's `<timestamp>` (to the minute). Use mtime.

## What tvdash gets wrong or misses (`src/sessions/cursor.rs`)
- `turn_ended` with `status: error` becomes "done" (cursor.rs:66 to 71). The statuses are all user stops
  (Interrupted), and the `error` string that says so is unread.
- `ASKS_USER` lists `askquestion, askuserquestion, askfollowupquestion, requestuserinput`. None
  of these appeared here, so the list is unverified.
- Subagents count only as activity (mtime). Their `turn_ended` is not read, so a finished
  subagent and a stuck one look alike. `state.vscdb` (status, titles via `name`, times) is
  unread.
- Tool calls stall into "may need approval" after 20 s. Cursor's auto-run settings are unknown.

## Open questions
- The `toolFormerData` field paths and status values; whether `composerData.status` reads
  `generating` while live (the copy was made while Cursor was idle).
- `<cursor_commands>` and other wrappers; any ask-user tool name; how `cursor-agent` (CLI)
  sessions differ (the CLI is not installed on this Mac: `~/.cursor/cli-config.json` exists, the binary is
  not on PATH; not checked further).
- Env overrides Cursor honours: not researched.
