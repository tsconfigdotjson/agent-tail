# Cursor Mac smoke (already signed in)

This is the periodic confirmation path for a Mac where Cursor is **already signed in**. It
needs no new Cursor login, no API key, and no CI secret. Harness CI stays offline-only; this
is a manual smoke run.

No live snapshot was produced in the #108 lane: it still needs an already-signed-in Mac and
Lee's interaction, so CI keeps the synthetic `state.vscdb` and transcript fixtures instead.

Run from the repository root.

## 1. Capture the approval wait

1. Open a Cursor Agent chat and turn **auto-run off**.
2. Ask for a harmless tool that needs approval, such as creating a file in a scratch folder.
3. Leave the approval card up, then run:

   ```sh
   ./harness-ci/cursor/snapshot-state-vscdb.sh capture --out /tmp/cursor-approval
   ```

   The script auto-detects the Mac database at
   `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`. It prompts when to
   snapshot waiting, approve, and deny a second wait.

The result is three redacted dumps:

```text
/tmp/cursor-approval/waiting.json
/tmp/cursor-approval/after-approve.json
/tmp/cursor-approval/after-deny.json
```

If the interactive prompts are awkward, run the phases by hand instead:

```sh
./harness-ci/cursor/snapshot-state-vscdb.sh waiting --out /tmp/cursor-approval
# approve in Cursor
./harness-ci/cursor/snapshot-state-vscdb.sh after-approve --out /tmp/cursor-approval
# start a second wait and deny it
./harness-ci/cursor/snapshot-state-vscdb.sh after-deny --out /tmp/cursor-approval
```

## 2. Optional: check agent-tail's live status

Build once:

```sh
cargo build --release
```

While the approval card is still up, read the live Cursor project:

```sh
./target/release/agent-tail ls --agents cursor --hours 1
```

The Cursor session should show `awaiting_approval`, `certainty: firm`, and
`reason: composer_status`. For a live view, use:

```sh
./target/release/agent-tail watch --agents cursor --hours 1
```

After approving or denying, the session should leave `awaiting_approval` on the next poll.

## 3. What to share

Attach the three JSON dumps (and optionally the `ls` line) to #43 or the follow-up PR. Note
the Cursor version, whether auto-run was off, and whether the waiting dump showed
`additionalData.status: pending` or `userDecision: pending`.

The dumps replace ids, titles and errors with placeholders, but give them a quick look before
posting. If the live status does not match, the dumps are the evidence needed to adjust the
reader.
