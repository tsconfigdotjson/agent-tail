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
  "run_for_ms": 14000
}
```

Hard failures so far: `AwaitingApproval` when the screen shows no permission prompt,
`AwaitingApproval` in a `no_approval` scenario, and a real prompt left un-inferred past the
stall threshold. The driver exits non-zero on any hard failure.

### What the rig has already found

- **Permission prompts are visible only on screen.** In default mode, a `touch` command
  showed the real prompt; no transcript record is written while it is up. The library says
  `RunningTool` until the 20 s stall threshold, then `AwaitingApproval` (inferred), exactly
  as designed. The captured screen is in the detector tests.
- **Bypass mode currently fails the score, as expected before B1.** With
  `--dangerously-skip-permissions`, a 60 s `sleep` stays `RunningTool` on screen while the
  library turns `AwaitingApproval` after 20 s. That is the headline bug this rig is here to
  prove fixed.
- **Model latency matters.** With an instant mock response, Claude Code can append the
  user prompt record *after* the assistant reply, which makes the reducer read `Thinking`
  after `Done`. Real model latency hides this; scenarios give the first token a 1 s delay so
  the transcript order matches reality. This is still worth a status-rule test (a prompt
  record older than the latest activity should not move the state backwards).
- **Onboarding, trust and bypass disclaimers** are all skipped by seeding `.claude.json`
  and `.claude/settings.json` (`skipDangerousModePermissionPrompt: true`). No key presses
  are needed before the prompt.

### Remaining A0/A2 work

- Codex CLI is not installed on this machine. Its `model_providers` / `CODEX_HOME` spike is
  still to do (issue A4).
- Cursor CLI may need Cursor's backend; its spike is A5.
- More screen detectors: ask-the-user, denial, interrupt. They need captured screens first.
- CI wiring and version matrix (A6) are not done yet.
