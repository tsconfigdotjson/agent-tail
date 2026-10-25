# Decisions

What was decided, and why. Newest last. PLAN.md section 2 holds the decisions made before
any code; these are the ones made while writing it.

## Phase 1a: the extraction (2026-10-23)

**Behaviour-identical first.** The adapters, `lean`, the line reader, the status rules and
the `Tracker` were moved from tvdash (`src/sessions.rs`, `src/sessions/{claude,codex,cursor,lean}.rs`)
with their logic unchanged, and all 60 of their tests were ported. Three test code that
stays in tvdash and were ported only for the part that moved: `view_reports_minutes_since_last_change`
(the minute rounding stays; `since` is tested), `view_names_the_agent_and_host` (hosts stay;
ids and names are tested) and `filters_are_checked_with_a_useful_message` (the hours limit
stays; unknown ids are tested). Where the shape had to
change for a sans-IO library with injected time, the change is listed under "Where
behaviour differs" below.

**Layout.** A cargo workspace: `crates/agenttail` (the library) and `crates/agenttail-cli`
(the `agenttail` binary). Edition 2024, MSRV 1.88 (let chains), checked with a 1.88
toolchain. `publish = false` until the name and licence are settled.

**Dependencies: serde, serde_json, memchr.** No chrono: tvdash used it for one date
subtraction (Codex's day folders) and for Cursor's `<timestamp>` dates. `time.rs` does both
with Howard Hinnant's days/civil algorithms (the same tvdash's `parse_rfc3339` already
used) plus a date check, and is tested against round trips and known epochs. Dev-deps:
proptest and tempfile. No criterion: the bench is a plain `harness = false` binary, which
is all a single "how long does the first read take" number needs.

**Time is a `Timestamp(f64)`**, seconds since the epoch: what tvdash computed with
throughout, and what remote frames carry. It converts to and from `SystemTime`. No code in
the library reads the clock; the tests use a fixed `NOW` (2026-10-23T12:00Z), except the
`Tracker` tests, which pass the wall clock in because the OS sets file mtimes.

**The sans-IO core is `SessionReducer`.** `feed(bytes, ChunkCtx { offset, mtime, path })`
takes complete lines plus a last line that's already a whole JSON object (tvdash's
`complete_lines`), and holds back the rest. `feed_part` is the same for a chunk that more
follows at once (tvdash's `whole_lines`, used for every 1 MB chunk but the last). The
reducer owns the held-back tail, as tvdash's remote `RemoteFile.pending` did, and the
offset rules came from there: a chunk must start at `received()`; at `consumed()` (the
held-back line resent: it's dropped; this is also how a reconnect resumes); or at 0 (file
replaced: start over). A fresh reducer takes any offset. Anything else is
`Error::OffsetMismatch`. The remote's "no newline and no closing brace: don't parse yet"
shortcut is in `feed` too, so a huge line arriving in small pieces isn't rescanned each
time.

**The `Tracker` re-reads the held-back line from disk, as tvdash does**, rather than
keeping it between polls: it reads from `consumed()`, which drops what the reducer held.
Same bytes read, same memory, same results.

**Helpers: `helper_written(mtime)`, not `feed_helper(bytes)`.** Today a helper's transcript
isn't read, only its mtime counts as its session's activity; taking bytes nobody reads would
make callers ship them. When Phase 2 reads helpers, `feed_helper` arrives with them.
`transcript_written(mtime)` is its counterpart for the session's own file (the tracker
updates the mtime on every poll, new bytes or not).

**`SessionState` and `Activity`.** `Activity` has every variant of PLAN.md section 4 now
(`#[non_exhaustive]`), so tvdash writes its mapping once; today's rules produce seven:
Question → `AwaitingAnswer`; tool call stalled 20 s → `AwaitingApproval` with certainty
`Inferred`; tool call → `RunningTool`; prompt or tool result → `Thinking`; end of turn →
`Done`; nothing written for 15 min → `Idle`; nothing that moves the state read yet →
`Unknown`. Every other combination is `Firm`, including idle-by-age (it's the definition of
idle, not a guess). `Reason` names the rule (`no_entries`, `no_writes_for`, `ask_user_tool`,
`tool_call_stalled`, `tool_call_pending`, `prompt_sent`, `tool_result_in`, `turn_ended`).
The internal `Last` is kept as it was.

**A coarse `StatusClass` (needs-you, working, done, idle) is in the library**, as
`Activity::class()`: the brief asked for the status class, the tracker sorts by it as
tvdash does, and it's an enum, not wording. tvdash's six labels ("may need approval"…) are
not in the library; `testutil::label` shows the mapping tvdash keeps, and lets the ported
tests read as they did.

**Activity lines are structured.** `ActivityLine::{Prompt, Command, Tool}` instead of
preformatted strings, with a `Display` that writes exactly tvdash's lines (`› fix the
build`, `$ cargo test`, `Edit routing.ts`, `Edit ` for a patch with no file). The ring's
length is `ReducerOptions::recent_lines` (5).

**`ParseMode::Lean` is the default**, as tvdash reads; `Full` parses lines whole. There's
no event layer yet to carry a per-event `truncated` flag, so `Diagnostics` counts lines
cut (and lines read, and non-JSON lines skipped) instead: data, never logs.

**Layouts are data.** `Harness::layout()` returns `Layout { root, max_depth, sessions_in,
helpers_in, day_folders }` with `role()`, `walk_depth()` and `may_hold_since()` on it: what
tvdash's `remote::SCRIPT` reads off the adapters today. The `Adapter` trait itself (start,
apply) stays crate-private: its methods take the internal `Session`.

**`Filter { window: Duration, harnesses }`** replaces tvdash's `SessionFilter { hours,
agents }`. A `Duration` can't be negative or NaN, so there's nothing to check; tvdash's
`MAX_HOURS` limit and its messages stay in tvdash. Unknown harness ids are an `Error` with
tvdash's message (`no agent "vim" (there are: …)`) through `Harness::from_str`.

**The CLI parses its few arguments by hand**: two commands and two options don't need
clap. Output is JSON lines.

**Formatting is dense** (`max_width = 120`, `use_small_heuristics = "Max"`), close to how
tvdash reads.

### Where behaviour differs from tvdash, and why

1. **Entries with no time of their own** (Claude Code and Codex lines without a
   `timestamp`, none seen in practice; every Cursor line) are dated by the transcript's
   mtime as of the chunk, not by the time they were read: the library can't read the
   clock. For Cursor this is what tvdash already did whenever the mtime was known (always,
   outside tests). For the others it differs only when a timestamp is missing, and then
   the mtime is the better answer (a transcript first read hours after it was written no
   longer looks fresh).
2. **The sweep timer runs on the injected `now`**, not on `Instant`. A `now` earlier than
   the last sweep (the clock went back) sweeps again rather than waiting.
3. **Ties sort by path.** tvdash ordered sessions of equal class and write time in hash
   map order; the tracker now breaks ties by key, so a poll's order is deterministic.
4. **Cost is `Option<f64>`**: `None` until the harness records one, where tvdash had `0.0`.
   tvdash maps `None` to `0.0`.
5. **A session with nothing read is `Unknown` for reason `no_entries`** even when its file
   is old; tvdash said idle (its age rule came first). Issue #62 switched this from the
   extraction's `Idle` to match PLAN.md section 4: an unread session is not idle work.
6. **On non-Unix systems** a file's identity is unknown, so a transcript replaced by a
   longer file isn't noticed (one replaced by a shorter one is). tvdash only built on Unix.

## Phase B: firm status (2026-10-24)

**The real-harness rig came first.** `harness-ci/` has a scripted mock model server, a PTY
driver with a vt100 screen parser, and a scorer that compares the screen with
`SessionReducer::state` every 100 ms. Claude Code 2.1.310 runs fully offline against it
(temp `HOME`/`CLAUDE_CONFIG_DIR`, seeded onboarding, `sandbox-exec` allowing only
localhost). The rig found the headline bug before it was fixed: under
`--dangerously-skip-permissions`, a 60 s `sleep` is `RunningTool` on screen while the old
rule said `AwaitingApproval` after 20 s. It now passes.

**Permission modes are data, not a guess.** `PermissionMode` parses the values Claude Code
and Codex write (`default`, `acceptEdits`, `plan`, `bypassPermissions`, `dontAsk`, `auto`,
`manual`, `normal`, `never`, `on-request`, `untrusted`; anything else is `Unknown`).
`Session::can_ask_human()` is false for `bypassPermissions`, `dontAsk`, `never`, Claude
Code's `entrypoint: sdk-cli` and Codex's `approvals_reviewer: auto_review`. A pending tool
in such a session is `RunningTool`, firm, however long it runs. An unknown or absent mode
keeps the 20 s inference. The mode is exposed as `SessionState::permission`.

**Turn ends, interrupts and errors are separate.** `Last` gained `Interrupted` and
`Errored`, with `Activity::Interrupted`/`Errored` and matching `Reason`s. Claude's
`system/turn_duration` and `agents_killed` end a turn; the interrupt text ends it as
`Interrupted`; a synthetic assistant row with `isApiErrorMessage`/`error` is `Errored` and
keeps the error string in `SessionState::error`. Codex `turn_aborted` is `Interrupted`, and
`task_complete`/`turn_complete` with an `error` is `Errored`. Cursor's `turn_ended` is
`Interrupted` for `status: aborted` or an error string containing a user-stop phrase
("abort", "cancel", "interrupt", "stopped by user"), and `Errored` otherwise when
`status: error`. A tool error alone does not error the session.

**Mock latency matters for transcript order.** With an instant mock reply, Claude Code can
append the user-prompt record after the assistant reply; the reducer then reads a prompt
after `Done` and says `Thinking`. Real model latency hides this, so scenarios give the
first token a 1 s delay. A prompt record older than the latest activity should not move
the state backwards: still to test and decide.

## Phase B5: background helpers (2026-10-24)

**Helpers are read, not just counted as writes.** `Layout` gained `helper_owner`: Cursor
keeps a session at `<id>/<id>.jsonl` with helpers in `<id>/subagents` (`SessionFolder`),
while Claude Code keeps `<id>.jsonl` with helpers in the sibling `<id>/subagents`
(`SessionFileStem`). `walk_depth` and `Role::role` account for the extra folder depth.
Claude Code's `max_depth` stays 2 for sessions, but the walk reaches 4 for helpers.

**`SessionReducer::feed_helper(id, bytes, ctx)`** follows each helper with the same parser
and status rules as its parent. Helper records carry `isSidechain: true`; `Session` has an
`is_helper` flag so the Claude adapter stops filtering them for helper reducers only. The
Tracker remembers each helper's byte offset and feeds new bytes to the session it helps,
keeping the old mtime-only `helper_written` behavior as well.

**`SessionState.background` and `BackgroundWork`.** `HelperState` carries the helper's id
(its transcript path), activity, certainty, reason and times. A helper is running unless its
own state is `Done`/`Interrupted`/`Errored`/`Idle`/`Unknown`. When the parent turn ended and
either a helper is running or Claude's `turn_duration.pendingBackgroundAgentCount` is above
zero, the activity becomes `BackgroundWork` (reason `BackgroundHelpers`). The parent's count
is firm; a helper transcript alone is inferred. The count is decremented by each
`task-notification` (`completed`/`failed`/`stopped`), and `task-notification` records are
explicitly excluded from prompts via `origin.kind`. `agents_killed` clears the count.

**Cursor helpers now have their own `turn_ended` read**, so a finished subagent and a stuck
one no longer look alike. The existing fs test now asserts BackgroundWork while the helper
runs and Done after its `turn_ended`.

**B5 linkage is done.** A parent `toolUseResult` with `status: async_launched`,
`isAsync: true`, or a `backgroundTaskId` is remembered by its `agentId`, `outputFile`,
task id and spawning tool id. The launch alone keeps a finished turn at
`BackgroundWork` (inferred) until its helper transcript is read, and the transcript's
path is matched back to the launch by `outputFile`/`agent-<agentId>.jsonl`. A helper's
sibling `.meta.json` is fed with the same id; `finished: true` or `stoppedByUser: true`
is a firm end even when the transcript's last entry is still a pending tool. A
`task-notification` carrying `<task-id>`, `<tool-use-id>` or `<output-file>` ends the
matching launch and counts it down once; one without ids keeps the old count fallback.
Missing or malformed metadata, and metadata for another helper, change nothing. The
`background-link` status fixture covers launch → `BackgroundWork` → `.meta.json` →
`Done` without relying on `pendingBackgroundAgentCount`. The real background-Agent
driver scenario is still count-driven: its driver does not tail helpers yet.

## Phase B6: parser edges (2026-10-24)

**Full RFC 3339 offsets.** `parse_rfc3339` now accepts `Z`, `z`, `+02:00`, `-0500`, and a
missing offset (UTC). The date is parsed once and the offset is subtracted from the local
time, so a timestamp with an offset no longer falls back to the transcript's mtime.

**Several JSON objects on one line are all read.** `parse_many` uses
`serde_json::Deserializer` as a stream. A line with trailing junk is still rejected whole
(nothing is guessed), but a line holding two complete objects applies both. The same path
handles a last line with no newline. `Diagnostics::lines` counts entries now, not physical
lines.

**CRLF** needed no special-case (`serde_json` accepts the `
`), but a test now covers it
together with an offset timestamp.

## Phase B7: usage (2026-10-24)

**Claude's per-message rows repeat or grow, so use the highest value.** A structure-only
survey of this Mac's Claude Code transcripts found 11,798 assistant messages split over
24,617 rows: 8,107 messages had identical usage on every row, 369 grew (e.g. output 8 then
135), and none shrank. `Session::record_usage` keeps the highest value each field has
reported for a `message.id` and adds only the increases, so repeated rows don't overcount
and an early row doesn't undercount. The old first-row rule was the undercounting case.

**`SessionState.usage` is the reported counts.** `TokenUsage` has `input_tokens`,
`output_tokens`, `cache_read_input_tokens` and `cache_creation_input_tokens`, each
`Option<u64>` and `None` when the harness doesn't record it. `out_tokens` stays as the
StatusWall card convenience and equals `usage.output_tokens.unwrap_or(0)`. Codex's
`token_count.total_token_usage` is a running total, so it replaces the usage instead of
being summed; `cached_input_tokens` maps to cache-read. Cursor reports none.

**B7 Codex titles are done.** The tracker reads `session_index.jsonl` beside the Codex
`sessions/` root (so `CODEX_HOME` works), keyed by `session_meta.payload.id`. A non-empty
`thread_name` wins over the opening prompt; the prompt remains the fallback when the index
is missing, malformed, empty, or no longer names the thread, and later prompts never
overwrite either title. Cursor's title from `state.vscdb` is still to do (B9).

## Phase B8: folder overrides (2026-10-24)

`Tracker::from_env(home, filter)` reads the harnesses' own folder overrides and the CLI uses
it. `CLAUDE_CONFIG_DIR` points at the config folder, so its root becomes
`$CLAUDE_CONFIG_DIR/projects`; `CODEX_HOME` becomes `$CODEX_HOME/sessions`. Only absolute
paths are used. Claude Code 2.1.310 treats `CLAUDE_CONFIG_DIR` as **one** path, not the
comma-separated list ccusage assumes: the binary's `s()` returns `process.env.CLAUDE_CONFIG_DIR`
directly, and the offline driver run wrote to the temp folder. A4 confirmed Codex 0.160.1's
`CODEX_HOME` is the `.codex` home with rollouts below `sessions/`; it also accepts relative
values against the cwd, which the tracker intentionally ignores so a random cwd cannot move
the root.

The remaining B8 work is the `spec/discovery/*.toml` data files (ask-user lists, auto-approve
lists, mode vocabulary, quirks with evidence) and loading `Layout` from them.

## API errors stay Errored (2026-10-24)

Claude Code writes a synthetic assistant row (`isApiErrorMessage` / `error`) and then
`system/turn_duration`. The duration record is still the firm end of a normal turn, but it no
longer replaces `Errored`: that row is the error, and the duration only timestamps the close.
A later real assistant `end_turn` still clears it. `pendingBackgroundAgentCount` on the same
duration record does not promote the error into `BackgroundWork`.

On an instant failure, Claude Code 2.1.310 can flush the user prompt *after* that error, with
an earlier timestamp. A prompt older than activity already recorded does not start a new turn,
so the session stays `Errored` (the same rule keeps an instant `Done` from rewinding to
`Thinking`). A prompt newer than that activity still does.

## Firm turn ends and Codex policies (2026-10-24)

**Claude's `system` ends are one turn.** A normal turn is `assistant(stop_reason=end_turn)`,
then `system/stop_hook_summary` when hooks ran, then `system/turn_duration` about 0.06 s
later. All three are firm ends here, and `turn_duration` is still the firmest: it is written
once after the hooks and carries `pendingBackgroundAgentCount`. `last-prompt` is a metadata
flush written mid-turn as well as at its end, so it is not read as an end. `agents_killed`
ends the turn and clears the pending-helper count. A system end after an API-error row only
moves its timestamp; the session stays `Errored`.

**"User rejected tool use" ends the turn.** Claude Code's A3 denial scenario records the
rejection as a tool result (`toolUseResult: "User rejected tool use"`, `toolDenialKind:
user-rejected`, or the "doesn't want to proceed" result text), followed by the interrupt
text and `turn_duration`. The rejection itself is now a firm end too, so a transcript read
between those records does not sit at `Thinking`; the interrupt then makes it `Interrupted`
until the duration closes it as `Done`. The exact top-level fields and tool-result text are
matched, not a phrase quoted in a prompt or assistant reply.

**Codex's sandbox is recorded, but never substitutes for an approval policy.**
`turn_context.sandbox_policy.type` becomes `SessionState.sandbox` (`danger-full-access`,
`read-only`, `external-sandbox`, `workspace-write`, or `unknown`). The sandbox bounds what a
command may do; `approval_policy` says when Codex may stop and ask. Full access is the
combination of `danger-full-access` with `approval_policy: never`, so `danger-full-access`
alone does not make a pending tool firm. `never`, `approvals_reviewer: auto_review`, and an
SDK entrypoint are the firm no-human cases; `on-failure` is read as Codex's older name for
`on-request`, and a `granular` policy counts as asking only when one of its interactive
categories is enabled.

**Cursor's record order is its clock for prompts.** Cursor puts timestamps only in prompts;
assistant and `turn_ended` records are dated by the file mtime. Comparing a later prompt's
timestamp with that mtime made a legitimate second turn look like a late flush and dropped
it from the log. Cursor prompts now use record order, while Claude and Codex keep the
older-timestamp guard that stops a late-flushed prompt from rewinding `Done`/`Errored`.

Fixtures with `expected/status.jsonl` checkpoints live under
`fixtures/<harness>/<version>/<os>/<scenario>/`; `crates/agenttail/tests/status_fixtures.rs`
runs them. The denial scenario is also scored by the existing A3 harness run.

## Phase B8: discovery specs, project from cwd, and the registry (2026-10-24)

**`spec/discovery/*.toml` is the source of the layouts.** `crates/agenttail/build.rs` parses
and validates the three files at build time and generates `Discovery` and `Layout`: roots,
depth, `sessions_in`, helper owner, day folders, the file suffix, ask-user names, observed
never-prompt tools, mode vocabulary, timestamp rules, project encoding, quirks, and the
registry decision. The adapters' ask-user matching and `Tracker`'s environment overrides come
from the same data, so `Harness::layout()` no longer has a second hard-coded copy. The build
fails on a missing field, contradictory layout, duplicate name, or missing evidence. The
generated source is in `OUT_DIR`; the TOML stays the file people edit.

**Project comes from `cwd` records, never the folder name.** Claude Code writes `cwd` on its
conversational records and Codex writes it in `session_meta`/`turn_context`; both already used
it. Cursor has no `cwd` records, so it uses a workspace path from a prompt or tool call when
one appears and keeps the lossy `<project>` folder only as a fallback. The Claude folder
encoding was checked against this machine's `~/.claude/projects` on 2026-10-24: 233 of 243
project folders had a `cwd` record, every one matched "ASCII alphanumerics kept, every other
character -> `-`", and the other 10 had no `cwd`. No non-ASCII or >200-character `cwd` was in
the sample, so the prior art's 200-character hash fallback is still unverified. The encoding is
lossy (`my-app`, `my/app`, and `my.app` can collide) and is not decoded.

**Never-prompt lists contain only what was observed.** Claude Code lists `Write` under
`acceptEdits`, and `Read`/`ToolSearch` under `plan`, all from A3. `Bash` is deliberately absent
because it prompted in default, plan, and `acceptEdits`. Codex's list stays empty after A4:
an `exec_command` may run inside `workspace-write` or may prompt when it requests escalation,
so the tool name alone does not prove a no-human path; `never` is a mode-level fact. The Cursor
corpus saw no approval prompt or auto-run setting. An empty list means "not observed", not
"this tool prompts".

**The `~/.claude/sessions/<pid>.json` registry is not read.** A3/PR #32 showed useful facts:
`busy` and `idle` agree with the screen, a permission prompt is `waiting` /
`waitingFor: "permission prompt"`, and a graceful exit removes the file. But `waiting` also
covers `/model` and `/background` dialogs, SIGKILL leaves a stale file, and the registry is a
snapshot rather than a transcript signal. Using it would need a liveness check
(`kill(pid, 0)`) and would still not replace the existing transcript rules. The TOML records
the evidence with `used = false`; the tracker does not open the registry, so it adds no
process probe to "what this touches".

## Codex real harness (A4, 2026-10-24)

**Codex 0.160.1 runs offline against the mock.** A temp `CODEX_HOME` plus a user-level
`config.toml` custom `model_providers` entry (`base_url`, `env_key`, `wire_api = "responses"`)
is enough; `HOME` is also temp. The driver adds `--no-daemon` because the interactive CLI
otherwise starts a persistent `app-server --managed-daemon` that outlives the PTY and keeps
the temp home open.

**Approval and ask-user events are not persisted.** A pending approval or
`request_user_input` is a `response_item/function_call` with no matching
`function_call_output`. A4 saw no `exec_approval_request`, `apply_patch_approval_request`,
`request_permissions`, `elicitation_request`, `exec_command_begin/end` or `error` record. The
harness scenarios assert that negative. `approval_policy = "untrusted"` is retired in
0.160.1 and rejected at startup, so the live pending scenario uses `on-request` with an
escalating `exec_command`; the adapter still reads old `untrusted` rollouts.

**`task_complete.error` can be an object.** 0.160.1 writes
`error: {"message": …, "codex_error_info": …}` rather than the string in older fixtures. The
adapter now accepts either shape and keeps the message in `SessionState.error`, so a terminal
HTTP failure is `Errored`, not `Done`.

**Titles and index.** Codex requests a one-line thread title through an extra
`/v1/responses` call; the mock answers it without consuming a scenario step. The live CLI did
not write `session_index.jsonl`; its thread names live in SQLite. B7's index reader remains
for older/other layouts, and the harness records any index entries it finds but does not
fabricate one.

**Resume and history.** `codex resume <id>` appended to the same rollout file (one file, two
prompts). The live CLI did not create `.jsonl.zst`; old-day-folder resume and compressed
history remain fixture-backed. The Codex matrix pins 0.160.1, the version of the committed
rollouts, and keeps the Unix-musl/bwrap warning visible rather than hiding it.

## Phase B10: the event layer (2026-10-24)

**The adapters parse records into events; the reducer applies events.** Each adapter's
`parse` returns `EventSeed`s with normalized payloads, and `Session::apply` and the public
`parse_lines` go through the same parser. The reducer's `Session::apply_event` is the only
place an event becomes status, so an event dump cannot show a different story from the
status. `SessionReducer::feed_event`/`feed_events` expose that path too.

**One `Event` carries common provenance plus an `EventKind`.** Common fields are the
deterministic id, harness, source index, byte offset/length (in the original bytes, even
under `Lean`), one-based physical line, the harness's own record ids (`uuid`, `message.id`,
`call_id`, …), and the record time when it has one. `parse_lines_with_source` lets callers
number several files; `parse_lines` uses source 0.

**Usage events are additive.** Claude's per-row usage repeats or grows, so a `Usage` event
is emitted only when a message's highest value changes, with `usage` as the increase and
`reported` as the new high-water mark. Codex's running `token_count` emits the delta in
`usage` and the absolute `reported` total with `cumulative: true`. Summing `usage` fields
therefore equals the true total, while the reducer keeps the same reported state as before.

**Known metadata stays `Meta`; unknown top-level records stay `Unknown`.** `Meta` keeps the
whole record for known types with no richer event (attachments, request digests, ADR's
wider Codex catalogue). `Unknown` keeps the whole record and a reason for a new top-level
type, a non-object JSON value, or a malformed line, so none is silently dropped. Unknown
content blocks inside a known record are kept too; ADR's 20 Claude content types and the
baseline's Codex/Cursor content types map to a richer event or `Meta`.

**`agenttail events [--agent ID] <path>` prints the stream as JSONL.** It infers the
harness from the path first, then from the record shapes, and `--agent` overrides both.

**Sidechain rows are marked on the event, not decided by stand-alone parse state.**
Claude Code's `isSidechain: true` records are emitted by `parse_lines` too, with
`Event::sidechain` set, because an event dump should show every record. `Session::apply_event`
ignores them unless the reducer is following a helper transcript, so a parent fed the same
events as bytes comes to the same status. Their usage high-water marks are kept apart from
the parent's, so a helper row can't suppress a parent row that reuses a message id. The
adapter no longer uses a stand-alone `is_helper` parse flag to decide whether to parse them.

## Phase B9: Cursor `state.vscdb` and the A5 decision (2026-10-24)

**A5: Cursor stays out of the mock/PTY rig for v0.1.** `cursor-agent` 2026.10.01-e373342
honours `--endpoint`/`CURSOR_API_ENDPOINT`, but the endpoint is not a model API: the CLI
exchanges an API key at `/auth/exchange_user_api_key`, calls Cursor's Connect/protobuf
`aiserver.v1.*` services for config and models, then opens a streaming Cursor RPC rather than
Anthropic Messages or OpenAI Responses. A hand-made local endpoint got as far as the model
catalogue, then failed the chat RPC (`ERR_HTTP2_ERROR`). Cursor therefore ships through
donated fixtures, with a budget-capped real run only if a key is available later; the IDE was
already out of CI scope. The exact request sequence is in `harness-ci/README.md` and the
format notes.

**The `sqlite` feature snapshots, never opens the live database.** `read_state_vscdb` copies
`state.vscdb` plus `-wal` and `-shm` into a private temp folder (mode 0700, files 0600),
opens the copy `SQLITE_OPEN_READ_ONLY`, and removes the folder with an RAII guard. The WAL
copy is required because live Cursor writes land there. The tracker checks the three files'
stamps every poll but copies them at most every 30 s, and only when a Cursor session is in
the window; a first poll with no Cursor session never opens the database.

**Cursor titles and turn states come from `composerData`.** `name` is the title (B7), winning
over the opening prompt and Codex's index title, with the prompt restored if the name
disappears. `status: completed` is firm `Done`, `aborted` firm `Interrupted`, and
`generating` (or a non-empty `generatingBubbleIds` when the status is `none`) firm
`Thinking`. The database time must be at least as new as the transcript's last entry, so a
throttled read cannot rewind newer transcript activity. `Reason::ComposerStatus` says when the
database decided the state. Stale generating ids do not override `completed`.

**`toolFormerData` is read but not yet a status signal.** Bubbles give the tool name,
`status` (`loading`/`completed`/`error`/`cancelled`), `userDecision`
(`accepted`/`rejected`/`pending`) and `error`, from the shape documented by Cursor's public
storage/export code and prior-art snapshots. The missing fact is what those fields show
**while** Cursor is waiting for approval. A copy captured while Cursor is idle cannot answer
it, so the manual capture is deferred to Lee (the SQL is in
`docs/formats/cursor.md`). Until then Cursor keeps the shared 20 s `AwaitingApproval`
inference; turning `userDecision` into firm approval without that capture would be a guess.

**Fixtures.** `fixtures/cursor/handmade/vscdb-reader/` holds synthetic `cursorDiskKV` rows
(no real content): completed/generating/aborted/none composers, names, generating ids, a
malformed row and `toolFormerData` with both decision locations. The tests build a real
SQLite database from it, leave the rows in the WAL, and read them back; a separate fs test
checks the title/status override, the throttle, and that no Cursor session means no read.

## Phase C fixture redaction and the versioned seed layout (2026-10-25)

**The fixture tooling lives in `agenttail-cli`, not the library.** `fixture redact`,
`capture` and `scan` are local development/CI tools: they read files, write fixture
directories, and never touch a harness or the network. Keeping them out of the sans-IO core
means the library still only parses bytes and states.

**Redaction is deterministic and value-free.** JSONL records are parsed and re-serialized
with `serde_json`'s `preserve_order`, so object field order, array order and line count are
kept. Values are replaced in a fixed order: secret-valued keys, private keys/auth headers,
home paths, URL/SSH hosts, emails, secret patterns, then entropy. Placeholders are stable
per value: `<user>`, `<email>`, `<host>`, `<path-NNNN>`, and `<secret-NNNN>`. Token usage
look-alikes (`input_tokens`, `token_count`, `total_tokens`) are deliberately not secret
keys; secret detection combines known token prefixes and Shannon entropy. `--lorem`
replaces prose with lorem of the same character length and keeps whitespace, so line shapes
do not change.

**Every run writes a reviewable report, and the report is safe to commit.** The report has
replacement counts, per-line findings that name only the placeholder, and before/after byte
spans so event offsets can be remapped. It never records the original secret or home path.
The default report sits beside the output (or under the captured fixture as
`redaction.json`). The scan ignores generated reports because their contents are already
value-free.

**The scan is the guardrail, not a best-effort log.** It detects secrets and non-placeholder
home paths in fixture data and docs; emails and hosts are reported as warnings because the
issue only makes secrets and home paths hard failures. A path-like token (lowercase, digits,
`/_-.'`) is not misread as an entropy secret. `.githooks/pre-commit` runs the same scan when
`fixtures/` is staged, and a dedicated CI job runs it on every change to the CLI or
fixtures.

**The seed corpus moved through the tool.** The four tvdash recordings became
`fixtures/claude-code/2.1/linux/seed-session`, `fixtures/codex/0.160/linux/seed-exec`,
`fixtures/codex/0.160/linux/seed-resume` and
`fixtures/cursor/unknown/any/seed-session`. Each has `meta.toml`,
`expected/events.jsonl` and `expected/status.jsonl`; the event fixture is compared exactly
by the CLI tests. The fake usernames became canonical `<user>` path segments, and the Cursor
fixture test now uses the correspondingly encoded folder name. The status fixture runner
also compares a checkpoint's `title` against `meta.title`, which is where `SessionState`
carries it.

**Nightly capture uses the same redactor before opening a PR.** `harness-ci/ci/survey.py`
still has its local fallback for tests, but when `AGENTTAIL_BIN` is set (the nightly workflow
builds `agenttail-cli` and sets it) the captured fixture is re-redacted by the Rust tool.
The workflow then runs `agenttail fixture scan fixtures` before it commits or opens the
drift PR, so an incomplete redaction fails closed instead of publishing a fixture.

## Phase A7: Pi harness (2026-10-25)

**Pi joins as a fourth harness, from real 1.4.0 artifacts.** The CLI writes JSONL sessions
below `~/.pi/agent/sessions/<encoded-cwd>/<timestamp>_<session-id>.jsonl`; the default
`PI_CODING_AGENT_DIR` moves the agent folder, and `PI_CODING_AGENT_SESSION_DIR`/`--session-dir`
replaces the whole sessions layout (files can be flat). The tracker reads both override
variables, prefers the session-dir one, and ignores relative values like the other harnesses.
The discovery spec and `docs/formats/pi.md` carry the evidence, including the observed
entry catalogue and quirks.

**Pi does not ask for approval.** Its built-in tools run with the OS user's permissions, and
the transcript records no permission mode. `Session` marks Pi `no_approval`, so a pending
tool is firm `RunningTool`; it never stalls into inferred `AwaitingApproval`. There is no
built-in ask-user tool, so the discovery list stays empty rather than guessing at extension
names. Turn ends are firm: assistant `stopReason: stop`/`length` is Done, `aborted` is
Interrupted, `error` is Errored with `errorMessage`. `stopReason: toolUse` leaves the tool
calls pending until their `toolResult` entries arrive. Usage and `cost.total` are per-call
and summed; `session_info.name` is the title, falling back to the opening prompt.

**Harness CI uses Pi's JSON mode, not a PTY.** Pi can point at the existing mock through a
custom `anthropic-messages` provider (`baseUrl` without the `/v1` suffix). The driver runs
`pi --mode json`, tails the real session file with the same reducer, and checks firm
activities. The GHA matrix installs pinned 1.4.0 and npm latest on Linux and macOS; latest is
not soft-skipped.

## Pi branches: reduce only the active leaf chain (2026-10-25)

**Pi's tree is reduced along the active leaf's parent chain, not the file's write order.**
The last entry in the file is the leaf Pi appended; walking its `parentId` links gives the
branch that owns the current status. `SessionReducer` now applies only entries on that
chain for status, usage, cost, recent activity and title. Abandoned-branch tool calls,
usage and `session_info` names no longer move the session. The committed
`fixtures/pi/1.4/linux/branched` scenario is the handmade main-plus-abandoned fixture, and
its checkpoints assert the reset when the leaf switches back to an older entry.

**The event layer stays lossless; it marks what it excludes.** `parse_lines` still emits an
event for every record in the file so a debugging dump has no gaps. Pi records off the
active chain set `Event::off_chain`, and `SessionReducer::feed_event` ignores those events.
This keeps the raw-bytes path and the `parse_lines` path in agreement: both end with the
same state for the branched fixture. A linear Pi session flags nothing and keeps the old
behaviour. `docs/formats/pi.md` and the Pi discovery quirk state the new rule.

**Normal appends are incremental; only a leaf change replays.** The reducer retains the
lean-parsed Pi records it has already seen (the same values the chunk reader uses, not a
second raw transcript) and applies a new entry directly when its parent is the current
leaf. If a new entry branches or switches back to an older leaf, the reducer rebuilds from
those retained records, keeping the restored ancestors and dropping the abandoned path.
That replay is bounded by the session file seen so far; it does not re-read the file or run
on every normal poll. Pi's in-memory `branch()` writes nothing until the next append, so a
switch-back is observed as the first new child of the older leaf.

**Cost dashboards see branch truth, not lifetime spend.** A Pi session's usage/cost is the
sum along the active branch, so after a switch to an older leaf the reported total can go
down. That is the intended model-context total; it now agrees with what Pi would send and
what a status/usage dashboard should attribute to the live branch. A dashboard that wants
lifetime spend across abandoned branches should sum the event dump (off-chain events are
still present and flagged), not read `SessionState.cost_usd`. Compaction is not a pruning
boundary here: `firstKeptEntryId` affects Pi's model context, while active-chain ancestors
before it and the compaction entry's own usage stay counted.

## Phase D: OpenCode 1.19.4 (2026-10-25)

**OpenCode is a database harness, not a transcript-file harness.** 1.19.4 keeps sessions in
`$XDG_DATA_HOME/opencode/opencode.db` (default `~/.local/share/opencode/opencode.db`) and
rewrites rows as a turn progresses. The tracker snapshots the database with its WAL/SHM
sidecars when the stamp changes, then rebuilds each session in the window from all of its
rows. It does not pretend there is an append offset to follow, and it does not open the live
database.

**The record stream is an explicit row export.** Each row becomes
`{"table": "session" | "message" | "part", "row": {…}}`; that is what the adapter,
`parse_lines(Harness::OpenCode, …)`, the driver's transcript dumps and the committed fixtures
all consume. This keeps the event layer testable without making a SQLite file a fixture input
or leaking a binary format into the redactor. The 1.19.4 runs wrote the legacy
`message`/`part` tables; the v2 `session_message` table existed but was empty and is left as
`Unknown` for a future version rather than guessed at.

**Approvals remain inferred.** `permission.asked` / `permission.v2.asked` are ephemeral
events, not rows. A pending/running tool part is the only on-disk evidence, so OpenCode keeps
the shared 20 s `AwaitingApproval` inference; `question` and `plan_exit` are firm
`AwaitingAnswer` tools. The session's `permission` column is a ruleset, not a mode string, so
the reader does not invent a mode or a never-prompt list.

**Live child sessions are helpers, not extra top-level cards.** A child session has
`session.parent_id`; the tracker feeds direct children as helpers of the parent, so a child
still working after the parent's turn is `BackgroundWork`. Nested grandchildren are left for
later, as are the v2 event log and a real interactive approval capture.

**Pinned and latest both run.** Harness CI installs OpenCode **1.19.4** for reproducible row
fixtures and npm **latest** for drift. The latest job is allowed to fail loudly; a
latest-only failure means upstream changed the database or turn vocabulary, not that the
pinned behaviour should be ignored. The mock gained OpenAI Chat Completions and answers
OpenCode's separate title request without consuming a scenario step. Fixtures are exported,
redacted rows under `fixtures/opencode/1.19.4/linux/` (complete text turn, completed tool
turn, and a live running-tool snapshot).

## Phase D follow-up: empty sessions are Unknown (issue #62, 2026-10-25)

**A session with nothing read is `Unknown` / `no_entries`, not `Idle`.** The extraction kept
tvdash's display behaviour, but PLAN.md section 4 reserves `Idle` for a session whose last
activity is older than `idle_after`; an unread or metadata-only transcript has no activity to
call idle. `Activity::class()` still puts `Unknown` in `StatusClass::Idle`, so sorting does
not change. Consumers that render `activity` directly must map `Unknown` to their idle look
if they want the old display; StatusWall's adoption (#16) is not integrated yet, so no
consumer is broken by this change today.

## Phase E: first-read budget in the local verify gate (2026-10-25)

**The budget is enforced locally now; the GHA job is committed ready for next month.**
Multibeast decided on issue #61 to wire PLAN.md section 6.2 into the box verify path rather
than wait for harness CI. The target is a 500 MB Lean first read under 3 s with peak RSS under
50 MB. [`harness-ci/ci/first-read-budget.sh`](harness-ci/ci/first-read-budget.sh) builds the
bench with `cargo bench --no-run`, runs the emitted binary under GNU `/usr/bin/time -v`, and
fails on either overrun.

**The script measures the bench child, not cargo.** `/usr/bin/time -v cargo bench` reports the
cargo process's peak RSS (38.7 MB on this box), while the bench itself peaks at 9.4-9.9 MB, so
the cargo wrapper would make the memory gate meaningless. The bench generates its transcript
in memory, so no large scratch file is written; the script's small logs live under
`target/first-read-budget.*` and are removed on exit.

**Measured on Multibeast's Linux box** (2 vCPU, rustc 1.88.0): Lean **0.76-1.95 s**, Full
**1.29-3.75 s**, peak RSS **9.4-9.9 MB**. The 3 s / 50 MB budgets remain PLAN's headroom rather
than the observed floor. The workflow's `first-read` job runs the same script on
`ubuntu-latest`; it is Linux-only because the gate needs GNU time, and it is ready but was not
required while this month's Actions billing was exhausted.

## Phase F: Gemini CLI 0.65.0 and Copilot CLI 1.0.103 (2026-10-25)

**Gemini is a journal harness, not a plain append log.** 0.65.0 writes
`~/.gemini/tmp/<project>/chats/session-*.jsonl` (or below `GEMINI_CLI_HOME`), with message
records upserted by id, `$set` metadata updates, `$patch` tool-result patches and
`$rewindTo` records. The adapter reads the top-level journal, skips the `<session_context>`
record as a prompt but takes its workspace path, and treats a `gemini` message with text and
no tool calls as the firm end of the turn. Project short-id folders are labels, not decoded.

**The prior-art claim about Gemini tool calls is confirmed on 0.65.0.** A tool call is
written only after it completes, with `result` and `status`; there is no on-disk pending or
`awaiting_approval` state, and the approval mode is not recorded. A tool call therefore
arrives as a result, not as `RunningTool`, and a normal Gemini session cannot produce firm
`AwaitingApproval`. The adapter still recognises an explicit future `awaiting_approval`
status, but that is not observed support. Subagent journals under
`chats/<parent-session-id>/` are not followed.

**Copilot persists permission requests.** 1.0.103 writes
`~/.copilot/session-state/<uuid>/events.jsonl` (or below `COPILOT_HOME`). A tool call is in
`assistant.message.toolRequests` before execution; a request that may need a human is
followed by `permission.requested` (with `toolCallId` and `permissionMode`) and then
`permission.completed`. That makes a pending approval firm, unlike every other harness
except an explicit ask-user tool. In prompt mode without an interactive user, the completed
result is `denied-no-approval-rule-and-could-not-request-from-user`, followed by a failed
tool result. Because Copilot always records the request when it needs one, a pending tool
without a request is firm `RunningTool`; the shared 20 s stall inference is disabled for
Copilot.

**Copilot's `assistant.turn_end` is a model-step boundary.** A tool-requesting model call
has a `turn_end` before the tool runs and the next model call has another. The adapter only
turns that record into `Done` after an assistant message with no tool requests; otherwise it
keeps the interaction at `RunningTool`/`Thinking`.

**Both harnesses run through the mock without accounts.** Gemini uses a dummy
`GEMINI_API_KEY`, an absolute `GEMINI_CLI_HOME`, settings selecting `gemini-api-key`, and
`GOOGLE_GEMINI_BASE_URL`; the mock answers the routing classifier and the Gemini SSE stream.
Copilot uses its BYOK provider (`COPILOT_PROVIDER_BASE_URL`, `COPILOT_OFFLINE=true`) against
the existing OpenAI Chat Completions mock, so GitHub sign-in is not required. Fixtures are
real CLI artifacts captured through the mock and redacted by `agenttail fixture capture`;
the Gemini `tool` fixture and status checkpoints record the "call only after completion"
finding, and the Copilot `permission-denied` fixture records the persisted request and
unattended denial.

## Phase G: language-neutral schemas and status.md (issue #67, 2026-10-25)

**The v0.1 contract is now frozen under `spec/schema/` and `spec/status.md`.** The schemas are
hand-written from the Rust `Event`, `EventKind`, `SessionState` and fixture checkpoint types,
not generated at build time, so they stay readable and reviewable. There are three:
`event.schema.json` (every v0.1 event kind with common provenance and payloads),
`session-state.schema.json` (the full reducer state, including certainty, reason, helpers,
usage and diagnostics) and `status.schema.json` (a fixture checkpoint line, including the
optional helper/meta steps). `spec/status.md` explains the event/status meaning, checkpoint
protocol, canonical JSON and how a port passes. `docs/conformance.md` and the README link
them.

**The fixture corpus is the validation test.** `crates/agenttail/tests/schema_fixtures.rs`
validates every committed `expected/events.jsonl` and `expected/status.jsonl` line, parses
every scenario input so harnesses without an events fixture (OpenCode today) are covered,
validates each final `SessionState` and checks one sample of every event kind. The test uses
the `jsonschema` crate only as a dev-dependency.

**Schema writing found and fixed two serde flatten collisions.** `EventKind::ToolCall.activity_line`
and `EventKind::Title.title_source` used to serialize as `line` and `source`, colliding with
the common provenance fields of the same names. The raw fixture lines therefore contained
duplicate JSON keys, and a normal JSON parser silently kept the payload value and lost the
provenance one. The fields are now named `activity_line` and `title_source`, the expected
events were regenerated, and tests assert that provenance `source`/`line` survive alongside
the payload fields. This is a pre-release event-shape change, made before any port froze it.

**QUESTIONS #4 is resolved by keeping `Activity::class()`.** The library has shipped the
four-class enum since Phase 1a; it has no wording and StatusWall maps it to its own labels.
Moving it out would remove a useful sort key for no gain.

**QUESTIONS #6 is resolved: untimed entries stay dated by transcript mtime.** This is the
existing behaviour, and it is the better answer for a transcript first read long after it was
written. Cursor's prompts still carry their own time; the mtime is the cap/fallback for its
other records.

**QUESTIONS #7 is resolved: `Timestamp` stays `f64` seconds for v0.1.** That matches tvdash
and the remote frames. Integer milliseconds or nanoseconds would make equality exact but add
conversion pain now; revisit only if an adopting port proves equality is a real problem.

**QUESTIONS #9 is resolved: remote callers substitute arrival time for `mtime = 0`.** The
sans-IO reducer cannot read the clock. `ChunkCtx::mtime_or(arrival)` now makes the remote
convention explicit and tested, and `ChunkCtx`/`spec/status.md` document it. StatusWall's
remote loop is still not wired to the reducer; #16 remains parked, as decided.

## Phase G follow-up: QUESTIONS #4/#6/#7 (issue #72, 2026-10-25)

**QUESTIONS #4: `StatusClass` and `Activity::class()` stay in the library.** The class is a
wording-free sort key, not a display vocabulary: `AwaitingAnswer`/`AwaitingApproval` are
`NeedsYou`, `RunningTool`/`Thinking`/`BackgroundWork` are `Working`,
`Done`/`Interrupted`/`Errored` are `Done`, and `Idle`/`Unknown` are `Idle`. `StatusClass`
derives `Ord` and sorts `NeedsYou < Working < Done < Idle`, so the tracker's most-urgent-first
order is stable. Both the mapping and that order are a public promise for compatible releases;
a new `Activity` variant must choose its bucket explicitly. `class()` matches exhaustively
inside the crate, and `status_class_is_pinned_for_every_activity_variant` pins every current
variant (with a companion order test) so a new variant cannot silently inherit the wrong
class. Consumers still own all wording; the library exposes none.

**QUESTIONS #6: entries without their own timestamp are dated by the transcript's mtime.**
Claude Code and Codex lines missing `timestamp` use the mtime supplied with the chunk, never
the time the line was read. The reducer is sans-IO and does not read the clock, so an old
transcript first read today still shows its real age (and can be `Idle` immediately). Cursor's
prompts keep their own `<timestamp>`, capped by the transcript mtime; only Cursor's other
records use the fallback. `entries_without_a_timestamp_use_the_transcript_mtime_not_the_read_time`
covers both Claude Code and Codex: the old write time becomes `last_at`, `since` and
`last_write`, and a late read does not make the session look fresh. A remote frame with an
unknown mtime must substitute the arrival time before `feed`; the reducer cannot invent it.

**QUESTIONS #7: `Timestamp` stays `f64` seconds for v0.1.** It matches tvdash, the remote
frames and the JSON contract: timestamps are plain JSON numbers, and millisecond precision
survives canonical event/status output without exponent or precision drift for epoch-scale
values. `canonical_event_and_state_timestamps_round_trip_milliseconds` checks both the event
`at` field and the status state's `last_at`/`since`/`last_write` through the same canonical
JSON path the conformance commands use; `timestamps_serialize_as_plain_seconds` and
`timestamps_order_and_compare_as_seconds` pin the serialization, equality and ordering the
reducer relies on. Integer milliseconds or nanoseconds would make equality exact but add
conversion pain now; revisit only if an adopting port proves equality is a real problem, not
speculatively before 0.1.

## Phase H: Claude Desktop local-agent-mode adapter (issue #78, 2026-10-25)

**The Code tab and local agent mode are separate sources, not one Desktop adapter.** Code-tab
transcripts are Claude Code JSONL under `~/.claude/projects` and stay on the Claude Code
adapter. Cowork/Chat sessions are app-data `local-agent-mode-sessions/…/local_<id>/audit.jsonl`
and get the `claude_desktop` harness. Conflating them would apply the wrong roots, project
derivation and sidecar rules to one of the two.

**The audit stream reuses the Claude record parser.** ADR's prior-art parser and the public
Claude Agent SDK types show `system/init`, `user`, `assistant`, `result`,
`rate_limit_event` and `tool_use_summary`. The shared Claude reader already handles the
conversation records; `claude_desktop` adds `result` (turn end/error/interruption, cost,
session-total usage), the two known meta records, and the sidecar. It does not copy the Claude
Code adapter.

**App-data roots are discovery data per OS.** `spec/discovery/claude-desktop.toml` lists six
platform roots: consumer and 3P on macOS, Linux and Windows. `Tracker` scans the roots for the
OS it runs on instead of assuming one home-relative path. The Linux consumer path is the
single `roots.default` used by one-root tooling; the 3P roots are vendor-documented, while
the consumer Linux/Windows rows remain prior art/TBD. A `session_dir_prefix = "local_"`
layout field keeps the walker from treating arbitrary `audit.jsonl` files as sessions, the
bare `audit.jsonl` pattern requires that exact file name inside each `local_*` folder, and
the five-folder depth allows dispatch's extra `agent/` level.

**The sidecar is read but is not a transcript.** The tracker reads the sibling
`local_<id>.json` when it appears or changes and feeds its cwd/title/model/session id through
a harness metadata hook. A missing or malformed sidecar does not reject the audit log. The
sidecar's `lastActivityAt` is not used as a message clock; SDK message records carry no
timestamp, so status times fall back to `audit.jsonl`'s mtime like any untimed entry.

**A pending tool is not firm awaiting-approval.** Unlike Copilot CLI, the Desktop audit stream
has no persisted permission request/completion pair. In a mode that can ask a human, a
pending tool becomes `awaiting_approval` only through the shared 20 s stall inference; in
`bypassPermissions`/`dontAsk` it stays firm `running_tool`. `tool-stall` pins both the
inference and the absence of a permission record, rather than inventing one.

**Support stays `ported-unverified`.** The fixtures are handmade from ADR's parser and its
contract test; no Desktop install, consumer/3P root, sidecar or HMAC chain was verified here.
The support matrix therefore moves Claude Desktop off the spike-only table because an adapter
and fixtures now exist, but not to donated- or CI-verified.
