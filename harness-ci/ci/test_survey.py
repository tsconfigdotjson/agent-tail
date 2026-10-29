import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import survey


def write_transcript(root: Path, records: list[dict]) -> Path:
    run = root / "run"
    transcripts = run / "transcripts"
    transcripts.mkdir(parents=True)
    (transcripts / "session.jsonl").write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )
    (run / "score.json").write_text("{}", encoding="utf-8")
    return run


def base_records() -> list[dict]:
    return [
        {
            "type": "user",
            "uuid": "11111111-1111-1111-1111-111111111111",
            "timestamp": "2026-01-01T00:00:00Z",
            "sessionId": "session-a",
            "cwd": "/tmp/home/work",
            "message": {"role": "user", "content": "hello"},
        },
        {
            "type": "assistant",
            "uuid": "22222222-2222-2222-2222-222222222222",
            "timestamp": "2026-01-01T00:00:01Z",
            "sessionId": "session-a",
            "message": {
                "content": [{"type": "text", "text": "hello back"}],
                "stop_reason": "end_turn",
            },
        },
    ]


class ShapeSurveyTests(unittest.TestCase):
    def test_ids_timestamps_and_content_values_are_not_drift(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            old_run = write_transcript(root / "old", base_records())
            changed = base_records()
            changed[0]["uuid"] = "99999999-9999-9999-9999-999999999999"
            changed[0]["timestamp"] = "2027-02-02T02:02:02Z"
            changed[0]["sessionId"] = "session-b"
            changed[0]["cwd"] = "/tmp/another/work"
            changed[0]["message"]["content"] = "a different prompt"
            changed[1]["message"]["content"] = [{"type": "text", "text": "different reply"}]
            new_run = write_transcript(root / "new", changed)

            old = survey.survey_runs([old_run], "claude_code", "1.0.0", "linux")
            new = survey.survey_runs([new_run], "claude_code", "1.0.1", "linux")
            diff = survey.shape_diff(old, new)
            self.assertFalse(survey.has_drift(diff), diff)
            self.assertIn("No shape changes.", survey.render_report(old, new, diff))

    def test_a_new_field_is_drift(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            old_run = write_transcript(root / "old", base_records())
            changed = base_records()
            changed[0]["turnPosition"] = {"promptIndex": 1}
            new_run = write_transcript(root / "new", changed)

            old = survey.survey_runs([old_run], "claude_code", "1.0.0", "linux")
            new = survey.survey_runs([new_run], "claude_code", "1.0.1", "linux")
            diff = survey.shape_diff(old, new)
            self.assertTrue(survey.has_drift(diff))
            self.assertIn("turnPosition", diff["added_fields"]["field_paths"]["user"])
            self.assertIn("turnPosition", survey.render_report(old, new, diff))

    def test_a_new_content_block_type_is_drift(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            old_run = write_transcript(root / "old", base_records())
            changed = base_records()
            changed[1]["message"]["content"].append({"type": "new_block"})
            new_run = write_transcript(root / "new", changed)

            old = survey.survey_runs([old_run], "claude_code", "1.0.0", "linux")
            new = survey.survey_runs([new_run], "claude_code", "1.0.1", "linux")
            diff = survey.shape_diff(old, new)
            self.assertTrue(survey.has_drift(diff))
            self.assertIn("new_block", diff["added_fields"]["content_types"]["assistant.content"])

    def test_capture_uses_the_external_redactor_when_configured(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            run = write_transcript(root, base_records())
            fake = root / "agent-tail"
            fake.write_text(
                "#!/bin/sh\n"
                "report=''\n"
                "input=''\n"
                "while [ \"$#\" -gt 0 ]; do\n"
                "  case \"$1\" in --report) shift; report=\"$1\" ;; esac\n"
                "  input=\"$1\"; shift\n"
                "done\n"
                "printf '%s\\n' '{\"redacted\":true}' > \"$input\"\n"
                "printf '%s\\n' '{}' > \"$report\"\n",
                encoding="utf-8",
            )
            fake.chmod(0o755)
            with mock.patch.dict(os.environ, {"AGENT_TAIL_BIN": str(fake)}):
                target = survey.capture_fixture(run, root / "fixtures", "claude_code", "1.0.0", "linux")
            self.assertIsNotNone(target)
            self.assertEqual((target / "input" / "session.jsonl").read_text(encoding="utf-8"), '{"redacted":true}\n')
            self.assertTrue((target / "redaction.json").is_file())

    def test_capture_does_not_treat_cache_control_path_arrays_as_file_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            records = base_records()
            records.append(
                {
                    "type": "api-request",
                    "cacheControl": [
                        {"path": ["system", 0], "value": {"type": "ephemeral"}},
                        {"path": ["messages", 0, "content", 0], "value": {"type": "ephemeral"}},
                    ],
                }
            )
            run = write_transcript(root, records)
            target = survey.capture_fixture(run, root / "fixtures", "claude_code", "1.0.0", "linux")
            self.assertIsNotNone(target)
            text = (target / "input" / "session.jsonl").read_text(encoding="utf-8")
            captured = json.loads(text.splitlines()[-1])
            self.assertEqual(captured["cacheControl"][0]["path"], ["system", 0])
            self.assertEqual(captured["cacheControl"][1]["path"], ["messages", 0, "content", 0])
            assistant = json.loads(text.splitlines()[1])
            self.assertIn("content", assistant["message"], "the assistant message keeps its content key")

    def test_capture_redacts_ids_and_temp_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            records = base_records()
            records.append({"type": "file-history-snapshot", "snapshot": {"/tmp/private/file.txt": {}}})
            run = write_transcript(root, records)
            out = root / "fixtures"
            target = survey.capture_fixture(run, out, "claude_code", "1.0.0", "linux")
            self.assertIsNotNone(target)
            text = (target / "input" / "session.jsonl").read_text(encoding="utf-8")
            self.assertNotIn("11111111-1111-1111-1111-111111111111", text)
            self.assertNotIn("/tmp/home/work", text)
            self.assertNotIn("/tmp/private/file.txt", text)
            self.assertIn("<path-", text)
            self.assertIn("<id-", text)
            meta = (target / "meta.toml").read_text(encoding="utf-8")
            self.assertIn('capture = "ci-scripted"', meta)
            self.assertIn('version = "1.0.0"', meta)


if __name__ == "__main__":
    unittest.main()
