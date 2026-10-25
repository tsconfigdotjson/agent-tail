import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).with_name("desktop_driver.py")
SPEC = importlib.util.spec_from_file_location("desktop_driver", MODULE_PATH)
driver = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
import sys
sys.modules[SPEC.name] = driver
SPEC.loader.exec_module(driver)


class FakeAction:
    nActions = 1

    def getName(self, _index):
        return "click"

    def doAction(self, _index):
        return True


class FakeNode:
    def __init__(self, role, name, description="", bounds=None):
        self._role = role
        self.name = name
        self.description = description
        self._bounds = bounds

    def getRoleName(self):
        return self._role

    def queryAction(self):
        return FakeAction()

    def queryComponent(self):
        if self._bounds is None:
            raise RuntimeError("no component")
        box = mock.Mock()
        box.x, box.y, box.width, box.height = self._bounds
        return mock.Mock(getExtents=mock.Mock(return_value=box))


def candidate(role="push button", name="Send", actions=None, cid="c-1"):
    return driver.Candidate(
        id=cid,
        role=role,
        name=name,
        description="",
        actions=actions or ["click"],
        bounds=[0, 0, 10, 10],
        node=None,
    )


class ScenarioTests(unittest.TestCase):
    def test_default_standin_scenario_parses_and_is_honest(self):
        scenario = driver.load_scenario(driver.DEFAULT_SCENARIO)
        self.assertEqual(scenario["target"], "standin")
        self.assertEqual(scenario["chooser"], "scripted")
        self.assertIn("not Claude Desktop", scenario["description"])
        self.assertEqual(scenario["expect"]["end"], "done")
        self.assertEqual([step["action"] for step in scenario["script"]], ["type", "click"])

    def test_claude_desktop_scenario_uses_jev_and_a_mock(self):
        scenario = driver.load_scenario(Path(__file__).with_name("scenarios") / "claude_desktop_plain_qa.json")
        self.assertEqual(scenario["target"], "claude-desktop")
        self.assertEqual(scenario["chooser"], "jev")
        self.assertEqual(scenario["agents"], "claude_desktop,claude_code")
        self.assertTrue(scenario["mock_scenario"].endswith("plain_qa.json"))

    def test_invalid_script_action_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "bad.json"
            path.write_text(json.dumps({"name": "bad", "script": [{"action": "teleport"}]}))
            with self.assertRaisesRegex(driver.DriverError, "action must be click or type"):
                driver.load_scenario(path)


class CandidateTests(unittest.TestCase):
    def test_button_and_entry_are_actionable_but_window_chrome_is_not(self):
        with mock.patch.object(driver, "extents", return_value=[1, 2, 30, 40]):
            button = driver.candidate_from_node(FakeNode("push button", "Send"), 0, "")
            entry = driver.candidate_from_node(FakeNode("entry", "Prompt"), 1, "hello")
            close = driver.candidate_from_node(FakeNode("push button", "Close"), 2, "")
        self.assertEqual(button.role, "push button")
        self.assertEqual(button.actions, ["click"])
        self.assertEqual(button.bounds, [1, 2, 30, 40])
        self.assertEqual(entry.actions, ["click", "type"])
        self.assertIsNone(close)

    def test_scripted_choice_matches_role_name_and_action(self):
        observation = {
            "candidates": [
                candidate("entry", "Prompt", ["click", "type"], "c-entry"),
                candidate("push button", "Send", ["click"], "c-send"),
            ],
            "window": "agenttail desktop stand-in",
        }
        typed = driver.choose_scripted(
            observation,
            [{"action": "type", "role": "entry", "name": "Prompt", "text": "hi {{prompt}}"}],
            0,
            "there",
        )
        clicked = driver.choose_scripted(
            observation,
            [{"action": "click", "role": "push button", "name": "Send"}],
            0,
            "there",
        )
        self.assertEqual((typed.action, typed.text, typed.candidate.id), ("type", "hi there", "c-entry"))
        self.assertEqual((clicked.action, clicked.candidate.id), ("click", "c-send"))

    def test_scripted_choice_reports_visible_candidates_on_miss(self):
        observation = {"candidates": [candidate()], "window": "w"}
        with self.assertRaisesRegex(driver.DriverError, "visible: push button:'Send'"):
            driver.choose_scripted(observation, [{"action": "click", "role": "entry", "name": "Prompt"}], 0, "")


class ChooserTests(unittest.TestCase):
    def observation(self):
        return {
            "window": "Claude",
            "candidates": [
                candidate("entry", "Message", ["click", "type"], "c-entry"),
                candidate("push button", "Send", ["click"], "c-send"),
            ],
        }

    def test_jev_request_is_typed_choice_with_the_pinned_model_and_ui_actions(self):
        body, lookup = driver.jev_request(
            "typesafe/jev-1.14",
            "send a prompt",
            "say hello",
            self.observation(),
            [{"action": "click", "candidate": {"name": "old"}}],
        )
        self.assertEqual(body["model"], "typesafe/jev-1.14")
        self.assertEqual(body["questions"]["action"]["type"], "choice")
        self.assertEqual(body["state"]["prompt_to_type"], "say hello")
        self.assertIn("c-entry:type", body["questions"]["action"]["criteria"])
        self.assertIn("c-send:click", body["questions"]["action"]["criteria"])
        self.assertEqual(lookup["c-send:click"][1], "click")

    def test_jev_without_a_key_stops_instead_of_falling_back_to_a_writer(self):
        with tempfile.TemporaryDirectory() as temp, mock.patch.dict(os.environ, {"OPENROUTER_API_KEY": ""}):
            with self.assertRaisesRegex(driver.DriverError, "No writer LLM"):
                driver.choose_jev(
                    self.observation(), "goal", "prompt", [], "typesafe/jev-1.14", 0.5, Path(temp) / "jev.jsonl"
                )

    def test_jev_low_confidence_is_not_acted_on(self):
        payload = {
            "answers": {"action": {"choice": "c-send:click", "confidence": 0.2, "probabilities": {}}},
            "model": "typesafe/jev-1.14",
        }
        with tempfile.TemporaryDirectory() as temp, mock.patch.dict(os.environ, {"OPENROUTER_API_KEY": "test-key"}):
            response = mock.MagicMock()
            response.__enter__.return_value.read.return_value = json.dumps(payload).encode()
            with mock.patch("urllib.request.urlopen", return_value=response):
                with self.assertRaisesRegex(driver.DriverError, "below"):
                    driver.choose_jev(
                        self.observation(), "goal", "prompt", [], "typesafe/jev-1.14", 0.5, Path(temp) / "jev.jsonl"
                    )


class ScoringAndEnvironmentTests(unittest.TestCase):
    def test_no_session_is_idle_and_thinking_is_working(self):
        self.assertEqual(driver.classify([])[0], "idle_no_session")
        self.assertEqual(driver.classify([{"activity": "thinking", "last_write": 1}])[0], "working")
        self.assertEqual(driver.classify([{"activity": "done", "last_write": 1}])[0], "done")

    def test_newest_session_wins(self):
        activity, session = driver.classify(
            [
                {"activity": "done", "last_write": 1},
                {"activity": "thinking", "last_write": 5},
            ]
        )
        self.assertEqual(activity, "working")
        self.assertEqual(session["last_write"], 5)

    def test_writer_and_anthropic_keys_are_scrubbed_from_the_app_environment(self):
        with mock.patch.dict(
            os.environ,
            {
                "ANTHROPIC_API_KEY": "must-not-leak",
                "CLICKER_WRITER_API_KEY": "must-not-leak",
                "CLICKER_WRITER_BASE_URL": "https://example.invalid",
                "OPENROUTER_API_KEY": "classifier-only",
            },
        ):
            env = driver.child_environment(Path("/home/claude"))
        self.assertNotIn("ANTHROPIC_API_KEY", env)
        self.assertNotIn("CLICKER_WRITER_API_KEY", env)
        self.assertNotIn("CLICKER_WRITER_BASE_URL", env)
        self.assertEqual(env["OPENROUTER_API_KEY"], "classifier-only")
        self.assertEqual(env["HOME"], "/home/claude")


if __name__ == "__main__":
    unittest.main()
