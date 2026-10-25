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

    def test_desktop_launch_opens_a_loopback_devtools_port(self):
        command = driver.desktop_launch_command(9223)
        self.assertIn("--remote-debugging-port=9223", command)
        self.assertIn("--remote-debugging-address=127.0.0.1", command)
        self.assertIn("--force-renderer-accessibility", command)
        self.assertNotIn("--remote-debugging-port=9223", driver.desktop_launch_command(None))
        args = driver.parse_args(["--target", "standin"])
        self.assertIsNone(driver.desktop_cdp_port(args))
        desktop = driver.parse_args(["--target", "claude-desktop"])
        with mock.patch.dict(os.environ, {"AGENTTAIL_CDP_PORT": "0"}):
            self.assertIsNone(driver.desktop_cdp_port(desktop))
        with mock.patch.dict(os.environ, {"AGENTTAIL_CDP_PORT": ""}):
            self.assertEqual(driver.desktop_cdp_port(desktop), 9223)

    def test_jev_waits_until_candidates_exist(self):
        now = 10.0
        self.assertFalse(driver.can_choose(None, now, 0, 0, 8))
        self.assertFalse(driver.can_choose({"candidates": []}, now, 0, 0, 8))
        self.assertFalse(driver.can_choose({"candidates": [candidate()]}, now, now, 0, 8))
        self.assertFalse(driver.can_choose({"candidates": [candidate()]}, now, 0, 8, 8))
        self.assertTrue(driver.can_choose({"candidates": [candidate()]}, now, 0, 0, 8))


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
