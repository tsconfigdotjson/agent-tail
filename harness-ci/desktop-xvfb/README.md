# Desktop Xvfb harness

A Docker + Xvfb path for observing a Linux desktop without a physical display. It has two
uses:

- an offline stand-in that exercises the observe → act → score loop with a small GTK app;
- an optional launch probe for Anthropic's Linux beta Claude Desktop package.

The nightly workflow runs only the offline stand-in. The Desktop probe is manual
`workflow_dispatch` only. The stand-in is a plumbing test, not a claim that Claude Desktop is
CI-verified.

## What runs

| Surface | Supported | Notes |
|---|---|---|
| Xvfb + openbox | yes | 1440x900x24 by default, no physical display |
| AT-SPI observation/action | yes | works with GTK and Electron when accessibility is enabled |
| Stand-in observe/act/score | yes | offline; no Desktop package required |
| Claude Desktop standard sign-in UI | launch probe | rendered under Xvfb; no account needed |
| Claude Desktop 3P gateway welcome UI | launch probe | uses a root-owned mock settings file |
| Claude Desktop Chat/Code end-to-end | not proven | no scored Desktop turn in this repository |
| Cowork | no | Linux Cowork needs KVM/QEMU, roughly 8 GB RAM and 25 GB disk |

## Quick start

From `harness-ci/desktop-xvfb`:

```sh
# Offline plumbing check: AT-SPI observe -> scripted act -> agent-tail disk score.
./run.sh standin

# Claude Desktop standard sign-in screen under Xvfb.
./run.sh probe

# Claude Desktop 3P gateway welcome screen with the mock settings.
./run.sh desktop-probe
```

The wrapper builds `agent-tail` on the host, then builds and runs the container. Set
`INSTALL_CLAUDE_DESKTOP=0` for a lighter stand-in-only image.

## What the stand-in does

1. A small GTK window presents a text field and a Send button.
2. The driver observes it through AT-SPI and gets a list of named candidate actions.
3. A scripted chooser selects the field, types a prompt, and clicks Send.
4. The stand-in writes a Claude Desktop-shaped `audit.jsonl` under a temporary home.
5. `agent-tail` scores the audit log as `idle → thinking → done`.

The stand-in is deliberately not a second model or a hidden fallback for the Desktop app. It
exists to test the observation, action, artifact writing, and scoring path without installing
Desktop or using external credentials.

## Nightly and manual lanes

- **Nightly:** only the stand-in. It builds the stand-in image
  (`INSTALL_CLAUDE_DESKTOP=0`), runs the offline observe/act/score path, and uploads `runs/`.
- **Manual:** `run_desktop_probe` installs Claude Desktop and probes the sign-in or 3P gateway
  welcome screen. It does not need an account.
- Desktop is deliberately absent from `push` and `pull_request`, so it is not in the hot PR
  matrix.

The workflow is [`.github/workflows/desktop-xvfb.yml`](../../.github/workflows/desktop-xvfb.yml).

## Evidence and limits

The container installs Anthropic's Linux beta Desktop package and can render the standard
sign-in and 3P gateway screens under Xvfb. The stand-in scenario passes locally.

What is still missing is a real Desktop turn that writes a redacted artifact and reaches a
known status checkpoint. Until that exists, the adapter and support matrix stay
**ported-unverified**, not CI-verified. See
[`docs/formats/claude-desktop.md`](../../docs/formats/claude-desktop.md) for the adapter
details and fixture status.

## Container details

The image includes Xvfb, openbox, AT-SPI tools, `xdotool`, and the Rust/Node prerequisites
needed by the harness. The compose file keeps the container isolated except for localhost
mock traffic. Output is written below `runs/` by default; set `AGENT_TAIL_OUT` to move it.

Useful environment variables:

| variable | meaning |
|---|---|
| `AGENT_TAIL_OUT` | host output directory mounted into the container |
| `AGENT_TAIL_SCREEN` | Xvfb geometry (default `1440x900x24`) |
| `AGENT_TAIL_MOCK_URL` | mock model endpoint seen by the Desktop gateway settings |
| `AGENT_TAIL_CDP_PORT` | optional Chrome DevTools port for builds that accept it |
| `AGENT_TAIL_DOCKER_NETWORK` | compose network mode |

The stand-in needs no secret. Do not put credentials in an image, scenario file, or committed
log.
