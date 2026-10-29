# Hard-kill mid-tool capture (#117)

- Captured from Pi **1.4.0** on Linux through the harness CI driver scenario
  `harness-ci/driver/scenarios/pi_hard_kill_mid_tool.json`.
- Command: the headless JSON path (`pi --mode json ...`); the mock returned one `bash`
  call (`sleep 60`) and no follow-up reply.
- Kill method: after a 15 s observation window, the driver ends the phase by killing the
  process and then SIGKILLing its process group (`kill -KILL -<pgid>`, followed by
  `kill -KILL <pid>`). No Esc, Ctrl-C, or graceful exit was sent.
- Rows flushed before death: `session`, `model_change`, `thinking_level_change`, the system
  message, the user message, and the assistant message containing the `bash` `toolCall`
  with `stopReason: toolUse`.
- Rows not flushed: **no** `toolResult`, **no** later assistant message, and **no**
  `stopReason: aborted` row. The session therefore ends on a pending tool call.
- Status consequence: immediately after the kill the disk evidence is firm `running_tool` /
  `tool_call_pending`; it remains there at +30 s and becomes firm `idle` / `no_writes_for`
  after the unchanged 15 minute `idle_after`. This is the silent-death shape, not an
  interrupted turn.
- The captured ids and temporary paths were redacted; the record shape is unchanged.
