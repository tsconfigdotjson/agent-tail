#!/usr/bin/env python3
"""Snapshot Cursor's Linux state.vscdb during a live approval wait, without opening it.

The live database is never opened. Each capture copies ``state.vscdb`` and its ``-wal`` and
``-shm`` sidecars into a private temporary folder, opens the copy read-only, runs the
queries documented in ``docs/formats/cursor.md``, and removes the copy. The output is a
redacted JSON dump: ids, titles and errors are replaced with stable placeholders, so the
file is safe to attach to the issue or turn into a fixture.

Typical use, from the repository root::

    python3 harness-ci/cursor/snapshot_state_vscdb.py waiting --out /tmp/cursor-approval
    # approve the tool in Cursor
    python3 harness-ci/cursor/snapshot_state_vscdb.py after-approve --out /tmp/cursor-approval
    # start a second tool wait, then deny it in Cursor
    python3 harness-ci/cursor/snapshot_state_vscdb.py after-deny --out /tmp/cursor-approval

``capture`` does the same three snapshots with prompts between them. It is only a convenience;
each phase can also be captured by hand with the commands above.

The default database is Cursor's Linux global storage path. On other platforms pass ``--db``.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import urllib.parse
from pathlib import Path
from typing import Any

DEFAULT_DB = Path.home() / ".config/Cursor/User/globalStorage/state.vscdb"
DEFAULT_OUT = Path(tempfile.gettempdir()) / "agent-tail-cursor-approval"
SIDECARS = ("-wal", "-shm")
PHASE_FILES = {
    "waiting": "waiting.json",
    "approve": "after-approve.json",
    "after-approve": "after-approve.json",
    "deny": "after-deny.json",
    "after-deny": "after-deny.json",
}

COMPOSER_SQL = """
    SELECT json_extract(value, '$.status') AS status,
           json_extract(value, '$.generatingBubbleIds') AS generating_bubble_ids,
           json_extract(value, '$.name') AS name,
           json_extract(value, '$.lastUpdatedAt') AS last_updated_at,
           json_extract(value, '$.createdAt') AS created_at
    FROM cursorDiskKV
    WHERE key = ?1
"""

BUBBLE_SQL = """
    SELECT key,
           json_extract(value, '$.type') AS bubble_type,
           json_extract(value, '$.toolFormerData.name') AS tool_name,
           json_extract(value, '$.toolFormerData.status') AS tool_status,
           json_extract(value, '$.toolFormerData.additionalData.status') AS additional_status,
           json_extract(value, '$.toolFormerData.userDecision') AS user_decision,
           json_extract(value, '$.toolFormerData.additionalData.userDecision') AS additional_user_decision,
           json_extract(value, '$.toolFormerData.error') AS tool_error
    FROM cursorDiskKV
    WHERE key >= ?1 AND key < ?2
    ORDER BY key
"""

LATEST_COMPOSER_SQL = """
    SELECT substr(key, 14)
    FROM (SELECT key, CAST(value AS TEXT) AS v FROM cursorDiskKV
          WHERE key >= 'composerData:' AND key < 'composerData;')
    WHERE json_valid(v) AND json_type(v, '$') = 'object'
    ORDER BY CAST(json_extract(v, '$.lastUpdatedAt') AS INTEGER) DESC, key DESC
    LIMIT 1
"""


class CaptureError(Exception):
    """A snapshot could not be read; the message is safe to show."""


def sidecar(path: Path, suffix: str) -> Path:
    return path.with_name(path.name + suffix)


def snapshot(source: Path) -> tuple[Path, dict[str, bool]]:
    """Copy ``source`` and its sidecars into a private folder; never open ``source``."""
    if not source.is_file():
        raise CaptureError(f"Cursor state.vscdb not found: {source} (pass --db PATH)")
    directory = Path(tempfile.mkdtemp(prefix="agent-tail-cursor-snapshot-"))
    os.chmod(directory, 0o700)
    copied = {"main": False, "wal": False, "shm": False}
    try:
        for suffix, label in (("", "main"), *((suffix, suffix[1:]) for suffix in SIDECARS)):
            origin = source if not suffix else sidecar(source, suffix)
            if not origin.is_file():
                continue
            target = directory / ("state.vscdb" + suffix)
            try:
                shutil.copyfile(origin, target)
            except FileNotFoundError:
                # Cursor rotated a sidecar between the stat and the copy: the next capture
                # will have it.
                continue
            os.chmod(target, 0o600)
            copied[label] = True
        if not copied["main"]:
            raise CaptureError(f"Cursor state.vscdb could not be copied: {source}")
        return directory, copied
    except Exception:
        shutil.rmtree(directory, ignore_errors=True)
        raise


def read_only(path: Path) -> sqlite3.Connection:
    quoted = urllib.parse.quote(path.resolve().as_posix(), safe="/")
    connection = sqlite3.connect(f"file:{quoted}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA query_only = ON")
    return connection


def latest_composer(connection: sqlite3.Connection) -> str:
    row = connection.execute(LATEST_COMPOSER_SQL).fetchone()
    if row is None or not row[0]:
        raise CaptureError("the snapshot has no composerData rows (open a Cursor Agent chat first)")
    return str(row[0])


def redact_name(value: Any) -> str | None:
    return "<redacted>" if value not in (None, "") else None


def redact_error(value: Any) -> str | None:
    return "<redacted>" if value not in (None, "") else None


def capture(db: Path, composer: str | None, phase: str, out: Path) -> str:
    directory, copied = snapshot(db)
    try:
        connection = read_only(directory / "state.vscdb")
        try:
            composer_id = composer or latest_composer(connection)
            composer_row = connection.execute(COMPOSER_SQL, (f"composerData:{composer_id}",)).fetchone()
            if composer_row is None:
                raise CaptureError(f"no composerData row for composer {composer_id}")
            generating = composer_row["generating_bubble_ids"]
            try:
                generating_count = len(json.loads(generating)) if generating else 0
            except (TypeError, ValueError):
                generating_count = 0
            start = f"bubbleId:{composer_id}:"
            end = f"bubbleId:{composer_id};"
            bubbles = []
            for index, row in enumerate(connection.execute(BUBBLE_SQL, (start, end)), start=1):
                bubbles.append(
                    {
                        "key": f"bubbleId:<composer>:<bubble-{index}>",
                        "type": row["bubble_type"],
                        "toolFormerData": {
                            "name": row["tool_name"],
                            "status": row["tool_status"],
                            "additionalStatus": row["additional_status"],
                            "userDecision": row["user_decision"],
                            "additionalUserDecision": row["additional_user_decision"],
                            "error": redact_error(row["tool_error"]),
                        },
                    }
                )
            document = {
                "phase": phase,
                "platform": "linux",
                "database": copied,
                "composer": {
                    "key": "composerData:<composer>",
                    "status": composer_row["status"],
                    "generatingBubbleIds": {"count": generating_count},
                    "name": redact_name(composer_row["name"]),
                    "lastUpdatedAt": composer_row["last_updated_at"],
                    "createdAt": composer_row["created_at"],
                },
                "bubbles": bubbles,
            }
            out.mkdir(parents=True, exist_ok=True)
            os.chmod(out, 0o700)
            target = out / PHASE_FILES[phase]
            temporary = target.with_name(target.name + f".tmp-{os.getpid()}")
            temporary.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
            os.chmod(temporary, 0o600)
            os.replace(temporary, target)
            return composer_id
        finally:
            connection.close()
    finally:
        shutil.rmtree(directory, ignore_errors=True)


def pause(message: str) -> None:
    try:
        input(f"{message}\nPress Enter when it is on screen... ")
    except EOFError as error:
        raise CaptureError("capture needs an interactive terminal; use the phase commands instead") from error


def capture_all(db: Path, out: Path) -> None:
    pause("Open a Cursor Agent chat and let it call a tool that needs approval.")
    composer = capture(db, None, "waiting", out)
    pause("Approve the tool in Cursor.")
    # The same chat can stop being the newest row while the decision is being made; keep the
    # id from the wait so the approved snapshot is the same composer.
    capture(db, composer, "after-approve", out)
    pause("Start another tool that needs approval, and leave the card up.")
    pause("Deny the second tool in Cursor.")
    capture(db, None, "after-deny", out)
    print(f"three redacted dumps written under {out}")


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Snapshot Cursor's state.vscdb for a live approval wait, without opening the live database.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "The live DB is copied with its -wal/-shm sidecars to a private temp folder, read only there,\n"
            "and deleted. Ids, titles and errors are redacted in the JSON dump."
        ),
    )
    p.add_argument(
        "phase",
        choices=[*PHASE_FILES, "capture"],
        help="waiting, after-approve, after-deny, or capture (interactive, all three)",
    )
    p.add_argument("--db", type=Path, default=DEFAULT_DB, help=f"state.vscdb path (default: {DEFAULT_DB})")
    p.add_argument("--composer", help="Cursor composer id (default: newest composerData row)")
    p.add_argument("--out", type=Path, default=DEFAULT_OUT, help=f"output directory (default: {DEFAULT_OUT})")
    return p


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.phase == "capture":
            capture_all(args.db, args.out)
        else:
            phase = {"approve": "after-approve", "deny": "after-deny"}.get(args.phase, args.phase)
            capture(args.db, args.composer, phase, args.out)
            print(f"wrote {args.out / PHASE_FILES[phase]}")
    except (CaptureError, sqlite3.Error) as error:
        print(f"snapshot_state_vscdb: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
