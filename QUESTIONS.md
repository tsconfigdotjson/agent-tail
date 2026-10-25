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

4. **`StatusClass` in the library.** PLAN.md section 3.2 keeps StatusWall's 4-class
   mapping in tvdash, but the Phase 1a brief asked for the status class, and the tracker
   needs an order to sort by. It's in the library as `Activity::class()`, an enum with no
   wording. Keep it, or move it to tvdash?
5. **`Idle` or `Unknown` for a session with nothing read yet?** Resolved (issue #62):
   `Unknown` with reason `no_entries`, matching PLAN.md section 4. tvdash/StatusWall can
   map `Unknown` to its display idle; the library does not present an unread session as
   idle work. StatusWall adoption (#16) is not integrated yet.
6. **Entries with no time of their own are dated by the transcript's mtime**, not by when
   they were read (DECISIONS.md, "Where behaviour differs", 1). This only changes anything
   for a Claude Code or Codex line missing its `timestamp`. OK?
7. **Time as `Timestamp(f64)` seconds**, matching tvdash and the remote frames. An integer
   of milliseconds or nanoseconds would make equality exact; worth it before 0.1?
8. **Harness folders moved by env vars** — resolved in B8 and A4: `Tracker::from_env`
   reads `CLAUDE_CONFIG_DIR` and `CODEX_HOME`; both CLIs use them. Claude Code 2.1.310's
   single-absolute-path semantics are confirmed in the binary and by an offline run. Codex
   0.160.1's `CODEX_HOME` is the `.codex` home with rollouts below `sessions/`; it also
   accepts relative values against the cwd, while the tracker ignores non-absolute paths.
9. **tvdash's remote loop.** The reducer now owns the held-back partial line and the
   offset checks that `remote::RemoteFile` does today, so `remote.rs` can drop its
   `pending`/`received` and call `feed`. One difference to decide when wiring it: tvdash's
   remote frames have `mtime = 0` when the remote's `stat` failed and substitute the
   arrival time; the reducer expects the caller to do that (it can't read the clock).
10. **Bench in CI.** `cargo bench --bench first_read` reads a synthetic 500 MB transcript
    (numbers in the Phase 1a report). Peak RSS is measured from outside (`/usr/bin/time`);
    a CI job would wrap it the same way and fail over budget. Wanted now, or with harness
    CI?
11. **Fixture layout.** Resolved: the seed fixtures now live under
    `fixtures/<harness>/<version>/<os>/<scenario>/` with `meta.toml`,
    `expected/events.jsonl` and `expected/status.jsonl`. `agenttail fixture redact|capture`
    and the fixture scan landed with #24.
