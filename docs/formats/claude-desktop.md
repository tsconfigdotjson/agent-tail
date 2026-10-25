# Claude Desktop agent mode + Jev via OpenRouter (spike)

**Status: documentation spike only.** This is not a supported harness. There is no
discovery spec, adapter, fixture or harness-CI scenario, and no Claude Desktop job runs in
GitHub Actions. The Jev/OpenRouter path below was written from public documentation and
source inspection, not from a live API call or a real Desktop session. No OpenRouter or
TypeSafe key was used for this spike.

Scope: issue #74 under parent #33, retargeted by #76, and the remaining Claude Desktop part
of #27. The goal is to decide whether a **Jev computer-use loop** can drive Claude Desktop
while `agenttail` scores the session from disk. It is not a claim that Desktop is supported.

Evidence is labelled **documented** (vendor docs), **source-inspected** (public repository
or pinned package source, not a live call), **prior art** (another project's code or survey),
or **TBD** (needs a real install or key). Nothing here is CI-verified.

## What is being proposed

Jev is TypeSafe System One: a typed `choice` / `score` / `noul` decision over a supplied
state, not a clicker or typer. The driver observes the desktop, turns what it sees into
candidate actions, asks Jev to choose, and our code performs the chosen UI action. Jev never
clicks a pixel directly.

The loop is:

1. **Observe** — screenshot OCR plus the accessibility tree become a small set of stable,
   named candidate actions.
2. **Choose** — Jev returns a typed choice (or `noul` / a low score) over those candidates.
3. **Act** — our driver executes the chosen action through the platform's UI automation
   (for example the reference Mac `clicker`, or a Linux AT-SPI adapter).
4. **Score** — poll `agenttail`'s `state(now)` and the Desktop session artifacts on disk.
5. **Stop** — on a terminal state, `noul`, low confidence, the step limit, or a stall (no new
   artifact and no progress for N steps).

`agenttail` is the scorer, not Jev. There is deliberately **no VLM as scorer** in this
design; a screenshots-only judge would not prove that the transcript state is correct.

Reference points:

- Jev on OpenRouter: <https://openrouter.ai/docs/guides/community/jev>
- TypeSafe SDK pointed at OpenRouter: <https://openrouter.ai/docs/guides/community/typesafe-sdk>
- Jev tutorial and Decisions API examples: <https://openrouter.ai/docs/guides/community/jev-tutorial>
- Decisions API reference: <https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-questions-and-answers-request>
- TypeSafe System One concepts: <https://docs.typesafe.ai/introduction/quickstart>
- Reference Mac agent: <https://github.com/awlevin/typesafe-computer-use>
- TypeSafe-direct API (optional alternate): <https://docs.typesafe.ai/sdk/python>

## Auth: OpenRouter by default

Lee's default is Jev through **OpenRouter**, billed to the existing OpenRouter credits. No
separate TypeSafe account or TypeSafe console key is needed.

**Documented:** Jev is `typesafe/jev-1.14`, with `~typesafe/jev-latest` tracking the newest
release. OpenRouter exposes it on two surfaces, both authenticated with
`Authorization: Bearer $OPENROUTER_API_KEY`:

| Surface | Endpoint | Use it when |
|---|---|---|
| Decisions API | `POST https://openrouter.ai/api/alpha/decisions` | Calling from any language with plain HTTP, or through OpenRouter's Decisions SDKs |
| System One API | `POST https://openrouter.ai/api/v1/systemone` | Using the TypeSafe SDK with base URL `https://openrouter.ai/api` |

**Documented and source-inspected:** the OpenRouter SDK guide says both official TypeSafe
SDKs (`typesafe-sdk`, `@typesafe-ai/sdk`) read the API key from `TYPESAFE_API_KEY` and the
base URL from `TYPESAFE_BASE_URL`. The pinned Python `typesafe-sdk` 0.6.0 source confirms
both variables, a default base URL of `https://api.typesafe.ai`, and a default model of
`jev-latest`. An existing `TypeSafeClient()` can therefore be migrated without code changes:

```sh
export OPENROUTER_API_KEY=…
export TYPESAFE_API_KEY="$OPENROUTER_API_KEY"
export TYPESAFE_BASE_URL=https://openrouter.ai/api
# optional: pin instead of the SDK default `jev-latest`
export TYPESAFE_DEFAULT_MODEL=typesafe/jev-1.14
```

The two `TYPESAFE_*` variables are an adapter for code that still reads them, not a second
account. Do not set `TYPESAFE_API_KEY` to the OpenRouter key while leaving the base URL at
`api.typesafe.ai`: the key and endpoint must travel as a pair. `jev-latest` (the SDK default)
is mapped by OpenRouter to `~typesafe/jev-latest`; IDs that already carry an author prefix,
such as `typesafe/jev-1.14`, are used as-is.

**Optional TypeSafe-direct alternate:** with a TypeSafe console key instead, set
`TYPESAFE_API_KEY` and leave `TYPESAFE_BASE_URL` unset (the SDK defaults to
`https://api.typesafe.ai`). This is no longer a prerequisite for the Mac run; OpenRouter is
the preferred path because Lee already has credits there.

## Does `typesafe-computer-use` work through OpenRouter?

**Source-inspected, not live-verified.** The reference `typesafe-computer-use` project is
**macOS-first** (macOS 14+): it needs Screen Recording and Accessibility permissions, and it
supports a dry run (`uv run clicker "goal"`) and live driving (`--act`). Its README and
`.env.example` mention only `TYPESAFE_API_KEY`, but the agent constructs a bare
`TypeSafeClient()` with no explicit `base_url` in `runner.py` and `browser/runner.py`. The
pinned Python SDK then resolves `TYPESAFE_BASE_URL` as described above, so the env migration
should point its classifier at OpenRouter. The CLI preflight checks only that
`TYPESAFE_API_KEY` is present; it does not validate the base URL.

That is not a live proof: no `clicker` run was made for this spike. Start with the Decisions
API curl smoke below, then a dry `clicker` run. The clicker's **writer** is separate:
`type_text`, writer-proposed URLs and the final answer still use `ANTHROPIC_API_KEY` unless
`CLICKER_WRITER_*` points at another Anthropic or OpenAI-compatible endpoint
(`CLICKER_WRITER_API`, `CLICKER_WRITER_BASE_URL`, `CLICKER_WRITER_API_KEY`). Exporting
`OPENROUTER_API_KEY` does not configure the writer by itself, so a classifier-only dry run can
work while typing and final answers are disabled.

Windows is experimental, and although the repository discusses OSWorld/Linux sandboxes, the
`clicker` itself is not a drop-in Linux driver.

## Claude Desktop availability

**Documented:** Claude Desktop has a Linux beta for Ubuntu 22.04+ / Debian 12+ on x64 and
arm64, installed as the apt package `claude-desktop`:

- <https://support.claude.com/en/articles/10065433-install-claude-desktop>
- <https://code.claude.com/docs/en/desktop-linux>

Linux limitations that matter here:

- Anthropic **Computer Use** (the app/screen-control feature) is **not available on Linux
  Desktop**. That is Anthropic's feature, not this Jev driver.
- Dictation is not available.
- Cowork needs KVM/QEMU, about 25 GB of disk and 8 GB of RAM.

So "Claude Desktop exists on Linux" only means the application can be installed there. It
does not mean Anthropic's own computer-use feature works there, and the reference Jev
`clicker` does not become a Linux driver just because the app installs.

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
- The consumer claude.ai sign-in path does **not** expose an arbitrary base URL. The 3P
  gateway is the path.
- The existing mock already answers streaming and non-streaming Anthropic Messages with
  `text`, `thinking` and `tool_use` blocks for the CLI, but **no Desktop↔mock handshake has
  been run**. Desktop may send fields or headers the CLI does not, and the gateway settings
  and key field have not been exercised. Mock compatibility is therefore **not proven**.

Auth and isolation are also not solved:

- The official Linux docs say that for API-key auth you should use the CLI; Desktop 3P is
  the enterprise/gateway path. A consumer subscription is not a substitute for a gateway.
- `/etc/claude-desktop/managed-settings.json` is root-owned, so a temp-`HOME` harness cannot
  isolate Desktop as easily as it isolates a CLI.
- Headless Linux keyring/secret-service availability is **TBD**; Desktop is a GUI app and
  was not launched on this box.

A future gateway-only lane could prove transcript scoring without any computer use, but that
is a different test from driving the UI. It still needs a real Desktop install and a gateway
key/permission.

## Where Desktop writes sessions

The two surfaces must not be conflated:

| Surface | Artifacts | Read by `agenttail` today? |
|---|---|---|
| **Code tab** | Desktop metadata under app-data `claude-code-sessions/`; the actual transcript in the Claude Code store, `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` | The Claude Code adapter can read the transcript once it appears there. Desktop metadata is not read. Not exercised. |
| **Cowork / agent / Chat (local agent mode)** | Self-contained under app-data `local-agent-mode-sessions/…/local_<id>.json` plus `local_<id>/audit.jsonl` (HMAC-chained) | **No.** This is not `~/.claude/projects`; there is no adapter, discovery spec or fixture. |

Official data-storage page:
<https://claude.com/docs/third-party/claude-desktop/data-storage>.

**Do not assume Desktop agent mode uses the Claude Code session layout.** A Code-tab
transcript and a Cowork/local-agent-mode `audit.jsonl` are different artifacts and need
different readers.

### App-data roots

The 3P roots are documented. Consumer builds commonly use the same names without `-3p`, but
those paths were not checked on a real install here.

| Build | macOS | Linux | Windows |
|---|---|---|---|
| 3P / gateway (documented) | `~/Library/Application Support/Claude-3p/` | `~/.config/Claude-3p/` | `%LOCALAPPDATA%\Claude-3p\` |
| Consumer (unverified here) | `~/Library/Application Support/Claude/` | `~/.config/Claude/` | `%APPDATA%\Claude\` (prior art; TBD) |

The prior-art survey in [`docs/prior-art.md`](../prior-art.md) saw the macOS consumer
`Claude/local-agent-mode-sessions` layout in ADR, not a 3P install. Treat every consumer row
as **TBD until verified on a real install**; the 3P row is the one the vendor documents.

## macOS versus Linux + Xvfb

Lee previously preferred Linux + Xvfb when a Linux app exists. Claude Desktop now does exist
on Linux, so Linux is first-class **for installing and launching the app**. That is not the
same as a first-class computer-use loop:

- `typesafe-computer-use` is macOS-first (Screen Recording + Accessibility). A Linux path
  needs an observe/act adapter built on AT-SPI (and OCR where the tree is incomplete).
- Running that adapter under Xvfb is follow-up work, not a drop-in change.
- Cowork on Linux needs KVM/QEMU. Nested CI VMs and Xvfb-only boxes often cannot provide it.
- GitHub Actions billing is expended this month, so this spike adds **no new desktop GHA
  job**. Linux Xvfb, KVM and accessibility support are not proven in CI.

Recommendation, in order:

1. **Manual Mac spike** with `typesafe-computer-use` + Claude Desktop using Lee's OpenRouter
   key. Start with a dry run, then a short `--act` run. This is the cheapest way to test the
   decision loop against a real desktop, and it needs no TypeSafe console key.
2. **Linux + Xvfb** for the Desktop Code/Chat UI only after a Linux observe/act adapter
   exists. Expect to skip Cowork there if the box has no KVM.
3. **Gateway/mock path** if the immediate goal is only transcript scoring: prove 3P settings
   and the Desktop↔mock handshake without computer use. This still needs a real install and
   gateway auth, and is not claimed here.

## How to run once Lee's OpenRouter key is available

Preconditions: macOS 14+ (the reference clicker is macOS-first), Python 3.12+, `uv`, Claude
Desktop installed and signed in, and Screen Recording + Accessibility granted to the terminal.

First check the OpenRouter path without the clicker. This is the only live call needed to
validate the key and endpoint before a desktop run, and it costs one small Jev request:

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

Expect JSON with `answers.ok.noul` and `usage.cost`. A `401` means the key is bad, `402`
means the OpenRouter account has no credits.

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

Use `--handoffs 0` on the first dry run to make the first classifier stop final instead of
handing back to the classifier again. If no writer key is configured, the run logs that the
writer is disabled and makes no writer call. If using a TypeSafe console key instead, replace
the two `TYPESAFE_*` exports with `export TYPESAFE_API_KEY=…` and leave `TYPESAFE_BASE_URL`
unset; the SDK then calls `https://api.typesafe.ai`.

Stop with Ctrl-C, move the mouse to the top-left corner, or let the driver stop on a
classifier `done`, low confidence, `noul`, the step limit or a stall.

Score separately with `agenttail` against:

- the Code-tab transcript under `~/.claude/projects/` (or the Desktop-configured Claude Code
  store), and/or
- `local-agent-mode-sessions/…/local_<id>/audit.jsonl` once a reader exists.

Do not feed the transcript to Jev as part of the action state if the run is meant to test
whether Jev can drive the UI; keep the scorer separate.

## What remains unverified

- No live OpenRouter/Jev or TypeSafe request was made, and no API key was used.
- The `typesafe-computer-use` → OpenRouter handoff is source-inspected only; no `clicker`
  dry-run decision was made.
- The clicker writer/`type_text` path was not evaluated through OpenRouter; it still expects
  `ANTHROPIC_*` or `CLICKER_WRITER_*`.
- No Claude Desktop install was launched, on macOS or Linux.
- No Desktop↔mock-gateway handshake or 3P managed-settings run exists.
- No Code-tab or Cowork artifact was captured or parsed.
- Linux AT-SPI + Jev under Xvfb, Linux Cowork under KVM, and any GHA desktop job are all
  unproven.
- Consumer app-data paths without `-3p`, headless keyring behaviour and the gateway API-key
  field name remain **TBD**.

The honest support-matrix status is therefore **spike / not CI-verified**, not
`ported-unverified` and not supported.
