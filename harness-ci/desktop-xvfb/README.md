# Desktop Xvfb harness

A Docker + Xvfb path for observing a Linux desktop without a physical display. It has four
uses:

- offline stand-ins that exercise the observe → act → score loop with small GTK apps, for
  both Claude Desktop-shaped and Cursor-shaped artifacts;
- a live Jev Desktop lane that chooses UI actions and scores the on-disk Claude Desktop
  audit log;
- optional launch probes for Anthropic's Linux beta Claude Desktop package and Cursor's Linux
  package.

The nightly workflow always runs both offline stand-ins. When the repository Actions secret
`OPENROUTER_API_KEY` is present, the same schedule also runs the live Jev lane; without it
that lane is skipped cleanly. The launch probes are manual `workflow_dispatch` only. The
stand-ins are plumbing tests, not claims that Claude Desktop or Cursor Desktop is
CI-verified.

## What runs

| Surface | Supported | Notes |
|---|---|---|
| Xvfb + openbox | yes | 1440x900x24 by default, no physical display |
| AT-SPI observation/action | yes | works with GTK and Electron when accessibility is enabled |
| Claude Desktop stand-in observe/act/score | yes | offline; no Desktop package required |
| Cursor stand-in observe/act/score | yes | offline; writes a Cursor transcript and `state.vscdb`; no Cursor package or account |
| Live Jev Claude Desktop Chat | nightly when the secret is set | Jev picks UI actions; `agent-tail` scores the on-disk audit log |
| Claude Desktop standard sign-in UI | launch probe | rendered under Xvfb; no account needed |
| Claude Desktop 3P gateway welcome UI | launch probe | uses a root-owned mock settings file |
| Cursor Desktop launch | launch probe | installs the Linux package and renders it under Xvfb; no account needed |
| Claude Desktop Code end-to-end | not proven | no scored Code-tab turn in this repository |
| Cursor Desktop end-to-end | not proven | no live Cursor turn; chat needs Cursor's auth/backend protocol |
| Cowork | no | Linux Cowork needs KVM/QEMU, roughly 8 GB RAM and 25 GB disk |

## Quick start

From `harness-ci/desktop-xvfb`:

```sh
# Offline plumbing check: AT-SPI observe -> scripted act -> agent-tail disk score.
./run.sh standin

# Offline Cursor artifact check: transcript + state.vscdb -> agent-tail cursor score.
./run.sh cursor-standin

# Live Jev Desktop lane (needs OPENROUTER_API_KEY in the environment).
./run.sh desktop

# Claude Desktop standard sign-in screen under Xvfb.
./run.sh probe

# Claude Desktop 3P gateway welcome screen with the mock settings.
./run.sh desktop-probe

# Cursor Desktop launch screen under Xvfb.
./run.sh cursor-probe
```

The wrapper builds `agent-tail` on the host, then builds and runs the container. The
stand-ins use a lighter image with neither real desktop package installed; override
`INSTALL_CLAUDE_DESKTOP=1` or `INSTALL_CURSOR_DESKTOP=1` only for the probe or live lanes.

## What the stand-ins do

### Claude Desktop stand-in

1. A small GTK window presents a text field and a Send button.
2. The driver observes it through AT-SPI and gets a list of named candidate actions.
3. A scripted chooser selects the field, types a prompt, and clicks Send.
4. The stand-in writes a Claude Desktop-shaped `audit.jsonl` under a temporary home.
5. `agent-tail` scores the audit log as `idle → thinking → done`.

### Cursor stand-in

1. A GTK window with the same AT-SPI entry/button shape is observed and driven.
2. The stand-in writes a Cursor transcript under
   `~/.cursor/projects/<encoded-workspace>/agent-transcripts/<id>/<id>.jsonl`, using the
   real `role` / `message.content` / `turn_ended` shapes.
3. It also writes a `state.vscdb` `composerData:<id>` row, moving `status` from `generating`
   to `completed`, so the adapter's database title/status path is exercised.
4. `agent-tail --agents cursor` scores the artifacts as `idle → thinking → done`.

The stand-ins are deliberately not second models or hidden fallbacks for the Desktop apps.
They exist to test the observation, action, artifact writing, and scoring paths without
installing the products or using external credentials. Synthetic Cursor artifacts do not
make Cursor **CI-verified**; the real adapter still needs a live/redacted capture.

## Nightly and manual lanes

- **Nightly stand-ins:** both always run on the `43 3 * * *` schedule. They build the
  stand-in-only image (`INSTALL_CLAUDE_DESKTOP=0`, `INSTALL_CURSOR_DESKTOP=0`), run the
  offline observe/act/score paths, and upload `runs/` only when the job fails (3-day
  retention).
- **Nightly live Jev:** also runs on that schedule when `secrets.OPENROUTER_API_KEY` is
  present. It installs Claude Desktop, uses Jev (`typesafe/jev-1.14` through OpenRouter) to
  choose UI actions, and scores the Desktop audit log written on disk. There is no writer
  LLM in the path; the driver removes writer credentials and fails instead of falling back.
  A missing secret skips only this lane, with a run-summary note; the stand-ins still run.
  Its `runs/` evidence also uses fail-only uploads with 3-day retention.
- **Manual:** `run_desktop_probe` installs Claude Desktop and probes the sign-in or 3P gateway
  welcome screen. `run_cursor_probe` installs Cursor and probes its launch screen; neither
  needs an account. `run_live_jev` opts into the live lane on a manual dispatch.
- Desktop is deliberately absent from `push` and `pull_request`, so live Jev is schedule or
  manual dispatch only and cannot spend credits on every PR. The Cursor probe is also
  dispatch-only and never part of the PR matrix.

The workflow is [`.github/workflows/desktop-xvfb.yml`](../../.github/workflows/desktop-xvfb.yml).

### Configure the live Jev secret

Add a repository Actions secret named `OPENROUTER_API_KEY` with the same OpenRouter key
Multibeast uses. In the GitHub UI that is **Settings → Secrets and variables → Actions → New
repository secret**. From a machine where the key is already in the environment, without
putting the value in the command line:

```sh
printf '%s' "$OPENROUTER_API_KEY" | \
  gh secret set OPENROUTER_API_KEY --repo tsconfigdotjson/agent-tail
```

Multibeast sets this secret after this PR merges. Until it exists, nightly runs report that
`OPENROUTER_API_KEY` is not set and skip the live lane while the stand-in continues to run.
The secret is read only by the scheduled/manual live job; pull requests from forks never
receive it.

## Evidence and limits

The container installs Anthropic's Linux beta Desktop package and can render the standard
sign-in and 3P gateway screens under Xvfb. The Claude stand-in scenario passes locally. The
scheduled live lane exercises a real Desktop turn when the secret is configured, but its
evidence is an ephemeral Actions artifact; no redacted Desktop capture is committed yet.

For Cursor, the nightly job proves only the artifact path: the stand-in writes synthetic
Cursor-shaped transcript and `state.vscdb` files and `agent-tail` scores them. The optional
probe can install Cursor's Linux package and render its launch screen, with no account. There
is no live Cursor turn and no committed redacted Cursor capture yet, so Cursor remains
**ported-unverified** rather than CI-verified. See
[`docs/formats/cursor.md`](../../docs/formats/cursor.md) for the adapter details and the
still-missing live approval-wait capture.

What is still missing for Claude Desktop is a committed redacted artifact from the live turn
that reaches a known status checkpoint. Until it exists, the adapter and support matrix stay
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
| `OPENROUTER_API_KEY` | OpenRouter key for the live Jev chooser; empty disables that lane |
| `AGENT_TAIL_JEV_MODEL` | Jev model (default `typesafe/jev-1.14`) |
| `AGENT_TAIL_JEV_CONFIDENCE` | optional Jev confidence floor |
| `AGENT_TAIL_CDP_PORT` | optional Chrome DevTools port for Claude builds that accept it |
| `AGENT_TAIL_CURSOR_BIN` | Cursor GUI binary for the probe (default `/usr/share/cursor/cursor`) |
| `AGENT_TAIL_DOCKER_NETWORK` | compose network mode |
| `INSTALL_CLAUDE_DESKTOP` | install Anthropic's Linux package (`0` for stand-ins) |
| `CLAUDE_DESKTOP_VERSION` | pinned Claude Desktop package version |
| `INSTALL_CURSOR_DESKTOP` | install Cursor's `.deb` for the manual probe (`0` for stand-ins) |
| `CURSOR_DESKTOP_VERSION` | pinned Cursor package version (default `3.27.12`) |
| `CURSOR_DESKTOP_COMMIT` | pinned Cursor build commit for the default download URL |
| `CURSOR_DESKTOP_DEB_URL` | optional explicit Cursor `.deb` URL; overrides the default download |

The stand-ins need no secret. Do not put credentials in an image, scenario file, or committed
log.
