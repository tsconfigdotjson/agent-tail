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

Every line has `type`, `id`, `timestamp`, `parentId` and `data`. A helper's records are
written into the **same** `events.jsonl`, with a top-level `agentId`; there is no separate
helper transcript directory.

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
| `session.error` | `errorType`, `message`, `statusCode`; a terminal model/query failure |
| `session.shutdown` | routine end of a prompt-mode process; may follow `abort` or `session.error` |
| `subagent.started` | top-level `agentId`; `data.toolCallId`, `agentName`, `agentDisplayName`, `agentType`, `executionMode` |
| `subagent.configured` / `subagent.completed` | top-level `agentId`; `completed` carries `data.toolCallId`, token and duration totals |
| `system.notification` | an `agent_idle` notification names `data.kind.agentId` after a helper finishes |
| helper records | any other record with a top-level `agentId` (prompt, model calls, tools, usage, turn ends) belongs to that helper |
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
  `assistant.turn_end`, is firm `Done`. A routine `session.shutdown` also ends an ordinary
  interaction. `abort` is firm `Interrupted`; `session.error` is firm `Errored` with its
  `message`. 1.0.103 writes `assistant.turn_end` before `abort`/`session.error`, then a
  routine `session.shutdown`; the adapter keeps the terminal interrupted/errored state
  instead of letting that shutdown turn it back into `Done`.
- **Usage limit:** `session.error` may carry `errorType: "rate_limit"`, `statusCode: 429`,
  and a message ending in the provider's usage-limit text. It stays firm `errored`, adds
  `stop_reason: "usage_limit"`, and is not undone by the routine shutdown.
- **Usage:** each `session.usage_record` is a per-call count keyed by `apiCallId`; the
  session total is their sum.
- **Subagents:** `subagent.started` creates a background helper, `subagent.completed` (or
  the later `agent_idle` notification) ends it. Records carrying its top-level `agentId` are
  reduced in their own helper state, so a helper prompt, tool or turn end never changes the
  parent's status, title or usage. While the parent's turn is `Done` and any helper is still
  running, the session is `BackgroundWork` / `background_helpers`; the helper appears in
  `SessionState.background` with its own activity. The committed `subagent-helper` fixture
  captures this from 1.0.103 against the offline mock.
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
- `copilot_hard_kill_mid_tool`: SIGKILL after `tool.execution_start` for a long `bash` call;
  the event log freezes at firm `RunningTool` with no `tool.execution_complete`, `abort` or
  `session.shutdown`, then falls to `idle` after 15 minutes.
- `copilot_parallel_tools`: two `bash` requests in one assistant message; the session stays
  `RunningTool` until both results arrive, then ends `Done`.
- `copilot_permission_denied`: no `--allow-all-tools`; prompt mode records
  `permission.requested` and the unattended denial, then the model answers after the tool
  error.
- `copilot_api_error`: a terminal HTTP 500 records `session.error`; the session is firm
  `Errored`, not `Done`, even after the routine shutdown.

The CI matrix installs pinned **1.0.103** and npm **latest** on Linux and macOS. A latest
failure is a failing job, not a silent skip. The committed fixtures are under
`fixtures/copilot/1.0.103/linux/` (`plain-qa`, `tool`, `permission-denied`), captured from
the real CLI through the mock and redacted by `agent-tail fixture capture`. The
`permission-approved` and `permission-user-denied` fixtures are **hand-made** from the
1.0.103 event/result shapes, not captured from a TUI; they are not additional CI
scenarios. `parallel-tools`, `interrupt` and `api-error` are hand-made coverage for
multi-call, abort and `session.error` sequences whose event order was observed in real
1.0.103 logs against the mock. `fixtures/copilot/1.0.103/linux/ci-copilot_hard_kill_mid_tool/`
is a real kernel SIGKILL capture; its `NOTES.md` lists the flushed and missing rows.

Local green runs (1.0.103, Linux):

```sh
cd harness-ci
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_plain_qa.json
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_tool.json
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_parallel_tools.json
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_permission_denied.json
cargo run -p harness-driver -- --scenario driver/scenarios/copilot_api_error.json
```

## Open questions and limits

- The real GitHub Copilot routing service, sign-in flow and premium-request accounting are
  not exercised; the BYOK path is. A GitHub-auth-only behaviour change would not be caught
  by this lane.
- A real interactive approval (user approves or denies at the TUI) was still not captured.
  The prompt-mode flow has no interactive user, and no safe non-interactive way was found to
  drive the TUI's approve/deny prompt; this PR does not add one. The event shape for
  `permission.requested` is real; the prompt-mode denial is the completed result that was
  available without a human. The user-approved (`result.kind: "approved"`) and user-denied
  (`result.kind: "denied-interactively-by-user"`) completion shapes remain exercised only by
  the hand-made `permission-approved` and `permission-user-denied` fixtures, derived from
  1.0.103. A live TUI capture is still the remaining gap.
- `session-store.db` is not read; the event log is authoritative for both parent and helper
  state.
