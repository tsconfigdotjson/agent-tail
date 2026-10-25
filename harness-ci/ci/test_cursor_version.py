import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import cursor_version


HERE = Path(__file__).resolve().parent
INSTALLER = """#!/usr/bin/env bash
DOWNLOAD_URL="https://downloads.cursor.com/lab/2026.10.22-27dcf5f/${OS}/${ARCH}/agent-cli-package.tar.gz"
FINAL_DIR="$HOME/.local/share/cursor-agent/versions/2026.10.22-27dcf5f"
"""


class CursorVersionTests(unittest.TestCase):
    def test_reads_the_versioned_download_url(self):
        self.assertEqual(cursor_version.version_from_installer(INSTALLER), "2026.10.22-27dcf5f")

    def test_rejects_an_installer_without_a_download_url(self):
        with self.assertRaisesRegex(ValueError, "versioned"):
            cursor_version.version_from_installer("echo install")

    def test_rejects_ambiguous_installers(self):
        text = INSTALLER + "https://downloads.cursor.com/lab/2026.10.23-abcdef0/linux/x64/p.tar.gz\n"
        with self.assertRaisesRegex(ValueError, "multiple"):
            cursor_version.version_from_installer(text)

    def test_rejects_an_unexpected_version_string(self):
        with self.assertRaisesRegex(ValueError, "format"):
            cursor_version.version_from_installer(
                "https://downloads.cursor.com/lab/latest/linux/x64/agent-cli-package.tar.gz"
            )

    def test_cli_reads_saved_installer_and_fails_loudly(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "install.sh"
            path.write_text(INSTALLER, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(HERE / "cursor_version.py"), "--installer", str(path)],
                check=True,
                text=True,
                capture_output=True,
            )
            self.assertEqual(result.stdout.strip(), "2026.10.22-27dcf5f")

            path.write_text("no download here", encoding="utf-8")
            failed = subprocess.run(
                [sys.executable, str(HERE / "cursor_version.py"), "--installer", str(path)],
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("versioned downloads.cursor.com URL", failed.stderr)


if __name__ == "__main__":
    unittest.main()
