# Claude Code transcript format

Survey: read-only, structure only, this Mac, 2026-10-23. It covered 150 main transcripts and
67 subagent transcripts (69 `.meta.json`), Claude Code **2.1.281 to 2.1.309** (24 versions;
subagents span 2.1.283 to 2.1.309). Every session ran under `--dangerously-skip-permissions`.
Scripts: `claude1.py` to `claude5.py` in the session scratchpad (not committed). Everything
labelled *inference* still needs a harness-CI fixture.

## Paths and layout

```
~/.claude/projects/<cwd with / and . made ->/<session id>.jsonl      main transcript
                    .../<session id>/subagents/agent-<agentId>.jsonl  one per subagent (flat, nested ones too)
                    .../<session id>/subagents/agent-<agentId>.meta.json
                    .../<session id>/subagents/agent-<agentId>.forked-skill{,.marker}.json   (forked Skill runs)
                    .../<session id>/tool-results/…                   persisted large tool outputs
                    .../memory/*.md                                   auto-memory (not transcripts)
~/.claude/sessions/<pid>.json        live-process registry (see "Registry")
~/.claude/jobs/<id>/{state.json,timeline.jsonl}   background jobs (state: working|done)
```
`CLAUDE_CONFIG_DIR` relocates `~/.claude`. In 2.1.310 it is one absolute path (not a
comma-separated list; a relative path is ignored), checked in the binary and by an offline
run with a temp config folder.

## Record catalogue (main transcripts; counts)

Common envelope on conversational records (`user`, `assistant`, `system`, `attachment`):
`uuid, parentUuid, timestamp (RFC 3339 UTC), sessionId, session_id (newer), cwd, gitBranch,
version, entrypoint (cli|sdk-cli), userType (external), isSidechain, slug?, agentId (subagent
files), sessionKind ("bg" for background sessions)`.

| type | n | notable fields |
|---|---|---|
| assistant | 20101 | `message.{id,model,role,stop_reason,stop_sequence,usage,content[]}`, `requestId`, `apiBlockIndex`, `effort`, `perTurnEffort`, `advisorModel`, `thinkingDurationMs`, `thinkingDisplay`, `error`, `isApiErrorMessage`, `isAbortedMidStream`, `truncatedAfterOutput`, `attribution*` |
| user | 10700 | `message.content` (str or blocks), `toolUseResult`, `sourceToolAssistantUUID`, `permissionDecision.{decision,source,reasonType,reasonCode,grantScope}`, `permissionMode`, `promptSource`, `promptId`, `origin.{kind,producer,runId}`, `interruptedMessageId`, `toolDenialKind`, `isMeta`, `isCompactSummary`, `queue*`, `sandboxed`, `classifierBoundary` |
| attachment | 17590 | `attachment.type` (26 kinds, e.g. `total_tokens_reminder`, `deferred_tools_record`, `queued_command`, `hook_cancelled`) |
| system | 2360 | `subtype` (below), `level`, `content`, `isMeta` |
| api-request / -blob / -shape | 4986 / 9739 / 111 | `querySource` (`repl_main_thread`, `agent:builtin:*`, `agent:custom`, `sdk`), request digests |
| mode, permission-mode | 4111 each | `mode` (`normal` only); `permissionMode` |
| last-prompt | 4240 | `lastPrompt`, `leafUuid`, `explicit` |
| ai-title | 3981 | `aiTitle` |
| atis-latch | 4214 | `atis` |
| pr-link | 1390 | `prNumber, prRepository, prUrl` |
| queue-operation | 1102 | `operation` enqueue/dequeue/remove, `reason` (`absorbed_mid_turn`, `delivered_to_agent`) |
| file-history-snapshot / -delta | 1065 / 339 | backups (keys are file paths: never dump) |
| cost-state | 307 | `totalCostUSD` (float, or int 9×), `totalDuration`, `totalAPIDuration`, `totalToolDuration`, lines added/removed |
| bridge-session | 315 | remote bridge ids |
| agent-name, frame-link, artifact-* | 26, 8, 4 | |
| fork-context-ref | 11 (subagent files only) | `parentSessionId, parentLastUuid, contextLength` |

**system subtypes:** `turn_duration` 1187 (`durationMs, messageCount, pendingBackgroundAgentCount?`),
`stop_hook_summary` 890 (`hookCount, hasOutput, preventedContinuation, stopReason` (always ""), `toolUseID`),
`away_summary` 225, `informational` 30, `local_command` 11 (`commandRun`), `bridge_status` 8,
`compact_boundary` 5 (`compactMetadata.{trigger,preTokens,postTokens,durationMs,cumulativeDroppedTokens}`,
`logicalParentUuid`), `scheduled_task_fire` 3 (`cron, cronKind, taskKind`: `loop`), `agents_killed` 1.

**Content blocks:** assistant: `thinking` (`thinking, signature`), `text`, `tool_use` (`id, name,
input, caller.type`: `direct`). user: `tool_result` (`tool_use_id, content` str|list of
`text|image|tool_reference|document`, `is_error`), `text`, `image`. `api-request-blob` also carries
`tool_addition` (`tool.type`: `tool_definition`).

**stop_reason:** `tool_use` 21503, `end_turn` 1985, `stop_sequence` 5, `refusal` 3, null 485 rows.
Rows per `message.id` run from 1 to 6. In 2.1.28x+ every row of a message carries the same final
`stop_reason` (null on only 17 messages), so a `thinking` row already says `end_turn` before its
`text` row is written.

## Status signals

**Permission mode.** A `mode` + `permission-mode` pair opens every session (records 0 and 1 in
135/150 files; the other 15 start with `queue-operation`). The pair is re-appended through the
session alongside `last-prompt`, mostly after a tool result or a prompt, so it is a
metadata-flush cluster, not a turn marker. Prompt records also carry `user.permissionMode`
(1311). Values seen: `bypassPermissions` (all 4111 records), `default` (2, on `sdk` prompts).
`mode` is always `normal`. Vocabulary from the 2.1.309 binary: `default, acceptEdits, plan,
bypassPermissions, dontAsk, auto`. **Firm for "can't be asking".**

**Post-hoc approval record (new, firm).** Each tool result's `user` record carries
`permissionDecision`: `config/mode/accept` 8300, `config/classifier/accept` 71 (auto-mode
classifier), `config/rule/accept` 21, `user_temporary/-/accept` 25, `user_reject/-/reject` 5,
`config/safetyCheck/reject` 1, and none on 3080. Under bypass, **every** `user_temporary` and
`user_reject` was `AskUserQuestion` (its answer goes through the permission UI; tool_use to
result took a median of 75 s, max 2.6 h). So no tool other than ask-the-user waited for a
human in this corpus.

**Pending permission prompt: no transcript record.** Nothing is written while a prompt is up.
The registry (below) does not set `waitingFor` for the `permission_prompt` dialog kind in
2.1.309 (from minified source, *inference*). AwaitingApproval stays inferred, and is
impossible under `bypassPermissions` except for the ask tools.

**Turn end, in order (n = turns):**
`assistant(stop_reason=end_turn)` → `system/stop_hook_summary` (when hooks ran) →
`system/turn_duration` → `last-prompt`/`mode`/`permission-mode` flush, `away_summary` or the next prompt.
Before `turn_duration`: `end_turn, stop_hook_summary` 864; `…, end_turn` (no hooks) ~290;
`stop_sequence` 4 (API error); after an interrupt 1. Gaps after the end_turn row: p50 0.06 s, p99 0.6 s, max 3 s.
**Firmest end: `system/turn_duration`.** It comes once per completed turn and after the hooks
(the hooks can still prevent continuation; `preventedContinuation` was false 890/890). `end_turn` is
firm but arrives earlier and repeats per row. `last-prompt` is **not** a turn end: it is
written mid-turn constantly.
Interrupted turns get **no** `turn_duration` (1 exception). The interrupt user record ends them.

**Background helpers at turn end:** `turn_duration.pendingBackgroundAgentCount` (1 to 6, on 73
records; absent when 0) is a **firm** "turn ended, N agents still running".

**Interruption:** a user record with text block `[Request interrupted by user]` (50) or
`[Request interrupted by user for tool use]` (26), always as **blocks** in 2.1.28x+ (tvdash also
handles a string), with `interruptedMessageId` (71) pointing at the cut assistant `message.id`.
Mid-tool: the pending tool's result is `is_error: true`, text starting `The user doesn't want to
proceed with this tool use`, `toolDenialKind: "user-rejected"`, no `permissionDecision`
(28/28 were followed by the "for tool use" interrupt).
`toolDenialKind` values: `user-rejected` 34, `permission-rule` 1, `interrupted` 1, `automode-blocked` 1.

**Errors:** a tool error is `is_error: true` (387). The content prefixes are `Exit code` (Bash),
`<tool_use_error>` and `Error: `. API errors are a synthetic assistant row (`model: "<synthetic>"`,
`isApiErrorMessage: true`, `error`: `server_error` | `authentication_failed`,
`stop_reason: stop_sequence`), followed by `turn_duration`. This is the firm **Errored** signal.
Mid-stream aborts: `isAbortedMidStream` (2).

**Subagents (firm linkage):**
1. Parent `assistant` `tool_use` named **`Agent`** (54; `Task` never seen), with input keys
   `description, prompt, subagent_type[, run_in_background, isolation]`. `run_in_background`
   was the *string* `"true"`.
2. Its `tool_result` comes back in 0.04 to 0.5 s with `toolUseResult.status: "async_launched"`,
   `isAsync`, `agentId`, `outputFile`, `canContinueAgent`. All 54 agents ran in the background.
3. File `subagents/agent-<agentId>.jsonl`. Every record has the same `agentId` and the
   parent's `sessionId`, with `isSidechain: true`, and no `system` records. `meta.json` has
   `toolUseId` (= the spawning `tool_use.id`), `agentType` (`general-purpose|Explore|fork`),
   `spawnDepth` (1|2), `parentAgentId` (nested), `requestShape: background`, and
   **`finished: true`** when done (52/69; `stoppedByUser` 1).
4. Completion reaches the parent as a `user` record with `origin.kind: "task-notification"`
   (`promptSource: system`). Its string content is a `<task-notification>` block with tags
   `task-id, tool-use-id, output-file, status, summary, result, usage, note`. `<status>`:
   `completed|failed|stopped` (absent for monitor/bash events). It lands a median 0.12 s after the
   subagent's last record. The same channel carries background Bash and Monitor tasks: their task
   ids are 9 characters, agent ids 17.
5. A subagent's own end: its last record is `assistant` `end_turn` (52/52 finished). Unfinished
   ones (17: 7 live, 10 stale) end on a tool_result, an interrupt or a tool_use.
**Helper-end order of firmness:** parent task-notification `<status>` > `meta.json finished` >
subagent `end_turn` (inferred: a subagent can end_turn and still be continued). `system/agents_killed`
(1, followed by `cost-state`) marks helpers killed at session exit.

**Compaction:** `system/compact_boundary` (`parentUuid: null`, `logicalParentUuid` → the last
pre-compaction uuid) is followed directly by a `user` record with `isCompactSummary: true`.
The transcript continues **in the same file**. All 5 seen were `trigger: auto`.

**Other:** `queue-operation` shows prompts typed while busy. `cost-state` is the last record in
140/150 files (*inference: written on exit/flush*). `api-request` is written as a request is
sent (median 3 s before the first assistant row), a possible firm "Thinking" *(inference)*.

## Registry: `~/.claude/sessions/<pid>.json` (one per live process)

Fields: `pid, sessionId, cwd, startedAt, procStart, version, kind (interactive), entrypoint,
name, nameSource, status, statusUpdatedAt, updatedAt, peerProtocol, peerFeatures,
messagingSocketPath, pidDomain`. From the binary, `status` is `busy | idle | waiting`, with
`waitingFor` ∈ {`input needed` (MCP elicitation), `dialog open`, `sandbox request`,
`worker request`, `goal proposal`}. This gives a live idle/busy without timing. It is
local-only, and liveness has to be checked by pid (*inference from source; one live file seen*).

## Versions seen
2.1.281, .283 to .287, .289 to .296, .299 to .306, .308, .309.

## What tvdash gets wrong or misses (`src/sessions/claude.rs`)
- Ignores `permission-mode` and `user.permissionMode`. Under bypass, every tool over 20 s shows
  "may need approval" (`sessions.rs` `status()`, `STALL_AFTER`), though the corpus proves only
  `AskUserQuestion` ever waits.
- Ends turns only on `stop_reason == "end_turn"` (claude.rs:95) and the interrupt prefix. It
  never reads `turn_duration`, `pendingBackgroundAgentCount`, `agents_killed` or the API-error rows.
  A synthetic `stop_sequence` error leaves the session "thinking" until idle.
- The interrupt is mapped to `Last::EndTurn` ("done"), so it can't tell Interrupted apart.
- `max_depth 2` skips `subagents/` entirely: nothing follows helpers, and with `async_launched`
  results the parent reads "thinking", then "done", while agents run. Task-notification prompts
  (`origin.kind`) are logged as user prompts (`› <task-notification>` is skipped only because
  it starts with `<`).
- `ASKS_USER` has `ExitPlanMode`, which never appeared here (no plan-mode sessions), but it misses
  nothing else seen.
- Usage is only `output_tokens` per `message.id` (correct dedupe). Cost takes the last `cost-state`,
  which is written rarely, so it lags.

## Open questions
- Does a `permission_prompt` in default mode change `sessions/<pid>.json`? Does anything get
  written while it's up? (Phase 3 harness CI.)
- `mode` values other than `normal`, and plan-mode and `ExitPlanMode` records: none in this corpus.
- Manual `/compact` (`trigger: manual`) not seen.
