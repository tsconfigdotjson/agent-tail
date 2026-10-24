import json
import tempfile
import unittest
from pathlib import Path

import nightly


REPO = Path(__file__).resolve().parents[2]


class NightlyTests(unittest.TestCase):
    def test_a_new_version_writes_baseline_report_and_fixture(self):
        baseline = json.loads(
            (REPO / "harness-ci/drift/baseline/claude-code/2.1.281.json").read_text(encoding="utf-8")
        )
        current = json.loads(json.dumps(baseline))
        current["version"] = "9.9.9"
        current["field_paths"].setdefault("user", []).append("turnPosition")

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            artifact = root / "artifacts" / "harness-latest-Linux"
            artifact.mkdir(parents=True)
            (artifact / "shape.json").write_text(json.dumps(current), encoding="utf-8")
            run = artifact / "claude_plain_qa" / "transcripts"
            run.mkdir(parents=True)
            (artifact / "claude_plain_qa" / "score.json").write_text("{}", encoding="utf-8")
            (run / "session.jsonl").write_text(
                json.dumps(
                    {
                        "type": "user",
                        "uuid": "11111111-1111-1111-1111-111111111111",
                        "cwd": "/tmp/private/work",
                        "message": {"role": "user", "content": "hello"},
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            baseline_dir = root / "baseline"
            baseline_dir.mkdir()
            (baseline_dir / "2.1.281.json").write_text(json.dumps(baseline), encoding="utf-8")

            status = nightly.main(
                [
                    "--artifacts",
                    str(root / "artifacts"),
                    "--baseline-dir",
                    str(baseline_dir),
                    "--reports-dir",
                    str(root / "reports"),
                    "--fixtures-root",
                    str(root / "fixtures"),
                ]
            )
            self.assertEqual(status, 0)
            self.assertTrue((baseline_dir / "9.9.9.json").is_file())
            self.assertTrue((root / "reports" / "claude-code-2.1.281-to-9.9.9.md").is_file())
            fixture = root / "fixtures" / "claude-code" / "9.9.9" / "linux" / "ci-claude_plain_qa"
            self.assertTrue((fixture / "meta.toml").is_file())
            text = (fixture / "input" / "session.jsonl").read_text(encoding="utf-8")
            self.assertNotIn("/tmp/private/work", text)
            self.assertNotIn("11111111-1111-1111-1111-111111111111", text)

    def test_an_existing_baseline_mismatch_is_a_regression(self):
        baseline = json.loads(
            (REPO / "harness-ci/drift/baseline/claude-code/2.1.281.json").read_text(encoding="utf-8")
        )
        current = json.loads(json.dumps(baseline))
        current["field_paths"].setdefault("user", []).append("somethingNew")

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            artifact = root / "artifacts" / "harness-2.1.281-Linux"
            artifact.mkdir(parents=True)
            (artifact / "shape.json").write_text(json.dumps(current), encoding="utf-8")
            baseline_dir = root / "baseline"
            baseline_dir.mkdir()
            (baseline_dir / "2.1.281.json").write_text(json.dumps(baseline), encoding="utf-8")
            status = nightly.main(
                [
                    "--artifacts",
                    str(root / "artifacts"),
                    "--baseline-dir",
                    str(baseline_dir),
                    "--reports-dir",
                    str(root / "reports"),
                    "--fixtures-root",
                    str(root / "fixtures"),
                ]
            )
            self.assertEqual(status, 2)
            self.assertFalse((root / "reports").exists())


if __name__ == "__main__":
    unittest.main()
