# Claude Desktop scenario stub

**There is no CI scenario or CI job here.** The current local stopgap is
[`../desktop-xvfb/`](../desktop-xvfb/README.md): Docker + Xvfb can install the Linux Desktop
package, render its standard sign-in and 3P gateway welcome screens, and expose them over
AT-SPI for an observe/Jev/act loop. The stand-in scenario in that folder passes locally, but
no live Jev call or real Desktop turn has been completed, so this is still a stub for matrix
purposes. The `claude_desktop` discovery spec and adapter read local-agent-mode
`audit.jsonl` files; their handmade fixtures are under `fixtures/claude-desktop/`. The
evidence is in [`docs/formats/claude-desktop.md`](../../docs/formats/claude-desktop.md).

## Why it is still a stub

The mock/PTY rig drives terminal CLIs. Claude Desktop is a GUI application, and the planned
driver is a Jev computer-use loop, not a PTY prompt:

- Jev chooses a typed action; our code observes the screen/accessibility tree and performs
  the click or keystroke.
- `agenttail` scores the resulting session from disk. It is not a screenshot/VLM scorer.
- The reference `typesafe-computer-use` agent is macOS-first and needs Screen Recording and
  Accessibility permissions.
- Jev is called through **OpenRouter** by default (`OPENROUTER_API_KEY`, model
  `typesafe/jev-1.14` or `~typesafe/jev-latest`), not a separate TypeSafe console key. Code
  that still reads `TYPESAFE_*` needs `TYPESAFE_API_KEY="$OPENROUTER_API_KEY"` and
  `TYPESAFE_BASE_URL=https://openrouter.ai/api` so the key and endpoint stay paired.

No OpenRouter key was used for the spike, so no live Jev call was made. The Xvfb container
now launches the real Linux Desktop package, but it stops at the standard sign-in or 3P
welcome screen; no Chat/Code turn or Desktop disk artifact has been driven. Source inspection
of `typesafe-computer-use` shows its bare `TypeSafeClient()` inherits `TYPESAFE_BASE_URL` from
`typesafe-sdk` 0.6.0, but that handoff has not been run.

## Candidate lanes

| lane | what it would prove | blocker |
|---|---|---|
| macOS manual/scripted | Desktop can be observed and driven by a Jev choice loop; Code/Cowork artifacts appear where expected | OpenRouter key + credits, Desktop install, screen/accessibility permission |
| Linux + Xvfb, Code/Chat UI | the same loop on Linux | **container + AT-SPI driver written**; waiting on a live Jev key and a completed Desktop turn |
| Linux + Xvfb, Cowork | Cowork transcript scoring on Linux | KVM/QEMU (about 25 GB disk, 8 GB RAM) is often unavailable; not passed by the Xvfb compose file |
| 3P gateway + mock, no computer use | Desktop can talk to the Anthropic Messages mock and write scoreable Code/Cowork artifacts | root-owned 3P settings and welcome screen now proven under Xvfb; Desktop↔mock request still not run |

The gateway path needs `inferenceProvider: gateway`, `inferenceGatewayBaseUrl`, and an API
key sent as Bearer or `x-api-key`. The gateway must implement `POST /v1/messages` with
streaming and tool use; `GET /v1/models` is optional. On Linux the managed settings file is
root-owned at `/etc/claude-desktop/managed-settings.json`. See the vendor gateway and
data-storage pages linked from the format note.

## Artifacts a future scenario must distinguish

- **Code tab:** Desktop metadata below app-data `claude-code-sessions/`; the transcript is a
  Claude Code transcript under `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
- **Cowork / agent / Chat (local agent mode):** app-data
  `local-agent-mode-sessions/…/local_<id>.json` plus `local_<id>/audit.jsonl` (HMAC-chained).
  This is **not** `~/.claude/projects`. The `claude_desktop` adapter reads it today; the
  committed fixtures are handmade from prior art, so support stays **ported-unverified**.

3P app-data roots are documented as `Claude-3p` (macOS `~/Library/Application Support`,
Linux `~/.config`, Windows `%LOCALAPPDATA%`). Consumer builds commonly use `Claude` without
`-3p`; those paths still need verification on a real install.

## Before adding a job

Do not schedule a GitHub Actions Desktop job from this stub. This month's Actions billing is
already expended. A `workflow_dispatch`-only stub exists at
[`.github/workflows/desktop-xvfb.yml`](../../.github/workflows/desktop-xvfb.yml); it is ready
for next month but has not run. Add a Desktop scenario to the normal matrix only after a real
run produces a redacted artifact and a readable status checkpoint. Until then the support
matrix says **ported-unverified**, not CI-verified.
