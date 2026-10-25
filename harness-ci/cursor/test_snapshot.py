import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "snapshot_state_vscdb.py"
COMPOSER = "composer-private"
BUBBLE = "bubble-private"
TITLE = "a private Cursor title"


class SnapshotStateVscdbTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.out = self.root / "out"
        self.tmp = self.root / "tmp"
        self.tmp.mkdir()
        self.db = self.root / "state.vscdb"
        self.connection = sqlite3.connect(self.db)
        self.connection.execute("PRAGMA journal_mode = WAL")
        self.connection.execute("PRAGMA wal_autocheckpoint = 0")
        self.connection.execute(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)"
        )
        self.write_composer("generating")
        self.write_bubble(
            {
                "name": "run_terminal_command_v2",
                "status": "loading",
                "additionalData": {"status": "pending", "blockReason": "mutation"},
                "error": {"message": "private error from /home/private/work"},
            }
        )
        self.connection.commit()

    def tearDown(self):
        self.connection.close()
        self.temp.cleanup()

    def write_composer(self, status):
        value = {
            "name": TITLE,
            "status": status,
            "generatingBubbleIds": ["private-generating-id"],
            "lastUpdatedAt": 1_792_756_800_000,
            "createdAt": 1_792_756_700_000,
        }
        self.connection.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            (f"composerData:{COMPOSER}", json.dumps(value)),
        )

    def write_bubble(self, tool):
        value = {"type": 2, "toolFormerData": tool}
        self.connection.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?, ?)",
            (f"bubbleId:{COMPOSER}:{BUBBLE}", json.dumps(value)),
        )

    def run_phase(self, phase):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), phase, "--db", str(self.db), "--out", str(self.out)],
            text=True,
            capture_output=True,
            env={**os.environ, "TMPDIR": str(self.tmp)},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads((self.out / f"{phase}.json").read_text())

    def test_three_phases_are_snapshotted_and_redacted(self):
        waiting = self.run_phase("waiting")
        self.assertEqual(waiting["phase"], "waiting")
        self.assertEqual(waiting["composer"]["status"], "generating")
        self.assertEqual(waiting["composer"]["generatingBubbleIds"], {"count": 1})
        self.assertEqual(waiting["bubbles"][0]["toolFormerData"]["additionalStatus"], "pending")
        self.assertIsNone(waiting["bubbles"][0]["toolFormerData"]["userDecision"])

        self.write_bubble(
            {
                "name": "run_terminal_command_v2",
                "status": "loading",
                "userDecision": "accepted",
                "additionalData": {"status": "loading"},
            }
        )
        self.connection.commit()
        approved = self.run_phase("after-approve")
        self.assertEqual(approved["bubbles"][0]["toolFormerData"]["userDecision"], "accepted")
        self.assertEqual(approved["bubbles"][0]["toolFormerData"]["additionalStatus"], "loading")

        self.write_bubble(
            {
                "name": "run_terminal_command_v2",
                "status": "completed",
                "userDecision": "rejected",
                "additionalData": {"status": "rejected"},
            }
        )
        self.write_composer("completed")
        self.connection.commit()
        denied = self.run_phase("after-deny")
        self.assertEqual(denied["composer"]["status"], "completed")
        self.assertEqual(denied["bubbles"][0]["toolFormerData"]["userDecision"], "rejected")
        self.assertEqual(denied["bubbles"][0]["toolFormerData"]["additionalStatus"], "rejected")

        all_dumps = "\n".join(path.read_text() for path in self.out.glob("*.json"))
        for private in (COMPOSER, BUBBLE, TITLE, "private-generating-id", "/home/private/work"):
            self.assertNotIn(private, all_dumps)
        self.assertIn("composerData:<composer>", all_dumps)
        self.assertIn("additionalStatus", all_dumps)
        self.assertEqual(list(self.tmp.iterdir()), [], "the private snapshot was removed")

    def test_the_newest_composer_is_chosen_by_default(self):
        older = {
            "name": "older",
            "status": "completed",
            "generatingBubbleIds": [],
            "lastUpdatedAt": 1,
        }
        self.connection.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:older', ?)",
            (json.dumps(older),),
        )
        self.connection.commit()
        waiting = self.run_phase("waiting")
        self.assertEqual(waiting["composer"]["status"], "generating")
        self.assertIn("run_terminal_command_v2", waiting["bubbles"][0]["toolFormerData"]["name"])

    def test_a_missing_database_fails_clearly(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "waiting", "--db", str(self.root / "missing.vscdb"), "--out", str(self.out)],
            text=True,
            capture_output=True,
            env={**os.environ, "TMPDIR": str(self.tmp)},
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("state.vscdb not found", result.stderr)


if __name__ == "__main__":
    unittest.main()
