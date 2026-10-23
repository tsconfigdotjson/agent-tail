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
5. **`Idle` or `Unknown` for a session with nothing read yet?** Kept `Idle` (as tvdash
   shows it), with reason `no_entries`; PLAN.md section 4 has `Unknown` for this. Switch
   in Phase 2?
6. **Entries with no time of their own are dated by the transcript's mtime**, not by when
   they were read (DECISIONS.md, "Where behaviour differs", 1). This only changes anything
   for a Claude Code or Codex line missing its `timestamp`. OK?
7. **Time as `Timestamp(f64)` seconds**, matching tvdash and the remote frames. An integer
   of milliseconds or nanoseconds would make equality exact; worth it before 0.1?
8. **Harness folders moved by env vars** (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) aren't read:
   `Tracker::new(home)` uses the default roots, and `Tracker::with_roots` takes others.
   Whether to read the env vars (in the library or the caller) waits on Phase 0 confirming
   them in each harness's docs.
9. **tvdash's remote loop.** The reducer now owns the held-back partial line and the
   offset checks that `remote::RemoteFile` does today, so `remote.rs` can drop its
   `pending`/`received` and call `feed`. One difference to decide when wiring it: tvdash's
   remote frames have `mtime = 0` when the remote's `stat` failed and substitute the
   arrival time; the reducer expects the caller to do that (it can't read the clock).
10. **Bench in CI.** `cargo bench --bench first_read` reads a synthetic 500 MB transcript
    (numbers in the Phase 1a report). Peak RSS is measured from outside (`/usr/bin/time`);
    a CI job would wrap it the same way and fail over budget. Wanted now, or with harness
    CI?
11. **Fixture layout.** The seed fixtures sit in `fixtures/legacy/` as they were. Moving them
    into `fixtures/<harness>/<version>/<os>/<scenario>/` with `meta.toml` and expected
    checkpoints waits for `fixture redact` and the status checkpoints (Phase 2).
