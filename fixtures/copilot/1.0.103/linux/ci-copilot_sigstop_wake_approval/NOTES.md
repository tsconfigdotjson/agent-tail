# SIGSTOP/SIGCONT wake capture

Captured from **copilot 1.0.103** on Linux through `harness-ci/driver/scenarios/copilot_sigstop_wake_approval.json`
and the local mock-server rig.

- The driver waits until the session reaches the target state, sends `SIGSTOP` to the CLI's
  process group, samples the unchanged transcript during the stop, then sends `SIGCONT`. The
  same process (or, for an ask/approval turn, the same live TUI) then records the next turn.
- Freeze line: `11`; wake line: `11`; stopped for `3000` ms.
- Long-gap variant: the mock's open connections are dropped while the process is stopped and
  `advance_now_ms` is `0`. The driver advances the reducer's `now` and dates writes made
  after the wake in that post-restore clock; records before the wake keep their original times.
  The fixture's post-wake record times are shifted by the same hour, with every record shape
  unchanged. That models a Linux VM restore rather than waiting an hour in CI.
- Observed after wake: the next request was accepted by the mock and the transcript continued
  in the same session; no reconnect/retry error became the final status.
- Disk evidence only: no PID list is committed. Redaction replaced ids, paths and secrets;
  record order and shape are unchanged.

## Copilot CLI reachability

- **done** (short and long-gap variants): the first TUI turn ends, then the live process is
  stopped.
- **awaiting_approval**: a real TUI permission request is
  waiting, so the status is firm `awaiting_approval`. The driver accepts the folder-trust card
  first, then freezes on the permission card and presses Enter after wake.
- **awaiting_answer:** unreachable with 1.0.103. No ask-user event or tool name is observed;
  `spec/discovery/copilot.toml` and `docs/formats/copilot.md` keep the list empty. The captured
  approval log contains no question record, so no answer-state shape was invented.
