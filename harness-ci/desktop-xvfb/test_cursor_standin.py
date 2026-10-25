import importlib.util
import json
import sqlite3
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("cursor_standin.py")
SPEC = importlib.util.spec_from_file_location("cursor_standin", MODULE_PATH)
standin = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = standin
SPEC.loader.exec_module(standin)


class CursorArtifactTests(unittest.TestCase):
    def test_project_folder_and_transcript_use_cursor_layout(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            session = "11111111-2222-3333-4444-555555555555"
            path = standin.write_prompt(home, "say hello", session_id=session)
            project = standin.encode_project(home / standin.WORKSPACE)
            self.assertEqual(
                path,
                home
                / ".cursor/projects"
                / project
                / "agent-transcripts"
                / session
                / f"{session}.jsonl",
            )
            self.assertEqual(
                standin.encode_project(Path("/home/dev/Work/My App")), "home-dev-work-my-app"
            )
            first = json.loads(path.read_text().splitlines()[0])
            text = first["message"]["content"][0]["text"]
            self.assertEqual(first["role"], "user")
            self.assertIn("<timestamp>", text)
            self.assertIn("<user_query>say hello\n</user_query>", text)

    def test_reply_appends_assistant_text_and_a_successful_turn_end(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            standin.write_prompt(home, "say hello")
            path = standin.write_reply(home, "say hello", "Hello from the stand-in.")
            records = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual([record.get("role") for record in records], ["user", "assistant", None])
            self.assertEqual(records[1]["message"]["content"][0]["text"], "Hello from the stand-in.")
            self.assertEqual(records[2], {"type": "turn_ended", "status": "success"})

    def test_state_db_has_composer_title_status_and_times(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            created = int(datetime(2026, 10, 16, tzinfo=timezone.utc).timestamp() * 1000)
            updated = created + 1200
            path = standin.write_state(home, "generating", "stand-in QA", created_ms=created, updated_ms=updated)
            standin.write_state(home, "completed", "stand-in QA", created_ms=created, updated_ms=updated + 1)
            connection = sqlite3.connect(path)
            try:
                row = connection.execute(
                    "SELECT value FROM cursorDiskKV WHERE key = ?", (f"composerData:{standin.SESSION_ID}",)
                ).fetchone()
            finally:
                connection.close()
            value = json.loads(row[0])
            self.assertEqual((value["name"], value["status"]), ("stand-in QA", "completed"))
            self.assertEqual((value["createdAt"], value["lastUpdatedAt"]), (created, updated + 1))
            self.assertEqual(value["generatingBubbleIds"], [])

    def test_timestamp_is_the_block_shape_the_cursor_adapter_parses(self):
        when = datetime(2026, 10, 16, 16, 41, tzinfo=timezone.utc)
        self.assertEqual(standin.cursor_timestamp(when), "Friday, Oct 16, 2026, 4:41 PM (UTC)")


if __name__ == "__main__":
    unittest.main()
