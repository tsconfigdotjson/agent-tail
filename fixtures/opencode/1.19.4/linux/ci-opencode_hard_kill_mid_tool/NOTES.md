# Hard-kill mid-tool capture (#117)

- Captured from OpenCode **1.19.4** on Linux through the harness CI driver scenario
  `harness-ci/driver/scenarios/opencode_hard_kill_mid_tool.json`.
- Command: `opencode run --format json --auto -m test/test-model sleep`. The mock returned
  one `bash` call (`sleep 60`) and no follow-up reply.
- Kill method: after a 20 s observation window, the driver ends the phase with
  `child.kill()` and then SIGKILLs the PTY process group (`kill -KILL -<pgid>`, followed by
  `kill -KILL <pid>`). No Esc, Ctrl-C, or graceful exit was sent.
- Rows flushed before death: the session row, user message and text part, assistant message,
  `step-start` part, and the `tool` part with `state.status: running` for the `bash` call.
- Rows not flushed: **no** completed tool state, **no** `step-finish` row (so no
  `completed`, `aborted`, `interrupted`, or `error` reason), and **no** later assistant text.
  The SQLite snapshot therefore ends on a running tool part.
- Status consequence: immediately after the kill the disk evidence is firm `running_tool` /
  `tool_call_pending`. At +30 s the existing OpenCode tool-stall inference makes it
  `awaiting_approval` (`tool_call_stalled`, inferred) because approval requests are not
  persisted; at +901 s it becomes firm `idle` / `no_writes_for` after the unchanged 15 minute
  `idle_after`. No interrupted row exists or is invented.
- Agent-tail itself does no process or PID tracking; the kill lives only in the test driver.
- The captured ids and temporary paths were redacted; the record shape is unchanged.
