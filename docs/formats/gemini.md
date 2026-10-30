# Gemini CLI

Gemini CLI is Google's terminal coding agent (`@google/gemini-cli`). This lane follows
**0.65.0** on Linux, run offline against the harness mock on 2026-10-25. Facts marked
**firm** were seen in a real journal from that run; **inferred** facts come from the 0.65.0
bundle and are not claimed as observed transcript behaviour.

## Paths and layout

```text
$GEMINI_CLI_HOME/.gemini/tmp/<project-short-id>/chats/session-<timestamp>-<short-id>.jsonl
# default GEMINI_CLI_HOME is the user's home:
~/.gemini/tmp/<project-short-id>/chats/session-<timestamp>-<short-id>.jsonl
```

**Firm.** `GEMINI_CLI_HOME` moves Gemini's whole home; the tracker uses the absolute value
and appends `.gemini/tmp`. The project folder is a short id from `projects.json`, not a
reversible encoding of the workspace path. `chats/<parent-session-id>/<session-id>.jsonl`
holds subagent journals; this version reads only the top-level journal.

**Inferred.** When Gemini runs under `SANDBOX=sandbox-exec`, its runtime directory moves to
`$GEMINI_CLI_HOME/.cache/.gemini` (`Storage.getGlobalRuntimeDir` in `paths.js`). The
discovery spec records only `.gemini/tmp`; a sandboxed session is not found by the tracker
yet.

The journal file starts with a metadata record, then message records and mutation records:

```json
{"sessionId":"…","projectHash":"…","startTime":"2026-10-25T03:00:39.270Z","lastUpdated":"…","kind":"main"}
```

## Journal records

| record | fields that matter |
|---|---|
| metadata | `sessionId`, `projectHash`, `startTime`, `lastUpdated`, `kind` |
| `type: "user"` | `id`, `timestamp`, `content` (string or parts). A text part is a prompt; a `functionResponse` part is a tool result. The first user record is a `<session_context>` record, not a prompt. |
| `type: "gemini"` | `id`, `timestamp`, `content`, `thoughts`, `tokens`, `model`, optional `toolCalls` |
| `{"$set": …}` | metadata updates: `lastUpdated`, `summary`, `directories`, `memoryScratchpad`, sometimes `messages` |
| `{"$patch": …}` | message/tool-result patches (`updates`, `removeIds`, `orderIds`) |
| `{"$rewindTo": id}` | removes the named message and everything after it |

A tool call record carries the completed call:

```json
{"id":"g2","type":"gemini","toolCalls":[{
  "id":"run_shell_command__call_mock_0_0",
  "name":"run_shell_command",
  "args":{"command":"sleep 1; printf gemini-tool-ok"},
  "result":[{"functionResponse":{"id":"…","name":"run_shell_command",
            "response":{"output":"<untrusted_context>…</untrusted_context>"}}}],
  "status":"success",
  "timestamp":"2026-10-25T03:02:06.193Z"
}]}
```

**Firm.** 0.65.0 appends the same message id twice: first without `toolCalls`, then with
the completed tool call. It then appends a synthetic `user` record containing the
`functionResponse`, and finally the next model message. The adapter emits the call once and
dedupes the two result records by call id. A real multi-tool run writes both completed calls
in one record's `toolCalls` array and both `functionResponse` parts in the following `user`
record; the `multi-tool` fixture pins that shape.

**Firm.** A `$set` record is written after every message and contains only
`lastUpdated`; those records do not move status. A `$rewindTo` record appears when the CLI
removes the session-context message before the first prompt.

**Inferred.** `summary` is the session title Gemini's session picker displays; the adapter
maps a non-empty summary to a title. `directories` is the authoritative workspace list when
present.

## Status signals

- **Prompt:** a non-context `user` text part is a prompt: firm `Thinking` /
  `prompt_sent`. The opening prompt is the fallback title.
- **Tool call:** only a completed call reaches disk. The adapter emits the call and its
  result together, so the session is `Thinking` / `tool_result_in`, not `RunningTool`, while
  the tool runs. This confirms the observed behavior: pending and `awaiting_approval` tool
  states never reach the journal in 0.65.0.
- **Approval:** the journal records neither the `--approval-mode` value nor a pending
  permission request. A normal Gemini run therefore cannot produce a firm
  `AwaitingApproval`. The adapter still recognises a future
  `toolCalls[].status == "awaiting_approval"` as firm evidence, because that status would be
  explicit on disk; it was only a synthetic case in earlier notes, not observed here.
- **Turn end:** a `gemini` message with text and no `toolCalls` is the completed answer:
  firm `Done` / `turn_ended`. A tool-call message is followed by a function response and
  another model message, so it is not an end.
- **Error:** `type: "error"` becomes firm `Errored`, with its content/message kept. A usage
  limit recognized in that record also sets `stop_reason: "usage_limit"`.
- **API failure rewind:** a real 429 can leave only the just-sent user prompt and a
  `$rewindTo` of that prompt; the error itself reaches stdout/stderr, not the journal. That
  rewind is now firm `Errored` with the error text `Gemini discarded the prompt before
  answering`, so the session is not left `thinking`. The journal records no cause or reset,
  so `stop_reason`/`resets_at` stay null. A pre-prompt context rewind remains metadata.
- **Project:** `directories[0]` from a metadata update when present; otherwise the project
  short-id folder is a label only. The `<session_context>` record also contains a
  `Workspace Directories` path, which the adapter reads without treating the record as a
  prompt.
- **Usage:** each `gemini` message's `tokens` is a per-call count (`input`, `output`,
  `cached`, `thoughts`, `tool`, `total`); the adapter records it without double-counting
  repeated rows.
- **Subagents:** `invoke_agent` is a normal tool call in the parent journal. The child
  journal under `chats/<parent-id>/` is not followed yet, so background subagent state is
  not claimed.

## Harness CI

The driver runs Gemini in one-shot JSON mode:

```sh
GEMINI_CLI_HOME="$TMP/home" GEMINI_API_KEY=dummy \
GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:<port> \
CI=true GEMINI_CLI_TRUST_WORKSPACE=true GEMINI_TELEMETRY_ENABLED=false \
gemini -p 'say hello' --output-format json --approval-mode yolo --skip-trust
```

`$GEMINI_CLI_HOME/.gemini/settings.json` selects `security.auth.selectedType =
"gemini-api-key"`; without it 0.65.0 refuses the API-key auth path even with
`GEMINI_API_KEY` set. The mock implements the Gemini API routes:

- `POST /v1beta/models/<model>:generateContent`
- `POST /v1beta/models/<model>:streamGenerateContent?alt=sse`

0.65.0 sends a **routing-classifier** request first, with a `complexity_score` JSON schema.
The mock answers it without consuming a scenario step. The main stream is translated from
the same scenario blocks used by the other APIs. Scenarios: `gemini_plain_qa`,
`gemini_tool`, `gemini_multi_tool` and `gemini_hard_kill_mid_tool`. The hard-kill case kills
the process while the shell command runs, before the completed-only `toolCalls` row exists,
so it pins firm `thinking` / `prompt_sent` rather than a fabricated interrupt. The CI matrix
installs pinned **0.65.0** and npm **latest** on Linux and macOS; a latest failure is a
failing job, not a skipped one.

Local green run (0.65.0, Linux):

```sh
cd harness-ci
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -p harness-driver -- --scenario driver/scenarios/gemini_plain_qa.json
cargo run -p harness-driver -- --scenario driver/scenarios/gemini_tool.json
```

Committed fixtures are under `fixtures/gemini/0.65.0/linux/`: `plain-qa`, `tool`,
`multi-tool` and `ci-gemini_hard_kill_mid_tool`, captured from the real CLI through the mock
and redacted by `agent-tail fixture capture`; the hard-kill fixture records only the
pre-tool journal that reached disk. `error` is hand-made because a real model/API failure is
not persisted as a `type:error` row (0.65.0 rewinds the just-sent prompt instead), so the
adapter's error mapping is exercised by the synthetic journal shape only.

## Open questions and limits

- Sandboxed (`SANDBOX=sandbox-exec`) sessions use the cache root and are not discovered.
- Subagent journals are not followed; a parent's `invoke_agent` call is visible only after
  it completes.
- `$rewindTo` and `$patch` are parsed for provenance and tool results, but the incremental
  reducer cannot undo earlier activity after a rewind.
- A real interactive approval prompt was not captured because the journal does not record
  one; the synthetic `awaiting_approval` status is supported but unobserved.
- Interrupted turns remain **deferred**: no real 0.65.0 journal shape exists for them here
  (an API failure rewinds the prompt and only completed tool calls are written), so there is
  no driver or fixture that claims that state. The hard-kill scenario is not an interrupt:
  it pins the pre-tool `thinking` / `prompt_sent` journal that a SIGKILL actually leaves.
  Multi-tool turns have a real captured shape and run in the matrix.
