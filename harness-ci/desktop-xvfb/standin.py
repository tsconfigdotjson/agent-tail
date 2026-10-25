#!/usr/bin/env python3
"""A tiny GTK3 desktop stand-in for the Xvfb plumbing scenario.

This is deliberately not Claude Desktop.  It gives the observe/act loop a real
X11 window with an AT-SPI entry and button, and writes a Claude Desktop
local-agent-mode `audit.jsonl` under the home folder so `agenttail` scores the
same disk shape the real adapter reads.  Docs and evidence label it a stand-in,
not Desktop verification.
"""

from __future__ import annotations

import json
import os
import sys
import time
import uuid
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402

SESSION_ID = os.environ.get("AGENTTAIL_STANDIN_SESSION", "local_11111111-2222-3333-4444-555555555555")
TITLE = "agenttail desktop stand-in"


def desktop_root(home: Path) -> Path:
    return home / ".config/Claude/local-agent-mode-sessions/agenttail/org" / SESSION_ID


def append_records(path: Path, records: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, separators=(",", ":")) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def write_sidecar(path: Path, prompt: str) -> None:
    sidecar = path.with_suffix(".json")
    sidecar.write_text(
        json.dumps(
            {
                "sessionId": SESSION_ID,
                "cliSessionId": SESSION_ID,
                "cwd": str(Path.home() / "work"),
                "title": prompt[:80],
                "model": "claude-sonnet-4-5",
                "permissionMode": "default",
                "createdAt": time.time(),
                "lastActivityAt": time.time(),
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )


def init_record() -> dict:
    return {
        "type": "system",
        "subtype": "init",
        "uuid": str(uuid.uuid4()),
        "session_id": SESSION_ID,
        "cwd": str(Path.home() / "work"),
        "model": "claude-sonnet-4-5",
        "permissionMode": "default",
        "claude_code_version": "standin",
        "tools": [],
    }


def user_record(prompt: str) -> dict:
    return {
        "type": "user",
        "uuid": str(uuid.uuid4()),
        "session_id": SESSION_ID,
        "message": {"role": "user", "content": prompt},
    }


def assistant_record(reply: str) -> dict:
    return {
        "type": "assistant",
        "uuid": str(uuid.uuid4()),
        "session_id": SESSION_ID,
        "message": {
            "id": "msg_standin",
            "model": "claude-sonnet-4-5",
            "role": "assistant",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 8},
            "content": [{"type": "text", "text": reply}],
        },
    }


def result_record(reply: str) -> dict:
    return {
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "duration_ms": 1200,
        "duration_api_ms": 1100,
        "num_turns": 1,
        "session_id": SESSION_ID,
        "total_cost_usd": 0.0002,
        "usage": {"input_tokens": 12, "output_tokens": 8},
        "result": reply,
        "uuid": str(uuid.uuid4()),
    }


class Standin(Gtk.Window):
    def __init__(self) -> None:
        super().__init__(title=TITLE)
        self.set_default_size(640, 360)
        self.set_border_width(24)
        self.session_dir = desktop_root(Path.home())
        self.audit = self.session_dir / "audit.jsonl"
        self.started = self.audit.exists()

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        self.add(box)
        heading = Gtk.Label(label="agenttail desktop stand-in")
        heading.set_xalign(0)
        box.pack_start(heading, False, False, 0)
        hint = Gtk.Label(label="Type a prompt and press Send. This writes a Desktop-shaped audit.jsonl.")
        hint.set_xalign(0)
        hint.set_line_wrap(True)
        box.pack_start(hint, False, False, 0)

        self.entry = Gtk.Entry()
        self.entry.set_placeholder_text("Message Claude")
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
        self.audit.parent.mkdir(parents=True, exist_ok=True)
        records = [user_record(prompt)] if self.started else [init_record(), user_record(prompt)]
        append_records(self.audit, records)
        write_sidecar(self.audit, prompt)
        self.started = True
        GLib.timeout_add(1200, self.finish, prompt)

    def finish(self, prompt: str) -> bool:
        reply = f"Hello from the Xvfb stand-in. You said: {prompt}"
        append_records(self.audit, [assistant_record(reply), result_record(reply)])
        write_sidecar(self.audit, prompt)
        self.set_status("Done")
        self.entry.set_sensitive(True)
        self.button.set_sensitive(True)
        return False


def main() -> int:
    win = Standin()
    win.connect("destroy", Gtk.main_quit)
    Gtk.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
