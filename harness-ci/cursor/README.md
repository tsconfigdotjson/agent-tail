# Cursor `state.vscdb` approval capture

Issue #43 needs one fact that the idle Mac survey could not see: what Cursor writes **while**
an approval card is up. This directory holds the macOS/Linux snapshot tool for that capture.
It never opens the live database.

## Periodic confirmation

The supported path is Lee's already-signed-in Mac: follow
[`mac-smoke.md`](mac-smoke.md). It needs no new Cursor login, no API key and no CI secret.
Harness CI stays offline-only.

## Status in this lane

The Linux + Xvfb capture remains **blocked on authentication**:

- Cursor Agent (CLI) is installed in this lane, but its stored Cursor tokens expired and there
  is no interactive browser sign-in available under Xvfb.
- The Cursor IDE can be installed under Xvfb, but its sign-in needs a real Cursor account
  session. The CLI token store is not a substitute for an IDE login.

Do not add a Cursor login step to CI. The periodic Mac smoke is the confirmation path; if an
automated Linux capture is ever wanted, that auth step is still the blocker.

The reader already treats a persisted pending decision as firm `AwaitingApproval` (see
`crates/agent-tail/src/sqlite.rs`); the committed Cursor 3.27.12 Mac dumps under
[`mac-smoke-dumps/`](mac-smoke-dumps/) confirm Cursor's exact field paths and the composer
status during the wait.

## Commands

From the repository root, on macOS or Linux:

`snapshot-state-vscdb.sh` is the entry point; it runs `snapshot_state_vscdb.py`, which uses
only the Python standard library.

```sh
# While the approval card is visible.
harness-ci/cursor/snapshot-state-vscdb.sh waiting --out /tmp/cursor-approval

# Approve it, then capture immediately.
harness-ci/cursor/snapshot-state-vscdb.sh after-approve --out /tmp/cursor-approval

# Start a second tool wait, deny it, then capture.
harness-ci/cursor/snapshot-state-vscdb.sh after-deny --out /tmp/cursor-approval
```

Or let the script pause between steps and take all three:

```sh
harness-ci/cursor/snapshot-state-vscdb.sh capture --out /tmp/cursor-approval
```

The default database follows the platform:

```text
macOS: ~/Library/Application Support/Cursor/User/globalStorage/state.vscdb
Linux: ~/.config/Cursor/User/globalStorage/state.vscdb
```

Pass `--db` for a temp `HOME`, a non-standard install, or a copied database. Pass
`--composer` to select a chat; otherwise the newest `composerData` row is used.

The three dumps are:

| file | when |
|---|---|
| `waiting.json` | the approval card is up and the tool has not run |
| `after-approve.json` | after approving, before/while the tool runs |
| `after-deny.json` | after denying a second wait |

## What the script does

1. Copies `state.vscdb`, `state.vscdb-wal` and `state.vscdb-shm` into a new private
   (`0700`) folder under the system temp directory. It never opens the live files.
2. Opens only the copy `mode=ro` and runs the SQL documented in
   [`docs/formats/cursor.md`](../../docs/formats/cursor.md).
3. Writes a JSON dump containing the platform (`macos` or `linux`), composer status, title
   presence, timestamp, `generatingBubbleIds` count, and each bubble's tool
   name/status/`userDecision` (top-level and `additionalData`).
4. Replaces the composer id, bubble ids, title and error text with stable placeholders before
   writing, then deletes the private snapshot folder.

The output still deserves a quick review before it is committed, but it contains no prompt
text, title text, tool output, file paths or ids.

Useful fields to compare between the dumps:

- waiting: `toolFormerData.additionalStatus: "pending"` and/or
  `toolFormerData.userDecision: "pending"` (the reader maps either to firm approval)
- approved: `userDecision: "accepted"`, `additionalStatus` moves to `loading`/`running`
- denied: `userDecision: "rejected"`, or `additionalStatus` becomes
  `rejected`/`skipped`/`cancelled`

## Tests

```sh
python3 -m unittest discover -s harness-ci/cursor -p 'test_*.py' -v
```

The tests build a synthetic WAL-backed database, run all three phases, and check platform
path auto-detection, the dump's platform field, removal of the private snapshot, and
redaction of ids/title/error.

The temp-directory copy is the same approach as Cursor Desktop CI (#103) should use if that
lane grows a live snapshot helper. Keep the copy/never-open-live rule there too.
