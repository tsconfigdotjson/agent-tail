# Hard-kill mid-tool capture (#117)

- Captured from Gemini CLI **0.65.0** on Linux through the harness CI driver scenario
  `harness-ci/driver/scenarios/gemini_hard_kill_mid_tool.json`.
- Command: the headless JSON path with `--approval-mode yolo --skip-trust -p sleep`. The mock
  returned one `run_shell_command` call (`sleep 60`) and no follow-up reply.
- Kill method: after a 30 s observation window, the driver ends the phase by killing the
  process and then SIGKILLing its process group (`kill -KILL -<pgid>`, followed by
  `kill -KILL <pid>`). No Esc, Ctrl-C, or graceful exit was sent.
- Rows flushed before death: the session metadata line, the `session_context` user record,
  the user prompt, its `$set.lastUpdated` marker, and one `type: gemini` record with empty
  `content`/`thoughts` and the request token usage.
- Rows not flushed: **no** `toolCalls` entry and **no** `functionResponse`. Gemini 0.65.0
  writes a tool call only after the command completes, so the journal ends before the tool is
  recorded. There is also no error or interrupt row.
- Status consequence: immediately after the kill the honest disk evidence is firm
  `thinking` / `prompt_sent`, **not** `running_tool`: the pending call never reached disk.
  It remains there at +30 s and becomes firm `idle` / `no_writes_for` after the unchanged
  15 minute `idle_after`. No Gemini interrupt shape was invented.
- The captured ids and temporary paths were redacted; the record shape is unchanged.
