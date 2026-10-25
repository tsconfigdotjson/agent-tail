#!/usr/bin/env python3
"""A Cursor-shaped GTK3 stand-in for the Xvfb plumbing scenario.

This is deliberately not Cursor Desktop.  It gives the observe/act loop a real
X11 window with an AT-SPI entry and button, then writes the two artifacts the
Cursor adapter reads: a transcript under `~/.cursor/projects/...` and a
`state.vscdb` composer row.  Docs and evidence label it a stand-in, not Cursor
Desktop verification.
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

TITLE = "agent-tail Cursor stand-in"
WORKSPACE = Path("work") / "demo-app"
SESSION_ID = os.environ.get("AGENT_TAIL_CURSOR_STANDIN_SESSION", str(uuid.uuid4()))


def encode_project(path: Path) -> str:
    """Cursor's lossy folder name: each run of non-alphanumerics becomes one '-'."""
    out: list[str] = []
    for char in str(path):
        if char.isalnum():
            out.append(char.lower())
        elif out and out[-1] != "-":
            out.append("-")
    return "".join(out).strip("-")


def transcript_path(home: Path, workspace: Path | None = None, session_id: str = SESSION_ID) -> Path:
    workspace = workspace or home / WORKSPACE
    return (
        home
        / ".cursor/projects"
        / encode_project(workspace)
        / "agent-transcripts"
        / session_id
        / f"{session_id}.jsonl"
    )


def state_db_path(home: Path) -> Path:
    return home / ".config/Cursor/User/globalStorage/state.vscdb"


def cursor_timestamp(when: datetime | None = None) -> str:
    """A timestamp block as Cursor writes it, to the minute, in UTC."""
    when = when or datetime.now(timezone.utc)
    hour = when.hour % 12 or 12
    half = "AM" if when.hour < 12 else "PM"
    return f"{when.strftime('%A, %b')} {when.day}, {when.year}, {hour}:{when.minute:02d} {half} (UTC)"


def prompt_record(prompt: str, workspace: Path, when: datetime | None = None) -> dict[str, Any]:
    text = (
        f"<timestamp>{cursor_timestamp(when)}</timestamp>\n"
        f"<user_info>Workspace Path: {workspace}</user_info>\n"
        f"<user_query>{prompt}\n</user_query>"
    )
    return {"role": "user", "message": {"content": [{"type": "text", "text": text}]}}


def assistant_record(reply: str) -> dict[str, Any]:
    return {"role": "assistant", "message": {"content": [{"type": "text", "text": reply}]}}


def turn_ended_record() -> dict[str, Any]:
    return {"type": "turn_ended", "status": "success"}


def append_records(path: Path, records: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, separators=(",", ":")) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def write_prompt(home: Path, prompt: str, workspace: Path | None = None, session_id: str = SESSION_ID) -> Path:
    workspace = workspace or home / WORKSPACE
    path = transcript_path(home, workspace, session_id)
    append_records(path, [prompt_record(prompt, workspace)])
    return path


def write_reply(
    home: Path, prompt: str, reply: str, workspace: Path | None = None, session_id: str = SESSION_ID
) -> Path:
    path = transcript_path(home, workspace, session_id)
    append_records(path, [assistant_record(reply), turn_ended_record()])
    return path


def write_state(
    home: Path,
    status: str,
    name: str,
    session_id: str = SESSION_ID,
    created_ms: int | None = None,
    updated_ms: int | None = None,
) -> Path:
    """Write a Cursor `composerData:<id>` row so the adapter can read title and status."""
    path = state_db_path(home)
    path.parent.mkdir(parents=True, exist_ok=True)
    now_ms = int(time.time() * 1000)
    value = {
        "composerId": session_id,
        "name": name,
        "status": status,
        "generatingBubbleIds": [],
        "createdAt": created_ms or now_ms,
        "lastUpdatedAt": updated_ms or now_ms,
        "unifiedMode": "agent",
        "forceMode": "edit",
    }
    connection = sqlite3.connect(path)
    try:
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT PRIMARY KEY, value BLOB NOT NULL)")
        connection.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            (f"composerData:{session_id}", json.dumps(value, separators=(",", ":"))),
        )
        connection.commit()
    finally:
        connection.close()
    return path


def run() -> int:
    import gi

    gi.require_version("Gtk", "3.0")
    from gi.repository import GLib, Gtk  # noqa: E402

    class CursorStandin(Gtk.Window):
        def __init__(self) -> None:
            super().__init__(title=TITLE)
            self.set_default_size(640, 360)
            self.set_border_width(24)
            self.home = Path.home()
            self.workspace = self.home / WORKSPACE
            self.transcript = transcript_path(self.home)
            self.created_ms = int(time.time() * 1000)
            self.composer_name = "Cursor stand-in QA"

            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
            self.add(box)
            heading = Gtk.Label(label="agent-tail Cursor stand-in")
            heading.set_xalign(0)
            box.pack_start(heading, False, False, 0)
            hint = Gtk.Label(
                label=(
                    "Type a prompt and press Send. This writes a Cursor-shaped transcript "
                    "and state.vscdb composer row."
                )
            )
            hint.set_xalign(0)
            hint.set_line_wrap(True)
            box.pack_start(hint, False, False, 0)

            self.entry = Gtk.Entry()
            self.entry.set_placeholder_text("Ask Cursor")
            self.entry.set_name("Prompt")
            self.entry.get_accessible().set_name("Prompt")
            box.pack_start(self.entry, False, False, 0)

            self.button = Gtk.Button(label="Send")
            self.button.connect("clicked", self.on_send)
            box.pack_start(self.button, False, False, 0)

            self.status = Gtk.Label(label="Idle")
            self.status.set_xalign(0)
            self.status.get_accessible().set_name("Idle")
            box.pack_start(self.status, False, False, 0)

            self.entry.connect("activate", lambda _entry: self.on_send(None))
            self.show_all()

        def set_status(self, text: str) -> None:
            self.status.set_text(text)
            self.status.get_accessible().set_name(text)

        def on_send(self, _button: Gtk.Button | None) -> None:
            if not self.button.get_sensitive():
                return
            prompt = self.entry.get_text().strip() or "say hello"
            self.button.set_sensitive(False)
            self.entry.set_sensitive(False)
            self.set_status("Working…")
            self.composer_name = prompt[:80]
            write_prompt(self.home, prompt, self.workspace)
            write_state(self.home, "generating", self.composer_name, created_ms=self.created_ms)
            GLib.timeout_add(1200, self.finish, prompt)

        def finish(self, prompt: str) -> bool:
            reply = f"Hello from the Xvfb Cursor stand-in. You said: {prompt}"
            write_reply(self.home, prompt, reply, self.workspace)
            write_state(self.home, "completed", self.composer_name, created_ms=self.created_ms)
            self.set_status("Done")
            self.entry.set_sensitive(True)
            self.button.set_sensitive(True)
            return False

    win = CursorStandin()
    win.connect("destroy", Gtk.main_quit)
    Gtk.main()
    return 0


def main() -> int:
    return run()


if __name__ == "__main__":
    raise SystemExit(main())
