import json
import subprocess
import sys
import unittest
from pathlib import Path

import matrix


HERE = Path(__file__).resolve().parent


class MatrixTests(unittest.TestCase):
    def test_claude_keeps_both_pins_and_latest_on_each_os(self):
        entries = matrix.build_matrix("claude-code", "9.9.9")["include"]
        self.assertEqual(
            [(entry["label"], entry["version"], entry["os"]) for entry in entries],
            [
                ("2.1.281", "2.1.281", "ubuntu-latest"),
                ("2.1.281", "2.1.281", "macos-latest"),
                ("2.1.310", "2.1.310", "ubuntu-latest"),
                ("2.1.310", "2.1.310", "macos-latest"),
                ("latest", "9.9.9", "ubuntu-latest"),
                ("latest", "9.9.9", "macos-latest"),
            ],
        )

    def test_pi_and_opencode_keep_their_pins_and_latest_lanes(self):
        pi = matrix.build_matrix("pi", "1.5.0")["include"]
        self.assertIn({"label": "1.4.0", "version": "1.4.0", "os": "ubuntu-latest"}, pi)
        self.assertIn({"label": "latest", "version": "1.5.0", "os": "macos-latest"}, pi)
        self.assertEqual(len(pi), 4)

        opencode = matrix.build_matrix("opencode", "1.20.0")["include"]
        self.assertIn({"label": "1.19.4", "version": "1.19.4", "os": "ubuntu-latest"}, opencode)
        self.assertIn({"label": "latest", "version": "1.20.0", "os": "macos-latest"}, opencode)
        self.assertEqual(len(opencode), 4)

    def test_gemini_and_copilot_have_pinned_and_latest_lanes(self):
        gemini = matrix.build_matrix("gemini", "0.66.0")["include"]
        self.assertIn({"label": "0.65.0", "version": "0.65.0", "os": "ubuntu-latest"}, gemini)
        self.assertIn({"label": "latest", "version": "0.66.0", "os": "macos-latest"}, gemini)
        self.assertEqual(len(gemini), 4)

        copilot = matrix.build_matrix("copilot", "1.1.0")["include"]
        self.assertIn({"label": "1.0.103", "version": "1.0.103", "os": "ubuntu-latest"}, copilot)
        self.assertIn({"label": "latest", "version": "1.1.0", "os": "macos-latest"}, copilot)
        self.assertEqual(len(copilot), 4)

    def test_codex_has_a_pinned_and_latest_lane(self):
        entries = matrix.build_matrix("codex", "0.164.0")["include"]
        self.assertIn({"label": "0.160.1", "version": "0.160.1", "os": "ubuntu-latest"}, entries)
        self.assertIn({"label": "latest", "version": "0.164.0", "os": "macos-latest"}, entries)
        self.assertEqual(len(entries), 4)

    def test_cursor_has_a_pinned_and_latest_lane(self):
        entries = matrix.build_matrix("cursor", "2026.10.22-27dcf5f")["include"]
        self.assertIn(
            {"label": "2026.10.01-e373342", "version": "2026.10.01-e373342", "os": "ubuntu-latest"},
            entries,
        )
        self.assertIn({"label": "latest", "version": "2026.10.22-27dcf5f", "os": "macos-latest"}, entries)
        self.assertEqual(len(entries), 4)

    def test_empty_latest_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "empty"):
            matrix.build_matrix("codex", "  ")

    def test_cli_emits_json_or_github_output(self):
        command = [sys.executable, str(HERE / "matrix.py"), "codex", "--latest", "0.164.0"]
        plain = subprocess.run(command, check=True, text=True, capture_output=True)
        parsed = json.loads(plain.stdout)
        self.assertEqual(len(parsed["include"]), 4)

        github = subprocess.run(command + ["--github-output"], check=True, text=True, capture_output=True)
        self.assertTrue(github.stdout.startswith("value={"))
        self.assertEqual(json.loads(github.stdout.removeprefix("value=")), parsed)


if __name__ == "__main__":
    unittest.main()
