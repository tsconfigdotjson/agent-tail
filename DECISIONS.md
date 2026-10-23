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
(`#[non_exhaustive]`), so tvdash writes its mapping once; today's rules produce six:
Question → `AwaitingAnswer`; tool call stalled 20 s → `AwaitingApproval` with certainty
`Inferred`; tool call → `RunningTool`; prompt or tool result → `Thinking`; end of turn →
`Done`; nothing read, or nothing written for 15 min → `Idle`. Every other combination is
`Firm`, including idle-by-age (it's the definition of idle, not a guess). `Reason` names
the rule (`no_entries`, `no_writes_for`, `ask_user_tool`, `tool_call_stalled`,
`tool_call_pending`, `prompt_sent`, `tool_result_in`, `turn_ended`). The internal `Last` is
kept as it was.

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
5. **A session with nothing read is `Idle` for reason `no_entries`** even when its file is
   old; tvdash also said idle (the age rule came first), so only the reason is new.
6. **On non-Unix systems** a file's identity is unknown, so a transcript replaced by a
   longer file isn't noticed (one replaced by a shorter one is). tvdash only built on Unix.
