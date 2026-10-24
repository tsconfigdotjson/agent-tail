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

A scripted Anthropic Messages and OpenAI Responses server. It answers streaming and
non-streaming requests, logs every request, and returns a 500 with a readable explanation
when a request is not the next scripted step.

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
  (`delay_ms`), `slow` (`delay_ms`).
- `expect.user_also_contains` is a second user-text substring. `expect.not_user_contains`
  rejects a step when the user's text has that substring (tool results are not user text).
  System prompt text is part of `contains`, not of `user_contains`.
- `repeat` expands one step into that many identical steps (cap 30), for a fault the CLI retries.
- `dispatch` is `sequential` (default) or `first_match`. `first_match` answers the first unused
  step whose `expect` matches, so a subagent's model request can arrive before the parent's
  next turn. A request that matches nothing is still a 500.
- Claude Code's separate session-title request is answered with a fixed title and does not
  consume a step. Without that, the title request steals the scripted first reply.

Codex's OpenAI Responses subset is implemented (`/v1/responses`), but its CLI spike is A4.

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
- **Model latency matters.** With an instant mock response, Claude Code can append the
  user prompt record *after* the assistant reply, which makes the reducer read `Thinking`
  after `Done`. Real model latency hides this; scenarios give the first token a 1 s delay so
  the transcript order matches reality. This is still worth a status-rule test (a prompt
  record older than the latest activity should not move the state backwards).
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

- Codex CLI is not installed on this machine. Its `model_providers` / `CODEX_HOME` spike is
  still to do (issue A4). The Codex and Cursor screen readers return `unknown` until those
  spikes capture real panes; they do not reuse Claude's strings.
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
| `claude_api_error_529` | HTTP 529, repeated for retries. Firm `errored` at the end |
| `claude_api_error_stream` | The stream closes after `message_start`, repeated for retries. Firm `errored` at the end |

`turn_duration` still ends a normal turn. It does not replace an API-error row: that row stays `Errored` when the duration record is written after it, including in the same read. A later real assistant `end_turn` clears the error. Keys are not sent until the prompt has been typed, and the key clock restarts then, so a startup spinner is not the Esc target. `keys[].occurrence` of `0` means any visit to that screen, so a short flicker does not consume the Esc meant for the long working streak.

The background scenario does not link `async_launched` or `.meta.json` into the library's helper state. `BackgroundWork` comes from `pendingBackgroundAgentCount` on the parent transcript. `Done` while the screen still says background agents are running remains a hard failure.

```sh
cd harness-ci
cargo run -p harness-driver -- --scenario driver/scenarios/claude_background_agent.json
```

- CI wiring and version matrix (A6) are not done yet.
- The detector and scorer unit tests do not spawn Claude Code. A real PTY run of the four green
  scenarios is still the check that the new disagreement list stays empty on those timelines.
