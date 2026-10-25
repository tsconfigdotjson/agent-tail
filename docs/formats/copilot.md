# GitHub Copilot CLI

GitHub Copilot CLI is the `@github/copilot` terminal agent. This lane follows **1.0.103**
on Linux, run offline against the harness mock on 2026-10-25. Facts marked **firm** were
seen in real event logs; **inferred** facts come from the 1.0.103 binary/help or the prior
art and are labelled as such.

## Paths and layout

```text
$COPILOT_HOME/session-state/<session-uuid>/events.jsonl
# default COPILOT_HOME is ~/.copilot:
~/.copilot/session-state/<session-uuid>/events.jsonl
```

**Firm.** `COPILOT_HOME` moves the config/state folder; the tracker uses an absolute value
and appends `session-state`. Each session folder contains `events.jsonl`, a workspace file,
checkpoints and lock files. `session-store.db` is a separate session index and is not read.
The project is `session.start.data.context.cwd`.

## Event catalogue

Every line has `type`, `id`, `timestamp`, `parentId` and `data`.

| event | fields that matter |
|---|---|
| `session.start` | `data.sessionId`, `data.copilotVersion`, `data.selectedModel`, `data.context.cwd` |
| `user.message` | `data.content`; `transformedContent` adds a `<current_datetime>` prefix but is not the prompt text |
| `system.message` | the system prompt; kept as metadata |
| `assistant.turn_start` / `assistant.turn_end` | one model call's boundaries, not the whole user interaction |
| `assistant.message` | `data.content` and `data.toolRequests[]` (`toolCallId`, `name`, `arguments`, `toolTitle`, `intentionSummary`) |
| `tool.execution_start` | `toolCallId`, `toolName`, `arguments` |
| `tool.execution_complete` | `toolCallId`, `success`, `result`, optional `error` |
| `permission.requested` | `toolCallId` directly and under `permissionRequest`/`promptRequest`, `permissionMode`, request kind |
| `permission.completed` | `toolCallId`, `result.kind`, `decisionSource` |
| `session.usage_record` | per-call `usage` (`inputTokens`, `outputTokens`, cache counts, `apiCallId`) |
| `session.shutdown` | normal end of a prompt-mode process |
| `abort` | `reason` such as `user_abort` |

**Firm.** A tool call is persisted in `assistant.message` before execution. In prompt mode
with `--allow-all-tools`, the next records are `tool.execution_start` and later
`tool.execution_complete`. In prompt mode without an interactive answer, the CLI writes
`permission.requested`, then `permission.completed` with result kind
`denied-no-approval-rule-and-could-not-request-from-user` and
`decisionSource: "unattended_fallback"`, then a failed `tool.execution_complete`.

**Firm.** The same `assistant.turn_end` appears after each model call, including the model
call that only requested a tool. The adapter therefore treats a turn end as a completed
answer only after an assistant message with no tool requests; otherwise the interaction
continues.

## Status signals

- **Prompt:** `user.message` is firm `Thinking` / `prompt_sent`; its first line is the
  fallback title.
- **Tool running:** `assistant.message.toolRequests` creates a pending call. Copilot
  persists a permission request whenever a tool may wait for a human, so a pending call
  with no `permission.requested` is firm `RunningTool` / `tool_call_pending`, not the shared
  20 s approval inference.
- **Approval pending:** `permission.requested` is **firm** `AwaitingApproval` /
  `approval_requested`, joined by `toolCallId`. This matches the observed 1.0.103 behavior. The
  observed `permissionMode` is `manual`.
- **Approval completed:** `permission.completed` clears the firm approval. An approved
  request is followed by the tool run; a denied request is followed by a failed tool result.
  The adapter treats result kinds beginning `approved` as approved and all other kinds as
  not approved. The 1.0.103 kinds covered by the fixtures are `approved`,
  `approved-for-session`, `approved-for-location`, `approved-read-only-for-session`,
  `denied-interactively-by-user`, `denied-by-rules`,
  `denied-no-approval-rule-and-could-not-request-from-user`,
  `denied-by-content-exclusion-policy`, `denied-by-permission-request-hook`, `cancelled`
  and `user-not-available`. A user decision carries `decisionSource: "human_response"`;
  the unattended fallback carries `"unattended_fallback"`.
- **Tool result:** `tool.execution_complete` is firm `Thinking` / `tool_result_in` when no
  other calls are pending.
- **Turn end:** an assistant message with text and no tool requests, followed by
  `assistant.turn_end`, is firm `Done`. `session.shutdown` also ends the interaction.
  `abort` is firm `Interrupted`.
- **Usage:** each `session.usage_record` is a per-call count keyed by `apiCallId`; the
  session total is their sum.
- **Subagents:** `subagent.started`/`subagent.completed` are kept as metadata; helper
  transcripts are not followed yet. The parent event log is the only state source.
- **Ask-user:** no ask-user event or tool name was observed. The discovery list stays empty
  rather than guessing from a prompt fragment.

## Harness CI

Copilot supports BYOK, so the rig does **not** need GitHub sign-in:

```sh
HOME="$TMP/home" COPILOT_HOME="$TMP/home/.copilot" \
COPILOT_OFFLINE=true \
COPILOT_PROVIDER_BASE_URL=http://127.0.0.1:<port>/v1 \
COPILOT_PROVIDER_TYPE=openai COPILOT_PROVIDER_API_KEY=dummy \
COPILOT_PROVIDER_MODEL_ID=test-model COPILOT_MODEL=test-model \
CI=true copilot -p 'say hello' --allow-all-tools --trust-folder --output-format json
```

The mock's OpenAI **Chat Completions** route is enough. Scenarios:

- `copilot_plain_qa`: one text answer, firm `Done`.
- `copilot_tool`: a two-second `bash` call sampled as firm `RunningTool`, then `Done`.
- `copilot_permission_denied`: no `--allow-all-tools`; prompt mode records
  `permission.requested` and the unattended denial, then the model answers after the tool
  error.

The CI matrix installs pinned **1.0.103** and npm **latest** on Linux and macOS. A latest
failure is a failing job, not a silent skip. The committed fixtures are under
`fixtures/copilot/1.0.103/linux/` (`plain-qa`, `tool`, `permission-denied`), captured from
the real CLI through the mock and redacted by `agent-tail fixture capture`. The
`permission-approved` and `permission-user-denied` fixtures are **hand-made** from the
1.0.103 event/result shapes, not captured from a TUI; they are not additional CI
scenarios.

Local green runs (1.0.103, Linux):

```sh
cd harness-ci
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_plain_qa.json
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_tool.json
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_permission_denied.json
```

## Open questions and limits

- The real GitHub Copilot routing service, sign-in flow and premium-request accounting are
  not exercised; the BYOK path is. A GitHub-auth-only behaviour change would not be caught
  by this lane.
- A real interactive approval (user approves or denies at the TUI) was still not captured.
  The event shape for `permission.requested` is real; the prompt-mode denial is the completed
  result that was available without a human. The user-approved (`result.kind: "approved"`)
  and user-denied (`result.kind: "denied-interactively-by-user"`) completion shapes are
  exercised by the hand-made `permission-approved` and `permission-user-denied` fixtures,
  derived from 1.0.103. A live TUI capture remains optional future work.
- `subagent.started`/`subagent.completed` are not followed into helper state.
- `session-store.db` is not read; the event log is authoritative.
