# Claude Desktop scenario status

This directory is a placeholder for a future Claude Desktop PR scenario. There is no scored
Desktop lane in the normal PR matrix.

The current coverage is:

- the `claude_desktop` discovery spec and adapter, with handmade fixtures under
  `fixtures/claude-desktop/`;
- an offline Xvfb stand-in that tests the observe/act/score plumbing without Desktop;
- a live Jev Desktop lane that runs on the nightly schedule when the repository
  `OPENROUTER_API_KEY` Actions secret is present, and on manual dispatch opt-in;
- a manual launch probe that installs the Linux beta Desktop package and renders its sign-in
  and 3P gateway welcome screens without an account.

The adapter is **ported-unverified**: the scheduled live lane can score a real Desktop turn,
but its evidence is an ephemeral Actions artifact and no redacted capture is committed yet.
See [`docs/formats/claude-desktop.md`](../../docs/formats/claude-desktop.md) for the
record shape and fixture details, and
[`../desktop-xvfb/`](../desktop-xvfb/README.md) for the headless harness and secret setup.

## Why this is not in the PTY matrix

The mock/PTY rig drives terminal CLIs. Claude Desktop is a GUI application, so a Desktop lane
needs a display server, accessibility observation, and UI actions. The Xvfb harness is the
current path for that work.

## Artifacts a real scenario must distinguish

- **Code tab:** Desktop metadata below app-data `claude-code-sessions/`; the transcript is a
  Claude Code transcript under `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
- **Cowork / agent / Chat (local agent mode):** app-data
  `local-agent-mode-sessions/…/local_<id>.json` plus `local_<id>/audit.jsonl` or
  `<first-8-hex>/audit.jsonl` (HMAC-chained). This is **not** `~/.claude/projects`.

The `claude_desktop` adapter reads both local-agent-mode folder shapes. The committed
fixtures are handmade from the public Claude Agent SDK shape, so support stays
**ported-unverified** until a real capture is added.

3P app-data roots are documented as `Claude-3p` (macOS `~/Library/Application Support`,
Linux `~/.config`, Windows `%LOCALAPPDATA%`). Consumer builds commonly use `Claude` without
`-3p`; those paths still need verification on a real install.

## Before adding a job

Add a Desktop scenario to the hot PR matrix only after a live run produces a committed,
redacted artifact and a readable status checkpoint. Until then the nightly live lane stays
schedule/manual-only and the support matrix says **ported-unverified**, not CI-verified.
