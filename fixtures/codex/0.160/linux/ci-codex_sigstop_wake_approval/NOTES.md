# SIGSTOP/SIGCONT wake capture

Captured from **codex 0.160.1** on Linux through `harness-ci/driver/scenarios/codex_sigstop_wake_approval.json`
and the local mock-server rig.

- The driver waits until the session reaches the target state, sends `SIGSTOP` to the CLI's
  process group, samples the unchanged transcript during the stop, then sends `SIGCONT`. The
  same process (or, for an ask/approval turn, the same live TUI) then records the next turn.
- Freeze line: `10`; wake line: `10`; stopped for `3000` ms.
- Long-gap variant: the mock's open connections are dropped while the process is stopped and
  `advance_now_ms` is `0`. The driver advances the reducer's `now` and dates writes made
  after the wake in that post-restore clock; records before the wake keep their original times.
  The fixture's post-wake record times are shifted by the same hour, with every record shape
  unchanged. That models a Linux VM restore rather than waiting an hour in CI.
- Observed after wake: the next request was accepted by the mock and the transcript continued
  in the same session; no reconnect/retry error became the final status.
- Disk evidence only: no PID list is committed. Redaction replaced ids, paths and secrets;
  record order and shape are unchanged.

## Codex reachability

The mock/PTY rig reaches all three requested states on the same live process:

- **done**: the first turn ends, then the process is stopped.
- **awaiting_answer** (short and long-gap variants): plan mode's `request_user_input`
  question is on screen.
- **awaiting_approval**: an `on-request` escalation has passed
  the unchanged 20 s stall, so the status is inferred `awaiting_approval`.

No unreachable requested state for Codex.
