# Desktop Xvfb stopgap (issue #80)

A local Docker + Xvfb path for the Desktop UI loop. It runs a headed Linux
desktop without a Mac runner and without spending GitHub Actions minutes. The
production chooser is **Jev through OpenRouter only**; the stand-in smoke test
uses a scenario script because there is no decision to make, not a second model.

**Evidence status:** the container installs Anthropic's Linux beta Desktop.
Sign-in and the 3P gateway welcome render under Xvfb, and the stand-in scenario
passes locally. A live Jev run on Multibeast (2026-10-25, package `3.6556.2`,
`typesafe/jev-1.14`) reached the Chat UI and the mock, then failed: Desktop's
`"."` gateway probe was rejected, and AT-SPI exposed no editable, so the prompt
was never typed and `agenttail` stayed at `idle_no_session`. This tree accepts
that probe, fixes the mock scenario path, and types through a loopback DevTools
composer when AT-SPI has no editable. **A scored Chat turn has not been
re-run.** The adapter matrix remains **ported-unverified**, not CI-verified.
Do not spend OpenRouter credits in CI; the live Jev path stays manual.

## What runs, and what does not

| Surface | Under this container | Notes |
|---|---|---|
| Xvfb + openbox | yes | 1440x900x24 by default, no physical display |
| AT-SPI observation/action | yes | GTK and Electron with `--force-renderer-accessibility` |
| Desktop standard sign-in UI | yes | probed, screenshot + AT-SPI tree captured |
| Desktop 3P gateway welcome UI | yes | root-owned `/etc/claude-desktop/managed-settings.json`, mock URL |
| Desktop Chat/Code end-to-end | **not proven** | live Jev reached Chat; scoring a turn is still a manual box run |
| Cowork | **no** | Linux Cowork needs KVM/QEMU, about 8 GB RAM and 25 GB disk; this container does not pass `/dev/kvm` |
| GHA | **not run** | `.github/workflows/desktop-xvfb.yml` is `workflow_dispatch` only |

The host box may have `/dev/kvm`, but the compose file deliberately does not ask
for it. This stopgap is an Xvfb path, not a Cowork VM path.

## Quick start

The wrapper builds `agenttail` on the host, then builds/runs the container. The
default image includes Claude Desktop; set `INSTALL_CLAUDE_DESKTOP=0` for a
lighter stand-in-only image.

```sh
cd harness-ci/desktop-xvfb

# Offline plumbing check: AT-SPI observe -> scripted act -> agenttail disk score.
./run.sh standin

# Standard Claude Desktop sign-in screen under Xvfb (no account needed).
./run.sh probe

# Claude Desktop 3P gateway welcome screen, with the root-owned mock settings.
./run.sh desktop-probe

# The real loop. This refuses to run without an OpenRouter key and does not
# fall back to a writer LLM or any other model.
# AGENTTAIL_JEV_CONFIDENCE is optional. Unset, the Desktop scenario uses 0.25
# because Jev scored Continue at 0.31 and the generic 0.55 floor stopped the run.
# AGENTTAIL_CDP_PORT=0 disables the DevTools typing fallback (default port 9223).
OPENROUTER_API_KEY=... AGENTTAIL_JEV_MODEL=typesafe/jev-1.14 ./run.sh desktop
```

Each run gets a timestamped directory under `runs/` (ignored by git) with:

- `score.json` or `probe.json`
- `timeline.jsonl`, `jev.jsonl` (Jev request/response, no key)
- `step-NN.png` and `step-NN-atspi.txt`
- app and mock logs

Do not commit `runs/`: screenshots and logs can contain prompt text or local
paths.

## The loop

1. **Observe** — `pyatspi` walks the active Desktop window into stable
   candidates (role, name, description, bounds, actions). A node with
   `STATE_EDITABLE` is a type target even when its role is `section` rather
   than `entry`. Non-click AT-SPI actions on non-editables are dropped, and
   Jev is not called until the candidate list is non-empty. When the tree
   still has no editable, the driver reads the page DOM on loopback DevTools
   (`--remote-debugging-port`, default `9223`) and adds the bottom composer
   plus a Send button or Enter key. A screenshot is saved as evidence but is
   never sent to a VLM and is never the scorer.
2. **Choose** — for Desktop, `desktop_driver.py` sends a typed `choice` request
   to OpenRouter's Decisions API at `typesafe/jev-1.14`. The model only returns
   an option and confidence; our code performs the click/type. The confidence
   floor is `--confidence`, else `AGENTTAIL_JEV_CONFIDENCE`, else the scenario
   value, else `0.55`. `claude_desktop_plain_qa` sets `0.25`: on the live run,
   Continue was `0.31` and the `0.55` floor stopped the driver before it
   clicked. There is no `ANTHROPIC_API_KEY`, `CLICKER_WRITER_*` or writer call
   in the path; the driver also scrubs those variables from the app environment.
3. **Act** — AT-SPI `doAction` first, with an `xdotool` click/type fallback from
   the candidate's screen bounds. A DevTools candidate is typed with
   `Input.insertText` after focusing the DOM node our code found, or clicked
   with `element.click()` / Enter. Our code owns every action. DevTools URLs
   that are not loopback are refused.
4. **Score** — poll `agenttail ls --hours 2 --agents …` with `HOME` pointed at
   the container user's home. The Desktop scenario reads both
   `claude_desktop` (`local-agent-mode-sessions/.../audit.jsonl`) and
   `claude_code` (`~/.claude/projects`, the Code tab). The stand-in writes only
   a Desktop-shaped audit log.
5. **Stop** — expected end activity, low confidence, step limit, timeout, app
   exit, or missing key. Failures leave `blocker.json` and the logs.

The stand-in scenario deliberately writes its own `audit.jsonl`. It proves the
container, AT-SPI, action, and scoring plumbing. It is **not** Claude Desktop
verification and its result must not be used as a Desktop support-matrix claim.

## Container details

- Base Ubuntu 24.04, Python 3, GTK3, Xvfb, openbox, D-Bus, AT-SPI, xdotool,
  scrot.
- Claude Desktop is installed from Anthropic's apt repository. Pin/override with
  `CLAUDE_DESKTOP_VERSION`; the local run used `3.6556.2`.
- `entrypoint.sh` starts Xvfb, then runs the D-Bus session and every GUI process
  as the unprivileged `claude` user. Starting the session bus as root and the
  app as another user makes D-Bus EXTERNAL auth fail.
- For 3P, the entrypoint writes
  `/etc/claude-desktop/managed-settings.json` as root with mode 0644. The app
  rejects a file that is not root-owned or is group/world-writable.
- The existing Rust `mock-server` runs on the fixed loopback address
  `127.0.0.1:8765` with `mock-server/scenarios/desktop_plain_qa.json` (the
  `plain_qa` step, plus a health-probe allowlist). The path in the Desktop
  scenario is `../mock-server/scenarios/desktop_plain_qa.json` relative to
  `desktop-xvfb/` (`../../` misses `harness-ci/` and the file is not found).
  Inside the container the same relative path is also tried from
  `/repo/harness-ci/desktop-xvfb`. The scenario logs requests to
  `mock-requests.jsonl`. A probe is logged with `"probe": true` and does not
  consume the say-hello step. `plain_qa.json` itself stays strict for the CLI
  harnesses.
- The image is large (Claude Desktop is about 600 MB installed); it is built
  once and cached by Docker.

## Host network when bridge DNS cannot reach OpenRouter

`./run.sh desktop` has to resolve `openrouter.ai` from inside the container.
On a dockerd started with `--iptables=false` (the Multibeast box, where bridge
iptables could not be installed), the default bridge DNS at `127.0.0.11` does
not resolve external names, so Jev never gets a choice. Run that container on
the host network instead of building another network stack:

```sh
AGENTTAIL_DOCKER_NETWORK=host OPENROUTER_API_KEY=... ./run.sh desktop
```

`network_mode` defaults to `bridge`, which is the right mode when Docker's
embedded DNS works. `host` shares the host network namespace: loopback in the
container is the host's loopback, which is also what the mock
(`127.0.0.1:8765`) and the DevTools port use. Do not publish extra ports for
this; the harness is still one container.

## Local evidence, 2026-10-25 (Multibeast)

Host: Ubuntu 26.04, Docker 29.1.3, 2 vCPU, 3.7 GiB RAM, `/dev/kvm` present but
not passed to the container. No GitHub Actions run was made.

- `./run.sh standin` (stand-in only image): `ok: true`, seen
  `idle_no_session -> working (thinking/prompt_sent) -> done (done/turn_ended)`.
  This is the committed thin scenario and the only end-to-end run here.
- `./run.sh probe` (standard Desktop): window `Claude` found, sign-in document
  and cookie controls visible in AT-SPI. This proves the standard UI renders
  under Xvfb; it does not sign in.
- `./run.sh desktop-probe` (3P gateway): package `3.6556.2`, window `Claude`
  found, welcome screen with `Continue` and `Menu` candidate actions. This proves
  the root-owned 3P policy is read and the gateway welcome renders.
- `./run.sh desktop` without `OPENROUTER_API_KEY`: exits with
  `OPENROUTER_API_KEY is empty; refusing to fake the Jev choose step`.
- Live Jev, with the key set, `AGENTTAIL_JEV_MODEL=typesafe/jev-1.14`,
  `AGENTTAIL_JEV_CONFIDENCE=0.25`, and `network_mode: host` because dockerd was
  started with `--iptables=false`: exit 1. Claude Desktop `3.6556.2` rendered
  the Chat UI ("You're here!", prompt placeholder visible, model "Mock Sonnet
  Extended") after Jev clicked Continue and New. The mock received one
  `POST /v1/messages` whose user text was `"."` and `max_tokens` was 1.
  `plain_qa` required `"say hello"`, returned 500, and the UI showed
  "Gateway returned an error." `agenttail` saw only `idle_no_session` (no
  `audit.jsonl`, no Code-tab session). AT-SPI candidates were push buttons
  only (`Menu`, `Continue`, `New`, `claude Gateway`, …), so Jev had nothing
  to type into and looped on chrome. About $0.001 of OpenRouter spend. That
  run is why the probe allowlist, the `../` mock path, the confidence floor,
  and the DevTools composer exist. It is not a scored turn.

Known noise that did not block the probes: Chromium logs repeated GPU-process
launch failures under Xvfb, and xdg-desktop-portal reports that the document
portal cannot mount its FUSE path because the container has no fuse device.

## What is still missing

- A scored Chat turn: idle → working → done from a real Desktop `audit.jsonl`
  or Code-tab transcript, re-run on the box with the fixes above. Do not mark
  the support matrix CI-verified from the failed live run.
- Cowork under KVM, which is explicitly outside this stopgap.
- A green GHA run. The workflow is committed only as a manual stub for next
  month; do not schedule it or claim CI verification for Desktop. Live Jev
  stays on the box so CI does not spend OpenRouter credits.
