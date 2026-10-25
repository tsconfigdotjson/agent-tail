# Harness CI

Integration tests that run the real coding-agent CLIs against a mock model server, under a
PTY, and score `agenttail`'s status against what the CLI is actually showing on screen.

This directory is a separate cargo workspace (`harness-ci/Cargo.toml`) so the library's
dependencies stay small. CI builds it explicitly; the root `cargo test` does not.

## A0 spike: Claude Code can run fully offline (2026-10-24)

Verified on macOS against Claude Code **2.1.310**, with outbound network blocked by
`sandbox-exec` except to `localhost`.

### Pointing it at a mock

- `ANTHROPIC_BASE_URL=http://localhost:<port>` is honoured.
- A dummy `ANTHROPIC_API_KEY` is accepted by the CLI; the mock never validates it.
- `--bare` makes auth strictly `ANTHROPIC_API_KEY` (no keychain, OAuth or keychain reads),
  which is exactly what CI needs. It also skips user/project settings, hooks, plugins,
  auto-memory and `CLAUDE.md`, so a run only sees the mock and the temp work folder.

The CLI calls, in order:

1. `GET /v1/models` (harmless; the mock can return an empty list).
2. `POST /v1/messages?beta=true` (streaming SSE).
3. Sometimes a second, non-streaming `POST /v1/messages?beta=true` (the CLI retries a
   failed stream / falls back). A mock should answer both the same way.

The request is the normal Anthropic Messages API shape: `model`, `messages`, `system`,
`tools`, `max_tokens`, `thinking`, `metadata`, `stream`. Usage is read from
`message_start.message.usage` and `message_delta.usage` in streaming mode.

### Turning off everything else

These are confirmed in the 2.1.310 binary and were used in the offline run:

| variable | effect |
|---|---|
| `DISABLE_AUTOUPDATER=1` | no update check |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | no telemetry, feature flags, update pings |
| `DISABLE_TELEMETRY=1` | no telemetry |
| `DISABLE_ERROR_REPORTING=1` | no error reports |
| `CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY=1` | no feedback survey |

With those set and `sandbox-exec` denying everything but `localhost`, a plain prompt still
completes. No non-localhost request was attempted.

### Isolation

Use a temp `HOME` **and** `CLAUDE_CONFIG_DIR` pointing inside it. A non-interactive run
writes only:

```
<HOME>/.claude/.claude.json
<HOME>/.claude/backups/.claude.json.backup.<timestamp>
<HOME>/.claude/projects/<encoded-cwd>/<session-id>.jsonl
```

Interactive runs also create a messaging socket (normally `/tmp/cc-socks/<pid>.sock` on
macOS). The driver must treat that as an allowed exception or set a socket dir inside the
temp home if the CLI grows an override. The CLI never reads the real `~/.claude` when both
variables point into the temp home.

### Skipping onboarding in the PTY

Interactive mode shows three one-time screens before the prompt: theme, custom-API-key
confirmation, security notes, and terminal setup, then a workspace-trust dialog. Driving
them with keys is brittle. Seed `<CLAUDE_CONFIG_DIR>/.claude.json` before launch instead:

```json
{
  "hasCompletedOnboarding": true,
  "lastOnboardingVersion": "2.1.310",
  "numStartups": 10,
  "theme": "dark",
  "customApiKeyResponses": { "approved": ["dummy-key"], "rejected": [] },
  "projects": {
    "/absolute/path/to/work": {
      "hasTrustDialogAccepted": true,
      "allowedTools": [],
      "mcpServers": {}
    }
  }
}
```

The approved key is the **last 20 characters** of the key (the binary's `customApiKeyResponses`
check is `key.trim().slice(-20)`). With this seed, the PTY goes straight to the prompt.
The interactive TUI writes ANSI cursor-positioning sequences, so screen parsing must use a
vt100 emulator, not text stripping.

### Reproducing the spike

```sh
# 1. a mock that answers /v1/messages (see mock-server/)
# 2. a temp home + work folder
export HOME="$TMP/home" CLAUDE_CONFIG_DIR="$TMP/home/.claude"
export ANTHROPIC_BASE_URL="http://localhost:8765"
export ANTHROPIC_API_KEY="dummy-key"
export DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY=1

sandbox-exec -f offline.sb claude --bare -p 'say hello' --model claude-sonnet-4-5
```

`offline.sb`:

```scheme
(version 1)
(allow default)
(deny network*)
(allow network-bind (local ip "localhost:*"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
```

The result was a clean `end_turn` and a Claude Code transcript under the temp home. The
check is committed as [`offline.sb`](offline.sb). The
same profile runs the whole PTY driver offline; the driver's in-process mock needs the
`network-bind`/`network-inbound` allowances, not only outbound.

```sh
sandbox-exec -f offline.sb harness-ci/target/debug/harness-driver \
  --scenario driver/scenarios/claude_plain_qa.json --out /tmp/run
```

That run produced 0 hard failures and `Done` while the screen was idle.

## A1: mock model server (`mock-server/`)

A scripted Anthropic Messages, OpenAI Responses, and OpenAI Chat Completions server. It
answers streaming and non-streaming requests, logs every request, and returns a 500 with a
readable explanation when a request is not the next scripted step.

```sh
cd harness-ci
cargo run -p mock-server -- --scenario mock-server/scenarios/plain_qa.json --log /tmp/mock.jsonl
# prints MOCK_ADDR=127.0.0.1:<port>, then serves
```

A scenario is a list of steps:

```json
{
  "name": "plain_qa",
  "steps": [
    {
      "expect": { "user_contains": "say hello" },
      "reply": {
        "content": [{ "type": "text", "text": "Hello from the mock." }],
        "usage": { "input_tokens": 10, "output_tokens": 5 },
        "fault": { "type": "slow_first_token", "delay_ms": 1000 }
      }
    }
  ]
}
```

- `expect`: `contains`, `user_contains`, `tool_result_id`, `tool_result_contains`,
  `has_tool_result`, `request_index`.
- `reply.content`: `text`, `thinking` (with a signature), `tool_use` (with `input`). A
  `tool_use` defaults its id to `toolu_mock_<request>_<block>`.
- `reply.fault`: `http` (`status`, `message`), `broken_stream`, `slow_first_token`
  (`delay_ms`), `slow` (`delay_ms`), `pause_after_first_block` (`delay_ms`). A streaming
  `broken_stream` sends `message_start` and closes. Claude Code then retries that turn
  *without* streaming, so a non-streaming `broken_stream` is HTTP 500 instead of the
  scripted body. `pause_after_first_block` finishes the first content block, then
  waits before the rest of the stream, so Esc can land after thinking has started.
- `expect.user_also_contains` is a second user-text substring. `expect.not_user_contains`
  rejects a step when the user's text has that substring (tool results are not user text).
  System prompt text is part of `contains`, not of `user_contains`.
- `repeat` expands one step into that many identical steps (cap 30), for a fault the CLI retries.
- `dispatch` is `sequential` (default) or `first_match`. `first_match` answers the first unused
  step whose `expect` matches, so a subagent's model request can arrive before the parent's
  next turn. A request that matches nothing is still a 500.
- Claude Code's separate session-title request is answered with a fixed title and does not
  consume a step. Without that, the title request steals the scripted first reply.
- `probes` (optional) answers health checks without consuming a step. `user_texts` is an
  exact allowlist of the user text, and `max_tokens_at_most` requires `max_tokens` to be
  present and no larger than that. Empty `probes` (the default, including `plain_qa.json`)
  leaves every request on the script. `desktop_plain_qa.json` is `plain_qa`'s step plus
  `{"user_texts": ["."], "max_tokens_at_most": 1}` for Claude Desktop's gateway probe.
  A `"."` request with a larger or missing `max_tokens`, or any other user text, still
  has to match the step. The probe reply is a short `ok` message (`msg_mock_9001`) and
  is logged with `"probe": true`.

Codex's OpenAI Responses subset is implemented (`/v1/responses`); A4 exercises it from the live CLI.
OpenCode's Chat Completions subset is implemented at `/v1/chat/completions` and
`/chat/completions`; its title request (system prompt starts `You are a title generator.`) is
answered without consuming a scenario step, like Claude's and Codex's title requests.

## A2: PTY driver (`driver/`)

`harness-driver` starts the mock server in-process, starts the CLI under a real PTY in a
temp `HOME` and workspace, feeds the screen through a vt100 parser, and samples:

- the screen state (idle, working, permission prompt, error, …), from captured strings;
- `agenttail`'s `SessionState`, read from the transcript on disk at that moment.

It writes `score.json`, `timeline.jsonl`, `screen.txt`, `transcript.jsonl` and the mock's
`mock-requests.jsonl` to a run directory (default `harness-ci/runs/`).

```sh
cd harness-ci
cargo run -p harness-driver -- --scenario driver/scenarios/claude_plain_qa.json
```

A driver scenario adds the CLI command, prompt, key actions and run time:

```json
{
  "name": "claude_bash_approval",
  "harness": "claude_code",
  "mock": "../../mock-server/scenarios/bash_approval.json",
  "prompt": "run the command",
  "keys": [{ "on": "permission_prompt", "send": "\r", "after_ms": 2000 }],
  "run_for_ms": 14000,
  "expected_activity": [{ "activity": "done", "when": "end", "screen": "idle", "certainty": "firm" }]
}
```

`run_for_ms` is the observation window **after the prompt is typed**, not including CLI
startup. A prompt-driven phase gets up to 60 s to reach the composer, so a newer, slower CLI
startup is not scored as a transcript regression. A phase with no prompt uses `run_for_ms` from
launch as before.

Hard failures: `AwaitingApproval` when the screen shows no permission prompt (a sample within
1 s of a real prompt is write latency, not a failure), `AwaitingApproval` in a `no_approval`
scenario or a bypass / dontAsk / never permission mode, a real prompt left un-inferred past
`stall_after` plus 1 s, and `Done` while background agents are running (the library still lists
helpers, or the screen keeps the `Waiting for N background agent(s) to finish` status for more
than that 1 s). The driver exits non-zero on any hard failure. Other screen/library mismatches
are recorded as `disagreements` after the same allowances, and stay in `score.json` without
failing the run.

`score.json` carries the raw confusion matrix (`screen` → library activity → samples), the
disagreements that remain after `stall_after` and a 1 s write-latency window, and the
hard-failure list. `stall_after_ms` and `write_latency_ms` are written beside them.
`RunningTool` during the first `stall_after` of a permission prompt is agreement. `Thinking` /
`RunningTool` on a working screen, `AwaitingAnswer` on an ask-user question, `Errored` on an
error banner, and `Done` / `Idle` / `Interrupted` / `Errored` on an idle composer are agreement.
Samples within 1 s of an agreeing sample are the write-latency allowance. `starting` and
`unknown` screens are not scored.

### What the rig has already found

- **Permission prompts are visible only on screen.** In default mode, a `touch` command
  showed the real prompt; no transcript record is written while it is up. The library says
  `RunningTool` until the 20 s stall threshold, then `AwaitingApproval` (inferred), exactly
  as designed. The captured screen is in the detector tests.
- **Bypass mode now passes.** Before B1 a 60 s `sleep` stayed `RunningTool` on screen while
  the library turned `AwaitingApproval` after 20 s. After reading the permission mode the
  library stays `RunningTool` (firm) for the whole run, and the scenario has 0 failures.
- **Esc during streaming thinking does not write an interrupt row.** On Claude Code
  2.1.310 the thinking text is on screen, and the transcript still has no assistant
  row, when Esc restores the draft. There is no `[Request interrupted by user]`
  record, so the library stays `Thinking`. `claude_interrupt_thinking` keeps the
  firm `Interrupted` expectation and hard-fails on that version. Esc during a
  running tool does write the row, and that turn has no `turn_duration`.
- **A background-agent status above the mode footer is still working.** Bypass
  mode leaves `bypass permissions on` under `Waiting for N background agent(s)
  to finish`. The footer is chrome; the status line is the activity. A later
  `for Ns · done` line settles the screen back to idle.
- **Model latency matters.** With an instant mock response, Claude Code can append the
  user prompt record *after* the assistant reply. A prompt older than activity already
  recorded does not move the state backwards, so an instant `Done` or `Errored` stays put.
  Scenarios still delay the first token by about 1 s when the screen needs to show the
  working state before the reply.
- **Onboarding, trust and bypass disclaimers** are all skipped by seeding `.claude.json`
  and `.claude/settings.json` (`skipDangerousModePermissionPrompt: true`). No key presses
  are needed before the prompt.
- **Firm ends/interrupts/errors** can be asserted with `expected_activity` on a driver
  scenario. A string, or `"when": "end"`, must be the last library activity (the screen it
  names may lag by the 1 s write allowance). `"when": "ever"` must occur at least once, which
  is what an interrupt or an error needs when the run then settles somewhere else. Optional
  `"certainty": "firm"` or `"inferred"` tightens that check. The four green scenarios leave
  the field unset.
- **`{{workspace}}`** in a mock scenario is replaced with the temp work directory before the
  mock starts, JSON-escaped. A driver scenario's `files` object is written into that
  directory (relative paths only). `keys[].occurrence` is the 1-based entry into that screen
  state, so the same prompt can be approved and then denied. `min_screen_ms` fails the run
  when a screen was visible for less than that many milliseconds.
- **Denial and mode observations** come from the transcript, not the screen. `score.json`
  `observations.tool_rejections[].ends_turn` is true only when the rejection is not followed
  by another assistant message before the next user prompt, and an interrupt or
  `turn_duration` / `end_turn` is. `observations.tool_permissions` lists each tool result's
  `permissionDecision`: `source: config` was automatic, `user_temporary` / `user_reject`
  means a person answered. `observations.permission_modes` is the vocabulary the transcript
  actually wrote.

### Remaining A0/A2 work

- Codex is covered by the A4 section above and its own screen-reader tests. Cursor still has no
  real PTY capture; its screen reader returns `unknown` and does not reuse Claude's strings.
- Claude screen readers cover the permission prompt, an ask-the-user question (including the
  plan-exit dialog and `accept edits` / `plan mode` idle footers), the idle composer, the
  spinner / working line, an error banner, and the settled interrupt composer
  ("What should Claude do instead?"). Each has a unit test in `driver/src/screen.rs`. A real
  PTY capture should replace that interrupt string if 2.1.310 words it differently.

### A3 scenarios (permission and ask-user)

| scenario | what it checks |
|---|---|
| `claude_bash_approval_denied` | same Bash command approved, then denied (`Down Down Enter` on the second prompt). `observations.tool_rejections[].ends_turn` is the record for B2. On 2.1.310 the denial ends the turn, so the run expects firm `done` on the idle screen and the closing mock step stays unused |
| `claude_accept_edits` | `--permission-mode acceptEdits`, without `--bare` so Write is offered. Write, then `curl`. Up to two prompts are approved. `tool_permissions` shows which tool still asked |
| `claude_plan_exit` | `--permission-mode plan`, without `--bare`. Read, Bash `touch` (approved if it prompts; a denial ends the turn), `ToolSearch` loads deferred `ExitPlanMode`, then the plan dialog stays up. The run ends firm `awaiting_answer` on `ask_user_question` |
| `claude_ask_user` | `AskUserQuestion` without `--bare` (under `--bare` the tool is not offered). Answered after 30 s. Firm `awaiting_answer` while the question is up, then `done` |

`--bare` (the default command, and the four green scenarios) sets `CLAUDE_CODE_SIMPLE=1`, which only offers Bash, Edit, and Read. The three scenarios that need Write, AskUserQuestion, or ExitPlanMode omit it. ExitPlanMode is a deferred tool: the mock calls `ToolSearch` with `select:ExitPlanMode` before the plan dialog.

On Claude Code 2.1.310 those runs showed:

- **Denial** ends the turn. The transcript has `toolUseResult` "User rejected tool use", `toolDenialKind` `user-rejected`, then a user interrupt and `turn_duration`. The model is not called again (`remaining_mock_steps` 1). The screen returns to idle ("Interrupted · What should Claude do instead?") and the library is `done` / firm. That is the B2 input; this change only records it.
- **acceptEdits:** Write is accepted from config (`reason_type: mode`) with no prompt. `curl` still prompts and is `user_temporary` after Enter.
- **plan:** Read is accepted from config (`reason_type: mode`). Bash `touch` still prompts (`user_temporary`). ToolSearch is automatic. ExitPlanMode stays on screen as "Exit plan mode?" / "Claude wants to exit plan mode", with "Yes, and switch to default" and "No". The library is `awaiting_answer` / firm for that whole dialog. The short "Would you like to proceed?" form is still recognized.
- **AskUserQuestion:** the question stays on the ask screen for the 30 s wait (`min_screen_ms`), the library is firm `awaiting_answer` then, and Enter ends the turn at `done`.

```sh
cd harness-ci
cargo run -p harness-driver -- --scenario driver/scenarios/claude_ask_user.json
```

### A3 scenarios (interrupt, background, API error)

| scenario | what it checks |
|---|---|
| `claude_interrupt_thinking` | Esc while the first token is still delayed. Firm `interrupted` on the idle composer, not `done`. `observations.interrupts[].turn_duration_before_next_prompt` records whether a `turn_duration` was written |
| `claude_interrupt_tool` | Esc during a bypass-mode `sleep 30`. Same expectation. `no_approval` so a stall into `AwaitingApproval` is a hard failure |
| `claude_background_agent` | Without `--bare` (so `Agent` is offered) and with `--dangerously-skip-permissions`. A background `Agent` launches a nested `Agent`. `dispatch: first_match` answers the subagent's own model request, whichever order it arrives. Firm `background_work` on the working screen while `pendingBackgroundAgentCount` is set, then firm `done` after `task-notification`. Helper files are copied to `subagents/` and summarized on `observations.background` (`async_launched`, pending counts, task statuses, `spawnDepth` / `parentAgentId`) |
| `claude_api_error_529` | HTTP 529. `CLAUDE_CODE_MAX_RETRIES=0` makes the failure terminal. Firm `errored` at the end |
| `claude_api_error_stream` | The stream closes after `message_start`, with the same retry cap. Firm `errored` at the end |

`turn_duration` still ends a normal turn. It does not replace an API-error row: that row stays `Errored` when the duration record is written after it, including in the same read. A later real assistant `end_turn` clears the error. Keys are not sent until the prompt has been typed, and the key clock restarts then, so a startup spinner is not the Esc target. `keys[].occurrence` of `0` means any visit to that screen, so a short flicker does not consume the Esc meant for the long working streak.

The background scenario does not link `async_launched` or `.meta.json` into the library's helper state. `BackgroundWork` comes from `pendingBackgroundAgentCount` on the parent transcript. `Done` while the screen still says background agents are running remains a hard failure.

```sh
cd harness-ci
cargo run -p harness-driver -- --scenario driver/scenarios/claude_background_agent.json
```

### A3 scenarios (compact, mode, resume, long output, sessions)

| scenario | what it checks |
|---|---|
| `claude_compact_manual` | `/compact` on the idle after the first answer, then a follow-up in the same file. Firm `done` on the idle screen. `observations.compactions` wants trigger `manual`, a following summary, and a later real turn |
| `claude_compact_auto` | Three Bash results of 110000 `Z` bytes, with `settings.bashOutputMaxChars` 120000. Firm `done`. Trigger `auto` in the same file |
| `claude_mode_model` | Without `--bare`. Shift+Tab, `/model haiku`, Enter to confirm, then a follow-up. `model_switch` and permission modes `default` then `acceptEdits` |
| `claude_continue` | Two phases. The second is `--continue`. Same file, no `continued-in` record. Firm `done` (the screen check is omitted because the process has exited) |
| `claude_resume` | Same shape with `--resume {{session_id}}` taken from the previous phase |
| `claude_continued_in` | `/background` writes `continued-in` and a second transcript. Firm `done` at some sample, then firm `unknown` at the end |
| `claude_long_output` | Bypass-mode Bash prints 30000 `Z` bytes. Lean and full parses agree, and the tool result is at least 20000 bytes |
| `claude_non_ascii` | Prompt, `ai-title`, and a workspace path all contain non-ASCII. The mock `title` is the JSON title the CLI persists |
| `claude_sessions_registry` | A Bash approval. `expect_registry` wants `busy`, `idle`, and `waiting` / `permission prompt` while the screen is `permission_prompt`, and the pid file removed after a graceful exit |

On Claude Code 2.1.310 (Linux, the native binary on `PATH`):

- **Manual `/compact`** writes `compact_boundary` with trigger `manual` in the same file, then `isCompactSummary`, then the follow-up. The `/compact` slash line is a real user prompt, so the library is `thinking` on an idle screen for about 1.6 s until the follow-up's assistant `end_turn`. That is a disagreement, not a hard failure. A `<local-command-…>` user or a `turn_duration` does not count as continuity.
- **Auto-compact does not read `usage.input_tokens`.** It estimates bytes. `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=1` with `CLAUDE_CODE_AUTO_COMPACT_WINDOW=100000` left a 20k-character prompt at "95% until auto-compact" and wrote no boundary. Bash output above 30000 characters is replaced by a `<persisted-output>` stub (the ceiling is 128000). The scenario omits `--bare` so `settings.bashOutputMaxChars` 120000 is read, and one 110000-byte result tripped trigger `auto` (`pre_tokens` 27582, summary follows, the next assistant reply continues the same file). Two later mock steps stayed unused.
- **Shift+Tab** cycles the footer to "accept edits on" and the next user record's `permissionMode` is `acceptEdits`. **`/model haiku`** opens a dialog the registry calls `waiting` / `dialog open` while the screen reader still says idle. Enter confirms it: the header becomes Haiku 5.5 and the next request's model is `claude-haiku-5-5`. Esc cancels it ("Kept model as Sonnet 4.5").
- **`--continue` and `--resume`** append the same session file and do not write `continued-in`. **`/background`** does: a second file whose records use `continuedInSessionId`. The parent reaches `done`, then the driver's tail follows the new file, which is only `ai-title` and `agent-name`, so the end sample is firm `unknown` with no entries. The screen there is also `unknown`.
- **Lean** agrees with a full parse on a 30000-byte tool result and on a 120441-byte inline result. Both activities are `done`. A result under the 16KiB cut fails the length check as "not long enough".
- **Titles** stick only when the title request is answered with JSON `{"title":"…"}`. Plain text is discarded, so a non-ASCII title has to be the scenario's `title`.
- **Sessions registry** (`$CLAUDE_CONFIG_DIR/sessions/<pid>.json`), for the B8 decision: `running` is `busy` and matches a working screen, `idle` matches the idle composer, and a permission prompt is `waiting` / `waitingFor` `"permission prompt"` for the whole prompt (the screen reader agrees). `/model` and `/background` set `waiting` / `"dialog open"` while the screen reader says idle or unknown, so registry `waiting` is not the same fact as a permission prompt. A graceful `/exit` then Ctrl-D removes the file (`removed_on_exit` true, no files left). SIGKILL leaves it (`removed_on_exit` false).

```sh
cd harness-ci
cargo run -p harness-driver -- --scenario driver/scenarios/claude_sessions_registry.json
```

## A5: Cursor CLI spike (2026-10-24)

**Decision: Cursor does not join the mock/PTY rig for v0.1.** It goes through donated
fixtures (Phase C `fixture capture`), plus a budget-capped real run later if a key is
available. The IDE is out of scope for CI anyway. The A6 lane still installs pinned and latest
`cursor-agent` and runs its offline fixture suite; it does not claim a live chat.

Tested on Linux with `cursor-agent` **2026.10.01-e373342**, a temp `HOME`, a dummy
`CURSOR_API_KEY`, and `--endpoint` / `CURSOR_API_ENDPOINT` pointed at a local request logger:

- The endpoint override is honoured, but it is not a model API endpoint. The CLI first
  `POST`s `/auth/exchange_user_api_key` and expects `accessToken`/`refreshToken`.
- It then uses Cursor's own Connect/protobuf services:
  `aiserver.v1.DashboardService/*`, `ServerConfigService/GetServerConfig`,
  `AiService/AvailableModels`, `GetUsableModels`, `GetDefaultModelForCli`, and analytics.
  A hand-made token plus a model catalogue got it past login and model selection.
- The actual chat is a streaming Cursor RPC (HTTP/2/gRPC-style), not Anthropic
  `/v1/messages` or OpenAI `/v1/responses`. Against an HTTP/1.1 mock it ended in
  `ERR_HTTP2_ERROR` / `[internal] Protocol error`.

Joining the rig would mean implementing Cursor's auth exchange, protobuf model catalogue and
streaming chat protocol, not extending the existing mock by a scenario. That is not v0.1
work. The library still reads Cursor transcripts and, through the `sqlite` feature, a
snapshot of the IDE's `state.vscdb`; the missing piece is the live approval-waiting capture
(A5 #3), which needs Lee's Mac and is written up in
[`docs/formats/cursor.md`](../docs/formats/cursor.md#needs-lee-the-live-approval-wait-a5-3).

## A9: Claude Desktop adapter + Jev computer-use spike (2026-10-25, adapter #78)

**Decision: Claude Desktop's local agent mode is read by a ported-unverified adapter; the
Jev driver is now a local Docker + Xvfb implementation, not a harness lane.** There is still
no completed Desktop scenario, Desktop↔mock handshake or GHA job. The discovery/adapter
evidence is in [`docs/formats/claude-desktop.md`](../docs/formats/claude-desktop.md); the
container, driver and current gap checklist are in
[`desktop-xvfb/README.md`](desktop-xvfb/README.md) and
[`claude-desktop/README.md`](claude-desktop/README.md).

- Linux Desktop (beta) exists as `claude-desktop` on Ubuntu 22.04+/Debian 12+, but Anthropic
  Computer Use is not available on Linux, and Cowork needs KVM/QEMU. The reference
  `typesafe-computer-use` clicker is macOS-first, so Linux needs a separate AT-SPI observe/act
  adapter rather than a drop-in driver.
- A mock-backed Desktop is plausible only through Claude Desktop's third-party gateway mode:
  `inferenceProvider: gateway`, an Anthropic Messages `POST /v1/messages` gateway with
  streaming and tool use, and the Linux managed-settings file at
  `/etc/claude-desktop/managed-settings.json`. A later live run did reach the mock with
  Desktop's `"."` health probe; the say-hello turn still has not.
- Desktop Code-tab transcripts are Claude Code transcripts under `~/.claude/projects/` and
  stay on that adapter. Cowork/local-agent-mode sessions are separate
  `local_<id>/audit.jsonl` files below app-data `local-agent-mode-sessions/`; the
  `claude_desktop` adapter reads them with their `local_<id>.json` sidecar. They must not be
  treated as the same source. The fixtures are handmade, so the matrix is
  **ported-unverified**, not donated/CI-verified.
- Jev's default auth path is **OpenRouter**, billed to the existing OpenRouter credits:
  `OPENROUTER_API_KEY`, model `typesafe/jev-1.14` / `~typesafe/jev-latest`, Decisions API
  `POST https://openrouter.ai/api/alpha/decisions`, or the TypeSafe SDK at base URL
  `https://openrouter.ai/api`. No separate TypeSafe console key is required.
- Code that still reads `TYPESAFE_*` must set `TYPESAFE_API_KEY="$OPENROUTER_API_KEY"` and
  `TYPESAFE_BASE_URL=https://openrouter.ai/api` together. The reference
  `typesafe-computer-use` only documents `TYPESAFE_API_KEY`, but its bare `TypeSafeClient()`
  inherits the base URL from `typesafe-sdk` 0.6.0; that handoff is source-inspected, not
  live-verified.
- The Xvfb loop uses Jev only to choose a typed action; our code observes and acts, and
  `agenttail` scores the disk artifacts. No VLM is used as the scorer. One live OpenRouter
  call was made from the Multibeast box (see A10); CI still must not spend those credits,
  and no scored Desktop turn exists.

Do not schedule a Desktop GHA job until a real install has produced a redacted artifact and
a live Jev-driven turn has completed. Billing for this month is already expended.

## A10: Desktop Xvfb stopgap (2026-10-25, issue #80)

**Decision: the current local Desktop verify path is Docker + Xvfb on the Multibeast box, not
a Mac runner and not paid GHA minutes.** The implementation is
[`desktop-xvfb/`](desktop-xvfb/README.md). It is a stopgap for the observe/Jev/act loop, not a
replacement for a future GHA Desktop regression lane.

- The Ubuntu 24.04 image installs Anthropic's Linux beta package (pinned
  `3.6556.2` in the Dockerfile). Claude Desktop launches under Xvfb; the standard sign-in
  screen and the 3P gateway welcome screen are visible through AT-SPI.
- `desktop_driver.py` observes AT-SPI candidates, asks **Jev via OpenRouter only**
  (`typesafe/jev-1.14`) for a typed choice, performs the click/type itself, and scores
  `agenttail ls` output from the Desktop audit log and/or Code-tab transcripts. It refuses to
  run without `OPENROUTER_API_KEY`; there is no writer LLM or other chooser fallback for
  Desktop. `ANTHROPIC_API_KEY` and `CLICKER_WRITER_*` are scrubbed from the child environment.
- The committed thin scenario is `standin_plain_qa`: a GTK stand-in window writes a
  Desktop-shaped `audit.jsonl`, and the scripted chooser selects its prompt field and Send
  button. It proves the container/AT-SPI/action/scoring plumbing offline. It is explicitly
  **not** Desktop verification; the stand-in writes its own artifact.
- Local evidence on Multibeast: the stand-in run saw `idle -> working/thinking -> done`;
  the standard and 3P Desktop probes found the real windows and their candidate controls.
  A later live Jev run (`typesafe/jev-1.14`, confidence `0.25`, host networking) reached
  the Chat UI and one Desktop→mock `POST /v1/messages`, then failed: the body was the
  `"."` / `max_tokens: 1` gateway probe, which `plain_qa` rejected, and AT-SPI listed
  only push buttons so the prompt was never typed. `agenttail` stayed at
  `idle_no_session`. A re-run after that fix confirmed the probe allowlist, and
  Desktop `3.6556.2` refused to start with `--remote-debugging-port`. With the
  switch off, AT-SPI was still push buttons plus `document web`, so the prompt
  was not typed. The driver now clicks that document with `xdotool` and presses
  Enter; the default launch does not pass the rejected switch. See
  [`desktop-xvfb/README.md`](desktop-xvfb/README.md) for the probe allowlist, the
  `../` mock path, the document-web type path, and the `--iptables=false`
  bridge-DNS note (`AGENTTAIL_DOCKER_NETWORK=host`). Neither live run is a scored
  turn, and neither changed the support matrix.
- Cowork is out of scope: the compose file does not pass `/dev/kvm`, and the box lacks the
  required ~8 GB RAM / ~25 GB disk headroom for the Linux Cowork VM. Chromium GPU-process
  warnings and a missing FUSE document portal are known Xvfb noise; they did not stop the
  launch probes.

`.github/workflows/desktop-xvfb.yml` is committed as a `workflow_dispatch`-only stub with no
push, PR or schedule trigger. It has not run. Do not claim CI-verified Desktop UI until a real
redacted Desktop artifact and a green container run exist; the support matrix stays
**ported-unverified**.

## OpenCode: real-harness spike and scenarios (2026-10-25)

OpenCode **1.19.4** (`opencode-ai` on npm) runs fully offline through the same PTY driver.
It is not a JSONL transcript harness: sessions live in
`$XDG_DATA_HOME/opencode/opencode.db` (`~/.local/share/opencode/opencode.db` by default), with
the legacy `session`/`message`/`part` tables in use even though the v2 `session_message` table
also exists. `part` rows carry a tool call's mutable `state.status` (`pending`, `running`,
`completed`, `error`), and a `step-finish` part carries the finish reason. Rows are rewritten
in place, so the driver reads the database snapshot and rebuilds the session; it does not tail
an append offset. See [`docs/formats/opencode.md`](../docs/formats/opencode.md).

### Pointing it at the mock

The driver gives OpenCode a temporary home and an inline provider config:

```sh
HOME="$TMP/home" \
XDG_CONFIG_HOME="$TMP/home/.config" XDG_DATA_HOME="$TMP/home/.local/share" \
XDG_STATE_HOME="$TMP/home/.local/state" XDG_CACHE_HOME="$TMP/home/.cache" \
OPENCODE_CONFIG_CONTENT='{"provider":{"test":{...,"npm":"@ai-sdk/openai-compatible",
  "options":{"baseURL":"http://127.0.0.1:<port>/v1"}}}}' \
OPENCODE_DISABLE_PROJECT_CONFIG=1 OPENCODE_PURE=1 OPENCODE_DISABLE_AUTOUPDATE=1 \
OPENCODE_DISABLE_AUTOCOMPACT=1 OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_AUTH_CONTENT='{}' \
opencode run --format json --auto -m test/test-model run the tool
```

`--auto` approves tools that are not explicitly denied. The driver additionally allows
`bash`/`edit` in the provider config so the non-interactive tool scenario cannot block on a
prompt. Pass prompt words as separate argv entries: `opencode run "run the tool"` stores the
surrounding quotes in the database, while `opencode run run the tool` does not.

The title request is a separate `/v1/chat/completions` call; the mock answers it with the
scenario `title` and does not consume a step. A pending approval is not a row at all
(`permission.asked` is ephemeral), so the library can only infer `AwaitingApproval` from a
pending/running tool after the 20 s stall threshold.

### Scenarios

| scenario | what it checks |
|---|---|
| `opencode_plain_qa` | one text turn; the SQLite turn ends `stop`, library firm `done` |
| `opencode_tool` | a bash `sleep 2` tool call is sampled as firm `running_tool`, then the answer after its result ends firm `done` |

The three committed fixtures under `fixtures/opencode/1.19.4/linux/` are exported, redacted
database rows: a complete text turn, a completed tool turn, and a live snapshot while the tool
was running (including the inferred approval checkpoint). The driver writes the same row
stream to each run's `transcripts/<session-id>.jsonl`, so scoring and survey see it.

### Matrix: pinned + latest

The `opencode` job installs a pinned **1.19.4** and npm **latest** on Linux and macOS, checks
the reported `opencode --version`, and runs both scenarios. The pinned job is the
reproduction; the latest job is deliberately red. A new release that changes the database
shape or turn vocabulary fails the latest job with its run artifacts, rather than being
silently ignored. A pinned failure means this repository regressed; a latest-only failure is
upstream drift and needs a new exported fixture + adapter update.

```sh
cd harness-ci
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -p harness-driver -- --scenario driver/scenarios/opencode_plain_qa.json
cargo run -p harness-driver -- --scenario driver/scenarios/opencode_tool.json
```

## A6: GitHub Actions, version matrix and nightly drift

[`.github/workflows/harness-ci.yml`](../.github/workflows/harness-ci.yml) builds this workspace
explicitly. The root `cargo test` still does not build it.

- **Pinned + latest:** users autoupdate constantly, so the scheduled/PR lanes always install
  the **latest** release as well as the pinned known-good releases used for reproduction. They
  are separate matrix entries, not an either/or choice: a pinned job can replay an old failure,
  while the latest job is the alarm that an upstream update broke a scenario. A latest install,
  version check, launch or scenario failure fails the job loudly (with artifacts) rather than
  being skipped.
- **PR/push:** one `unit` job runs `cargo fmt --check`, `cargo clippy -D warnings`, the driver and
  mock-server tests, and the Python survey/matrix tests. The `harness` matrix runs Claude Code
  **2.1.281**, **2.1.310** and npm **latest**, each on **ubuntu-latest** and **macos-latest**.
  The `pi` matrix runs pinned Pi **1.4.0** and npm **latest** on both OSes with
  `ci/scenarios-pi.txt`; the `codex` matrix runs pinned Codex **0.160.1** and npm **latest**
  with `ci/scenarios-codex.txt`; the `opencode` matrix runs pinned OpenCode **1.19.4** and npm
  **latest** with `ci/scenarios-opencode.txt`; the `gemini` matrix runs pinned Gemini CLI
  **0.65.0** and npm **latest** with `ci/scenarios-gemini.txt`; the `copilot` matrix runs
  pinned Copilot CLI **1.0.103** and npm **latest** with `ci/scenarios-copilot.txt`; and the
  `cursor` matrix runs pinned Cursor Agent **2026.10.01-e373342** and the latest build. Every
  lane runs on both **ubuntu-latest** and **macos-latest**. Every job installs exactly the
  version named in its matrix entry into `$RUNNER_TEMP`, checks the binary's reported version,
  builds `harness-driver --release`, and runs its scenarios sequentially. A hard scoring
  failure fails the job; run artifacts are uploaded even when a scenario fails.
- **Cursor:** Cursor needs its own backend for a chat, so it still cannot join the mock/PTY rig
  (A5). Its pinned + latest lane installs the versioned official download, proves `cursor-agent`
  reports that version and renders `--help` with networking blocked, and runs the offline
  transcript/status fixture suite (including Cursor). A backend-gated live chat is not claimed.
  The install and launch failure modes are loud, not soft-skipped.
- **Offline:** `ci/offline-run.sh` wraps each driver in `sandbox-exec -f offline.sb` on macOS and
  in a fresh network namespace with loopback up on Linux. CI sets
  `HARNESS_CI_REQUIRE_OFFLINE=1`, so a host that cannot enforce either one fails instead of
  silently running with egress. The Claude/Codex/Pi/OpenCode/Gemini/Copilot jobs only ever
  receive the driver's dummy keys; the Cursor smoke uses a temp `HOME`. No repository secret is
  configured or read.
- **Version resolution:** `ci/matrix.py` owns the pinned versions and builds the pinned + latest
  matrix for every lane. `ci/cursor_version.py` reads the versioned download URL from Cursor's
  official installer; it does not execute the script. npm latest is resolved with `npm view`.
  An empty, ambiguous or unexpectedly formatted version fails the prepare job, so dependent
  lanes do not run with a guessed version.
- **Nightly:** the same workflow runs on the `43 3 * * *` schedule (and by hand with
  `open_drift_pr`). It installs each lane's `latest`, runs the matrix, and surveys the Claude and
  Codex transcripts with `ci/survey.py`. `ci/nightly.py` compares the Linux Claude shape to
  `drift/baseline/claude-code/<version>.json`. When a new version adds/removes record types,
  field paths, content-block types or enum values, it writes `drift/reports/` plus redacted
  fixtures under `fixtures/claude-code/<version>/<os>/ci-<scenario>/`, and the workflow opens a
  PR. Ids, timestamps and free text are not surveyed, so value-only changes do not open a PR.
  A committed baseline that changes under the same version is reported as a regression and does
  not get silently rewritten. The workflow builds `agenttail-cli`, passes it as
  `AGENTTAIL_BIN` so the capture uses the same Rust redactor as `fixture redact`, and runs
  `agenttail fixture scan fixtures` before the PR step. Latest Codex/Pi/OpenCode artifacts
  are uploaded even when they fail; Cursor is fixture-backed until the backend-gated live path
  exists.
- **Proof:** `drift/reports/claude-code-2.1.281-to-2.1.310.md` is the report from running the
  same suite on both versions locally. It catches the new `api-request*` records, `turnPosition`,
  `renderedRole`/`renderedBesideToolResult`, request fields and permission-decision fields.
- **Deferred:** Cursor's live mock/PTY chat remains out until its auth/protobuf/streaming backend
  can be mocked. The first-read `/usr/bin/time` budget is no longer deferred; it is enforced by
  the `first-read` job and the local script in A8.

## A8: first-read performance budget (2026-10-25)

**Decision (Multibeast, Oct 24):** enforce the `first_read` budget in the local verify gate now;
the GitHub Actions job is committed ready for next month's billing.

[`ci/first-read-budget.sh`](ci/first-read-budget.sh) builds `agenttail --bench first_read`, runs
the emitted binary on a synthetic 500 MB Claude Code transcript under GNU `/usr/bin/time -v`,
and exits non-zero when either budget is exceeded:

| metric | budget | measured on Multibeast's Linux box (2026-10-25, rustc 1.88.0) |
|---|---|---|
| Lean first read | < 3.0 s | 0.76-1.95 s |
| Peak RSS (whole bench process) | < 50 MB | 9.4-9.9 MB |
| Full parse | reported, not budgeted | 1.29-3.75 s |

```sh
PATH="$HOME/.cargo/bin:$PATH" harness-ci/ci/first-read-budget.sh
```

`cargo bench --no-run` only builds the bench; the script then invokes the emitted executable
directly. `/usr/bin/time -v cargo bench` reports cargo's own peak RSS (38.7 MB here), not the
bench child's, so wrapping cargo would make the memory gate meaningless. The bench generates
its transcript in memory, so there is no large scratch file. The script keeps its small build
and run logs under `target/first-read-budget.*` and removes them on exit.

The workflow's `first-read` job runs the same script on `ubuntu-latest`; it is Linux-only
because the gate needs GNU time. It is committed ready but was not required to pass while this
month's Actions billing was exhausted.

## A7: Pi real-harness spike and scenarios (2026-10-25)

Pi **1.4.0** (npm latest at the time) runs fully offline against the existing mock through a
custom Anthropic-compatible provider. `/v1/messages` needs the provider base URL **without**
`/v1` (`http://127.0.0.1:<port>`); Pi appends the API path itself. A temp agent directory gets:

```json
{
  "providers": {
    "mock": {
      "name": "Mock",
      "baseUrl": "http://127.0.0.1:<port>",
      "api": "anthropic-messages",
      "apiKey": "dummy-key",
      "models": [{ "id": "mock-model", "name": "Mock Model", "reasoning": false,
                  "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                  "contextWindow": 128000, "maxTokens": 8192 }]
    }
  }
}
```

`settings.json` selects `mock`/`mock-model`. The driver sets `HOME`, `PI_CODING_AGENT_DIR`,
`PI_CODING_AGENT_SESSION_DIR`, `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1` and
`PI_TELEMETRY=0`, and runs:

```sh
pi --mode json --model mock/mock-model --no-extensions --no-skills --no-prompt-templates \
   --no-themes --no-context-files --no-mcp '<prompt>'
```

Unlike Claude Code and Codex, Pi's driver path is **JSON mode, not PTY screen scored**: it
tails the real session file with `agenttail` and checks firm activities/events. The screen is
recorded as `unknown`, so Pi scenarios assert activities without screen strings. The real
`pi --version` is `1.4.0`. The `pi` GHA matrix installs pinned **1.4.0** and npm **latest**;
the latest job is not soft-skipped, so a new Pi release fails loudly when the mocked turns,
transcript shape or status rules break. No repository secret is used; the CLI only sees the driver's dummy key.

| scenario | what it checks |
|---|---|
| `pi_plain_qa` | One prompt, one text answer, `stopReason: stop`; firm `done` |
| `pi_tool_call` | A `bash` call runs for 1.5 s; firm `running_tool` while pending, then firm `done` |
| `pi_api_error` | Retries disabled, HTTP 401; firm `errored` with the harness's error |

Pi has **no approval prompt** and no built-in ask-user tool, so there is no approval/ask
scenario to capture. A pending Pi tool is firm `RunningTool`, never inferred
`AwaitingApproval`; this is asserted in the fixture suite and the tool-call scenario.

```sh
cd harness-ci
cargo run -p harness-driver -- --scenario driver/scenarios/pi_plain_qa.json
```

## A4: Codex real-harness spike and scenarios (2026-10-24)

Codex CLI **0.160.1** (the version that wrote the committed rollout fixtures) runs fully
offline against the mock. npm latest at spike time was 0.164.0; the harness matrix keeps
0.160.1 as the pinned reproduction lane because the scenario shapes and `sessions/YYYY/MM/DD`
rollout format are the ones the repository already reads, and runs npm latest beside it so a
new Codex release that breaks those turns fails the latest job loudly.

### Pointing it at the mock

Install into a temp prefix and give it a temp `CODEX_HOME`:

```sh
npm install --prefix "$TMP/codex" --no-save @openai/codex@0.160.1
HOME="$TMP/home" CODEX_HOME="$TMP/home/.codex" "$TMP/codex/node_modules/.bin/codex" --no-daemon
```

`$CODEX_HOME/config.toml` uses a custom provider (the current docs' `model_providers` /
`base_url` / `env_key` / `wire_api` fields):

```toml
model = "gpt-5.4"
model_provider = "mock"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[tui]
disable_paste_burst = true
animations = false

[model_providers.mock]
name = "Mock"
base_url = "http://127.0.0.1:<mock-port>/v1"
env_key = "MOCK_API_KEY"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
```

The driver writes that file, supplies `MOCK_API_KEY=dummy-key`, and starts Codex with
`--no-daemon`. Without `--no-daemon`, 0.160.1 starts a persistent `app-server --managed-daemon`
outside the driver's process group and the temp `CODEX_HOME` is held open after the run.
`--no-daemon` keeps the whole harness inside the PTY driver's lifetime.

Codex honours an absolute `CODEX_HOME`: it is the `.codex` home, and rollouts go to
`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`, exactly as `Tracker::from_env` assumes.
A relative `CODEX_HOME` is resolved by the CLI against its current working directory; the
tracker deliberately ignores relative values (B8), so CI always sets an absolute temp path.
`HOME` is still pointed at the temp home, and no real OpenAI credential is read or accepted.

### What the live TUI needed

- Interactive Codex renders `Ask Codex to do anything` when idle, `Working (… · esc to interrupt)`
  while a turn runs, `Would you like to run the following command?` for an approval, and
  `Question 1/1 (… unanswered)` / `enter to submit answer` for `request_user_input`.
- Plan mode is entered with `/plan`; `request_user_input` is only offered in that mode.
- Codex's paste-burst detection otherwise swallows the immediate Enter after a scripted prompt.
  `tui.disable_paste_burst = true` makes the driver's type-then-Enter deterministic.
- Codex asks the model for a one-line thread title on a separate `/v1/responses` request.
  The mock answers that request with a fixed title and does not consume a scenario step, just
  like Claude's title request.
- In 0.160.1 the requested title did **not** create `session_index.jsonl`; the live CLI keeps
  thread state in its SQLite files. The tracker's `session_index.jsonl` reader (B7) remains
  covered by the existing fixture tests, but there is no live index for this version to assert.

### Approval events: none are written

A pending approval is only a `response_item/function_call` with no matching
`function_call_output`. The rollout's `turn_context` records `approval_policy`, and no
`exec_approval_request`, `apply_patch_approval_request`, `request_permissions`,
`request_user_input`, `elicitation_request`, `exec_command_begin/end` or `error` event was
written in the on-request, never, ask-user, parallel, abort or error runs. The scenarios assert
`codex_no_approval_events: true` for the pending captures. This confirms the prior-art claim for
0.160.1: a pending approval or ask-user is visible only as a tool call with no output.

`approval_policy = "untrusted"` cannot be exercised on 0.160.1: the CLI exits with
`approval_policy = "untrusted" is no longer supported`, and `-a` only accepts `on-request` and
`never`. The current Codex docs mark `untrusted` retired. The scenario therefore leaves an
on-request escalation pending; the adapter still parses `untrusted` for older rollouts.

### Scenario matrix

| scenario | what it checks |
|---|---|
| `codex_plain_qa` | One text turn; firm `done`, the rollout contains `session_meta`, `turn_context`, `task_started`, `token_count`, `task_complete` |
| `codex_token_count` | Two turns; `token_count.total_token_usage` is a running total (`5` then `12` output tokens) |
| `codex_approval_pending` | `on-request` + `require_escalated` leaves the command pending on screen; inferred `awaiting_approval` after the 20 s stall, no approval event on disk |
| `codex_approval_never` | `-a never` keeps a 60 s command `running_tool`/firm on the working screen, never `awaiting_approval` |
| `codex_request_user_input` | `/plan`, a `request_user_input` question, firm `awaiting_answer` on the ask screen, then `done` after Enter |
| `codex_parallel_tool_calls` | Three `function_call`s before any output; the mock requires the third call id's result before the text turn; `max_pending_function_calls` is 3 |
| `codex_turn_aborted` | Esc while delayed writes `turn_aborted.reason = "interrupted"`; firm `interrupted` on the settled composer |
| `codex_task_error` | Terminal HTTP 500 writes `task_complete.error` as an object (`{"message": …}`); firm `errored` on the error banner |
| `codex_resume` | `codex resume <id>` appends the second prompt to the same rollout (one file, two prompts); no `continued-in` record |

```sh
cd harness-ci
export PATH="$HOME/.cargo/bin:$PATH"
cargo run -p harness-driver -- --scenario driver/scenarios/codex_plain_qa.json
```

The live 0.160.1 runs also confirmed `turn_aborted.reason`, `task_complete.error` (an object,
not a string), resume appending to the same day folder, and no `.jsonl.zst` history from the
live CLI. Older-day-folder resume and compressed history remain fixture-backed; they need a
session old enough to have moved folders or been compressed.

Green-path local check (the PTY matrix itself is also run by GHA):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cd harness-ci
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test -p harness-driver -p mock-server --offline
python3 -m unittest discover -s ci -p 'test_*.py' -v

# One real version (install latest Claude Code first):
cargo run -p harness-driver -- --scenario driver/scenarios/claude_plain_qa.json
```

To reproduce the drift proof with two npm versions:

```sh
npm install --prefix /tmp/cc-281 @anthropic-ai/claude-code@2.1.281
PATH="/tmp/cc-281/node_modules/.bin:$PATH" cargo run -p harness-driver -- \
  --scenario driver/scenarios/claude_plain_qa.json --out /tmp/run-281
python3 ci/survey.py survey --runs /tmp/run-281 --version 2.1.281 --os linux \
  --out /tmp/281.json
python3 ci/survey.py compare --baseline drift/baseline/claude-code/2.1.281.json \
  --current /tmp/281.json --report /tmp/drift.md
```

- The detector and scorer unit tests do not spawn Claude Code. A real PTY run of the four green
  scenarios is still the check that the new disagreement list stays empty on those timelines.

## Gemini CLI: real harness and scenarios (2026-10-25)

Gemini CLI **0.65.0** runs fully offline through a one-shot JSON mode, not a PTY. The
driver gives it a temp home, selects the API-key auth path in `.gemini/settings.json`, and
points `GOOGLE_GEMINI_BASE_URL` at the in-process mock:

```sh
GEMINI_CLI_HOME="$TMP/home" GEMINI_API_KEY=dummy \
GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:<port> \
CI=true GEMINI_CLI_TRUST_WORKSPACE=true GEMINI_TELEMETRY_ENABLED=false \
gemini -p 'say hello' --output-format json --approval-mode yolo --skip-trust
```

The mock implements `:generateContent` and `:streamGenerateContent?alt=sse`. 0.65.0 sends a
routing-classifier request first (a `complexity_score` JSON schema); the mock answers it
without consuming a scenario step. Scenarios:

| scenario | what it checks |
|---|---|
| `gemini_plain_qa` | one text turn; the journal ends firm `done` |
| `gemini_tool` | a `run_shell_command` call; the call is written only after completion, so there is no sampled `running_tool` or `awaiting_approval`, then firm `done` |

The committed fixtures under `fixtures/gemini/0.65.0/linux/` are real CLI journals captured
through the mock and redacted. See [`docs/formats/gemini.md`](../docs/formats/gemini.md).

## GitHub Copilot CLI: real harness and scenarios (2026-10-25)

Copilot CLI **1.0.103** runs offline through its BYOK provider, so GitHub sign-in is not
needed:

```sh
HOME="$TMP/home" COPILOT_HOME="$TMP/home/.copilot" COPILOT_OFFLINE=true \
COPILOT_PROVIDER_BASE_URL=http://127.0.0.1:<port>/v1 \
COPILOT_PROVIDER_TYPE=openai COPILOT_PROVIDER_API_KEY=dummy \
COPILOT_PROVIDER_MODEL_ID=test-model COPILOT_MODEL=test-model CI=true \
copilot -p 'say hello' --allow-all-tools --trust-folder --output-format json
```

The mock's OpenAI Chat Completions route is enough. Scenarios:

| scenario | what it checks |
|---|---|
| `copilot_plain_qa` | one text turn; firm `done` |
| `copilot_tool` | a two-second bash call sampled as firm `running_tool`, then firm `done` |
| `copilot_permission_denied` | prompt mode without `--allow-all-tools`; `permission.requested`, unattended denial, failed tool result, then firm `done` |

The permission event is persisted, so the adapter's `awaiting_approval` is firm when a
request is pending; the prompt-mode denial completes it too quickly for a reliable screen
sample, so the committed `permission-denied` status fixture checks the checkpoint after the
request line. See [`docs/formats/copilot.md`](../docs/formats/copilot.md).

Both lanes have pinned + latest entries in `.github/workflows/harness-ci.yml`; the latest
job is not soft-skipped. The local green runs in this change used 0.65.0 and 1.0.103 on
Linux. GHA was not run for this PR, so CI verification is not claimed.
