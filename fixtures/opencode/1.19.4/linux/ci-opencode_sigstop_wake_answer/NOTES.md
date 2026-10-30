# SIGSTOP/SIGCONT wake capture

Captured from **opencode 1.19.4** on Linux through `harness-ci/driver/scenarios/opencode_sigstop_wake_answer.json`
and the local mock-server rig.

- The driver waits until the session reaches the target state, sends `SIGSTOP` to the CLI's
  process group, samples the unchanged transcript during the stop, then sends `SIGCONT`. The
  same process (or, for an ask/approval turn, the same live TUI) then records the next turn.
- Freeze line: `6`; wake line: `6`; stopped for `3000` ms.
- Long-gap variant: the mock's open connections are dropped while the process is stopped and
  `advance_now_ms` is `0`. The driver advances the reducer's `now` and dates writes made
  after the wake in that post-restore clock; records before the wake keep their original times.
  The fixture's post-wake record times are shifted by the same hour, with every record shape
  unchanged. That models a Linux VM restore rather than waiting an hour in CI.
- Observed after wake: the next request was accepted by the mock and the transcript continued
  in the same session; no reconnect/retry error became the final status.
- Disk evidence only: no PID list is committed. Redaction replaced ids, paths and secrets;
  record order and shape are unchanged.

## OpenCode reachability

- **done** (short and long-gap variants): the first TUI turn ends, then the live process is
  stopped.
- **awaiting_answer**: the `question` tool is pending and is
  firm `awaiting_answer`; after wake Enter selects the first option and the answer turn finishes.
- **awaiting_approval:** a real interactive permission wait is not persisted as a row.
  `permission.asked` is ephemeral, so only a pending/running tool part can reach inferred
  `awaiting_approval` after the unchanged 20 s stall. This capture does not claim a firm
  approval row or invent one.


## Two database snapshots

OpenCode rewrites rows in place, so the final export no longer contains the pending `question`
part that was on disk during the freeze. The live driver saved a snapshot at freeze time; it is
committed as the companion frozen fixture (before/frozen checkpoints). This main fixture is
the final export with the wake's next turn at the end. Together they are the same
live run, not two different sessions.
