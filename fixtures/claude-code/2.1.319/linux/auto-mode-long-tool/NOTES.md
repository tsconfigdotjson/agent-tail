# Claude Code auto mode: a long tool is not an approval prompt

Captured from a real **Claude Code 2.1.319** auto-mode session on Linux (donated offline),
not from the mock model. `input/session.jsonl` is a contiguous 15-row slice around a Bash
call; deterministic redaction replaced values (with `--lorem` for prose after the sensitive
value checks), and the two tool names were restored from the capture so the tool shape stays
readable. Record shapes, order, timestamps, and the `permission-mode: auto` / `tool_use` /
`tool_result` linkage are unchanged.

The Bash call at fixture line 14 started at `19:29:06.456Z` and its result arrived at
`19:29:38.366Z`, **31.9 s later**. The live session showed no permission prompt; auto mode
approved the call (the result records `permissionDecision` with `reasonType: "classifier"`).
The checkpoints are:

- after line 14 at 0 s: `running_tool` / `firm` / `tool_call_pending`;
- after line 14 at 30 s: `running_tool` / `inferred` / `tool_call_pending` (before this
  change the same row produced `awaiting_approval` / `inferred` / `tool_call_stalled`);
- after line 15: the result returns the session to `thinking`.

## Is there a firm on-disk signal for a fallback prompt?

No transcript record marks a pending auto-mode permission prompt. When the classifier falls
back to asking a human, the transcript only gains the tool result's `permissionDecision`
**after** the prompt is answered; while the prompt is up the evidence is the terminal screen,
not the project JSONL. The 2.1.322 binary also has a `system/permission_check_status` event
for SDK/output-stream hosts when an automatic check runs long, but it is not written to the
project transcript and it does not say that a person was asked. `agent-tail` therefore has
no pre-result disk record to turn into firm `awaiting_approval`; it keeps a stalled auto-mode
tool `running_tool` with inferred certainty. A harness that persists a real permission
request (GitHub Copilot's `permission.requested`) still reports firm `awaiting_approval`
first through `pending_approvals`.

No PID tracking is used, and `idle_after` remains 15 minutes.
