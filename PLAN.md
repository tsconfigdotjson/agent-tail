# Plan: an open-source library that knows what every coding agent is doing right now

Working name **`agenttail`** (free on crates.io, PyPI and npm as of 2026-10-23; `sessiontail`
is the runner-up, and `agentlog` is taken on crates.io). Nothing gets published until the
name is confirmed.

This plan adapts a generic "universal session-log parser" brief to what StatusWall (the
`tvdash` engine) actually needs. StatusWall decided on #189 that agent state comes from
session history, not hooks, and that "better history parsing will be built as our own
library in a separate project". This is that project. Read the whole plan before writing
code.

---

## 1. What StatusWall needs, and where it struggles today

StatusWall's Agents face shows every Claude Code, Codex and Cursor session on this Mac and
on SSH hosts, so you see the moment one needs you. Today that logic lives in
`tvdash/src/sessions.rs` and `src/sessions/{claude,codex,cursor,lean}.rs` (about 1,300
lines plus tests). It folds transcript lines into a single `Last` value and decides:

| status | label | rule today |
|---|---|---|
| needs-you | needs your answer | last event was an ask-the-user tool (`AskUserQuestion`, `ExitPlanMode`, `request_user_input`, Cursor's ask tools) |
| needs-you | may need approval | a tool call with no result for **20 s** (a guess) |
| working | working / thinking | tool running / prompt or tool result last |
| done | done | end of turn or interruption |
| idle | idle | nothing written for 15 min |

Each card also shows `project`, `branch`, `title`, `cost_usd`, `out_tokens` and the last 5
activity lines (`› prompt`, `$ cargo test`, `Edit routing.ts`).

**Where it's weak, from a read-only survey of this Mac's real transcripts (structure only,
last 200 Claude Code sessions, 2026-10-23):**

1. **"May need approval" fires when no approval is possible.** All 4,106 `permission-mode`
   records seen say `bypassPermissions`, but the adapter ignores that record type, so every
   build or test run longer than 20 s shows "may need approval". Codex's `turn_context`
   carries an approval policy that is likewise unread.
2. **Firm signals go unread.** Claude Code writes `system` records with subtypes
   `turn_duration` (1,183 seen), `stop_hook_summary`, `away_summary`, `compact_boundary`,
   `agents_killed`, `local_command` and `scheduled_task_fire`. Only `stop_reason ==
   "end_turn"` and the interruption text end a turn today.
3. **Background subagents are invisible.** The 51 `Agent`/`Task` calls seen got their
   result in a median of 0.13 s (they now run in the background), so a session reads
   "done" while its agents are still working. Claude Code's `<id>/subagents/*.jsonl` files
   aren't followed at all; only Cursor's are, and only as "activity".
4. **Formats drift silently.** Record types the adapters have never seen are already live
   on this machine: `mode`, `atis-latch`, `api-request`, `api-request-blob`,
   `api-request-shape`, `bridge-session`, `file-history-delta`, `frame-link`, and the
   content type `tool_addition`. Nothing notices when a harness changes shape.
5. **Status is untestable against the real thing.** Tests use 3 scrubbed recordings and
   synthetic lines, re-timed against the wall clock. Nobody checks the inferred status
   against what the agent's UI actually showed.

The library's job is to make status **firm wherever the transcript allows, honest about
inference where it doesn't, and verified against real harnesses in CI**.

---

## 2. Decisions

These are settled unless you find a concrete reason they can't work; if you do, stop and
report the reason.

- **Rust.** The engine is Rust, so StatusWall takes it as a plain crate dependency with no
  FFI. Rust also gives other ecosystems a fast core: a C ABI, PyO3 (Python), napi-rs (Node)
  and WASM are later, separate packages. Edition 2024, MSRV pinned in `Cargo.toml`.
- **Two layers, with status as the headline.**
  1. `events`: normalized, provenance-carrying events parsed from transcript lines.
  2. `status`: a reducer that folds events into a `SessionState`: what the agent is doing,
     whether it needs the user, and how sure we are.
  Status is what no other parser offers and what StatusWall ships. The event layer is a
  general-purpose base other tools can use, but v0.1 models only what status, title,
  usage and activity lines need, *losslessly for what it models* (see 5.1).
- **Sans-IO core.** Parsing and status take bytes plus context (path, byte offset, file
  mtime, `now`), never a filesystem. StatusWall's SSH loop ships framed chunks (`agent,
  offset, length, mtime, path`) and must feed the same code path as local files. Filesystem
  discovery and tailing live in an optional `fs` module (cargo feature, on by default).
- **Time is injected.** Every status call takes `now`. No `SystemTime::now()` inside the
  library, so status timelines are deterministic and testable.
- **Bounded memory, with the hot path built for a TV running all day.** The reducer keeps a
  small state per session and never retains events. Long strings in big lines are cut
  before parsing (port of `lean.rs`, #139), but only in an explicit `Lean` parse mode that
  flags what it cut (`truncated: true`). The default `Full` mode never truncates.
- **Read-only and local.** No network, no subprocesses, no writes to harness directories,
  no hooks installed (#189). The README gets a "what this library touches on your
  machine" section.
- **License: `MIT OR Apache-2.0`** (the Rust convention). Code is written fresh, informed by
  prior art rather than ported, which keeps dual licensing possible. If Apache-only code
  (ADR) is ever ported, that crate becomes Apache-2.0 and the port is recorded in `NOTICE`.
  **Lee to confirm.**
- **Extracted from StatusWall, not rewritten from scratch.** The adapters, `lean`, the
  partial-line rules, the Codex day-folder skipping and the scrubbed fixtures move into the
  library first, behaviour-identical, with their tests. Improvements land after.

---

## 3. Scope

### 3.1 Harnesses

| Tier | Harness | Source | Notes |
|---|---|---|---|
| 1 (v0.1) | Claude Code CLI, plus Claude Code sessions run by the desktop app | `~/.claude/projects/<enc-path>/<id>.jsonl`, `<id>/subagents/*.jsonl` | per-content-block rows share `message.id`; dedupe usage |
| 1 | Codex CLI | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (older: `.jsonl.zst`) | day folders by local start date; a resumed session appends to its old file |
| 1 | Cursor agent (IDE and CLI) | `~/.cursor/projects/<enc-path>/agent-transcripts/<id>/<id>.jsonl` | no timestamps (mtime, plus prompts' `<timestamp>` block, #192); `<user_query>` wrappers |
| 1 | Cursor `state.vscdb` (SQLite) | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`, `cursorDiskKV` `composerData:%`/`bubbleId:%` | data exists on Lee's Mac; read from a temp snapshot with `-wal`/`-shm`, never the live DB (`rusqlite`, bundled) |
| 1b (v0.1, after Claude Code's harness CI works) | Gemini CLI, OpenCode | per their docs, confirmed in CI | open source and pointable at a mock server, so CI captures real artifacts; no local data needed. **OpenCode 1.19.4 now ships**: SQLite reader, discovery spec, offline row fixtures, pinned + latest harness-ci lane. **Gemini CLI 0.65.0 now ships**: journal adapter, real offline fixtures and a pinned + latest lane; tool calls are written only after completion. |
| 2 (spike first) | Copilot CLI, Claude Desktop agent mode | | GitHub sign-in and desktop automation may block CI; ships when verified, via CI or donated fixtures. **Copilot CLI 1.0.103 now ships through BYOK**: real event-log fixtures, firm persisted permission requests, and a pinned + latest lane; GitHub routing/sign-in is not exercised. Claude Desktop remains deferred. |
| 3 | Cline, Antigravity, Warp, Amp, Droid, Kiro, Codex desktop | | community, or donated fixtures |

Tiers set the order, not a release cutoff: a harness ships in whatever release it is
verified for. The one hard dependency is that 1b waits for the harness CI (section 8), so
it is CI-verified rather than `ported-unverified`.

Never claim support for a format not seen in a real artifact. A format known only from
someone else's code is marked `ported-unverified` in the support matrix.

### 3.2 Non-goals for v0.1

- Hooks, MCP servers, process wrappers, or anything that edits a harness's config (#189).
- Pricing. Cost is passed through only where the harness records it (Claude Code's
  `cost-state.totalCostUSD`); otherwise it is `None`, never estimated.
- OpenTelemetry, ADR `AgentEvent`, Markdown and chat projections, until a consumer needs
  one. StatusWall needs none of them, and building them first would mean designing an API
  with no user.
- Reading old `.jsonl.zst` Codex history (live sessions are never compressed). Feature-gated
  in v0.2 for history tools.
- UI, wording, or StatusWall's 4-class and 6-label mapping. Those stay in tvdash (#4).

---

## 4. The status model (the core of the spec)

```rust
pub struct SessionState {
    pub activity: Activity,
    /// Firm: the transcript says so. Inferred: a timing or shape heuristic, named in `reason`.
    pub certainty: Certainty,              // Firm | Inferred
    pub reason: Reason,                    // machine-readable: why this activity (see below)
    pub since: Option<Timestamp>,          // when the activity began (event time, or mtime if the harness has none)
    pub last_write: Option<Timestamp>,     // newest write to the transcript or any helper's
    pub background: Vec<HelperState>,      // subagents and background tasks still running
    pub permission: Option<PermissionMode>,// as the harness last recorded it
    pub meta: SessionMeta,                 // cwd, project, branch, title (+ its source), models
    pub usage: Usage,                      // as reported, deduped; None fields when not recorded
    pub cost_usd: Option<f64>,             // passed through, never computed
    pub recent: RingBuffer<ActivityLine>,  // last N lines, N configurable (StatusWall: 5)
}

pub enum Activity {
    AwaitingAnswer,     // an ask-the-user or plan-approval tool is pending
    AwaitingApproval,   // a tool call is pending and the harness could be asking permission
    RunningTool,        // a tool call is pending and can't be awaiting approval (or isn't yet)
    Thinking,           // prompt sent or tool result returned; model is generating
    BackgroundWork,     // turn ended, but helpers are still running
    Done,               // turn ended normally
    Interrupted,        // user interrupted, or turn aborted
    Errored,            // turn ended with an error the harness recorded
    Idle,               // no write for `idle_after` (default 15 min)
    Unknown,            // nothing recognizable yet
}
```

`state(now)` is a pure function of the reduced state and `now`. Thresholds (`stall_after`,
default 20 s; `idle_after`, default 15 min) are configurable per harness and per tool.

**Rules that fix section 1's problems; each needs a fixture proving it:**

- **Approval is only possible when the mode allows it.** Pending tool plus
  `bypassPermissions` (Claude) or a never-ask approval policy (Codex) gives `RunningTool`,
  never `AwaitingApproval`, however long it runs. Unknown mode keeps today's 20 s guess,
  marked `Inferred`.
- **Tools that never ask** (read-only tools the harness auto-approves in its default mode)
  don't stall into `AwaitingApproval`. The list per harness and version comes from docs or
  observed behaviour in harness CI, never from memory.
- **Firm turn ends**: Claude `system/turn_duration`, `stop_reason`, the interruption text and
  `agents_killed`; Codex `task_complete`, `turn_complete` and `turn_aborted`; Cursor
  `turn_ended` (status `error` gives `Errored`).
- **Helpers are tracked, not just counted as writes.** Claude Code's `<id>/subagents/*.jsonl`
  and Cursor's subagents feed `background`, and a session whose turn ended with helpers
  still writing is `BackgroundWork`, not `Done`. How a helper's own end is detected is
  verified per harness.
- **Ask-the-user tools** are a per-harness, per-version list in the discovery spec (data),
  matched case- and separator-insensitively where the harness is inconsistent (Cursor).
- **Every reducer step is total**: an unknown record type moves no state, increments a
  per-type counter in `diagnostics`, and never errors.

StatusWall maps `Activity` to its classes and labels in tvdash, e.g. `AwaitingApproval +
Inferred` gives "may need approval", and `RunningTool` and `BackgroundWork` give "working".

---

## 5. The spec (`spec/`, language-neutral)

### 5.1 Events (`spec/schema/*.schema.json`)

Keep the brief's principles, scoped to v0.1:

- Every event carries **provenance**: source index, byte offset and length, line number,
  and the harness's own record id(s), such as Claude `uuid`s merged by `message.id`.
- **Deterministic ids**, so the same input always gives the same ids.
- v0.1 event types: `UserPrompt` (with wrappers such as Cursor `<user_query>`/`<timestamp>`
  and Claude `<command-name>` split into structured `wrappers`), `AssistantText`,
  `Reasoning` (presence and signature only), `ToolCall` (name, call id, `kind`, full input,
  `asks_user`), `ToolResult` (call id, `is_error`, size), `TurnStart`, `TurnEnd` (`reason:
  end_turn|interrupted|aborted|error|duration-record`), `Usage` (as reported, deduped),
  `PermissionMode`, `ModelChange`, `Title` (with source), `Cost`, `HelperSpawn`/`HelperEnd`,
  `Compaction`, `Meta` (known record types with no richer form, kept whole) and `Unknown`
  (kept whole, plus a diagnostic).
- Every record type in section 1's survey, and every type in ADR's
  `_METADATA_RECORD_TYPES`/`_KNOWN_CONTENT_TYPES`, maps to a type above or to `Meta`, and a
  test enforces that none is dropped.
- Usage dedupe: Claude 2.1+ writes one row per content block, sharing `message.id`; usage
  repeats on each row. Summing `Usage` events must equal the true total; a test asserts it.
  Codex `token_count` is a running total, so the deltas are emitted and the reported total
  is also kept.

### 5.2 Discovery and layout (`spec/discovery/<harness>.toml`)

These are data files the crate embeds at build time. Each one also says enough for
**StatusWall's remote shell loop (`remote::SCRIPT`) to be generated from it**, so the local
walk and the remote `find` can't disagree:

- Roots per OS, with confirmed env overrides (verify `CLAUDE_CONFIG_DIR` and `CODEX_HOME`
  in each harness's current docs and source).
- Layout: `max_depth`, `sessions_in` (Cursor's `agent-transcripts`), `helpers_in`
  (`subagents`), `day_folders` (Codex), file patterns, and how project and session id
  derive from paths (including the lossy `/`-to-`-` encoding and how to recover it from
  transcript paths).
- `asks_user` tool lists, auto-approved tool lists and permission-mode vocabulary, each
  per version range.
- Timestamps: field, format and timezone, or "none, use mtime" (Cursor).
- Liveness: how to tell a live session (writes, helper writes, records).
- Quirks, each linked to its evidence: a fixture, a tvdash issue, or a source line.

### 5.3 Status conformance (`spec/status.md`)

This is what makes the spec useful beyond parsing. Each fixture's `expected/status.jsonl`
lists checkpoints of the form `{"after_line": N, "now_offset_s": T, "activity": ...,
"certainty": ..., "reason": ...}`. A port in another language passes if it produces the
same states at the same checkpoints.

---

## 6. The crate (`crates/agenttail`)

### 6.1 API sketch

```rust
// Sans-IO
let mut s = SessionReducer::new(Harness::ClaudeCode, ReducerOptions::default());
s.feed(chunk, ChunkCtx { offset, mtime, path });  // complete lines only; holds back a partial tail
s.feed_helper(helper_id, chunk, ctx);             // subagent transcripts
let st: SessionState = s.state(now);
let cursor: Cursor = s.cursor();                  // resume point (byte offset, inode/identity)
let events = parse_lines(Harness::Codex, bytes, ParseMode::Full); // iterator of Event

// fs feature
let mut t = Tracker::new(roots, Filter { window, harnesses });  // what tvdash's Tracker is today
let views: Vec<(SessionKey, SessionState)> = t.poll(now);
detect(path) -> Vec<FormatMatch>;                  // ranked, with reasons
layout(Harness) -> &'static Layout;               // for remote loops like StatusWall's
```

### 6.2 Engineering requirements, drawn from StatusWall's history

- **Partial lines** (current `complete_lines` rules): an incomplete last line is held back;
  a last line with no newline that is already a whole JSON object counts (Cursor writes the
  newline late); malformed middle lines give a diagnostic and parsing continues.
- **Truncation and replacement**: a shorter file, or a new inode, means re-read from 0.
- **First read of a huge transcript** in 1 MB chunks (#203). A 500 MB transcript reaches
  state in under 3 s in `Lean` mode, with peak RSS under 50 MB (bench in CI).
- **Polling cost** (#139, #26, #64): folder-mtime caching, sweeps every 10 s, and Codex
  day-folder skipping are kept. The bench from tvdash `bench.rs` (a year of Codex history,
  hundreds of Cursor projects) runs in CI with a stat-count budget.
- **No global state, no logging**; diagnostics are data.
- **macOS and Linux first** (StatusWall's hosts); Windows paths and CRLF are handled and
  tested, but Windows harness CI is v0.2.

### 6.3 CLI (`crates/agenttail-cli`), small, for debugging and CI

- `agenttail ls` lists sessions with their live state.
- `agenttail watch [id]` streams state changes with their reasons, which shows why a card
  says what it says.
- `agenttail events <path>` prints normalized events as JSONL.
- `agenttail survey [--drift]` runs the shape survey from section 1 against this machine
  (structure only, no content) and diffs it against the committed baseline, exiting
  non-zero on drift.
- `agenttail fixture capture|redact|verify` manages fixtures.

---

## 7. Fixtures, conformance, drift

```
fixtures/<harness>/<version>/<os>/<scenario>/
  input/                 # artifacts as the harness wrote them (redacted), original relative layout
  expected/events.jsonl
  expected/status.jsonl  # status checkpoints (5.3)
  meta.toml              # version, OS, capture method (ci-scripted|donated|handmade), date, redaction report
```

**Seed corpus:** tvdash's `claude-code-2.1.jsonl`, `codex-0.160-exec.jsonl` and
`codex-0.160-resume.jsonl`, already scrubbed. `cursor-synthetic.jsonl` stays as
`handmade`.

**Scenarios for each Tier-1 harness**, with the status-relevant ones first:
1. Plain Q&A to done.
2. A tool call that needs approval, left pending (default mode).
3. The same long tool under bypass/never-ask, which must be `RunningTool` and never `AwaitingApproval`.
4. Ask-the-user and plan-approval tools.
5. Interrupt mid-tool, and mid-thinking.
6. Background subagent outliving the turn, plus nested subagents.
7. Tool error, and a rejected tool call.
8. Compaction (auto and manual) mid-session.
9. Resumed session (Codex appends to an old day folder).
10. Model switch, and permission-mode switch mid-session.
11. Live file with a partial last line.
12. Very long tool output (exercises `Lean`).
13. Non-ASCII and emoji in prompts, paths and titles.
14. Cursor: `<user_query>`/`<timestamp>`/`<cursor_commands>` wrappers, and project-name
    recovery from the encoded folder.

**Redaction** (`fixture redact`) is deterministic and structure-preserving: home paths,
usernames, hosts and emails become placeholders; secrets are caught by pattern and entropy;
prose can be replaced with lorem text of the same length. A report is always written, a
human reviews it, and a pre-commit hook plus CI scan `fixtures/`. Real sessions from Lee's
machines enter the repo only through this path.

**Conformance runner:** a language-neutral command contract (`<impl> events <input-dir>`,
`<impl> status <input-dir> --checkpoints <file>`) and canonical JSON compare (sorted keys,
absent equals null for optional fields).

**Drift:** the committed `spec/baseline/<harness>.json` holds observed record types,
content types, system subtypes and field paths per version. `survey --drift` runs nightly
in harness CI and can be run by hand on a dev machine.

---

## 8. Harness CI (`harness-ci/`): the integration tests

Get Claude Code fully working first, then Codex, then Cursor.

- **Mock model server** in Rust (axum): enough of the Anthropic Messages API and the OpenAI
  Responses API, with SSE streaming, to replay scripted turns ("tool_use Bash `sleep 30`,
  then text"). Each harness is pointed at it through its own documented base-URL setting;
  confirm the exact variable or key from current docs and source (likely
  `ANTHROPIC_BASE_URL` for Claude Code and a custom `model_providers` entry for Codex).
- **Isolated homes**: each run gets a temp `HOME` (and config dir overrides), so runs never
  touch a real profile and the artifacts are exactly what the run wrote.
- **Ground-truth status, the main payoff.** The harness runs **interactively under a PTY**
  (not `-p`/`exec`), and the driver reads the screen. When the TUI shows a permission
  prompt, an ask-the-user question or an idle input box, the driver records that as truth
  with a timestamp, and the library's `state(now)` on the transcript at that moment is
  scored against it. CI fails on any `AwaitingApproval` when no prompt is on screen in
  bypass mode, and on any missed real approval prompt. This is how "may need approval"
  stops being a guess we can't measure.
- **Matrix**: harness × (pinned versions + `latest`) × (ubuntu, macos). Nightly checks npm,
  Homebrew and GitHub releases for new versions, runs them, and opens a PR with new
  fixtures plus a drift report when the *shape* changes (ids and timestamps don't count).
- **Cursor**: first, a feasibility spike on whether the Cursor CLI (`cursor-agent`) can run
  against a mock server or needs Cursor's backend. If it needs the backend, fall back to
  donated fixtures via `fixture capture` on Lee's Mac, plus a nightly budget-capped real run
  if a key is available. The IDE is out of scope for CI in v0.1.
- Anything that can't be mocked (auth checks, update pings) is documented per harness.

---

## 9. Adopting it in StatusWall (the acceptance test that matters)

1. tvdash depends on the crate (path dependency during development, then a crates.io
   version).
2. `src/sessions/{claude,codex,cursor,lean}.rs` are deleted; `sessions.rs` keeps
   `SessionFilter`, `SessionView`, the UniFFI records and the `Activity` to status/label
   mapping (wording from #4 unchanged). The `Tracker` either moves into the crate's `fs`
   module or wraps it.
3. `remote.rs` feeds framed chunks to `SessionReducer::feed`, and `SCRIPT`'s roots come from
   `layout()`.
4. Faces see no breaking change: `SessionView` keeps its fields. New optional fields
   (`certainty`, `background_count`) are added to `context.schema.json` only if a face
   wants them (a taste call for Lee).
5. **Done when**: every existing tvdash test passes, `cargo test` and the CPU budget check
   (#140) are no worse, and on Lee's Mac a session in bypass mode running `sleep 60` shows
   "working" (not "may need approval") on the TV.

---

## 10. Repo layout

```
agenttail/
  spec/          schema/*.schema.json, discovery/*.toml, baseline/*.json, status.md, CHANGELOG.md
  fixtures/      (section 7)
  conformance/   runner + canonicalization rules
  crates/agenttail/       library (features: fs [default], lean, sqlite [default], zstd [later])
  crates/agenttail-cli/
  harness-ci/    mock-model server, scenario scripts, PTY drivers, workflows
  docs/          formats/<harness>.md, support-matrix.md (generated), statuswall-migration.md
  DECISIONS.md   QUESTIONS.md   NOTICE   SECURITY.md   README.md
```

---

## 11. Prior art (reference only; check licences before reading code closely)

- **tvdash itself**: the primary source. Adapters, `lean`, partial-line rules, Codex
  day-folder logic, the remote framing, and issues #4, #26, #64, #139, #176, #189, #192 and
  #203 for the reasoning.
- **Uber ADR `Sensor/`** (Apache-2.0, pin `6b33dffe…`): discovery paths, the record and
  content type catalogue, partial-line handling, incremental reads (commit #130), and the
  `gen_ai` mapping (for a later OTel projection). Don't copy its data model, which is lossy
  and built for security detection.
- **claude-session-parser** (Rust): its JSONL notes on per-block rows, `message.id` dedupe,
  path encoding and the `sessions/` registry.
- **normalize-chat-sessions** (Rust): trait-per-format with detect and parse, a registry,
  and subagent listing.
- **parse-cc** (TS): the drift-audit idea.
- **ccusage** (TS): subagent grouping and Codex `total_tokens`.
- **OpenAI Codex PR #35623**: Cursor wrapper stripping, which leaves unrecognised leading
  context alone.

---

## 12. Phases (stop and report at the end of each)

- **Phase 0, recon (no library code):** extend section 1's survey to Codex and Cursor on
  this Mac and on vibe-os (structure only); write `docs/formats/{claude-code,codex,cursor}.md`
  with evidence; confirm env overrides, ask-user and auto-approve tool lists, and Claude
  helper-end detection; spike the Cursor CLI against a mock server. Output: format notes, a
  draft schema and status spec, and `QUESTIONS.md`.
- **Phase 1, extract:** move tvdash's adapters, `lean`, tests and fixtures into the crate,
  behaviour-identical, sans-IO, with injected time; event layer, status reducer and CLI
  `events`/`watch`. Then point tvdash at the crate on a branch with all tvdash tests green.
- **Phase 2, firm status:** permission modes, firm turn ends, helper tracking,
  `BackgroundWork`, `Errored`; status fixtures for scenarios 1 to 6; redaction tool; drift
  baseline and `survey --drift`.
- **Phase 3, harness CI for Claude Code:** mock server, PTY driver, ground-truth scoring,
  Linux and macOS, and proof that drift detection fires across two Claude Code versions.
- **Phase 4, Codex harness CI; Cursor `state.vscdb` (snapshot reader); the Cursor decision**
  (CI or donated fixtures).
- **Phase 5, Gemini CLI and OpenCode** through the same harness CI (adapter, fixtures, status
  checkpoints), plus Copilot CLI and Claude Desktop feasibility spikes.
- **Phase 6, v0.1.0:** docs, support matrix, README, StatusWall switched over on `main`.
  Ask before publishing or making the repo public.
- **Later:** whatever the spikes unblock, zstd history, OTel/ADR/chat projections (once a
  consumer needs them), Python/Node bindings.

### Working rules

- Verify formats; never invent them. Every field read cites a real artifact, a CI fixture
  or source code; inference is labelled as inference.
- Look up current facts (versions, env vars, API shapes) from current docs; they change
  monthly.
- Real session data is sensitive: read-only, structure-only surveys, never committed or
  pasted anywhere; fixtures only through redaction plus review.
- Keep `DECISIONS.md` and `QUESTIONS.md` running; make small conventional commits.

---

## 13. Open questions for Lee

1. **Name:** `agenttail` or `sessiontail` (both free everywhere), or another.
2. **Licence:** `MIT OR Apache-2.0`?
3. ~~Home~~ **Decided:** `tsconfigdotjson/agenttail`, private for now (renamed if the name
   changes).
4. **Open-sourcing tvdash code:** OK to move the adapter code (written for a commercial,
   private repo) into the open library? The plan assumes yes.
5. **New face fields:** should the Agents face show certainty ("may need approval" versus a
   firm "needs approval") or background agents? This is a taste call, and it can wait until
   Phase 2.
