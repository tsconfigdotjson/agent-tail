# Hard-kill mid-tool capture (#117)

- Captured from Codex **0.160.1** on Linux through the harness CI driver scenario
  `harness-ci/driver/scenarios/codex_hard_kill_mid_tool.json`.
- Command: `codex --no-daemon -a never -s danger-full-access`. The mock returned one
  `exec_command` call (`sleep 60`) and no follow-up reply.
- Kill method: after a 16 s observation window, the driver ends the phase with
  `child.kill()` and then SIGKILLs the PTY process group (`kill -KILL -<pgid>`, followed by
  `kill -KILL <pid>`). No Esc, Ctrl-C, `/exit`, or graceful shutdown was sent.
- Rows flushed before death: `session_meta`, developer/environment context, `world_state`,
  `turn_context`, the user message and `item_completed`, the assistant `function_call`, and
  its `token_usage_record`.
- Rows not flushed: **no** `function_call_output`, **no** `turn_aborted`, and **no**
  `turn_complete`. The rollout therefore ends on a pending function call.
- Status consequence: immediately after the kill the disk evidence is firm `running_tool` /
  `tool_call_pending`; it remains there at +30 s and becomes firm `idle` / `no_writes_for`
  after the unchanged 15 minute `idle_after`. This is the silent-death shape, not an
  interrupted turn.
- The captured ids and temporary paths were redacted; the record shape is unchanged.
