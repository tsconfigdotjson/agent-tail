# Claude Desktop: local agent mode adapter + Jev via OpenRouter (driver spike)

There are two independent stories here. **Reading Desktop sessions is now product code:** a
discovery spec, a `claude_desktop` adapter, sidecar handling, fixtures and reduce tests.
**Driving the Desktop with Jev is still a documentation spike:** no live Jev call, key or
Desktop install was used for it.

Evidence is labelled **documented** (vendor docs), **source-inspected** (public repository or
pinned package source, not a live call), **prior art** (another project's code or survey),
and **TBD** (needs a real install or key). The adapter's fixtures are **handmade** from the
prior-art record shape, not a real captured Desktop session. Nothing here is CI-verified.

Support-matrix status: **ported-unverified**. Claude Desktop now has a discovery spec,
adapter and fixtures, so it is off the spike-only list; it has no real artifact or CI lane, so
it is not donated-verified or CI-verified either.

Scope: issue #78 under parent #33, on top of the #74/#76 driver spike. Code-tab transcripts
stay on the Claude Code adapter; this document explains the split rather than duplicating
that adapter.

## Two surfaces, two readers

Do not conflate the Code tab with Cowork / Chat / local agent mode:

| Surface | Artifacts | Read by `agenttail` |
|---|---|---|
| **Code tab** | Desktop metadata under app-data `claude-code-sessions/`; the transcript is a Claude Code transcript at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` | The existing **Claude Code** adapter, once the transcript appears there. Desktop metadata is not read. See [`claude-code.md`](claude-code.md). |
| **Cowork / agent / Chat (local agent mode)** | Self-contained under app-data `local-agent-mode-sessions/…/local_<id>.json` plus `local_<id>/audit.jsonl` (HMAC-chained) | The new **`claude_desktop`** adapter and discovery spec. Dispatch sessions at `<org>/agent/local_ditto_<id>/audit.jsonl` are read as their own sessions. |

Official data-storage page:
<https://claude.com/docs/third-party/claude-desktop/data-storage>.

A Code-tab transcript and a local-agent-mode `audit.jsonl` are different artifacts. The Code
tab must not be re-read through `claude_desktop`, and local agent mode must not be sent to the
Claude Code project walker.

## App-data roots

The discovery file is [`spec/discovery/claude-desktop.toml`](../../spec/discovery/claude-desktop.toml).
`Tracker` scans every platform root for the OS it runs on. The Linux consumer path is the
single `roots.default` for tooling that shows one root; it is not a claim that it is the only
root.

| Build | macOS | Linux | Windows |
|---|---|---|---|
| Consumer | `~/Library/Application Support/Claude/local-agent-mode-sessions/` | `~/.config/Claude/local-agent-mode-sessions/` | `%APPDATA%\Claude\local-agent-mode-sessions\` (prior art; TBD) |
| 3P / gateway (documented) | `~/Library/Application Support/Claude-3p/local-agent-mode-sessions/` | `~/.config/Claude-3p/local-agent-mode-sessions/` | `%LOCALAPPDATA%\Claude-3p\local-agent-mode-sessions\` |

The 3P roots are the vendor-documented ones. The consumer rows were seen on macOS by the ADR
survey, not on a 3P install; Linux and Windows consumer paths remain **TBD until verified on a
real install**. `%APPDATA%` and `%LOCALAPPDATA%` can be redirected, but the spec's Windows rows
are home-relative defaults (`AppData/Roaming`, `AppData/Local`), so a redirected profile is
not handled yet.

## Session layout

```text
<root>/<user>/<org>/local_<uuid>/audit.jsonl
<root>/<user>/<org>/local_<uuid>.json                 # sidecar metadata
<root>/<user>/<org>/agent/local_ditto_<uuid>/audit.jsonl
<root>/<user>/<org>/agent/local_ditto_<uuid>.json     # dispatch metadata
```

The discovery layout requires `audit.jsonl` inside a folder whose name starts with `local_`,
allows five folders below the root (so dispatch's extra `agent/` level fits), and walks no
other file as a session. The metadata file is `.json`, so it is not a second transcript; it
is read as a sidecar.

Dispatch sessions (`local_ditto_*`) are delegated background agents. They are **not** helpers
of an interactive session in this adapter: each has its own audit log and is reported as its
own session. Linking a dispatch run back to the interactive session that spawned it is not
implemented.

## Sidecar: `local_<id>.json`

The sidecar carries session facts the audit stream does not repeat. The fields this adapter
uses are:

| Field | Use |
|---|---|
| `sessionId`, `cliSessionId` | session identity |
| `cwd` | project name (last path component) |
| `title` | display title, source `other` |
| `model` | model-change event |
| `permissionMode` | permission mode when present |
| `lastActivityAt`, `createdAt` | session facts only; not used as message clocks |

Other fields seen in prior art (`sessionType`, `initialMessage`, `hostLoopMode`,
`memoryEnabled`, `skillsEnabled`, `pluginsEnabled`, `slashCommands`) are not consumed yet.
A missing or malformed sidecar does not fail the session: the audit log still parses with its
own `system/init` facts. The tracker reads the sidecar when it appears or changes, so a title
or cwd written after the first poll is picked up.

## What the adapter reads

`audit.jsonl` is the Claude Agent SDK's stream-json shape. The shared Claude record reader
(`crates/agenttail/src/adapters/claude.rs`) already knows the conversation records, so
`claude_desktop` reuses it rather than copying it. Desktop-specific handling:

| Record | Mapping |
|---|---|
| `system` / `subtype: init` | shared reader parses `cwd`, `session_id`, `permissionMode`; this adapter adds `model` as a model change and keeps the record as `meta` |
| `user` | prompt, or `tool_result` blocks (including the SDK's snake-case root `tool_use_result` payload) |
| `assistant` | text, reasoning, tool calls; `stop_reason: end_turn` is a firm turn end |
| `result` | turn end / errors / interruption, session `total_cost_usd`, and session-total usage; `terminal_reason: aborted_streaming`/`aborted_tools` is `interrupted` |
| `rate_limit_event`, `tool_use_summary` | known `meta`, never `unknown` |
| anything else | `unknown`, kept whole, never dropped |

The SDK message records carry `uuid` and `session_id` but **no timestamp** (confirmed in the
public `claude-agent-sdk-python` types). Status times therefore fall back to `audit.jsonl`'s
mtime, exactly as a Claude Code line without a timestamp does. A `timestamp` is used when one
is present; the fixtures do not invent one.

The sidecar's `lastActivityAt` is not used to move `since`/`last_at`: a metadata write can be
later than the last message, and making it the turn clock would turn a finished session back
into work.

## Status

The shared status rules apply unchanged:

- a prompt or tool result is `thinking`;
- an accepted `end_turn` / `result` is `done`;
- a `result` with `is_error`, `errors`, or an SDK error subtype is `errored`;
- an aborted `result` is `interrupted`;
- no writes for 15 minutes is `idle`;
- a pending tool is `running_tool` while the recorded mode cannot ask a human
  (`bypassPermissions`, `dontAsk`), and `awaiting_approval` after the shared 20 s stall
  **inference** when it can.

Unlike Copilot CLI, the audit stream has no persisted `permission.requested` /
`permission.completed` pair, so a pending tool cannot be firm `awaiting_approval` from a
permission record. The `tool-stall` fixture pins the honest result: firm `running_tool` at 0 s
and inferred `awaiting_approval` at 25 s. If a real audit ever contains an ask-user tool, it
is currently treated as an ordinary pending tool; the discovery spec deliberately keeps
`asks_user` empty rather than borrowing Claude Code's list without an artifact.

## Evidence: fixtures and tests

Under `fixtures/claude-desktop/handmade/any/`:

| Fixture | What it pins |
|---|---|
| `plain-qa` | init, prompt, assistant `end_turn`, result frame; working → done → idle after 15 min |
| `tool-turn` | pending `Bash` call is firm `running_tool`; result returns to thinking; result frame closes it done |
| `tool-stall` | default mode: pending call is firm running at 0 s and inferred `awaiting_approval` at 25 s; bypass mode is covered by a unit test |

They are **handmade**, synthesized from ADR's Claude Desktop parser and its contract test
(the only prior-art source that shows the record shape), not captured from a real Desktop
install. `expected/events.jsonl` and `expected/status.jsonl` are generated and checked by the
normal fixture tests; the adapter also has unit tests for the sidecar, error/abort results,
no-prompt modes and the shared record parser. `agenttail fixture scan` is clean.

What is still missing: a real redacted artifact, a Desktop↔mock handshake, a CI lane, a
desktop-drive run, and verification of the consumer/3P roots on macOS, Linux and Windows.

---

# Driver spike: Jev via OpenRouter

**Status: documentation spike only.** No live OpenRouter/Jev or TypeSafe request was made, no
key was used, and no Desktop install was launched or driven. The adapter above is independent
of this driver story and does not require Jev.

## What is being proposed

Jev is TypeSafe System One: a typed `choice` / `score` / `noul` decision over a supplied
state, not a clicker or typer. The driver observes the desktop, turns what it sees into
candidate actions, asks Jev to choose, and our code performs the chosen UI action. Jev never
clicks a pixel directly.

The loop is:

1. **Observe** — screenshot OCR plus the accessibility tree become a small set of stable,
   named candidate actions.
2. **Choose** — Jev returns a typed choice (or `noul` / a low score) over those candidates.
3. **Act** — our driver executes the chosen action through platform UI automation (the
   reference Mac `clicker`, or a Linux AT-SPI adapter).
4. **Score** — poll `agenttail`'s `state(now)` against the Code-tab transcript or the
   local-agent-mode `audit.jsonl` this document describes.
5. **Stop** — on a terminal state, `noul`, low confidence, the step limit, or a stall.

`agenttail` is the scorer, not Jev. There is deliberately **no VLM as scorer**; a
screenshots-only judge would not prove that the transcript state is correct.

Reference points:

- Jev on OpenRouter: <https://openrouter.ai/docs/guides/community/jev>
- TypeSafe SDK pointed at OpenRouter: <https://openrouter.ai/docs/guides/community/typesafe-sdk>
- Jev tutorial and Decisions API examples: <https://openrouter.ai/docs/guides/community/jev-tutorial>
- Decisions API reference: <https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-questions-and-answers-request>
- Reference Mac agent: <https://github.com/awlevin/typesafe-computer-use>

## Auth: OpenRouter by default

Jev is `typesafe/jev-1.14`, with `~typesafe/jev-latest` tracking the newest release.
OpenRouter exposes it on two surfaces, both authenticated with
`Authorization: Bearer $OPENROUTER_API_KEY`:

| Surface | Endpoint | Use it when |
|---|---|---|
| Decisions API | `POST https://openrouter.ai/api/alpha/decisions` | Calling from any language with plain HTTP, or through OpenRouter's Decisions SDKs |
| System One API | `POST https://openrouter.ai/api/v1/systemone` | Using the TypeSafe SDK with base URL `https://openrouter.ai/api` |

The official TypeSafe SDKs read `TYPESAFE_API_KEY` and `TYPESAFE_BASE_URL`, so **source-
inspected** code that already constructs `TypeSafeClient()` can point at OpenRouter without
code changes:

```sh
export OPENROUTER_API_KEY=…
export TYPESAFE_API_KEY="$OPENROUTER_API_KEY"
export TYPESAFE_BASE_URL=https://openrouter.ai/api
# optional: pin instead of the SDK default jev-latest
export TYPESAFE_DEFAULT_MODEL=typesafe/jev-1.14
```

The two `TYPESAFE_*` variables are an adapter for code that still reads them, not a second
account. Do not set `TYPESAFE_API_KEY` to the OpenRouter key while leaving the base URL at
`api.typesafe.ai`: the key and endpoint must travel as a pair. A TypeSafe console key is the
optional alternate: set `TYPESAFE_API_KEY` and leave `TYPESAFE_BASE_URL` unset.

## Does `typesafe-computer-use` work through OpenRouter?

**Source-inspected, not live-verified.** The reference project is **macOS-first** (macOS 14+),
needs Screen Recording and Accessibility permissions, and supports a dry run
(`uv run clicker "goal"`) and live driving (`--act`). Its README mentions only
`TYPESAFE_API_KEY`, but the agent constructs a bare `TypeSafeClient()` with no explicit
`base_url`, so the pinned Python SDK resolves `TYPESAFE_BASE_URL`; that should point the
classifier at OpenRouter. No `clicker` run was made.

The clicker's **writer** is separate: `type_text`, writer-proposed URLs and the final answer
still use `ANTHROPIC_API_KEY` unless `CLICKER_WRITER_*` points at another Anthropic or
OpenAI-compatible endpoint. Exporting `OPENROUTER_API_KEY` does not configure the writer, so a
classifier-only dry run can work while typing and final answers are disabled.

Windows is experimental, and although the repository discusses OSWorld/Linux sandboxes, the
`clicker` itself is not a drop-in Linux driver.

## Claude Desktop availability

**Documented:** Claude Desktop has a Linux beta for Ubuntu 22.04+ / Debian 12+ on x64 and
arm64, installed as the apt package `claude-desktop`:

- <https://support.claude.com/en/articles/10065433-install-claude-desktop>
- <https://code.claude.com/docs/en/desktop-linux>

Linux limitations that matter here:

- Anthropic **Computer Use** is **not available on Linux Desktop**. That is Anthropic's
  feature, not this Jev driver.
- Dictation is not available.
- Cowork needs KVM/QEMU, about 25 GB of disk and 8 GB of RAM.

So "Claude Desktop exists on Linux" only means the application installs there. It does not
mean Anthropic's computer-use feature works there, and the reference Jev `clicker` does not
become a Linux driver just because the app installs.

## Pointing Desktop at a mock or gateway

**Documented:** Claude Desktop in **third-party (3P) mode** supports
`inferenceProvider: gateway` with `inferenceGatewayBaseUrl` and an API key sent as Bearer or
`x-api-key`. The gateway must implement the Anthropic Messages API:

- `POST /v1/messages` with streaming and tool use;
- `GET /v1/models` is optional.

Linux managed settings live at `/etc/claude-desktop/managed-settings.json` (root-owned).

- Gateway: <https://claude.com/docs/third-party/claude-desktop/gateway>
- Overview: <https://claude.com/docs/third-party/claude-desktop/overview>

Implications for `harness-ci`:

- A mock-backed Desktop is **plausible only through 3P/gateway mode**, pointed at the
  existing mock or a thin Anthropic-Messages shim.
- The consumer claude.ai sign-in path does **not** expose an arbitrary base URL.
- The existing mock already answers streaming and non-streaming Anthropic Messages with
  `text`, `thinking` and `tool_use` blocks for the CLI, but **no Desktop↔mock handshake has
  been run**. Mock compatibility is therefore **not proven**.
- `/etc/claude-desktop/managed-settings.json` is root-owned, so a temp-`HOME` harness cannot
  isolate Desktop as easily as it isolates a CLI; headless keyring behaviour is **TBD**.

## macOS versus Linux + Xvfb

- `typesafe-computer-use` is macOS-first. A Linux path needs an observe/act adapter built on
  AT-SPI (and OCR where the tree is incomplete); Xvfb is follow-up work, not a drop-in change.
- Cowork on Linux needs KVM/QEMU. Nested CI VMs and Xvfb-only boxes often cannot provide it.
- GitHub Actions billing is expended this month, so this work adds **no new desktop GHA job**.

Recommendation, in order:

1. **Manual Mac spike** with `typesafe-computer-use` + Claude Desktop using Lee's OpenRouter
   key. Start with a dry run, then a short `--act` run.
2. **Linux + Xvfb** for the Desktop Code/Chat UI only after a Linux observe/act adapter
   exists. Expect to skip Cowork there if the box has no KVM.
3. **Gateway/mock path** if the immediate goal is only transcript scoring: prove 3P settings
   and the Desktop↔mock handshake without computer use.

## How to run once Lee's OpenRouter key is available

Preconditions: macOS 14+ (the reference clicker is macOS-first), Python 3.12+, `uv`, Claude
Desktop installed and signed in, and Screen Recording + Accessibility granted to the terminal.

First check the OpenRouter path without the clicker:

```sh
export OPENROUTER_API_KEY=…   # Lee's OpenRouter key; never commit it

curl --fail-with-body https://openrouter.ai/api/alpha/decisions \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  --data @- <<'JSON'
{
  "model": "typesafe/jev-1.14",
  "state": {"probe": "agenttail Jev smoke test"},
  "questions": {
    "ok": {
      "type": "noul",
      "instructions": "Is this request a smoke test?",
      "criteria": {
        "true": "The state says it is a smoke test.",
        "false": "It is a real task."
      }
    }
  }
}
JSON
```

Expect JSON with `answers.ok.noul` and `usage.cost`. A `401` means a bad key; `402` means no
OpenRouter credits.

Then, from the `typesafe-computer-use` clone, point its TypeSafe client at OpenRouter:

```sh
export TYPESAFE_API_KEY="$OPENROUTER_API_KEY"
export TYPESAFE_BASE_URL=https://openrouter.ai/api
export TYPESAFE_DEFAULT_MODEL=typesafe/jev-1.14   # optional pin; default is jev-latest

# optional for type_text / final answer (the writer does not use the OpenRouter key):
export ANTHROPIC_API_KEY=…                         # or configure CLICKER_WRITER_*

# dry-run first: observe and choose, but do not act
uv run clicker "open Claude Desktop and start a Cowork task that …"

# live run, with explicit limits
uv run clicker "…" --act --steps 40 --delay 2
```

Use `--handoffs 0` on the first dry run. If using a TypeSafe console key instead, replace the
two `TYPESAFE_*` exports with `export TYPESAFE_API_KEY=…` and leave `TYPESAFE_BASE_URL` unset.

Score separately with `agenttail` against:

- the Code-tab transcript under `~/.claude/projects/` (Claude Code adapter), and/or
- `local-agent-mode-sessions/…/local_<id>/audit.jsonl` (the `claude_desktop` adapter above).

Do not feed the transcript to Jev as part of the action state if the run is meant to test
whether Jev can drive the UI; keep the scorer separate.

## What remains unverified

- No real Claude Desktop artifact was captured or parsed; all Desktop fixtures are handmade.
- Consumer roots on Linux/Windows, 3P roots, dispatch sessions, the HMAC chain and a real
  sidecar were not checked on an install.
- No Desktop↔mock-gateway handshake or 3P managed-settings run exists.
- No live OpenRouter/Jev or TypeSafe request was made, and no key was used.
- The `typesafe-computer-use` → OpenRouter handoff is source-inspected only; no `clicker`
  dry-run decision was made.
- The clicker writer/`type_text` path was not evaluated through OpenRouter.
- Linux AT-SPI + Jev under Xvfb, Linux Cowork under KVM, and any GHA desktop job are unproven.

The support matrix therefore says **ported-unverified** for the adapter, not CI-verified and
not donated-verified.
