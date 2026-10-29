# Hard-kill mid-tool capture (#117)

- Captured from Claude Code **2.1.312** on Linux through the harness CI driver scenario
  `harness-ci/driver/scenarios/claude_hard_kill_mid_tool.json`.
- The mock returned one `Bash` tool call (`sleep 60`) and no follow-up reply, so the turn
  could not complete before the kill.
- Kill method: after an 8 s observation window, the driver ends the phase with
  `child.kill()` and then SIGKILLs the PTY process group (`kill -KILL -<pgid>`, followed by
  `kill -KILL <pid>`). No Esc, Ctrl-C, `/exit`, or graceful shutdown was sent.
- Rows flushed before death: `mode`, `permission-mode`, `atis-latch`,
  `file-history-snapshot`, the user prompt, attachments, `api-request-shape`,
  `api-request`, `tools-used`, and the assistant `tool_use` row with `stop_reason:
  tool_use`.
- Rows not flushed: **no** `tool_result`, **no** `[Request interrupted by user ...]` row,
  no `interruptedMessageId`, and no `turn_duration`/`stop_hook_summary`. The transcript
  therefore ends on a pending tool call.
- Status consequence: immediately after the kill the disk evidence is firm
  `running_tool` / `tool_call_pending` (not `interrupted`); it stays there at +30 s under
  bypass permissions and becomes firm `idle` / `no_writes_for` after the unchanged 15 minute
  `idle_after`. This is the silent-death shape, distinct from the Esc interrupt fixture,
  which does have an explicit interrupt row.
- The captured ids and temporary paths were redacted; the record shape is unchanged.
