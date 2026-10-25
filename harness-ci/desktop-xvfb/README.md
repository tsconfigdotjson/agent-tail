# Desktop Xvfb stopgap (issue #80)

A local Docker + Xvfb path for the Desktop UI loop. It runs a headed Linux
desktop without a Mac runner and without spending GitHub Actions minutes. The
production chooser is **Jev through OpenRouter only**; the stand-in smoke test
uses a scenario script because there is no decision to make, not a second model.

**Evidence status:** the container installs Anthropic's Linux beta Desktop and
its standard sign-in screen and 3P/gateway welcome screen render under Xvfb on
Multibeast. The stand-in scenario passes locally. **No live Jev/OpenRouter call
was made and no real Desktop turn was driven**, because no `OPENROUTER_API_KEY`
was available. The adapter matrix therefore remains **ported-unverified**, not
CI-verified.

## What runs, and what does not

| Surface | Under this container | Notes |
|---|---|---|
| Xvfb + openbox | yes | 1440x900x24 by default, no physical display |
| AT-SPI observation/action | yes | GTK and Electron with `--force-renderer-accessibility` |
| Desktop standard sign-in UI | yes | probed, screenshot + AT-SPI tree captured |
| Desktop 3P gateway welcome UI | yes | root-owned `/etc/claude-desktop/managed-settings.json`, mock URL |
| Desktop Chat/Code end-to-end | **not proven** | needs the Jev key and a live UI run |
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
OPENROUTER_API_KEY=... ./run.sh desktop
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
   candidates (role, name, description, bounds, actions). A screenshot is saved
   as evidence but is never sent to a VLM and is never the scorer.
2. **Choose** — for Desktop, `desktop_driver.py` sends a typed `choice` request
   to OpenRouter's Decisions API at `typesafe/jev-1.14`. The model only returns
   an option and confidence; our code performs the click/type. A confidence
   below `AGENTTAIL_JEV_CONFIDENCE` (default 0.55) stops the run without acting.
   There is no `ANTHROPIC_API_KEY`, `CLICKER_WRITER_*` or writer call in the
   path; the driver also scrubs those variables from the app environment.
3. **Act** — AT-SPI `doAction` first, with an `xdotool` click/type fallback from
   the candidate's bounds. Our code owns every action.
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
  `127.0.0.1:8765` with `mock-server/scenarios/plain_qa.json`. The scenario also
  logs requests to `mock-requests.jsonl`.
- The image is large (Claude Desktop is about 600 MB installed); it is built
  once and cached by Docker.

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
  `OPENROUTER_API_KEY is empty; refusing to fake the Jev choose step`. No live
  Jev request, mock request, Desktop turn or disk artifact exists yet.

Known noise that did not block the probes: Chromium logs repeated GPU-process
launch failures under Xvfb, and xdg-desktop-portal reports that the document
portal cannot mount its FUSE path because the container has no fuse device.

## What is still missing

- A live OpenRouter/Jev run, including its cost and a real chosen action.
- A real Desktop Chat/Code turn and a redacted Desktop artifact captured from
  this container.
- A Desktop↔mock request, proving the 3P gateway handshake reaches the same
  Anthropic Messages mock used by the CLI harnesses.
- Cowork under KVM, which is explicitly outside this stopgap.
- A green GHA run. The workflow is committed only as a manual stub for next
  month; do not schedule it or claim CI verification for Desktop.
