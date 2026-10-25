# Claude Desktop scenario stub

**There is no scenario here yet, and no CI job runs Claude Desktop.** This folder documents
the gap so a future Desktop lane is not mistaken for an existing harness. The investigation
and evidence links are in [`docs/formats/claude-desktop.md`](../../docs/formats/claude-desktop.md).

## Why it is a stub

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

No OpenRouter key was used for the spike, so no live Jev call was made and no Desktop install
was driven. Source inspection of `typesafe-computer-use` shows its bare `TypeSafeClient()`
inherits `TYPESAFE_BASE_URL` from `typesafe-sdk` 0.6.0, but that handoff has not been run.

## Candidate lanes

| lane | what it would prove | blocker |
|---|---|---|
| macOS manual/scripted | Desktop can be observed and driven by a Jev choice loop; Code/Cowork artifacts appear where expected | OpenRouter key + credits, Desktop install, screen/accessibility permission |
| Linux + Xvfb, Code/Chat UI | the same loop on Linux | a Linux AT-SPI observe/act adapter; Xvfb/desktop automation not written |
| Linux + Xvfb, Cowork | Cowork transcript scoring on Linux | KVM/QEMU (about 25 GB disk, 8 GB RAM) is often unavailable |
| 3P gateway + mock, no computer use | Desktop can talk to the Anthropic Messages mock and write scoreable Code/Cowork artifacts | real Desktop install, 3P managed settings, gateway auth; Desktop↔mock handshake never run |

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
  This is **not** `~/.claude/projects`.

3P app-data roots are documented as `Claude-3p` (macOS `~/Library/Application Support`,
Linux `~/.config`, Windows `%LOCALAPPDATA%`). Consumer builds commonly use `Claude` without
`-3p`; those paths still need verification on a real install.

## Before adding a job

Do not add a GitHub Actions Desktop job from this stub. This month's Actions billing is
already expended, and Linux Xvfb/KVM/accessibility support is unproven. A scenario should be
committed only after a real run produces a redacted artifact and a readable status
checkpoint, or after a documented manual/fixture-only run. Until then the support matrix
must say **spike / not CI-verified**.
