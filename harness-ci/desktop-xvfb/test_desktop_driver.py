import importlib.util
import json
import os
import socket
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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


class TreeNode(FakeNode):
    def __init__(self, role, name, children=None, bounds=None, text="", ranges=None):
        super().__init__(role, name, bounds=bounds)
        self._children = list(children or [])
        self._text = text
        self._ranges = ranges or {}

    def __iter__(self):
        return iter(self._children)

    def queryText(self):
        node = self

        class Text:
            characterCount = len(node._text)

            def getText(self, start, end):
                return node._text[start:end]

            def getRangeExtents(self, start, end, _coord):
                if (start, end) not in node._ranges:
                    raise RuntimeError("no range")
                return node._ranges[(start, end)]

        return Text()


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
        self.assertEqual(scenario["mock_scenario"], "../mock-server/scenarios/desktop_plain_qa.json")
        self.assertEqual(scenario["confidence"], 0.25)
        resolved = driver.resolve_mock_scenario(scenario["mock_scenario"])
        self.assertIsNotNone(resolved)
        assert resolved is not None
        self.assertTrue(resolved.is_file(), resolved)
        self.assertEqual(resolved.name, "desktop_plain_qa.json")
        mock_scenario = json.loads(resolved.read_text())
        plain = json.loads((resolved.parent / "plain_qa.json").read_text())
        self.assertNotIn("probes", plain)
        self.assertEqual(mock_scenario["probes"]["user_texts"], ["."])
        self.assertEqual(mock_scenario["probes"]["max_tokens_at_most"], 1)
        self.assertEqual(mock_scenario["steps"], plain["steps"])
        stale = (driver.HERE / "../../mock-server/scenarios/plain_qa.json").resolve()
        self.assertFalse(stale.exists())
        self.assertNotEqual(stale, resolved)

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

    def test_non_click_actions_are_dropped_on_buttons(self):
        class PressAndShow:
            nActions = 2

            def getName(self, index):
                return ["press", "show"][index]

        class ShowOnly:
            nActions = 1

            def getName(self, _index):
                return "show"

        both = FakeNode("push button", "Continue")
        both.queryAction = lambda: PressAndShow()
        only = FakeNode("push button", "Menu")
        only.queryAction = lambda: ShowOnly()
        kept = driver.candidate_from_node(both, 0, "")
        self.assertIsNotNone(kept)
        assert kept is not None
        self.assertEqual(kept.actions, ["click"])
        self.assertIsNone(driver.candidate_from_node(only, 1, ""))

    def test_editable_state_includes_a_section_and_static_text_does_not(self):
        class State:
            def __init__(self, flags):
                self.flags = flags

            def contains(self, flag):
                return flag in self.flags

        pyatspi = mock.Mock()
        pyatspi.STATE_SHOWING = "SHOWING"
        pyatspi.STATE_EDITABLE = "EDITABLE"
        pyatspi.DESKTOP_COORDS = 0
        section = FakeNode("section", "How can I help you today?", bounds=[10, 20, 30, 40])
        section.getState = lambda: State({"SHOWING", "EDITABLE"})
        label = FakeNode("text", "You're here!", bounds=[0, 0, 10, 10])
        label.getState = lambda: State({"SHOWING"})
        with mock.patch.dict(sys.modules, {"pyatspi": pyatspi}):
            promoted = driver.candidate_from_node(section, 0, "")
            ignored = driver.candidate_from_node(label, 1, "")
        self.assertIsNotNone(promoted)
        assert promoted is not None
        self.assertEqual(promoted.actions, ["click", "type"])
        self.assertEqual(promoted.role, "section")
        self.assertIsNone(ignored)

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


class ConfidenceAndChooseTests(unittest.TestCase):
    def test_scenario_confidence_applies_when_env_and_cli_are_unset(self):
        args = driver.parse_args(["--target", "claude-desktop", "--chooser", "jev"])
        with mock.patch.dict(os.environ, {"AGENTTAIL_JEV_CONFIDENCE": ""}):
            self.assertEqual(driver.resolve_confidence(args, {"confidence": 0.25}), 0.25)

    def test_env_and_cli_override_the_scenario_floor(self):
        args = driver.parse_args([])
        with mock.patch.dict(os.environ, {"AGENTTAIL_JEV_CONFIDENCE": "0.4"}):
            self.assertEqual(driver.resolve_confidence(args, {"confidence": 0.25}), 0.4)
        explicit = driver.parse_args(["--confidence", "0.9"])
        with mock.patch.dict(os.environ, {"AGENTTAIL_JEV_CONFIDENCE": "0.4"}):
            self.assertEqual(driver.resolve_confidence(explicit, {"confidence": 0.25}), 0.9)

    def test_desktop_launch_leaves_remote_debugging_off_unless_opted_in(self):
        plain = driver.desktop_launch_command(None)
        self.assertEqual(
            plain,
            [
                "claude-desktop",
                "--no-sandbox",
                "--force-renderer-accessibility",
                "--disable-gpu",
                "--enable-logging=stderr",
            ],
        )
        self.assertFalse(any("remote-debugging" in arg for arg in plain))
        opted = driver.desktop_launch_command(9223)
        self.assertIn("--remote-debugging-port=9223", opted)
        self.assertIn("--remote-debugging-address=127.0.0.1", opted)
        args = driver.parse_args(["--target", "standin"])
        self.assertIsNone(driver.desktop_cdp_port(args))
        desktop = driver.parse_args(["--target", "claude-desktop"])
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertIsNone(driver.desktop_cdp_port(desktop))
        with mock.patch.dict(os.environ, {"AGENTTAIL_CDP_PORT": ""}):
            self.assertIsNone(driver.desktop_cdp_port(desktop))
        with mock.patch.dict(os.environ, {"AGENTTAIL_CDP_PORT": "0"}):
            self.assertIsNone(driver.desktop_cdp_port(desktop))
        with mock.patch.dict(os.environ, {"AGENTTAIL_CDP_PORT": "9223"}):
            self.assertEqual(driver.desktop_cdp_port(desktop), 9223)
            self.assertIn("--remote-debugging-port=9223", driver.desktop_launch_command(driver.desktop_cdp_port(desktop)))

    def test_jev_waits_until_candidates_exist(self):
        now = 10.0
        self.assertFalse(driver.can_choose(None, now, 0, 0, 8))
        self.assertFalse(driver.can_choose({"candidates": []}, now, 0, 0, 8))
        self.assertFalse(driver.can_choose({"candidates": [candidate()]}, now, now, 0, 8))
        self.assertFalse(driver.can_choose({"candidates": [candidate()]}, now, 0, 8, 8))
        self.assertTrue(driver.can_choose({"candidates": [candidate()]}, now, 0, 0, 8))


class ComposerFallbackTests(unittest.TestCase):
    def document(self, bounds=(200, 40, 1200, 820), text="", ranges=None, children=None):
        return TreeNode("document web", "Claude", children=children, bounds=list(bounds), text=text, ranges=ranges)

    def test_placeholder_extents_become_the_type_target(self):
        text = "You're here!\nHow can I help you today?"
        needle = "how can i help you today"
        start = text.lower().find(needle)
        span = (start, start + len(needle))
        doc = self.document(text=text, ranges={span: (640, 560, 480, 22)})
        found = driver.composer_from_areas(driver.collect_web_areas(doc))
        self.assertEqual([item.name for item in found], ["Chat prompt", "Send prompt"])
        self.assertEqual(found[0].role, "entry")
        self.assertEqual(found[0].actions, ["type"])
        self.assertEqual(found[0].bounds, [640, 560, 480, 22])
        self.assertIsNone(found[0].node)
        self.assertIsNone(found[0].dom_socket)
        self.assertEqual(found[1].actions, ["click"])
        self.assertEqual(found[1].input_key, "Return")
        self.assertIsNone(found[1].dom_socket)

    def test_missing_text_range_clicks_the_upper_composer_card(self):
        doc_bounds = [200, 40, 1200, 820]
        doc = self.document(bounds=doc_bounds, text="How can I help you today?")
        found = driver.composer_from_areas(driver.collect_web_areas(doc))
        card = driver.scale_box(doc_bounds, *driver.COMPOSER_CARD)
        bounds = found[0].bounds
        center_x = bounds[0] + bounds[2] // 2
        center_y = bounds[1] + bounds[3] // 2
        self.assertEqual(bounds, driver.composer_type_bounds(doc_bounds))
        self.assertGreater(center_x, card[0])
        self.assertLess(center_x, card[0] + card[2])
        self.assertGreater(center_y, card[1])
        self.assertLess(center_y, card[1] + int(card[3] * 0.45))

    def test_welcome_continue_does_not_invent_a_composer(self):
        areas = driver.collect_web_areas(self.document())
        welcome = driver.merge_composer_candidates([candidate("push button", "Continue")], areas)
        self.assertEqual([item.name for item in welcome], ["Continue"])
        chat = driver.merge_composer_candidates([candidate("push button", "New")], areas)
        self.assertEqual([item.name for item in chat], ["New", "Chat prompt", "Send prompt"])

    def test_an_existing_editable_suppresses_the_estimated_composer(self):
        areas = driver.collect_web_areas(self.document())
        merged = driver.merge_composer_candidates(
            [candidate("entry", "Prompt", ["click", "type"], "c-entry")],
            areas,
        )
        self.assertEqual([item.name for item in merged], ["Prompt"])

    def test_child_text_inside_document_web_is_walked(self):
        text = "How can I help you today?"
        child = TreeNode("text", text, bounds=[610, 548, 420, 24], text=text, ranges={(0, len(text)): (610, 548, 420, 24)})

        class State:
            def contains(self, flag):
                return flag == "SHOWING"

        child.getState = lambda: State()
        doc = self.document(children=[child])
        frame = TreeNode("frame", "Claude", children=[TreeNode("push button", "New", bounds=[8, 8, 40, 24]), doc])
        pyatspi = mock.Mock()
        pyatspi.STATE_SHOWING = "SHOWING"
        pyatspi.STATE_EDITABLE = "EDITABLE"
        pyatspi.DESKTOP_COORDS = 0
        with mock.patch.dict(sys.modules, {"pyatspi": pyatspi}):
            found = driver.candidates_for_window(frame, "say hello", 80, None)
        typed = [item for item in found if "type" in item.actions]
        self.assertEqual([item.name for item in typed], ["Chat prompt"])
        self.assertEqual(typed[0].bounds, [610, 548, 420, 24])
        self.assertTrue(all(item.dom_socket is None for item in found))

    def test_overlapping_embedded_document_outside_the_window_is_kept(self):
        window = TreeNode("frame", "Claude", bounds=[0, 0, 1440, 900])
        page = TreeNode("document web", "Page", bounds=[180, 40, 1200, 820])
        other = TreeNode("frame", "Other", bounds=[2000, 0, 80, 80])
        desktop = TreeNode(
            "desktop frame",
            "",
            children=[
                TreeNode("application", "claude-desktop", children=[window, page]),
                TreeNode("application", "openbox", children=[other]),
            ],
        )
        pyatspi = mock.Mock()
        pyatspi.Registry.getDesktop.return_value = desktop
        pyatspi.DESKTOP_COORDS = 0
        with mock.patch.dict(sys.modules, {"pyatspi": pyatspi}):
            roots = driver.embedded_roots(window)
        self.assertIn(page, roots)
        self.assertNotIn(window, roots)
        self.assertNotIn(other, roots)

    def test_observation_records_document_web_text_and_bounds(self):
        text = "You're here! How can I help you today?"
        doc = self.document(bounds=(200, 40, 1000, 800), text=text)
        frame = TreeNode("frame", "Claude", children=[TreeNode("push button", "New"), doc])
        dump = driver.observation_text(frame)
        self.assertIn("document web:", dump)
        self.assertIn("How can I help you today?", dump)
        self.assertIn("bounds=[200, 40, 1000, 800]", dump)
        self.assertIn("push button: 'New'", dump)

    def test_xdotool_types_the_prompt_and_presses_enter(self):
        doc_bounds = [200, 40, 1200, 820]
        frame = TreeNode(
            "frame",
            "Claude",
            bounds=[0, 0, 1440, 900],
            children=[
                TreeNode("push button", "New", bounds=[8, 8, 40, 24]),
                self.document(bounds=doc_bounds),
            ],
        )
        found = driver.candidates_for_window(frame, "say hello", 80, None)
        observation = {"candidates": found, "window": "Claude"}
        typed_choice = driver.choose_scripted(
            observation,
            [{"action": "type", "role": "entry", "name": "Chat prompt"}],
            0,
            "say hello",
        )
        send_choice = driver.choose_scripted(
            observation,
            [{"action": "click", "role": "push button", "name": "Send prompt"}],
            0,
            "say hello",
        )
        self.assertEqual(typed_choice.text, "say hello")
        self.assertEqual(send_choice.candidate.input_key, "Return")
        calls = []

        def run(cmd, **_kwargs):
            calls.append(list(cmd))
            return mock.Mock(returncode=0)

        with tempfile.TemporaryDirectory() as temp, mock.patch.object(driver.subprocess, "run", side_effect=run):
            typed = driver.act(typed_choice, Path(temp))
            sent = driver.act(send_choice, Path(temp))
        x, y, width, height = typed_choice.candidate.bounds
        self.assertEqual(typed["method"], "xdotool-type")
        self.assertEqual(typed["text_length"], len("say hello"))
        self.assertEqual(calls[0][:3], ["xdotool", "mousemove", "--sync"])
        self.assertEqual(calls[0][3:5], [str(x + width // 2), str(y + height // 2)])
        self.assertEqual(calls[1], ["xdotool", "type", "--clearmodifiers", "--delay", "20", "--", "say hello"])
        self.assertEqual(sent["method"], "xdotool-key")
        self.assertEqual(sent["key"], "Return")
        self.assertEqual(calls[3], ["xdotool", "key", "--clearmodifiers", "Return"])
        self.assertFalse(any("remote-debugging" in arg for arg in driver.desktop_launch_command(None)))

    def test_opt_in_devtools_composer_wins_over_the_estimate(self):
        frame = TreeNode(
            "frame",
            "Claude",
            children=[
                TreeNode("push button", "New", bounds=[8, 8, 40, 24]),
                self.document(),
            ],
        )
        dom = driver.dom_nodes_to_candidates(
            [("ws://127.0.0.1:9223/devtools/page/1", dom_node("textbox", "How can I help you today?", 1, 40, 700, 600, 80))]
        )
        with mock.patch.object(driver, "dom_candidates_for_port", return_value=dom):
            found = driver.candidates_for_window(frame, "say hello", 80, 9223)
        typed = [item for item in found if "type" in item.actions]
        self.assertEqual([item.name for item in typed], ["How can I help you today?"])
        self.assertEqual(typed[0].dom_socket, "ws://127.0.0.1:9223/devtools/page/1")


def dom_node(kind, name, index, x, y, width, height):
    return {"kind": kind, "name": name, "index": index, "x": x, "y": y, "width": width, "height": height}


class DomCandidateTests(unittest.TestCase):
    def test_the_lower_composer_is_kept_and_enter_is_offered_without_a_send_button(self):
        nodes = [
            ("ws://127.0.0.1:9223/devtools/page/1", dom_node("textbox", "", 0, 0, 10, 800, 500)),
            ("ws://127.0.0.1:9223/devtools/page/1", dom_node("textbox", "How can I help you today?", 1, 40, 700, 600, 80)),
            ("ws://127.0.0.1:9223/devtools/page/1", dom_node("button", "New", 2, 0, 0, 40, 40)),
        ]
        found = driver.dom_nodes_to_candidates(nodes)
        self.assertEqual([item.name for item in found], ["How can I help you today?", "Send prompt"])
        self.assertEqual(found[0].actions, ["type"])
        self.assertEqual(found[0].dom_index, 1)
        self.assertEqual(found[1].dom_key, "Enter")
        self.assertEqual(found[0].public()["via"], "cdp")

    def test_a_labeled_send_button_replaces_the_enter_key(self):
        nodes = [
            ("ws://127.0.0.1:9223/devtools/page/1", dom_node("textbox", "", 0, 40, 700, 600, 80)),
            ("ws://127.0.0.1:9223/devtools/page/1", dom_node("button", "Send message", 3, 680, 720, 36, 36)),
        ]
        found = driver.dom_nodes_to_candidates(nodes)
        self.assertEqual([item.name for item in found], ["Chat prompt", "Send message"])
        self.assertNotIn("Send prompt", [item.name for item in found])

    def test_devtools_candidates_fill_the_gap_atspi_left(self):
        atspi = [candidate("push button", "New", ["click"], "c-new")]
        dom = driver.dom_nodes_to_candidates(
            [("ws://127.0.0.1:9223/devtools/page/1", dom_node("textbox", "Chat prompt", 0, 40, 700, 600, 80))]
        )
        merged = driver.merge_dom_candidates(atspi, dom)
        self.assertEqual([item.name for item in merged], ["New", "Chat prompt", "Send prompt"])

    def test_an_atspi_editable_suppresses_the_dom_textbox(self):
        atspi = [candidate("entry", "Prompt", ["click", "type"], "c-entry")]
        dom = driver.dom_nodes_to_candidates(
            [
                ("ws://127.0.0.1:9223/devtools/page/1", dom_node("textbox", "Chat prompt", 0, 40, 700, 600, 80)),
                ("ws://127.0.0.1:9223/devtools/page/1", dom_node("button", "Send message", 1, 680, 720, 36, 36)),
            ]
        )
        merged = driver.merge_dom_candidates(atspi, dom)
        self.assertEqual([item.name for item in merged], ["Prompt", "Send message"])

    def test_devtools_urls_must_be_loopback(self):
        with self.assertRaises(driver.DriverError):
            driver.ensure_loopback_ws("ws://openrouter.ai/devtools/page/1")
        driver.ensure_loopback_ws("ws://127.0.0.1:9223/devtools/page/1")


class DevToolsClientTests(unittest.TestCase):
    def test_type_and_discover_roundtrip_unmasked_frames(self):
        received = []

        def handler(message):
            received.append(message)
            if message["method"] == "Runtime.evaluate":
                if "insert" in message["params"]["expression"] or "focus" in message["params"]["expression"]:
                    return {"result": {"type": "boolean", "value": True}}
                return {
                    "result": {
                        "type": "object",
                        "value": [
                            dom_node("textbox", "How can I help you today?", 0, 40, 700, 600, 90),
                            dom_node("button", "Send message", 1, 680, 740, 32, 32),
                        ],
                    }
                }
            if message["method"] in {"Input.insertText", "Input.dispatchKeyEvent"}:
                return {}
            raise AssertionError(message["method"])

        ws_port = start_cdp_server(handler)
        http_port = start_json_server(ws_port)
        try:
            found = driver.discover_dom_candidates(http_port)
            self.assertEqual([item.name for item in found], ["How can I help you today?", "Send message"])
            self.assertTrue(any("__agenttailCdp" in item["params"]["expression"] for item in received))
            received.clear()
            driver.cdp_type_text(found[0].dom_socket, found[0].dom_index, 'say "hello"')
            self.assertEqual([item["method"] for item in received], ["Runtime.evaluate", "Input.insertText"])
            self.assertEqual(received[1]["params"]["text"], 'say "hello"')
            self.assertIn("[0]", received[0]["params"]["expression"])
            received.clear()
            driver.cdp_press_key(found[0].dom_socket, found[0].dom_index, "Enter")
            self.assertEqual(
                [item["method"] for item in received],
                ["Runtime.evaluate", "Input.dispatchKeyEvent", "Input.dispatchKeyEvent"],
            )
            self.assertEqual(received[1]["params"]["key"], "Enter")
            self.assertEqual(received[2]["params"]["type"], "keyUp")
        finally:
            stop_server(http_port)
            stop_cdp_server(ws_port)

    def test_discover_is_empty_when_devtools_is_closed(self):
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
        sock.close()
        self.assertEqual(driver.discover_dom_candidates(port), [])

    def test_click_and_type_record_the_devtools_method(self):
        entry = driver.Candidate(
            id="c-dom-entry-0",
            role="entry",
            name="Chat prompt",
            description="",
            actions=["type"],
            bounds=None,
            dom_index=4,
            dom_socket="ws://127.0.0.1:9223/devtools/page/1",
        )
        send = driver.Candidate(
            id="c-dom-enter",
            role="push button",
            name="Send prompt",
            description="",
            actions=["click"],
            bounds=None,
            dom_index=4,
            dom_socket=entry.dom_socket,
            dom_key="Enter",
        )
        with tempfile.TemporaryDirectory() as temp, mock.patch.object(driver, "cdp_type_text") as typed:
            record = driver.act(driver.Choice(entry, "type", "say hello", "jev"), Path(temp))
        typed.assert_called_once_with(entry.dom_socket, 4, "say hello")
        self.assertEqual(record["method"], "cdp-type")
        with tempfile.TemporaryDirectory() as temp, mock.patch.object(driver, "cdp_press_key") as pressed:
            record = driver.act(driver.Choice(send, "click", None, "jev"), Path(temp))
        pressed.assert_called_once_with(entry.dom_socket, 4, "Enter")
        self.assertEqual(record["method"], "cdp-key")


_SERVERS = {}
_CDP_SERVERS = {}


def start_cdp_server(handler):
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(8)
    port = server.getsockname()[1]

    def run():
        while True:
            try:
                conn, _addr = server.accept()
            except OSError:
                break
            conn.settimeout(5)
            try:
                data = b""
                while b"\r\n\r\n" not in data:
                    chunk = conn.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                if b"\r\n\r\n" not in data:
                    conn.close()
                    continue
                conn.sendall(
                    b"HTTP/1.1 101 Switching Protocols\r\n"
                    b"Upgrade: websocket\r\n"
                    b"Connection: Upgrade\r\n"
                    b"\r\n"
                )
                link = driver.CdpConnection(conn)
                while True:
                    raw = link.recv_text()
                    message = json.loads(raw)
                    result = handler(message)
                    link.send_text(json.dumps({"id": message["id"], "result": result}), masked=False)
            except Exception:
                pass
            finally:
                try:
                    conn.close()
                except Exception:
                    pass

    threading.Thread(target=run, daemon=True).start()
    _CDP_SERVERS[port] = server
    return port


def start_json_server(ws_port):
    body = json.dumps(
        [
            {
                "type": "page",
                "url": "http://127.0.0.1/chat",
                "webSocketDebuggerUrl": f"ws://127.0.0.1:{ws_port}/devtools/page/1",
            }
        ]
    ).encode()

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path.startswith("/json"):
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            else:
                self.send_error(404)

        def log_message(self, _format, *_args):
            return

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    _SERVERS[port] = server
    return port


def stop_cdp_server(port):
    server = _CDP_SERVERS.pop(port, None)
    if server is not None:
        server.close()


def stop_server(port):
    server = _SERVERS.pop(port, None)
    if server is not None:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    unittest.main()
