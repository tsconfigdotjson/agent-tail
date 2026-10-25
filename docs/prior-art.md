# Prior art review

Read-only review, 2026-10-23. Nothing was built or run. Scope was cut midway to save
budget: ADR, claude-session-parser and Codex PR #35623 are read first-hand. parse-cc,
claude-devtools, ccusage, SpecStory and normalize-chat-sessions are summarised briefly
from delegated reads, and their citations were spot-checked only where marked ✓. Codex
rollout, Gemini CLI and OpenCode facts marked CDX, GEM or OPC come from a delegated read
of the upstream source; the rest is from other projects' code and docs. Treat all of it as
`ported-unverified` until it is seen in a real artifact.

**Pins** (short SHAs; full ones in the table)

| key | repo | commit |
|---|---|---|
| ADR | uber/ADR | `6b33dffecd719708c435f772c9e93f1210944e31` (the requested pin, exists) |
| CSP | sevenevesai/claude-session-parser | `c0e406f10cec788ca936b86476d301135d31af7b` |
| NCS | rhi-zone/normalize (crate normalize-chat-sessions 0.3.2) | `3fa72ba` |
| PCC | d1ll0n/parse-cc | `a27ba0a4fab2e5eb11de40c5e4466e55d545ea8f` |
| CDT | matt1398/claude-devtools | `1486f2042c87bb547a4e34808ab54ab34a33f132` |
| CCU | ryoppippi/ccusage | `e42e1b44737fb1eedc200d1dd187426f0ee3013d` |
| SPS | specstoryai/getspecstory (`specstory-cli/`) | `084402d5957cb46e5a6593e82286a0463deea693` |
| CPR | openai/codex PR #35623 (merged 2026-07-27), head | `bd2de422aa287b97b06ca6425a10935bcf1b3731` |
| CDX | openai/codex HEAD | `970111f` |
| GEM | google-gemini/gemini-cli | `d968ea0` |
| OPC | sst/opencode (MIT) | `f9f463b` |
| TVD | tvdash (this repo's source) | `cd033f9` |

Paths are relative to each repo. ADR's Sensor code is under `Sensor/adr_sensor/`,
abbreviated `S/`.

## 1. Licences

| project | licence | can it be ported into `MIT OR Apache-2.0`? |
|---|---|---|
| ADR | Apache-2.0 (`LICENSE`, `Sensor/LICENSE`) | Only by making the crate Apache-2.0 with a NOTICE (PLAN §2). Facts and format knowledge are free to use. |
| CSP | MIT OR Apache-2.0 (`Cargo.toml:5`) | Yes, as is. |
| NCS | MIT OR Apache-2.0 (`Cargo.toml:66`) | Yes. |
| PCC, CDT | MIT (`LICENSE:1-3` in each) | Yes, keeping the MIT notice. |
| CCU | MIT (`LICENSE:1`); the engine is now Rust (`rust/adapters/`) | Yes, keeping the notice. |
| SPS | Apache-2.0 (`LICENSE.txt`), Go | Same as ADR. Its `*-FORMAT.md` docs are the valuable part. |
| CPR (openai/codex) | Apache-2.0 (repo licence) | Same as ADR. The wrapper rule is ~40 lines and easy to rewrite fresh. |

**Verdict:** fresh code stays the plan. If anything is ported, CSP's project-path encoder is
the best candidate: it is permissively licensed and well specified.

## 2. Status signals: what anyone uses

No project reduces transcripts to a live state the way PLAN §4 does. CDT's "ongoing"
detector is the only status logic of any kind. Everything else below is a *field that
exists*, which we can turn into a signal.

| signal | harness | source and evidence | used today by |
|---|---|---|---|
| Permission mode | Claude | `permission-mode` record type, listed in ADR `S/parsers/claude_parser.py:44` but never read. A per-prompt `user.permissionMode` field also appears in PCC's baseline (`tests/fixtures/log-schema-baseline.json:1276`). Desktop agent mode has `system/init.permissionMode` (`S/parsers/claude_desktop_parser.py:463`). | Nobody acts on it. PCC returns the *first* mode seen (`src/derive/first-observed.ts:5-29`), which is wrong for live status after a mid-session switch. |
| Approval policy | Codex | `turn_context.{approval_policy, sandbox_policy, approvals_reviewer, permission_profile}`. `approval_policy` is `untrusted`, `on-request` (alias `on-failure`), `granular{…}` or `never`. `sandbox_policy.type` is `danger-full-access`, `read-only`, `external-sandbox` or `workspace-write`. `approvals_reviewer` is `user` or `auto_review`, where an AI reviewer decides instead of the user (CDX `protocol/src/protocol.rs:1098-1225, 3455-3496`; `protocol/src/config_types.rs:183-190`). ADR reads only `model` (`S/parsers/codex_parser.py:652-654`). | Nobody. |
| **Codex approvals are never persisted** | Codex | The rollout policy never writes `exec_approval_request`, `apply_patch_approval_request`, `request_permissions`, `request_user_input`, `elicitation_request`, `exec_command_begin/end` or `error` (CDX `rollout/src/policy.rs:160-225`). So a pending approval *or ask-user* is visible only as a `function_call` with no output. | – |
| Turn end | Codex | Always persisted: `task_started`/`turn_started`, `task_complete`/`turn_complete` (with `error`, `duration_ms`), and `turn_aborted.reason` = `interrupted`, `replaced`, `review_ended` or `budget_limited` (CDX `policy.rs:133-139`; `protocol.rs:2296-2323, 4394-4426`) | tvdash (without `error` or the reason) |
| **Firm permission pending** | Copilot CLI | `permission.requested` / `permission.completed` events in `events.jsonl`, joined by `toolCallId` (directly or under `permissionRequest`/`promptRequest`); `result.kind == "approved"` (`S/parsers/copilot_parser.py:310-343, 598-640`) | ADR (to annotate tools) |
| Approval pending? | Gemini CLI | ADR treats `toolCalls[].status == "awaiting_approval"` as a pending approval (`S/parsers/gemini_parser.py:113-116`), but only a synthetic test uses it (`Sensor/tests/test_gemini_parser.py:33`). Upstream writes tool calls only once they complete (GEM `core/geminiChat.ts:1801-1830`). Inference: pending and awaiting states never reach disk. Approval mode (`default`, `autoEdit`, `yolo`, `plan`) is not recorded either. | ADR |
| Tool state | OpenCode | part `state.status`: pending/running/completed/error (`S/parsers/opencode_parser.py:515-522`) | ADR |
| Turn end | OpenCode 2.x | `session_message.type == "idle"` with `outcome` succeeded/failed/interrupted (OPC `packages/schema/src/session-message.ts:285-295`; SPS `OPENCODE-FORMAT.md:38-106`). `permission.asked`/`replied` are ephemeral and never stored (OPC `packages/schema/src/permission.ts:45-46`). | SPS documents it only |
| Session replaced | Claude | A `continued-in` record with `continuedInSessionId`: backgrounding replaces the session with a continuation file that includes its history (CPR `sessions/records_cla.rs:43-51`) | Codex importer |
| Turn end | Claude | `stop_reason`, `system/turn_duration`, `stop_hook_summary`; `*_hook_summary` records carry `preventedContinuation` (CSP `src/usage.rs:366-382, 778-814` ✓ partial) | tvdash uses `stop_reason` only |
| Turn end (heuristic) | Claude | CDT: a session is ongoing if a thinking block, tool_use or tool_result follows the last "ending event". Ending events: non-empty assistant text, `ExitPlanMode`, a tool result `"User rejected tool use"`, or `[Request interrupted by user…`. A session is also not ongoing if `mtime` is more than 5 min old (`src/main/utils/sessionStateDetection.ts:43-151`, `services/discovery/ProjectScanner.ts:751-755`) | CDT |
| Rejected tool | Claude | the string `toolUseResult === "User rejected tool use"` (CDT, same file) | CDT. tvdash doesn't know it. |
| Interruption | Claude | `[Request interrupted by user]` and `[Request interrupted by user for tool use]` (CDT `src/main/types/messages.ts:199-205`). PCC matches the looser prefix. | tvdash (prefix match) |
| Background agents | Claude | `toolUseResult.{isAsync, backgroundTaskId, assistantAutoBackgrounded, backgroundedByUser, status, agentId}` are in PCC's baseline (lines 1305-1877) | Nobody reads them |
| Subagent start | Copilot CLI | `subagent.started` with `toolCallId` and `agentName` (`S/parsers/copilot_parser.py:376-389`) | ADR |
| Subagent lineage | Codex | `session_meta.payload.forked_from_id` or `source.subagent.thread_spawn.parent_thread_id` (CCU `rust/adapters/codex/src/replay.rs:407-428`) | CCU (usage only) |
| Compaction | Claude | `user.isCompactSummary`; `system.compactMetadata{trigger, preTokens, postTokens}`; subagent files named `agent-acompact*` are compaction artefacts (CDT `SubagentResolver.ts:107-110`); content type `compaction` (ADR `:67`) | CDT/PCC (token phases only) |
| API error | Claude | assistant `isApiErrorMessage`, `error`; `system` retry fields `retryAttempt`/`maxRetries`/`retryInMs` (PCC baseline lines 26, 41, 1087) | CCU reads the usage-limit text only |
| Mode / plan | Copilot, Cursor IDE, OpenCode | Copilot `session.mode_changed`, `session.plan_changed` (ADR `copilot_parser.py:395-430`); Cursor `unifiedMode` 1=Ask, 2=Agent, 5=Plan and `toolFormerData.userDecision` (SPS `CURSORIDE-FORMAT.md:156-198`); OpenCode `agent-switched` (SPS) | none for status |
| Headless | Claude | `entrypoint`: `cli` vs `sdk-cli`. In headless mode nobody can approve anything (inference). (CSP `src/usage.rs:305-311`) | CSP (reporting) |
| Liveness | Claude | `~/.claude/sessions/<pid>.json` with `{sessionId, cwd, startedAt}`. CSP matches `cwd`, and picks the earliest `startedAt >= spawn − 10 s`. **There is no pid check.** (CSP `src/usage.rs:938-1034` ✓) | CSP (for its own spawned PTY) |

**For the bypass false positive.** Nothing in prior art solves it, and nobody reads
`permission-mode`. The firm fix is still PLAN §4's mode gate. Prior art adds three things:
(a) the mode can also be read per prompt from `user.permissionMode`, which matters if the
`permission-mode` record is written only on change; (b) `entrypoint: sdk-cli` implies no
human approver; (c) of the harnesses checked, only Copilot persists an explicit approval-pending event, so
only Copilot's `AwaitingApproval` can be `Firm`. Codex, OpenCode and (by inference) Gemini
never write approval prompts to disk. For Codex, the firm negative still works:
`approval_policy: never` (or `approvals_reviewer: auto_review`) means no human prompt.

## 3. Per harness

### Claude Code

| item | finding |
|---|---|
| Roots | All read `~/.claude/projects`. **ADR, PCC, CDT and CSP ignore `CLAUDE_CONFIG_DIR`** (ADR `S/parsers/claude_parser.py:79`). CCU treats it as a *comma-separated list*, and also probes `$XDG_CONFIG_HOME/claude` (default `~/.config/claude`) (`rust/adapters/claude/src/paths.rs:26-63`). SPS treats it as a single path (`providers/claudecode/path_utils.go:12-24`). Upstream docs are still unchecked. |
| Windows | Only CDT has a home fallback chain: `HOME` > `USERPROFILE` > `HOMEDRIVE+HOMEPATH` (`src/main/utils/pathDecoder.ts:246-329`). ADR uses `Path.home()`. |
| Project encoding | **The projects disagree.** CSP: every UTF-16 code unit that isn't ASCII alphanumeric becomes `-` (so an emoji becomes `--`); paths over 200 chars are cut to 200 plus `-<base36 hash>`. CSP says this was checked against Claude Code 2.1.287's own sanitizer (`src/usage.rs:470-519`, tests `:1210-1227`). SPS: every character not in `[A-Za-z0-9-]` becomes `-`, on the symlink-resolved path (`path_utils.go:27-64`). CDT and NCS replace only `/` and `\`, which is wrong per CSP. Every scheme is lossy, so take `cwd` from the records. CDT also splits one encoded dir with several `cwd`s into `dir::sha256(cwd)[:8]` (`SubprojectRegistry.ts:1-12`). |
| Session id | The file stem (CSP `src/usage.rs:711-715`). ADR keys on `(sessionId, agentId)` (`S/parsers/claude_parser.py:257-258`). |
| Subagents | `<project>/<sessionId>/subagents/agent-<agentId>.jsonl` (ADR `:220-225`; ADR `Sensor/README.md:31`), plus a legacy flat `<project>/agent-<id>.jsonl` (CDT `SubagentLocator.ts:86-180`). A companion `agent-<id>.meta.json` gives `agentType` (NCS `formats/claude_code.rs:298-325`). Every record in a subagent file is `isSidechain:true`. Subagents whose first prompt is `"Warmup"` are noise (CDT, SPS). The parent is linked by the tool result's `toolUseResult.agentId`. **No project detects when a subagent ends** except CDT's ongoing rule applied to the subagent file. |
| Usage dedupe | **Four different rules.** CSP: first row per `message.id`. NCS: max per `requestId`. PCC and CDT: last row per `requestId` ("progressively larger output_tokens"). CCU: key `message.id + requestId` across all files, keep the row with the largest total, prefer non-sidechain rows (`rust/adapters/claude/src/lib.rs:190-212, 343-372`). tvdash takes the first `output_tokens` per `message.id` (TVD `src/sessions/claude.rs:85-88`). If later rows really carry larger counts, first-row rules undercount. Only real artifacts can settle this. Skip `model == "<synthetic>"` rows (CSP, CDT, CCU). |
| Cost | `cost-state.totalCostUSD` appears only in tvdash. |
| Version | Discovery reads the version from the install layout `~/.local/share/claude/versions/<version>` without executing anything (ADR `Discovery/adr_discovery/identifier/ladder.py:166-206`; catalog `catalog/catalog.json:8`). Records also carry a `version` field (PCC). SPS and ADR's catalog `version_probe` run `--version`, which our rules forbid. |

**Record types.** Columns: A=ADR, P=PCC baseline/README, D=CDT, C=CSP, T=tvdash.

| record `type` | A | P | D | C | T |
|---|---|---|---|---|---|
| user, assistant | ✓ | ✓ | ✓ | ✓ | ✓ |
| system | ✓ | ✓ | ✓ | ✓ (hook subtypes) | – |
| summary, file-history-snapshot, queue-operation | ✓ | ✓ | ✓ | – | – |
| progress, attachment, custom-title, agent-name, last-prompt, permission-mode, pr-link | ✓ | ✓ | – | partial | – |
| ai-title, tag, agent-color, content-replacement | ✓ | – | – | – | ai-title |
| worktree-state | – | ✓ | – | – | – |
| cost-state | – | – | – | – | ✓ |
| mode, atis-latch, api-request*, bridge-session, file-history-delta, frame-link (PLAN §1) | – | – | – | – | – |

System subtypes known: `init`, `turn_duration`, `local_command`, `stop_hook_summary` (P);
`*_hook_summary` (C); `compact_boundary` (D, in a comment only). Attachment subtypes (P):
`skill_listing, deferred_tools_delta, hook_success, hook_additional_context,
hook_system_message, command_permissions, opened_file_in_ide, selected_lines_in_ide,
queued_command`. Nobody else lists `away_summary`, `agents_killed` or `scheduled_task_fire`.

**Content types.** ADR knows 20 (`S/parsers/claude_parser.py:49-72`): `text, tool_use,
tool_result, image, document, thinking, redacted_thinking, tool_reference, search_result,
server_tool_use, web_search_tool_result, web_fetch_tool_result,
code_execution_tool_result, bash_code_execution_tool_result,
text_editor_code_execution_tool_result, container_upload, compaction, resource,
resource_link, audio`. PCC adds nothing beyond those. `tool_addition` (PLAN §1) is in
nobody's list.

### Claude Desktop agent mode (ADR only)
- Roots: macOS `~/Library/Application Support/Claude/local-agent-mode-sessions`;
  Windows `%APPDATA%\Claude\local-agent-mode-sessions` (`S/parsers/claude_desktop_parser.py:36-39`).
- Layout: `<user>/<org>/local_<uuid>/audit.jsonl` with sibling metadata
  `local_<uuid>.json`. Dispatch (background) sessions live at
  `<user>/<org>/agent/local_ditto_<uuid>/` (`:119-171`).
- Metadata fields: `lastActivityAt` (epoch ms), `createdAt`, `title`, `sessionId`,
  `cliSessionId`, `sessionType`, `initialMessage`, `cwd`, `model`, `hostLoopMode` (`:239-290`).
- Records use the SDK stream-json shape: `system/init` carries `permissionMode` and
  `claude_code_version` (`:455-468`); results are `tool_use_result` in snake case (`:498`).

### Codex CLI
| item | finding |
|---|---|
| Roots | `$CODEX_HOME/sessions`, default `~/.codex` (ADR `S/parsers/codex_parser.py:66-68`). CCU accepts a comma list and also reads `archived_sessions/` (`rust/adapters/codex/src/paths.rs:236-248`, `README.md:3-13`). |
| **Thread catalog** | ADR opens `$CODEX_HOME/state_*.sqlite` read-only (`?mode=ro`, `timeout=0`). Table `threads(id, rollout_path, updated_at\|updated_at_ms)` holds absolute or `CODEX_HOME`-relative rollout paths (`:124-207`). This could replace day-folder walking. tvdash doesn't use it. |
| zstd | ADR, SPS and NCS read `.jsonl` only. CCU reads `.jsonl.zst` too, and when both copies exist the plain one wins, because resume decompresses the file and then deletes the `.zst` (`paths.rs:92-93`). |
| Tokens | `token_count.info.{total_token_usage, last_token_usage}` with `input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens`, plus `model_context_window` (ADR `:52-59, 582-608`). CCU: `input` includes cached; `output` includes reasoning; use `last` only when the total advanced, because identical totals repeat (`parser.rs:482-500`). |
| Format (CDX) | Record `type` values: `session_meta, response_item, inter_agent_communication(_metadata), compacted, turn_context, token_usage_record, world_state, retained_context, security_risk_score, event_msg, realtime_item` (`history/src/rollout_payload.rs:32-70`). `session_meta.history_mode` is `legacy` or `paginated`; paginated rollouts persist `item_completed` instead of `user_message`/`agent_message` (`rollout/src/policy.rs:141-158, 251-283`). Subagents are separate rollouts linked by `session_meta.parent_thread_id` / `source.SubAgent.ThreadSpawn` (`protocol.rs:2982-3079`). Revert renames a file to `rollout-<ts>-<id>_<rollout_id>.jsonl` (`rollout_file_name.rs:67-70`). |
| Compression (CDX) | A background job zstd-compresses rollouts with mtime at least 7 days old. **Resume decompresses the file back to `.jsonl` (new inode, old mtime copied from the archive), then deletes the `.zst`** (`rollout/src/compression.rs:26, 115-171, 988-1013`). An old day folder can therefore come back to life. |
| Version | `session_meta.cli_version` is **re-recorded on resume**, so it can't gate per-line format (CCU `codex/src/README.md:55`). |
| Status | ADR reads no `event_msg` except `token_count`. tvdash already does more (`task_complete`/`turn_complete`/`turn_aborted`). Whether Codex persists approval requests to the rollout is unconfirmed (open question 2). |
| Subagent replay | A child rollout replays the parent's history; its own turn starts at `event_msg/task_started` followed by `inter_agent_communication_metadata` with `trigger_turn:true` (CCU `README.md:49-55`). |
| Partial lines | ADR's Codex parser just does `strip()` and `json.loads`, and counts failures (`:264-288`). |

### Cursor
- **ADR reads only `state.vscdb`**, from macOS, Linux (`~/.config/Cursor/...`) and
  `%APPDATA%` (`S/parsers/cursor_parser.py:29-40`). It **opens the live DB with plain
  `sqlite3.connect`** (`:69`), not read-only and not a snapshot; avoid that.
- Keys: `composerData:<id>` (`createdAt`/`lastUpdatedAt`) and
  `bubbleId:<conv>:<bubble>`. Bubble `type` 1=user, 2=assistant;
  `toolFormerData.{name|tool, params|rawArgs, status, error, result}` (`:105-328`).
- ADR sorts bubbles by `_bubble_id` (`:213`), a field it never sets. Inference: the order
  is undefined in practice.
- It does not read `agent-transcripts` at all.
- SPS adds:
  - WSL roots `/mnt/c/Users/*/AppData/Roaming/Cursor`.
  - `unifiedMode` and `toolFormerData.userDecision`.
  - Live writes land in `-wal`, which is deleted and recreated, so watch the directory
    (`CURSORIDE-FORMAT.md:40-103, 156-198`).
  - **Cursor CLI also keeps `~/.cursor/chats/<md5(cwd)>/<sessionId>/store.db`** (SQLite
    `blobs` and `meta`) (`cursorcli/path_utils.go:13-61`).
  - It calls `agent-transcripts` "a flattened text-only transcript (no tool results)"
    (`docs/CURSOR-CLOUD-AGENTS.md:23`), which matches tvdash's "model writing again" rule.
- **Wrapper handling** (CPR `codex-rs/external-agent-migration/src/sessions/records_cur.rs:140-240`):
  - Records are keyed by top-level `role`. `isMeta` and `isSidechain` records are skipped.
  - Timestamp order: `timestamp` (RFC 3339), then `timestamp_ms`/1000, then file mtime.
  - A user record whose content is only `tool_result` is treated as assistant.
  - `unwrap_user_query` unwraps only when four conditions hold:
    1. `<user_query>…</user_query>` exists.
    2. Nothing but whitespace follows `</user_query>`.
    3. Everything before it is a sequence of complete `<timestamp>` and `<cursor_commands>` blocks.
    4. The inner text is non-empty.
  - Otherwise it returns the **whole original text**.
  - **tvdash differs:**
    - It takes the text after the first `<user_query>` whatever surrounds it.
    - It treats any text starting with `<` as all-context, and logs no prompt for it
      (TVD `src/sessions/cursor.rs:107-114`).
    - It doesn't read `timestamp_ms`.

### Gemini CLI (ADR, SPS; checked against GEM)
- Roots: `$GEMINI_CLI_HOME` (default `~`)`/.gemini/tmp`, and on macOS also
  `~/.cache/.gemini/tmp` (ADR `S/parsers/gemini_parser.py:26-34`).
- Layout: `<project>/chats/*.{json,jsonl}`. The project path comes from
  `<project>/.project_root`, or from `projects.json` (`:262-286`). The older layout uses
  `sha256(path)` as the folder name (SPS).
- JSONL "journals" (`:120-159`) hold three kinds of record:
  - messages, upserted by `id`;
  - `{"$set": {...}}` metadata updates (`sessionId`, `startTime`, `lastUpdated`, `kind`);
  - `{"$rewindTo": id}`.
  - Legacy `.json` files hold the whole session and can coexist with the `.jsonl`.
- Message `type`: `user`, `gemini`, `info`, `error`, `warning` (NCS).
- `tokens.{input, output, cached, thoughts, tool, total}`.
- Subagents: metadata `kind:"subagent"` in a `chats/<parentId>/` subfolder (`:231-232`;
  GEM `services/chatRecordingService.ts:786-814`).
- Upstream (GEM `paths.ts:13-28`, `storage.ts:92-233`, `chatRecordingService.ts:1037-1056`):
  - The project slug comes from `projects.json`.
  - The `~/.cache/.gemini` root applies only under `SANDBOX=sandbox-exec`.
  - **A message is re-appended whole on each update under the same `id`, so the last line wins.**
  - A `{$patch}` record exists too.

### OpenCode (ADR, SPS; checked against OPC, MIT-licensed)
- Root: `$XDG_DATA_HOME/opencode`, default `~/.local/share/opencode` **on every OS**, plus a
  defensive macOS `Library/Application Support/opencode` probe
  (ADR `S/parsers/opencode_parser.py:4-7, 106-112`).
- Database: `$OPENCODE_DB` (`:memory:` means none), else `opencode.db`, else
  `opencode-<channel>.db` (`:120-150`). Open it read-only (`?mode=ro`, `:163`).
- Tables:
  - 1.x: `session`, `message`, `part`, with JSON in `data` (`:9-22`).
  - 2.x: `session_v2`, `session_message`, whose `type` includes `idle`, `compaction`,
    `shell`, `agent-switched` (SPS).
  - Older: the JSON tree `storage/session|message|part/...` (`:318-381`).
- Subagents:
  - a `subtask` part (`:479-487`);
  - `session_v2.parent_id`, and `metadata.sessionID` on the `subagent` tool (SPS).
- Change detection: the assistant row is rewritten in place, so detect changes with
  `time_updated` (SPS).
- Upstream `session_v2` has `parent_id`, `time_idle`, `idle_outcome` and `time_compacting`
  (OPC `packages/core/src/session/sql.ts:23-69`). There is also an ordered durable `event`
  table (`packages/core/src/event/sql.ts:10-25`).
- Windows path: `%USERPROFILE%\.local\share\opencode`
  (`packages/web/src/content/docs/troubleshooting.mdx:27-36`).

## 4. Quirks

| quirk | evidence |
|---|---|
| Several JSON objects on one physical line, and NUL padding between them | ADR accepts both via `raw_decode` in a loop (`S/parsers/claude_parser.py:164-191`; tests `Sensor/tests/test_claude_diagnostics.py:116-123`). **tvdash's `serde_json::from_slice` per line drops such a line** (TVD `src/sessions.rs:729-735`). Whether Claude really writes them is unknown. |
| Incomplete-tail classifier | An unterminated final line whose decode error is "Unterminated string", a truncated literal or number, or a partial `\u` escape counts as `incomplete_record` (expected). The same error on a terminated line counts as corruption (`claude_parser.py:193-207`; tests `:100-113`). This is finer than tvdash's "ends with `}`" probe. |
| Final line without a newline | CDT consumes it if it parses, otherwise pins the offset. It strips CRLF and fully reparses after a shrink (`FileWatcher.ts:731-786`). This is the same rule as tvdash. |
| Timestamps | ADR normalises ISO strings with any offset, and epoch seconds or ms (>1e12 means ms) (`S/utils/timestamp_utils.py:9-50`). **tvdash's `parse_rfc3339` only strips `Z`; an offset such as `+02:00` fails and the time silently becomes `now()`** (TVD `src/sessions.rs:409-430`). Cursor CLI uses `timestamp_ms` (CPR). Desktop and Cursor DB use epoch ms. |
| Huge lines | SPS caps a record at 64 MB and skips it (`spi/jsonl.go:8-49`). CSP caps a file at 200 MB. |
| fd exhaustion | SPS's kqueue watcher ran out of descriptors at about 25k Codex files, so it watches a 7-day window (`spi/watch_window.go:14-25`). This supports tvdash's polling design. |
| SQLite opened writable | SPS forces `journal_mode=WAL` read-write (`spi/sqlite.go:18-29`). ADR's Cursor reader uses a plain connect. Both break our read-only rule. |
| Pi | Writes nothing until the first assistant message (SPS `PI-FORMAT.md:25`). |

## 5. Architecture worth taking

- **Detect API**: NCS `SessionSource::detect(&Path) -> f64`, a registry where the highest
  score wins, and `SessionRef{format, location: File|Directory|Database, mtime,
  parent_session_id, agent_id, subagent_type}` (NCS
  `crates/normalize-chat-sessions/src/formats/mod.rs:105-281`). This fits PLAN's
  `detect(path) -> Vec<FormatMatch>`. Add `Database{db_path, session_id}` for Cursor and
  OpenCode.
- **Diagnostics as data**: ADR has a closed vocabulary of content-free codes
  (`S/parsers/base_parser.py:27-63`), with `suspected_schema_drift` derived from
  `unsupported_record_type|unsupported_content_block|unsupported_schema`
  (`S/diagnostics.py:51-85`). Copy this as-is for PLAN §4's "unknown record type increments
  a counter".
- **Drift audit**: PCC inventories path→kind maps per record type and diffs them against a
  committed baseline; the exit code means drift (`scripts/audit-log-schema.ts`,
  `scripts/audit/compare.ts:26-55`). **Gap:** it records value *kinds*, not discriminator
  *values*, so a new `system.subtype` or `permissionMode` value is invisible. Our baseline
  must record value sets for `type`, `subtype`, `attachment.type`, `content[].type`,
  `permissionMode`, `stop_reason` and Codex `payload.type`.
- **Incremental**: ADR's "incremental" (#130) is snapshot-level only: it re-parses the whole
  file and compares `(last_event_at, event_count)` revisions and content hashes
  (`S/observer.py:737-790`, `S/exporters/delivery_checkpoint.py`). There is no byte-offset
  tailing to borrow. CDT's `FileWatcher` offset tail is the closest match to tvdash.
- **Redaction**: ADR Discovery keeps names and shapes and drops only values
  (`Discovery/adr_discovery/redact/rules.py:1-10`). That is the same principle as PLAN §7.
- **Avoid ADR's model**:
  - text is flattened (`content` = joined text);
  - args and results are cut at 1,000 chars (`S/parsers/claude_parser.py:150-162, 345`);
  - usage is per session, and Claude usage is not read at all;
  - there is a 14-day mtime window (`:25`);
  - it uses no record ordering beyond file order;
  - it has no status.

## 6. Where tvdash is contradicted or extended

1. **`CLAUDE_CONFIG_DIR`** is ignored by tvdash. CCU treats it as a comma list plus an XDG
   root; SPS as a single path. Which is right needs upstream docs.
2. **`CODEX_HOME`** is ignored by tvdash (hard-coded `.codex/sessions`, TVD
   `src/sessions/codex.rs:24-26`). ADR, NCS, SPS and CCU all honour it.
3. **Codex `state_*.sqlite` thread catalog** (ADR) could find live rollouts without
   day-folder walks.
4. **Rejected tool** (`"User rejected tool use"`, CDT) isn't a turn end in tvdash; today it
   reads "thinking".
5. **Background agents**: `toolUseResult.isAsync`/`backgroundTaskId` exist (PCC baseline).
   tvdash's helpers are Cursor-only, and Claude `max_depth = 2` excludes `subagents/`
   (TVD `src/sessions/claude.rs:26-28`).
6. **Usage dedupe** rule (first row) disagrees with every other project.
7. **Timestamps with offsets** silently become `now()` (section 4).
8. **Cursor wrapper**: CPR's rule is stricter; adopt it, and also log Cursor's
   `<timestamp>`-only prompts.
9. **Concatenated objects on one line** are dropped (section 4).
10. **Claude `continued-in`** (CPR) and **Codex `turn_aborted.reason`, `task_complete.error`** (CDX) are unread; the first may make a "done" session continue in another file.
11. **Cursor CLI `~/.cursor/chats/.../store.db`** isn't read; it may hold tool results that
    `agent-transcripts` lacks.

## 7. Open questions (need real artifacts or harness CI)

1. Is `permission-mode` written once per change, or on every prompt? Does `user.permissionMode` agree with it? What is the full vocabulary (`default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk`?)? None of these projects lists the values.
2. Codex: CDX says approval and `request_user_input` events are never persisted. Confirm on a 0.16x rollout. If true, Codex `request_user_input` can only be detected as a pending `function_call` by name, as tvdash does today.
3. Which Claude tools auto-approve in `default` and `acceptEdits` mode, per version? Nobody documents this.
4. How does a Claude background agent's end appear: a parent record (`agents_killed`? a task-notification user message?), or only the subagent file going quiet?
5. Do per-content-block rows repeat identical `usage`, or do the counts grow? This settles first, last or max dedupe.
6. Claude's project encoding today: CSP's (all non-alphanumerics, 200-char hash) or a simpler one? Check against `cwd`s on Lee's Mac.
7. Is `~/.claude/sessions/<pid>.json` still written and removed on exit? Would a liveness check need a pid probe, which conflicts with "no subprocesses" (`kill(pid, 0)` is a syscall, not a subprocess)?
8. Cursor: what do `store.db` and `toolFormerData.userDecision` show while a tool awaits approval?
9. Is Gemini `awaiting_approval` ever on disk (inference says no)? Is Copilot `permission.requested` still written? Check both in harness CI.
10. Does any harness write CRLF, or several objects on one line, in practice?


## Verification against real artifacts (2026-10-25)

- **Gemini CLI 0.65.0:** the claim that tool calls are written only once they complete is
  **confirmed**. A `run_shell_command` call appeared only in the second append of its
  message, with `result`, `status: "success"` and the tool's own timestamp. The approval
  mode and a pending/`awaiting_approval` call were not written. See
  [`docs/formats/gemini.md`](formats/gemini.md).
- **Copilot CLI 1.0.103:** the claim that `permission.requested` / `permission.completed`
  are persisted is **confirmed**. The request carries `toolCallId` and `permissionMode:
  "manual"`; prompt mode without a user completed it with
  `denied-no-approval-rule-and-could-not-request-from-user`. See
  [`docs/formats/copilot.md`](formats/copilot.md).
