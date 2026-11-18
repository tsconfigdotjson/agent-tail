# SIGSTOP/SIGCONT wake capture

Captured from **copilot 1.0.103** on Linux through
`harness-ci/driver/scenarios/copilot_sigstop_wake_approval.json` and the local mock-server rig.

- The scenario approves the folder-trust card, then waits for a real manual permission request
  for `touch permission-marker.txt`. It uses a write command, not `sleep 2`, because 1.0.114
  auto-approves that short sleep and writes no `permission.requested` for it. Both supported
  Copilot versions persist a request for the write.
- Freeze line: `11` (`permission.requested`); wake line: `11`; stopped for `3000` ms. The
  driver sends `SIGSTOP` to the CLI's process group, samples the unchanged transcript during
  the stop, then sends `SIGCONT`.
- Disk evidence: line 10 is `tool.execution_start`, line 11 is `permission.requested` with
  `permissionMode: "manual"` and a `toolCallId` under `permissionRequest` that joins the pending bash call, line 12 is
  `permission.completed` with `result.kind: "approved"` and
  `decisionSource: "human_response"`, and line 13 is the matching
  `tool.execution_complete`. The approved tool result then reaches the final turn end.
- Observed after wake: the next request was accepted by the mock and the transcript continued
  in the same session; no reconnect/retry error became the final status.
- Disk evidence only: no PID list is committed. Redaction replaced ids, paths and secrets;
  record order and shape are unchanged.

## Copilot CLI reachability

- **done** (short and long-gap variants): the first TUI turn ends, then the live process is
  stopped.
- **awaiting_approval**: the real TUI permission request is waiting, so the status is firm
  `awaiting_approval` from `permission.requested`, joined to the pending tool by `toolCallId`.
  The driver accepts the folder-trust card first, then freezes on the write-permission card
  and presses Enter after wake.
- **awaiting_answer:** unreachable with 1.0.103. No ask-user event or tool name is observed;
  `spec/discovery/copilot.toml` and `docs/formats/copilot.md` keep the list empty. The captured
  approval log contains no question record, so no answer-state shape was invented.
