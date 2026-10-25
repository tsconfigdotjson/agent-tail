import subprocess
import sys
import unittest
from pathlib import Path

import parse_version


HERE = Path(__file__).resolve().parent


class ParseVersionTests(unittest.TestCase):
    def test_strips_trailing_period_from_copilot_output(self):
        self.assertEqual(
            parse_version.parse_version("GitHub Copilot CLI 1.0.103.\n"),
            "1.0.103",
        )

    def test_keeps_internal_dots_and_ignores_surrounding_punctuation(self):
        self.assertEqual(parse_version.parse_version("copilot (v1.0.103), ready"), "1.0.103")

    def test_rejects_output_without_a_three_component_version(self):
        with self.assertRaisesRegex(ValueError, "no three-component version"):
            parse_version.parse_version("GitHub Copilot CLI 1.0.")

    def test_does_not_partially_match_a_four_component_version(self):
        with self.assertRaisesRegex(ValueError, "no three-component version"):
            parse_version.parse_version("GitHub Copilot CLI 1.0.103.4")

    def test_cli_reads_stdin_and_fails_loudly(self):
        result = subprocess.run(
            [sys.executable, str(HERE / "parse_version.py")],
            input="GitHub Copilot CLI 1.0.103.\n",
            check=True,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.stdout, "1.0.103\n")

        failed = subprocess.run(
            [sys.executable, str(HERE / "parse_version.py")],
            input="GitHub Copilot CLI\n",
            text=True,
            capture_output=True,
        )
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("no three-component version", failed.stderr)


if __name__ == "__main__":
    unittest.main()
