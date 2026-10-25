# Questions

Open questions for Lee. PLAN.md section 13 has the ones from before any code; they're
repeated here when they're still open.

## Still open from the plan

1. **Licence.** Undecided, so the repo has no LICENSE file and no crate has a `license`
   field. PLAN.md proposes `MIT OR Apache-2.0`; nothing written so far stops that.
2. **Name**: `agenttail` or `sessiontail`. The crates are `publish = false` until it's
   settled.
3. **Moving tvdash code into an open library.** Phase 1a did move it (adapters, `lean`,
   the tracker, their tests, the scrubbed fixtures); the repo is private, so nothing is
   out yet. Confirm before it's ever made public.

## From Phase 1a (the extraction)

4. **`StatusClass` in the library.** Resolved (issue #67): keep `Activity::class()` in the
   library. It is an enum with no wording; StatusWall keeps its labels. See DECISIONS.md,
   "Phase G".
5. **`Idle` or `Unknown` for a session with nothing read yet?** Resolved (issue #62):
   `Unknown` with reason `no_entries`, matching PLAN.md section 4. tvdash/StatusWall can
   map `Unknown` to its display idle; the library does not present an unread session as
   idle work. StatusWall adoption (#16) is not integrated yet.
6. **Entries with no time of their own are dated by the transcript's mtime.** Resolved
   (issue #67): confirmed for v0.1. This only changes anything for a Claude Code or Codex
   line missing its `timestamp`; Cursor prompts keep their own time. See DECISIONS.md,
   "Phase G".
7. **Time as `Timestamp(f64)` seconds.** Resolved (issue #67): keep `f64` seconds for v0.1,
   matching tvdash and the remote frames. Revisit only if a port proves equality is a real
   problem. See DECISIONS.md, "Phase G".
8. **Harness folders moved by env vars** — resolved in B8 and A4: `Tracker::from_env`
   reads `CLAUDE_CONFIG_DIR` and `CODEX_HOME`; both CLIs use them. Claude Code 2.1.310's
   single-absolute-path semantics are confirmed in the binary and by an offline run. Codex
   0.160.1's `CODEX_HOME` is the `.codex` home with rollouts below `sessions/`; it also
   accepts relative values against the cwd, while the tracker ignores non-absolute paths.
9. **tvdash's remote loop.** Resolved (issue #67): when a remote frame has `mtime = 0`
   because the remote's `stat` failed, the caller substitutes the arrival time before
   `feed`; `ChunkCtx::mtime_or(arrival)` makes this explicit and tested. StatusWall's remote
   loop is still not wired (#16 remains parked).
10. **Bench in CI.** Resolved (2026-10-25): the local Multibeast verify gate runs
    `harness-ci/ci/first-read-budget.sh`, which fails over PLAN.md section 6.2's budget (Lean
    under 3 s and peak RSS under 50 MB for a 500 MB synthetic transcript). A `first-read` GHA
    job is committed ready for next month's billing; see DECISIONS.md and harness-ci/README.md.
11. **Fixture layout.** Resolved: the seed fixtures now live under
    `fixtures/<harness>/<version>/<os>/<scenario>/` with `meta.toml`,
    `expected/events.jsonl` and `expected/status.jsonl`. `agenttail fixture redact|capture`
    and the fixture scan landed with #24.


## Resolved by the Gemini and Copilot runs (2026-10-25)

9. **Gemini `awaiting_approval` on disk / Copilot `permission.requested` still written?**
   Gemini 0.65.0 writes a tool call only after it completes, with its result; neither a
   pending nor an `awaiting_approval` call was on disk, and the approval mode is not
   recorded. Copilot 1.0.103 does persist `permission.requested` and
   `permission.completed`, joined by `toolCallId`, so its pending approval is firm. See
   [`docs/formats/gemini.md`](docs/formats/gemini.md) and
   [`docs/formats/copilot.md`](docs/formats/copilot.md).
