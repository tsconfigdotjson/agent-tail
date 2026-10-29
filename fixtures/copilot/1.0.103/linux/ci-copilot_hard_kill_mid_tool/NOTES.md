# Hard-kill mid-tool capture (#117)

- Captured from GitHub Copilot CLI **1.0.103** on Linux through the harness CI driver
  scenario `harness-ci/driver/scenarios/copilot_hard_kill_mid_tool.json`.
- Command: the headless `-p` path with `--allow-all-tools`; the mock returned one `bash`
  call (`sleep 60`) and no follow-up reply.
- Kill method: after a 20 s observation window, the driver ends the phase by killing the
  process and then SIGKILLing its process group (`kill -KILL -<pgid>`, followed by
  `kill -KILL <pid>`). No abort key or shutdown command was sent.
- Rows flushed before death: `session.start`, `user.message`, `system.message`,
  `assistant.turn_start`, the usage record, the assistant message carrying the `bash`
  `toolRequests` entry, and `tool.execution_start`.
- Rows not flushed: **no** `tool.execution_complete`, **no** `assistant.turn_end`, **no**
  `abort` row, and **no** `session.shutdown`. The event log therefore ends on a running tool.
- Status consequence: immediately after the kill the disk evidence is firm `running_tool` /
  `tool_call_pending`; it remains there at +30 s and becomes firm `idle` / `no_writes_for`
  after the unchanged 15 minute `idle_after`. No interrupt state is written or inferred.
- The captured ids and temporary paths were redacted; the record shape is unchanged.
