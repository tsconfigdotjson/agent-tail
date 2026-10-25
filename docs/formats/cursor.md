# Cursor agent-transcript format

This note is based on a read-only survey of 12 session transcripts and 1 subagent transcript
across 5 project folders, plus a 5-minute look at a copy of `state.vscdb` (the copy was deleted
afterwards). No version string exists in either source.

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

## state.vscdb (survey and reader)

Tables: `ItemTable`, `cursorDiskKV`, `composerHeaders`. `cursorDiskKV` key prefixes:
`agentKv:` 5627, `bubbleId:` 2084, `checkpointId:` 64, `ofsContent:` 55,
`codeBlockPartialInlineDiffFates:` 48, `composerData:` 27,
`composerRootWriterDiagnostic:` 10, `inlineDiff:` 10, plus many
`composer.content.<sha>` and `composer.autoAccept.*` singletons.

- `composerData:<id>` has 87 top-level fields, including status-like ones: **`status`**
  (`none` 14, `completed` 8, `aborted` 5), **`generatingBubbleIds`**, `isContinuationInProgress`,
  `queueItems`, `unifiedMode` (`agent`|`chat`), `forceMode` (`edit`|`chat`), `subagentComposerIds`,
  `todos`, `contextUsagePercent`, `name`/`subtitle` (a title source), `createdAt`, `lastUpdatedAt`.
- `bubbleId:<composer>:<bubble>` has 87 fields, including `type` (1 = user, 2 = assistant,
  *inference*), `capabilityType`, `toolFormerData` (tool calls), `toolResults`, `thinking`,
  `startedAtMs`, `completedAtMs`, `turnDurationMs`, `subagentSpawnTaskToolCallId`.
- **Both:** all 12 transcript ids also exist as `composerData`. 15 composers have no
  transcript (older, or `chat`-mode, *inference*). IDE agent chats appear in **both** places.
  `state.vscdb` alone has times and a status.

### Reader

`agent-tail`'s `sqlite` feature (on by default) copies `state.vscdb` plus `-wal` and `-shm`
into a private temporary folder, opens the copy `mode=ro`, and deletes the folder before it
returns. It never opens the live database; the WAL is copied because live writes land there.

It reads:

- `composerData:*`: `name`, `status`, `generatingBubbleIds`, `lastUpdatedAt` and
  `createdAt`. `status: completed` is firm `Done`, `aborted` is firm `Interrupted`, and
  `generating` (or a non-empty `generatingBubbleIds` when the status is `none`) is firm
  `Thinking`. An unknown status changes nothing. The snapshot's `lastUpdatedAt` must be at
  least as new as the transcript's last entry, so a throttled read can't rewind a newer
  transcript. `name` is the Cursor title; the opening prompt remains the fallback.
- `bubbleId:*`: `toolFormerData.name`, `status`, `userDecision`, `additionalData.status`,
  `additionalData.userDecision` and `error`. Cursor also writes `status: loading`,
  `completed`, `error` and `cancelled`, and decisions `accepted`, `rejected` or `pending`.
  A pending decision is firm **`AwaitingApproval`**; a pending ask-user tool is firm
  **`AwaitingAnswer`**. Cursor writes it at `additionalData.status: pending` while the
  shell/write review card is up in the 3.27 IDE, and as `userDecision: pending` in public
  storage/export shapes; the reader accepts either (and `status: pending`). A completed or
  aborted composer wins over a bubble left behind from that turn. Otherwise pending wins
  over `generating`, because the composer can still say `generating` while the card waits.
  The pending mapping is based on Cursor 3.27 IDE source and the public values; the live
  snapshot that confirms the exact field path is still outstanding (below).

The tracker checks the three files' stamps on every poll but copies them at most every 30 s,
and only while a Cursor session is in the window. A database that hasn't changed is not
copied again. A first poll with no Cursor session never opens it.

`generating` was not in the 2026-10-23 survey copy, which was idle. It appears in Cursor's
stored default composer state and in third-party readers; the reader accepts it but the
`Thinking` mapping has not been checked against a live Cursor capture yet. Stale
`generatingBubbleIds` are ignored when the status says `completed`.

### Cursor CLI and the mock/PTY rig

Tested on Linux with `cursor-agent` **2026.10.01-e373342** and a temp `HOME`, a dummy
`CURSOR_API_KEY`, and `CURSOR_API_ENDPOINT` (also `--endpoint`) pointed at a local logging
server. The endpoint is honoured, but it is not an Anthropic Messages or OpenAI Responses
endpoint:

1. It `POST`s `/auth/exchange_user_api_key` and expects an access/refresh token.
2. It then speaks Cursor's own Connect/protobuf services: `aiserver.v1.DashboardService/*`,
   `aiserver.v1.ServerConfigService/GetServerConfig`, `aiserver.v1.AiService/AvailableModels`,
   `GetUsableModels`, `GetDefaultModelForCli`, and analytics. A hand-made token and model
   catalogue got it to the chat step.
3. The chat step is a streaming Cursor RPC (HTTP/2/gRPC-style), not `/v1/messages` or
   `/v1/responses`; against an HTTP/1.1 mock it failed with `ERR_HTTP2_ERROR` /
   `[internal] Protocol error`.

So Cursor needs its backend, or a new mock that implements Cursor's auth exchange,
protobuf model catalogue and streaming chat protocol. **Decision:** Cursor does not join the
mock/PTY matrix; it ships through donated fixtures and, if a key is available later, a
budget-capped nightly real run. A freshly created IDE `state.vscdb` during a live approval
wait is still needed.

### Live approval wait capture

Cursor keeps the database at `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
on macOS and `~/.config/Cursor/User/globalStorage/state.vscdb` on Linux. The capture tool
[`harness-ci/cursor/snapshot_state_vscdb.py`](../../harness-ci/cursor/snapshot_state_vscdb.py)
auto-detects the platform path, copies that file with `-wal` and `-shm` into a private temp
folder, opens **only the copy** `mode=ro`, runs the SQL below, redacts ids/titles/errors, and
writes three JSON dumps:

```sh
harness-ci/cursor/snapshot-state-vscdb.sh waiting       --out /tmp/cursor-approval
# approve the tool in Cursor
harness-ci/cursor/snapshot-state-vscdb.sh after-approve --out /tmp/cursor-approval
# start a second tool wait and deny it
harness-ci/cursor/snapshot-state-vscdb.sh after-deny    --out /tmp/cursor-approval
```

`capture` runs the same three snapshots with prompts. For the periodic Mac confirmation on an
already-signed-in Cursor, use
[`harness-ci/cursor/mac-smoke.md`](../../harness-ci/cursor/mac-smoke.md); it also shows the
one-line `agent-tail ls`/`watch` status check. A raw read of a copied database (never the
live one) is:

```sh
# macOS:
CURSOR_STATE="${CURSOR_STATE_DIR:-$HOME/Library/Application Support/Cursor/User/globalStorage}"
# Linux: CURSOR_STATE="${CURSOR_STATE_DIR:-$HOME/.config/Cursor/User/globalStorage}"
SNAP=$(mktemp -d)
cp "$CURSOR_STATE/state.vscdb"* "$SNAP/"
sqlite3 "$SNAP/state.vscdb" "
  SELECT json_extract(value,'$.status'), json_extract(value,'$.generatingBubbleIds'),
         json_extract(value,'$.name'), json_extract(value,'$.lastUpdatedAt')
  FROM cursorDiskKV WHERE key = 'composerData:<composer-id>';"
sqlite3 "$SNAP/state.vscdb" "
  SELECT key, json_extract(value,'$.toolFormerData.status'),
         json_extract(value,'$.toolFormerData.additionalData.status'),
         json_extract(value,'$.toolFormerData.userDecision'),
         json_extract(value,'$.toolFormerData.additionalData.userDecision')
  FROM cursorDiskKV
  WHERE key >= 'bubbleId:<composer-id>:' AND key < 'bubbleId:<composer-id>;';"
rm -rf "$SNAP"
```

**CI stays offline-only.** The Linux + Xvfb capture remains blocked: the CLI tokens
available there expired, and the IDE needs an interactive Cursor sign-in under Xvfb. Do not
add a Cursor login step to CI. The periodic confirmation path is
[`mac-smoke.md`](../../harness-ci/cursor/mac-smoke.md) on Lee's already-signed-in Mac. The
3.27 IDE source shows the wait writing `additionalData.status: pending`; that Mac smoke is
what confirms it and records what `composerData.status` says during the wait. It is no longer
the open question of *whether* a firm signal exists.

## Status signals
- **Turn end:** `turn_ended`. Firm. `status: error` with one of the `error` strings above means
  **Interrupted**. No real failure was seen, so Errored has no confirmed shape. With the
  `sqlite` feature, `composerData.status` gives the same firm ends (and `generating`) even
  when the transcript hasn't caught up.
- **Tool pending:** no results are recorded. The adapter treats later assistant text as "results are in"
  (inference, unchanged). A tool with no result and no pending decision in the database is
  firm `RunningTool`.
- **Approval pending:** a bubble's `userDecision: pending`, `status: pending`, or
  `additionalData.status: pending` is firm `AwaitingApproval`; a pending ask-user tool is
  firm `AwaitingAnswer`. Cursor does not use the shared 20 s stall inference. The source
  shape is from the 3.27 IDE; the live snapshot is still the confirmation step above.
- **Times:** none in transcripts except the prompt's `<timestamp>` (to the minute). Use mtime.
  The database has millisecond `createdAt`/`lastUpdatedAt`; the reader uses those for its
  status freshness.

## How agent-tail handles these records

- Maps `turn_ended` with a user-stop error string to firm `interrupted`; it does not collapse
  every `status: error` into `done`.
- Reads `state.vscdb` in the `sqlite` feature for `composerData.status`, bubble decisions,
  titles via `name`, and timestamps. A completed composer is firm `done`, aborted is firm
  `interrupted`, a pending decision is firm `awaiting_approval` (or `awaiting_answer`), and
  generating is firm `thinking`.
- Follows Cursor subagent transcripts and their own `turn_ended`, so a finished helper and a
  stuck one are distinguishable.
- Keeps tool calls pending until later assistant text indicates the result is in. Cursor is
  out of the shared 20 second approval inference: only the database's pending decision is
  treated as an approval wait.
- Treats the discovery spec's ask-user tool names as unverified data rather than assuming them.

## Open questions
- The pending decision is mapped from the Cursor 3.27 IDE source and public values above; the
  live approval-wait capture (dumps for waiting/approved/denied) is still outstanding.
- Whether `composerData.status` reads `generating` while an approval card waits: accepted by
  the reader (pending wins either way), not yet seen in a local live capture.
- `<cursor_commands>` and other wrappers; any ask-user tool name; how `cursor-agent` (CLI)
  sessions differ. The CLI is not in the rig; its `~/.cursor/chats/.../store.db` is not read.
- Environment overrides Cursor honours are not researched; `state.vscdb` uses the platform
  default below the home folder.
