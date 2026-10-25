#!/usr/bin/env python3
"""Observe -> choose -> act -> score loop for a desktop under Xvfb.

The chooser is either Jev through OpenRouter (the production path) or a small
scenario-scripted chooser used only by the desktop stand-in smoke test.  There is
no writer LLM here: our code performs every click/keystroke, and `agenttail`
scores the disk artifact.  A screenshot is kept as evidence but is never used as
the scorer.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
DEFAULT_SCENARIO = HERE / "scenarios" / "standin_plain_qa.json"
JEV_ENDPOINT = "https://openrouter.ai/api/alpha/decisions"
JEV_MODEL = "typesafe/jev-1.14"
DEFAULT_CONFIDENCE = 0.55
POLL_SECONDS = 0.20
ACTION_GAP_SECONDS = 0.55
CHROME_BUTTONS = {"Close", "Minimize", "Maximize", "Restore"}
ACTIONABLE_ROLES = {
    "push button",
    "toggle button",
    "menu item",
    "link",
    "check box",
    "radio button",
    "page tab",
    "combo box",
    "entry",
    "password text",
}
EDITABLE_ROLES = {"entry", "password text", "document text"}
TEXT_ROLES = {"text"}
DEFAULT_CDP_PORT = 9223
_DOM_CACHE: dict[str, Any] = {"port": None, "at": 0.0, "candidates": []}
COMPOSER_EXPRESSION = r"""
(() => {
  const found = [];
  window.__agenttailCdp = [];
  const seen = new Set();
  function clean(value) {
    return String(value || "").replace(/\s+/g, " ").trim().slice(0, 80);
  }
  function textboxName(el) {
    return clean(el.getAttribute("aria-label") || el.getAttribute("placeholder") || el.getAttribute("data-placeholder") || el.getAttribute("aria-placeholder") || "");
  }
  function isTextbox(el) {
    const tag = el.tagName;
    if (tag === "TEXTAREA") return true;
    if (tag === "INPUT") {
      const type = (el.getAttribute("type") || "text").toLowerCase();
      return type === "text" || type === "search" || type === "";
    }
    if (el.getAttribute("role") === "textbox") return true;
    return !!el.isContentEditable;
  }
  function consider(el) {
    if (!el || seen.has(el) || el.nodeType !== 1) return;
    seen.add(el);
    const rect = el.getBoundingClientRect();
    const width = Math.round(rect.width);
    const height = Math.round(rect.height);
    if (width < 8 || height < 8) return;
    let style = null;
    try { style = window.getComputedStyle(el); } catch (e) { style = null; }
    if (style && (style.visibility === "hidden" || style.display === "none")) return;
    const textbox = isTextbox(el);
    const role = el.getAttribute("role");
    const button = !textbox && (el.tagName === "BUTTON" || role === "button");
    if (!textbox && !button) return;
    const index = window.__agenttailCdp.length;
    window.__agenttailCdp.push(el);
    found.push({
      kind: textbox ? "textbox" : "button",
      name: textbox ? textboxName(el) : clean(el.getAttribute("aria-label") || el.getAttribute("title") || ""),
      index: index,
      x: Math.round(rect.x),
      y: Math.round(rect.y),
      width: width,
      height: height
    });
  }
  function walk(root) {
    if (!root || !root.querySelectorAll) return;
    for (const el of root.querySelectorAll("textarea, input, button, [contenteditable], [role='textbox'], [role='button']")) consider(el);
    for (const el of root.querySelectorAll("*")) {
      if (el.shadowRoot) walk(el.shadowRoot);
      if (el.tagName === "IFRAME") {
        try { if (el.contentDocument) walk(el.contentDocument); } catch (e) {}
      }
    }
  }
  walk(document);
  return found.slice(0, 40);
})()
"""


class DriverError(RuntimeError):
    pass


@dataclass
class Candidate:
    id: str
    role: str
    name: str
    description: str
    actions: list[str]
    bounds: list[int] | None
    node: Any = field(default=None, repr=False)
    dom_index: int | None = None
    dom_socket: str | None = None
    dom_key: str | None = None

    def public(self) -> dict[str, Any]:
        value = {
            "id": self.id,
            "role": self.role,
            "name": self.name,
            "description": self.description,
            "actions": self.actions,
            "bounds": self.bounds,
        }
        if self.dom_socket:
            value["via"] = "cdp"
        return value


@dataclass
class Choice:
    candidate: Candidate
    action: str
    text: str | None
    source: str
    confidence: float | None = None
    detail: dict[str, Any] = field(default_factory=dict)


def sleep(seconds: float) -> None:
    time.sleep(seconds)


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    tmp.replace(path)


def append_jsonl(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True) + "\n")


def load_scenario(path: Path | None) -> dict[str, Any]:
    if path is None:
        return {}
    try:
        scenario = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise DriverError(f"read scenario {path}: {error}") from error
    if not isinstance(scenario, dict):
        raise DriverError(f"scenario {path} must be a JSON object")
    if not isinstance(scenario.get("name", "unnamed"), str):
        raise DriverError("scenario name must be a string")
    script = scenario.get("script", [])
    if not isinstance(script, list):
        raise DriverError("scenario script must be a list")
    for index, step in enumerate(script):
        if not isinstance(step, dict):
            raise DriverError(f"script step {index} must be an object")
        action = step.get("action")
        if action not in {"click", "type"}:
            raise DriverError(f"script step {index}: action must be click or type")
        if not isinstance(step.get("role", ""), str) or not isinstance(step.get("name", ""), str):
            raise DriverError(f"script step {index}: role/name must be strings")
    return scenario


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", type=Path, help="scenario JSON (default: stand-in plain QA)")
    parser.add_argument("--target", choices=["standin", "claude-desktop"], default="standin")
    parser.add_argument("--chooser", choices=["scripted", "jev"], default=None)
    parser.add_argument("--probe-only", action="store_true", help="launch and observe, do not drive")
    parser.add_argument("--out", type=Path, default=Path(os.environ.get("AGENTTAIL_OUT", "/out")))
    parser.add_argument("--home", type=Path, default=Path(os.environ.get("HOME", "/home/claude")))
    parser.add_argument("--agenttail", type=Path, default=Path(os.environ.get("AGENTTAIL_BIN", "/repo/target/release/agenttail")))
    parser.add_argument("--mock-server", type=Path, default=Path(os.environ.get("AGENTTAIL_MOCK_SERVER", "/repo/harness-ci/target/release/mock-server")))
    parser.add_argument("--mock-scenario", type=Path, default=None)
    parser.add_argument("--mock-addr", default="127.0.0.1:8765")
    parser.add_argument("--agents", default=None, help="agenttail harness ids to score (default: scenario or claude_desktop)")
    parser.add_argument("--window-title", default=None)
    parser.add_argument("--goal", default=None)
    parser.add_argument("--prompt", default=None)
    parser.add_argument("--timeout", type=float, default=None)
    parser.add_argument("--max-steps", type=int, default=None)
    parser.add_argument(
        "--confidence",
        type=float,
        default=None,
        help="Jev confidence floor. Env AGENTTAIL_JEV_CONFIDENCE, else the scenario, else 0.55.",
    )
    parser.add_argument("--jev-model", default=os.environ.get("AGENTTAIL_JEV_MODEL", JEV_MODEL))
    return parser.parse_args(argv)


def scenario_value(
    scenario: dict[str, Any], args: argparse.Namespace, key: str, default: Any, cli_key: str | None = None
) -> Any:
    cli = getattr(args, cli_key or key, None)
    return cli if cli is not None else scenario.get(key, default)


def resolve_confidence(args: argparse.Namespace, scenario: dict[str, Any]) -> float:
    if args.confidence is not None:
        return float(args.confidence)
    raw = os.environ.get("AGENTTAIL_JEV_CONFIDENCE", "").strip()
    if raw:
        try:
            return float(raw)
        except ValueError as error:
            raise DriverError(f"AGENTTAIL_JEV_CONFIDENCE must be a number, got {raw!r}") from error
    if scenario.get("confidence") is not None:
        return float(scenario["confidence"])
    return DEFAULT_CONFIDENCE


def can_choose(
    observation: dict[str, Any] | None,
    now: float,
    last_choice_at: float,
    step_index: int,
    max_steps: int,
) -> bool:
    if observation is None or not observation.get("candidates"):
        return False
    if step_index >= max_steps:
        return False
    if now - last_choice_at < ACTION_GAP_SECONDS:
        return False
    return True


def mock_scenario_candidates(mock_scenario: str | Path) -> list[Path]:
    raw = Path(mock_scenario)
    if raw.is_absolute():
        return [raw]
    return [
        HERE / raw,
        Path("/repo/harness-ci/desktop-xvfb") / raw,
        Path.cwd() / raw,
    ]


def resolve_mock_scenario(mock_scenario: str | Path) -> Path | None:
    for path in mock_scenario_candidates(mock_scenario):
        if path.exists():
            return path.resolve()
    return None


def desktop_cdp_port(args: argparse.Namespace) -> int | None:
    if args.target != "claude-desktop":
        return None
    raw = os.environ.get("AGENTTAIL_CDP_PORT", "").strip()
    if not raw:
        return DEFAULT_CDP_PORT
    if raw == "0":
        return None
    try:
        port = int(raw)
    except ValueError as error:
        raise DriverError(f"AGENTTAIL_CDP_PORT must be an integer, got {raw!r}") from error
    if port == 0:
        return None
    if port < 0 or port > 65535:
        raise DriverError(f"AGENTTAIL_CDP_PORT is out of range: {port}")
    return port


def desktop_launch_command(cdp_port: int | None) -> list[str]:
    command = [
        "claude-desktop",
        "--no-sandbox",
        "--force-renderer-accessibility",
        "--disable-gpu",
        "--enable-logging=stderr",
    ]
    if cdp_port:
        command.extend(
            [
                f"--remote-debugging-port={cdp_port}",
                "--remote-debugging-address=127.0.0.1",
                "--remote-allow-origins=*",
            ]
        )
    return command


def extents(node: Any) -> list[int] | None:
    try:
        import pyatspi

        box = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        return [int(box.x), int(box.y), int(box.width), int(box.height)]
    except Exception:
        return None


def node_actions(node: Any) -> list[str]:
    actions: list[str] = []
    try:
        action = node.queryAction()
        for index in range(action.nActions):
            name = (action.getName(index) or "").strip().lower()
            actions.append("click" if name in {"click", "press", "activate", "toggle"} else name or "click")
    except Exception:
        pass
    return actions or ["click"]


def state_contains(node: Any, state_name: str) -> bool | None:
    try:
        import pyatspi
    except ImportError:
        return None
    flag = getattr(pyatspi, state_name, None)
    if flag is None:
        return None
    try:
        return bool(node.getState().contains(flag))
    except Exception:
        return None


def node_is_editable(node: Any, role: str) -> bool:
    if role in EDITABLE_ROLES:
        return True
    editable = state_contains(node, "STATE_EDITABLE")
    if editable is True:
        return True
    if role in TEXT_ROLES and editable is None:
        return True
    return False


def candidate_from_node(node: Any, index: int, prompt: str) -> Candidate | None:
    try:
        role = node.getRoleName()
        name = (node.name or "").strip()
        description = (node.description or "").strip()
    except Exception:
        return None
    if role in {"frame", "window", "application"} or name in CHROME_BUTTONS:
        return None
    editable = node_is_editable(node, role)
    if role not in ACTIONABLE_ROLES and not editable:
        return None
    if not name and not editable:
        return None
    showing = state_contains(node, "STATE_SHOWING")
    if showing is False:
        return None
    actions = node_actions(node)
    if editable:
        actions = [action for action in actions if action == "click"] or ["click"]
        actions.append("type")
    else:
        actions = [action for action in actions if action == "click"]
    if not actions:
        return None
    bounds = extents(node)
    digest = hashlib.sha1(
        f"{index}|{role}|{name}|{description}|{bounds}".encode("utf-8", "replace")
    ).hexdigest()[:10]
    return Candidate(
        id=f"c-{digest}",
        role=role,
        name=name,
        description=description,
        actions=actions,
        bounds=bounds,
        node=node,
    )


def walk_candidates(node: Any, prompt: str, found: list[Candidate], budget: int, depth: int = 0) -> None:
    if len(found) >= budget or depth > 18:
        return
    candidate = candidate_from_node(node, len(found), prompt)
    if candidate is not None:
        found.append(candidate)
    try:
        children = list(node)
    except Exception:
        children = []
    for child in children:
        walk_candidates(child, prompt, found, budget, depth + 1)


def find_windows(window_title: str) -> list[Any]:
    try:
        import pyatspi
    except ImportError as error:
        raise DriverError("python3-pyatspi is not installed; AT-SPI observation is unavailable") from error
    windows: list[tuple[bool, bool, Any]] = []
    try:
        desktop = pyatspi.Registry.getDesktop(0)
    except Exception as error:
        raise DriverError(f"cannot read the AT-SPI desktop: {error}") from error
    for app in desktop:
        try:
            for child in app:
                try:
                    name = child.name or ""
                    role = child.getRoleName()
                    state = child.getState()
                except Exception:
                    continue
                if role not in {"frame", "window", "dialog"} or window_title.lower() not in name.lower():
                    continue
                try:
                    if not state.contains(pyatspi.STATE_SHOWING):
                        continue
                except Exception:
                    pass
                try:
                    active = state.contains(pyatspi.STATE_ACTIVE)
                    focused = state.contains(pyatspi.STATE_FOCUSED)
                except Exception:
                    active = focused = False
                windows.append((active, focused, child))
        except Exception:
            continue
    # Prefer the active/focused window. A welcome dialog and the home window can
    # both be in the tree; choosing a hidden home button would be wrong.
    windows.sort(key=lambda item: (item[0], item[1]), reverse=True)
    return [child for _active, _focused, child in windows]


def observation_text(window: Any) -> str:
    lines: list[str] = []

    def walk(node: Any, depth: int = 0) -> None:
        if depth > 12:
            return
        try:
            name = (node.name or "").strip()
            role = node.getRoleName()
            if name or role in {"entry", "push button"}:
                lines.append("  " * depth + f"{role}: {name!r}")
            for child in node:
                walk(child, depth + 1)
        except Exception:
            return

    walk(window)
    return "\n".join(lines)


def screenshot(path: Path) -> bool:
    scrot = shutil.which("scrot")
    if not scrot:
        return False
    try:
        path.unlink(missing_ok=True)
        result = subprocess.run([scrot, "--silent", str(path)], capture_output=True, timeout=10)
        return result.returncode == 0 and path.exists()
    except Exception:
        return False


def ensure_loopback_ws(url: str) -> None:
    parsed = urllib.parse.urlparse(url)
    host = (parsed.hostname or "").lower()
    if parsed.scheme != "ws" or host not in {"127.0.0.1", "localhost", "::1"}:
        raise DriverError(f"refusing non-loopback DevTools url {url}")


class CdpConnection:
    def __init__(self, sock: socket.socket, leftover: bytes = b"") -> None:
        self.sock = sock
        self.buf = leftover

    def close(self) -> None:
        try:
            self.sock.close()
        except Exception:
            pass

    def _recv_exact(self, size: int) -> bytes:
        while len(self.buf) < size:
            chunk = self.sock.recv(max(size - len(self.buf), 4096))
            if not chunk:
                raise DriverError("DevTools websocket closed")
            self.buf += chunk
        out = self.buf[:size]
        self.buf = self.buf[size:]
        return out

    def send_text(self, text: str, masked: bool = True) -> None:
        payload = text.encode()
        header = bytearray([0x81])
        length = len(payload)
        mask_bit = 0x80 if masked else 0
        if length < 126:
            header.append(mask_bit | length)
        elif length < 65536:
            header.append(mask_bit | 126)
            header.extend(length.to_bytes(2, "big"))
        else:
            header.append(mask_bit | 127)
            header.extend(length.to_bytes(8, "big"))
        if masked:
            mask = os.urandom(4)
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
            self.sock.sendall(bytes(header) + mask + payload)
        else:
            self.sock.sendall(bytes(header) + payload)

    def recv_text(self) -> str:
        while True:
            first, second = self._recv_exact(2)
            fin = first & 0x80
            opcode = first & 0x0F
            masked = second & 0x80
            length = second & 0x7F
            if length == 126:
                length = int.from_bytes(self._recv_exact(2), "big")
            elif length == 127:
                length = int.from_bytes(self._recv_exact(8), "big")
            if length > 2_000_000:
                raise DriverError("DevTools websocket frame is too large")
            mask = self._recv_exact(4) if masked else b""
            payload = self._recv_exact(length)
            if masked:
                payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
            if opcode == 0x8:
                raise DriverError("DevTools websocket closed")
            if opcode == 0x9:
                continue
            if opcode == 0x1 and fin:
                return payload.decode()
            if opcode in {0x0, 0x1}:
                raise DriverError("DevTools websocket frame was fragmented")


def cdp_connect(ws_url: str, timeout: float = 5.0) -> CdpConnection:
    ensure_loopback_ws(ws_url)
    parsed = urllib.parse.urlparse(ws_url)
    host = parsed.hostname or "127.0.0.1"
    port = parsed.port
    if port is None:
        raise DriverError(f"DevTools url has no port: {ws_url}")
    path = parsed.path or "/"
    if parsed.query:
        path = f"{path}?{parsed.query}"
    try:
        sock = socket.create_connection((host, port), timeout=timeout)
    except OSError as error:
        raise DriverError(f"DevTools connection failed: {error}") from error
    sock.settimeout(timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Origin: http://{host}:{port}\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "\r\n"
    )
    try:
        sock.sendall(request.encode())
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                raise DriverError("DevTools websocket upgrade failed")
            data += chunk
            if len(data) > 65536:
                raise DriverError("DevTools websocket upgrade failed")
    except DriverError:
        sock.close()
        raise
    except OSError as error:
        sock.close()
        raise DriverError(f"DevTools connection failed: {error}") from error
    head, leftover = data.split(b"\r\n\r\n", 1)
    status = head.split(b"\r\n", 1)[0]
    if b"101" not in status:
        sock.close()
        raise DriverError(f"DevTools websocket upgrade failed: {status.decode('latin1', 'replace')[:200]}")
    return CdpConnection(sock, leftover)


def cdp_roundtrip(conn: CdpConnection, msg_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
    conn.send_text(json.dumps({"id": msg_id, "method": method, "params": params}))
    while True:
        message = json.loads(conn.recv_text())
        if message.get("id") != msg_id:
            continue
        if "error" in message:
            raise DriverError(f"DevTools {method} failed: {message['error']}")
        result = message.get("result", {})
        return result if isinstance(result, dict) else {}


def cdp_target_sockets(port: int, timeout: float = 0.4) -> list[str]:
    for path in ("/json/list", "/json"):
        url = f"http://127.0.0.1:{port}{path}"
        try:
            with urllib.request.urlopen(url, timeout=timeout) as response:
                payload = json.loads(response.read().decode())
        except Exception:
            continue
        if not isinstance(payload, list):
            continue
        sockets: list[str] = []
        for target in payload:
            if not isinstance(target, dict):
                continue
            if target.get("type") not in {"page", "iframe", "webview"}:
                continue
            page_url = str(target.get("url") or "")
            if page_url.startswith("devtools://"):
                continue
            ws_url = target.get("webSocketDebuggerUrl")
            if isinstance(ws_url, str) and ws_url.startswith("ws://"):
                sockets.append(ws_url)
        if sockets:
            return sockets[:8]
    return []


def cdp_composer_nodes(ws_url: str, timeout: float = 2.0) -> list[Any]:
    conn = cdp_connect(ws_url, timeout)
    try:
        result = cdp_roundtrip(
            conn,
            1,
            "Runtime.evaluate",
            {"expression": COMPOSER_EXPRESSION, "returnByValue": True},
        )
    finally:
        conn.close()
    value = result.get("result", {}).get("value")
    return value if isinstance(value, list) else []


def _box_of(node: dict[str, Any]) -> list[int] | None:
    try:
        x = int(node["x"])
        y = int(node["y"])
        width = int(node["width"])
        height = int(node["height"])
    except (KeyError, TypeError, ValueError):
        return None
    if width <= 0 or height <= 0:
        return None
    return [x, y, width, height]


def _usable_box(bounds: list[int] | None) -> bool:
    return bounds is not None and bounds[2] >= 40 and bounds[3] >= 16


def _bottom(candidate: Candidate) -> int:
    if not candidate.bounds:
        return -1
    return candidate.bounds[1] + candidate.bounds[3]


def choose_textbox(textboxes: list[Candidate]) -> Candidate | None:
    usable = [item for item in textboxes if _usable_box(item.bounds)]
    pool = usable or textboxes
    if not pool:
        return None
    short = [item for item in pool if item.bounds and item.bounds[3] <= 240]
    return max(short or pool, key=_bottom)


def is_send_name(name: str) -> bool:
    lowered = name.strip().lower()
    return "send" in lowered or lowered in {"submit", "submit message"}


def dom_nodes_to_candidates(tagged: list[tuple[str, dict[str, Any]]]) -> list[Candidate]:
    textboxes: list[Candidate] = []
    sends: list[Candidate] = []
    for ws_url, node in tagged:
        if not isinstance(node, dict):
            continue
        index = node.get("index")
        if not isinstance(index, int) or index < 0:
            continue
        kind = node.get("kind")
        name = str(node.get("name") or "").strip()
        bounds = _box_of(node)
        if kind == "textbox":
            textboxes.append(
                Candidate(
                    id=f"c-dom-entry-{index}",
                    role="entry",
                    name=name or "Chat prompt",
                    description=(
                        "Chat composer from the page DOM. Typing uses the loopback DevTools "
                        "protocol because AT-SPI did not expose an editable."
                    ),
                    actions=["type"],
                    bounds=bounds,
                    dom_index=index,
                    dom_socket=ws_url,
                )
            )
        elif kind == "button" and is_send_name(name):
            sends.append(
                Candidate(
                    id=f"c-dom-send-{index}",
                    role="push button",
                    name=name,
                    description="Send button from the page DOM.",
                    actions=["click"],
                    bounds=bounds,
                    dom_index=index,
                    dom_socket=ws_url,
                )
            )
    chosen = choose_textbox(textboxes)
    found: list[Candidate] = []
    if chosen is not None:
        found.append(chosen)
    found.extend(sends)
    if chosen is not None and not sends:
        found.append(
            Candidate(
                id="c-dom-enter",
                role="push button",
                name="Send prompt",
                description="Press Enter in the chat composer.",
                actions=["click"],
                bounds=chosen.bounds,
                dom_index=chosen.dom_index,
                dom_socket=chosen.dom_socket,
                dom_key="Enter",
            )
        )
    return found


def discover_dom_candidates(port: int) -> list[Candidate]:
    try:
        sockets = cdp_target_sockets(port, timeout=0.4)
    except Exception:
        return []
    tagged: list[tuple[str, dict[str, Any]]] = []
    for ws_url in sockets:
        try:
            ensure_loopback_ws(ws_url)
        except DriverError:
            continue
        try:
            nodes = cdp_composer_nodes(ws_url, timeout=2.0)
        except Exception:
            continue
        for node in nodes:
            if isinstance(node, dict):
                tagged.append((ws_url, node))
    return dom_nodes_to_candidates(tagged)


def dom_candidates_for_port(port: int | None) -> list[Candidate]:
    if not port:
        _DOM_CACHE["port"] = None
        _DOM_CACHE["candidates"] = []
        return []
    now = time.monotonic()
    if _DOM_CACHE["port"] == port and now - float(_DOM_CACHE["at"]) < 0.75:
        return list(_DOM_CACHE["candidates"])
    found = discover_dom_candidates(port)
    _DOM_CACHE["port"] = port
    _DOM_CACHE["at"] = now
    _DOM_CACHE["candidates"] = found
    return list(found)


def merge_dom_candidates(atspi: list[Candidate], dom: list[Candidate]) -> list[Candidate]:
    merged = list(atspi)
    names = {candidate.name.strip().lower() for candidate in atspi if candidate.name.strip()}
    can_type = any("type" in candidate.actions for candidate in atspi)
    for candidate in dom:
        if "type" in candidate.actions and can_type:
            continue
        key = candidate.name.strip().lower()
        if key and key in names and "type" not in candidate.actions:
            continue
        merged.append(candidate)
        if key:
            names.add(key)
    return merged


def focus_expression(index: int, click: bool) -> str:
    if index < 0:
        raise DriverError(f"bad DOM index {index}")
    click_js = "try { el.click(); } catch (e) {}" if click else ""
    return (
        "(() => {"
        f" const el = (window.__agenttailCdp || [])[{index}];"
        " if (!el) return false;"
        " try { el.scrollIntoView({block: 'center'}); } catch (e) {}"
        " try { el.focus(); } catch (e) {}"
        " if (el.isContentEditable) {"
        "   const range = document.createRange();"
        "   range.selectNodeContents(el);"
        "   range.collapse(false);"
        "   const sel = window.getSelection();"
        "   if (sel) { sel.removeAllRanges(); sel.addRange(range); }"
        " }"
        f" {click_js}"
        " return true;"
        "})()"
    )


def cdp_commands(ws_url: str, calls: list[tuple[str, dict[str, Any]]], timeout: float = 5.0) -> list[dict[str, Any]]:
    conn = cdp_connect(ws_url, timeout)
    try:
        results = []
        for msg_id, (method, params) in enumerate(calls, start=1):
            results.append(cdp_roundtrip(conn, msg_id, method, params))
        return results
    finally:
        conn.close()


def _focused(result: dict[str, Any]) -> bool:
    return result.get("result", {}).get("value") is True


def cdp_type_text(ws_url: str, index: int, text: str) -> None:
    results = cdp_commands(
        ws_url,
        [
            ("Runtime.evaluate", {"expression": focus_expression(index, click=False), "returnByValue": True}),
            ("Input.insertText", {"text": text}),
        ],
    )
    if not _focused(results[0]):
        raise DriverError("DevTools could not focus the chat prompt")


def cdp_click(ws_url: str, index: int) -> None:
    results = cdp_commands(
        ws_url,
        [("Runtime.evaluate", {"expression": focus_expression(index, click=True), "returnByValue": True})],
    )
    if not _focused(results[0]):
        raise DriverError("DevTools could not click the control")


def cdp_press_key(ws_url: str, index: int | None, key: str) -> None:
    if key != "Enter":
        raise DriverError(f"unsupported DOM key {key!r}")
    if index is not None:
        focused = cdp_commands(
            ws_url,
            [("Runtime.evaluate", {"expression": focus_expression(index, click=False), "returnByValue": True})],
        )
        if not _focused(focused[0]):
            raise DriverError("DevTools could not focus the chat prompt")
    event = {"key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13}
    cdp_commands(
        ws_url,
        [
            ("Input.dispatchKeyEvent", {"type": "keyDown", "text": "\r", **event}),
            ("Input.dispatchKeyEvent", {"type": "keyUp", **event}),
        ],
    )


def perform_cdp(choice: Choice, record: dict[str, Any]) -> None:
    candidate = choice.candidate
    if not candidate.dom_socket:
        raise DriverError(f"cannot perform {choice.action} through DevTools")
    if choice.action == "type":
        if choice.text is None or candidate.dom_index is None:
            raise DriverError("type action has no DOM composer")
        cdp_type_text(candidate.dom_socket, candidate.dom_index, choice.text)
        record["method"] = "cdp-type"
        record["text_length"] = len(choice.text)
        return
    if choice.action == "click" and candidate.dom_key:
        cdp_press_key(candidate.dom_socket, candidate.dom_index, candidate.dom_key)
        record["method"] = "cdp-key"
        record["key"] = candidate.dom_key
        return
    if choice.action == "click" and candidate.dom_index is not None:
        cdp_click(candidate.dom_socket, candidate.dom_index)
        record["method"] = "cdp-click"
        return
    raise DriverError(f"cannot perform {choice.action} through DevTools")


def observe(
    window_title: str,
    prompt: str,
    out: Path,
    step: int,
    budget: int = 80,
    cdp_port: int | None = None,
) -> dict[str, Any]:
    windows = find_windows(window_title)
    if not windows:
        return {"found": False, "window": None, "candidates": [], "atspi": "", "screenshot": None}
    window = windows[0]
    candidates: list[Candidate] = []
    walk_candidates(window, prompt, candidates, budget)
    candidates = merge_dom_candidates(candidates, dom_candidates_for_port(cdp_port))
    shot = out / f"step-{step:02d}.png"
    shot_ok = screenshot(shot)
    atspi = observation_text(window)
    dom_lines = [
        f"cdp {candidate.role}: {candidate.name!r} actions={','.join(candidate.actions)}"
        for candidate in candidates
        if candidate.dom_socket
    ]
    if dom_lines:
        atspi = atspi + "\n" + "\n".join(dom_lines)
    (out / f"step-{step:02d}-atspi.txt").write_text(atspi + "\n")
    return {
        "found": True,
        "window": window.name,
        "candidates": candidates,
        "atspi": atspi,
        "screenshot": str(shot) if shot_ok else None,
    }


def match_candidate(candidates: list[Candidate], step: dict[str, Any]) -> Candidate | None:
    role = str(step.get("role", ""))
    role_aliases = {
        "entry": {"entry", "text", "password text", "document text", "section"},
    }
    name = str(step.get("name", ""))
    for candidate in candidates:
        allowed_roles = role_aliases.get(role, {role} if role else set())
        if allowed_roles and candidate.role not in allowed_roles:
            continue
        if name and name.lower() not in candidate.name.lower():
            continue
        if step["action"] not in candidate.actions:
            continue
        return candidate
    return None


def choose_scripted(
    observation: dict[str, Any], script: list[dict[str, Any]], step_index: int, prompt: str
) -> Choice | None:
    if step_index >= len(script):
        return None
    spec = script[step_index]
    candidate = match_candidate(observation["candidates"], spec)
    if candidate is None:
        visible = ", ".join(f"{item.role}:{item.name!r}" for item in observation["candidates"]) or "<none>"
        raise DriverError(f"script step {step_index} matched no candidate ({spec}); visible: {visible}")
    text = spec.get("text", prompt if spec["action"] == "type" else None)
    if isinstance(text, str):
        text = text.replace("{{prompt}}", prompt)
    return Choice(candidate=candidate, action=spec["action"], text=text, source="script", detail={"step": step_index})


def jev_request(
    model: str, goal: str, prompt: str, observation: dict[str, Any], history: list[dict[str, Any]]
) -> tuple[dict[str, Any], dict[str, tuple[Candidate, str]]]:
    criteria: dict[str, str] = {}
    lookup: dict[str, tuple[Candidate, str]] = {}
    for candidate in observation["candidates"]:
        for action in candidate.actions:
            key = f"{candidate.id}:{action}"
            if action == "click":
                instruction = f"Click the {candidate.role} named {candidate.name!r}"
            else:
                instruction = f"Type the requested prompt into the {candidate.role} named {candidate.name!r}"
            if candidate.description:
                instruction += f" ({candidate.description})"
            if candidate.bounds:
                instruction += f" at {candidate.bounds}"
            criteria[key] = instruction
            lookup[key] = (candidate, action)
    if not criteria:
        raise DriverError("Jev was asked to choose, but the observed window has no candidate actions")
    state = {
        "goal": goal,
        "prompt_to_type": prompt,
        "window": observation.get("window"),
        "candidates": [candidate.public() for candidate in observation["candidates"]],
        "recent_actions": history[-4:],
    }
    return {
        "model": model,
        "state": state,
        "questions": {
            "action": {
                "type": "choice",
                "instructions": (
                    "Choose the single next UI action that advances the goal. "
                    "Prefer typing the requested prompt into an editable field, then clicking the "
                    "button that submits it. Do not choose window chrome."
                ),
                "criteria": criteria,
            }
        },
    }, lookup


def choose_jev(
    observation: dict[str, Any],
    goal: str,
    prompt: str,
    history: list[dict[str, Any]],
    model: str,
    confidence_min: float,
    log_path: Path,
) -> Choice:
    key = os.environ.get("OPENROUTER_API_KEY", "").strip()
    if not key:
        raise DriverError(
            "OPENROUTER_API_KEY is empty; Jev choose is unavailable. "
            "No writer LLM or other model is used as a fallback."
        )
    body, lookup = jev_request(model, goal, prompt, observation, history)
    request = urllib.request.Request(
        JEV_ENDPOINT,
        data=json.dumps(body).encode(),
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "HTTP-Referer": "https://github.com/tsconfigdotjson/agenttail",
            "X-Title": "agenttail desktop-xvfb",
        },
        method="POST",
    )
    append_jsonl(log_path, {"at": time.time(), "request": body})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read().decode())
    except urllib.error.HTTPError as error:
        detail = error.read().decode(errors="replace")[:2000]
        append_jsonl(log_path, {"at": time.time(), "status": error.code, "error": detail})
        raise DriverError(f"Jev decisions request failed with HTTP {error.code}: {detail}") from error
    except Exception as error:
        append_jsonl(log_path, {"at": time.time(), "error": str(error)})
        raise DriverError(f"Jev decisions request failed: {error}") from error
    append_jsonl(log_path, {"at": time.time(), "response": payload})
    answer = payload.get("answers", {}).get("action", {})
    selected = answer.get("choice")
    confidence = answer.get("confidence")
    if not isinstance(selected, str) or selected not in lookup:
        raise DriverError(f"Jev returned no usable action: {json.dumps(answer)[:1200]}")
    if isinstance(confidence, (int, float)) and confidence < confidence_min:
        raise DriverError(f"Jev confidence {confidence:.3f} is below {confidence_min:.3f}; stopping without acting")
    candidate, action = lookup[selected]
    text = prompt if action == "type" else None
    return Choice(
        candidate=candidate,
        action=action,
        text=text,
        source="jev",
        confidence=float(confidence) if isinstance(confidence, (int, float)) else None,
        detail={
            "selected": selected,
            "probabilities": answer.get("probabilities"),
            "usage": payload.get("usage"),
            "model": payload.get("model"),
        },
    )


def act(choice: Choice, out: Path) -> dict[str, Any]:
    candidate = choice.candidate
    record: dict[str, Any] = {
        "at": time.time(),
        "candidate": candidate.public(),
        "action": choice.action,
        "source": choice.source,
        "confidence": choice.confidence,
        "detail": choice.detail,
    }
    try:
        if candidate.dom_socket and (choice.action == "type" or candidate.node is None):
            perform_cdp(choice, record)
        elif choice.action == "click":
            try:
                candidate.node.queryAction().doAction(0)
                record["method"] = "at-spi"
            except Exception as error:
                if not candidate.bounds:
                    raise DriverError(f"cannot click {candidate.role} {candidate.name!r}: {error}") from error
                x, y, width, height = candidate.bounds
                subprocess.run(
                    ["xdotool", "mousemove", "--sync", str(x + width // 2), str(y + height // 2), "click", "1"],
                    check=True,
                    capture_output=True,
                    timeout=10,
                )
                record["method"] = "xdotool"
                record["error"] = str(error)
        elif choice.action == "type":
            if choice.text is None:
                raise DriverError("type action has no text")
            try:
                candidate.node.queryAction().doAction(0)
            except Exception:
                if candidate.bounds:
                    x, y, width, height = candidate.bounds
                    subprocess.run(
                        ["xdotool", "mousemove", "--sync", str(x + width // 2), str(y + height // 2), "click", "1"],
                        check=True,
                        capture_output=True,
                        timeout=10,
                    )
            subprocess.run(
                ["xdotool", "type", "--clearmodifiers", "--delay", "20", "--", choice.text],
                check=True,
                capture_output=True,
                timeout=30,
            )
            record["method"] = "xdotool-type"
            record["text_length"] = len(choice.text)
        else:
            raise DriverError(f"unsupported action {choice.action!r}")
    except subprocess.CalledProcessError as error:
        record["error"] = error.stderr.decode(errors="replace")[:1000]
        append_jsonl(out / "timeline.jsonl", record)
        raise DriverError(f"action failed: {record['error']}") from error
    append_jsonl(out / "timeline.jsonl", record)
    return record


def poll_agenttail(
    binary: Path, home: Path, agents: str = "claude_desktop", timeout: float = 15.0
) -> tuple[list[dict[str, Any]], str | None]:
    if not binary.exists():
        return [], f"agenttail binary not found at {binary}; build it with `cargo build --release -p agenttail-cli`"
    env = os.environ.copy()
    env["HOME"] = str(home)
    try:
        result = subprocess.run(
            [str(binary), "ls", "--hours", "2", "--agents", agents],
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
    except Exception as error:
        return [], f"agenttail ls failed: {error}"
    if result.returncode != 0:
        return [], f"agenttail ls exited {result.returncode}: {result.stderr.strip()[:1000]}"
    sessions: list[dict[str, Any]] = []
    for line in result.stdout.splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            sessions.append(value)
    return sessions, None


def classify(sessions: list[dict[str, Any]]) -> tuple[str, dict[str, Any] | None]:
    if not sessions:
        return "idle_no_session", None
    sessions.sort(key=lambda item: float(item.get("last_write") or item.get("last_at") or 0), reverse=True)
    session = sessions[0]
    activity = session.get("activity", "unknown")
    if activity in {"thinking", "running_tool", "background_work"}:
        return "working", session
    if activity in {"done", "idle", "errored", "interrupted", "unknown"}:
        return activity, session
    return str(activity), session


def child_environment(home: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["HOME"] = str(home)
    env.setdefault("DISPLAY", ":99")
    # The writer LLM is not part of this path. Do not leak a configured writer or
    # Anthropic key into the app/driver child environment.
    for key in list(env):
        if key.startswith("CLICKER_WRITER") or key == "ANTHROPIC_API_KEY":
            del env[key]
    return env


def start_process(command: list[str], env: dict[str, str], log_path: Path) -> subprocess.Popen[bytes]:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    handle = log_path.open("wb")
    process = subprocess.Popen(command, stdout=handle, stderr=subprocess.STDOUT, env=env, start_new_session=True)
    process._agenttail_log = handle  # type: ignore[attr-defined]
    return process


def stop_process(process: subprocess.Popen[bytes] | None) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(process.pid), signal.SIGTERM)
    except Exception:
        process.terminate()
    try:
        process.wait(timeout=5)
    except Exception:
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
        except Exception:
            process.kill()


def tail(path: Path, limit: int = 4000) -> str:
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return ""
    return text[-limit:]


def desktop_version() -> str | None:
    try:
        result = subprocess.run(
            ["dpkg-query", "-W", "-f=${Version}", "claude-desktop"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
    except Exception:
        pass
    return None


def start_mock(args: argparse.Namespace, scenario: dict[str, Any], out: Path, env: dict[str, str]) -> tuple[subprocess.Popen[bytes] | None, str | None]:
    mock_scenario = args.mock_scenario or scenario.get("mock_scenario")
    if not mock_scenario:
        return None, "no mock_scenario configured"
    candidates = mock_scenario_candidates(mock_scenario)
    mock_path = resolve_mock_scenario(mock_scenario)
    if mock_path is None:
        return None, f"mock scenario not found: {mock_scenario} (tried {', '.join(str(path) for path in candidates)})"
    if not args.mock_server.exists():
        return None, f"mock-server binary not found at {args.mock_server}"
    log = out / "mock.log"
    process = start_process(
        [str(args.mock_server), "--scenario", str(mock_path), "--addr", args.mock_addr, "--log", str(out / "mock-requests.jsonl")],
        env,
        log,
    )
    deadline = time.time() + 10
    while time.time() < deadline:
        if process.poll() is not None:
            return process, f"mock-server exited: {tail(log)}"
        try:
            text = log.read_text(errors="replace")
        except OSError:
            text = ""
        if "MOCK_ADDR=" in text:
            return process, None
        sleep(0.1)
    return process, f"mock-server did not become ready: {tail(log)}"


def launch_target(args: argparse.Namespace, scenario: dict[str, Any], out: Path, home: Path, env: dict[str, str]) -> subprocess.Popen[bytes]:
    if args.target == "standin":
        command = [sys.executable, str(HERE / "standin.py")]
    else:
        command = desktop_launch_command(desktop_cdp_port(args))
    return start_process(command, env, out / f"{args.target}.log")


def wait_for_window(
    window_title: str,
    timeout: float,
    process: subprocess.Popen[bytes],
    out: Path,
    cdp_port: int | None = None,
) -> tuple[dict[str, Any] | None, str | None]:
    deadline = time.time() + timeout
    last: dict[str, Any] | None = None
    while time.time() < deadline:
        if process.poll() is not None:
            return last, f"{process.args[0]} exited with {process.returncode}: {tail(out / (Path(str(process.args[0])).name + '.log'))}"
        try:
            last = observe(window_title, "", out, 0, cdp_port=cdp_port)
        except DriverError:
            last = None
        if last and last.get("found"):
            return last, None
        sleep(0.25)
    return last, f"window containing {window_title!r} did not appear within {timeout:.0f}s"


def run_probe(args: argparse.Namespace, scenario: dict[str, Any], out: Path, home: Path) -> dict[str, Any]:
    env = child_environment(home)
    mock, mock_error = (None, None)
    cdp_port = desktop_cdp_port(args)
    if args.target == "claude-desktop":
        mock, mock_error = start_mock(args, scenario, out, env)
    process = launch_target(args, scenario, out, home, env)
    window_title = args.window_title or scenario.get("window_title") or ("agenttail desktop stand-in" if args.target == "standin" else "Claude")
    startup = float(scenario_value(scenario, args, "startup_timeout_s", 45.0))
    try:
        observation, error = wait_for_window(window_title, startup, process, out, cdp_port)
        if observation and observation.get("found"):
            # A window can exist before the renderer has populated its AT-SPI tree.
            # Wait for the visible screen to settle, then keep the final observation
            # so probe.json and its screenshot describe the same moment.
            settle = float(scenario.get("probe_settle_s", 20.0))
            deadline = time.time() + settle
            while time.time() < deadline:
                try:
                    current = observe(window_title, "", out, 0, cdp_port=cdp_port)
                except DriverError:
                    current = None
                if current and current.get("found"):
                    observation = current
                sleep(0.25)
    finally:
        stop_process(mock)
        stop_process(process)
    result = {
        "kind": "probe",
        "target": args.target,
        "window_title": window_title,
        "claude_desktop_version": desktop_version(),
        "mock_error": mock_error,
        "found": bool(observation and observation.get("found")),
        "candidate_count": len(observation["candidates"]) if observation else 0,
        "candidates": [candidate.public() for candidate in observation["candidates"]] if observation else [],
        "screenshot": observation.get("screenshot") if observation else None,
        "error": error,
        "app_log_tail": tail(out / f"{args.target}.log", 2500),
    }
    write_json(out / "probe.json", result)
    return result


def run_scenario(args: argparse.Namespace, scenario: dict[str, Any], out: Path, home: Path) -> dict[str, Any]:
    chooser = args.chooser or scenario.get("chooser", "scripted")
    if args.target == "claude-desktop" and chooser != "jev":
        raise DriverError("Claude Desktop drives only through Jev; pass --chooser jev (no scripted fallback)")
    if chooser == "jev" and not os.environ.get("OPENROUTER_API_KEY", "").strip():
        raise DriverError("OPENROUTER_API_KEY is empty; refusing to fake the Jev choose step")
    env = child_environment(home)
    mock = None
    mock_error = None
    if args.target == "claude-desktop":
        mock, mock_error = start_mock(args, scenario, out, env)
        if mock_error and mock is None:
            raise DriverError(mock_error)
    process = launch_target(args, scenario, out, home, env)
    window_title = args.window_title or scenario.get("window_title") or ("agenttail desktop stand-in" if args.target == "standin" else "Claude")
    goal = args.goal or scenario.get("goal") or "Drive the desktop and finish the task."
    prompt = args.prompt or scenario.get("prompt") or "say hello"
    timeout = float(scenario_value(scenario, args, "timeout_s", 45.0, "timeout"))
    max_steps = int(scenario_value(scenario, args, "max_steps", 8))
    agents = args.agents or scenario.get("agents", "claude_desktop")
    confidence = resolve_confidence(args, scenario)
    cdp_port = desktop_cdp_port(args)
    script = scenario.get("script", []) if chooser == "scripted" else []
    expected = scenario.get("expect", {}) if isinstance(scenario.get("expect", {}), dict) else {}
    wanted = expected.get("activities", ["thinking", "done"])
    end_activity = expected.get("end", "done")

    timeline: list[dict[str, Any]] = []
    history: list[dict[str, Any]] = []
    observations: list[dict[str, Any]] = []
    seen: list[str] = []
    last_activity: str | None = None
    last_choice_at = 0.0
    step_index = 0
    started = time.time()
    error: str | None = None
    startup_deadline = started + float(scenario_value(scenario, args, "startup_timeout_s", 45.0))
    observation: dict[str, Any] | None = None
    try:
        while time.time() - started < timeout:
            if process.poll() is not None:
                error = f"{args.target} exited with {process.returncode}: {tail(out / f'{args.target}.log')}"
                break
            try:
                current = observe(window_title, prompt, out, len(observations), cdp_port=cdp_port)
            except DriverError as observe_error:
                if time.time() < startup_deadline:
                    sleep(POLL_SECONDS)
                    continue
                error = str(observe_error)
                break
            if current.get("found"):
                observation = current
                observations.append(
                    {
                        "at": time.time(),
                        "window": current.get("window"),
                        "candidate_count": len(current["candidates"]),
                        "candidates": [candidate.public() for candidate in current["candidates"]],
                        "screenshot": current.get("screenshot"),
                    }
                )
            elif time.time() < startup_deadline:
                sleep(POLL_SECONDS)
                continue

            sessions, poll_error = poll_agenttail(args.agenttail, home, agents)
            if poll_error:
                error = poll_error
                break
            activity, session = classify(sessions)
            if activity != last_activity:
                timeline.append(
                    {
                        "at": time.time(),
                        "activity": activity,
                        "session": session,
                        "screenshot": (observation or {}).get("screenshot"),
                    }
                )
                seen.append(activity)
                last_activity = activity
            if activity == end_activity and "working" in seen:
                break
            now = time.time()
            if can_choose(observation, now, last_choice_at, step_index, max_steps):
                if chooser == "scripted":
                    choice = choose_scripted(observation, script, step_index, prompt)
                else:
                    choice = choose_jev(
                        observation,
                        goal,
                        prompt,
                        history,
                        args.jev_model,
                        confidence,
                        out / "jev.jsonl",
                    )
                if choice is not None:
                    record = act(choice, out)
                    history.append(
                        {
                            "action": choice.action,
                            "candidate": choice.candidate.public(),
                            "source": choice.source,
                            "confidence": choice.confidence,
                        }
                    )
                    record["step"] = step_index
                    step_index += 1
                    last_choice_at = time.time()
                    sleep(0.25)
                    continue
            sleep(POLL_SECONDS)
        if error is None and not (end_activity in seen and "working" in seen):
            error = (
                f"did not observe working -> {end_activity} within {timeout:.0f}s; "
                f"seen={seen or ['<nothing>']}; mock={mock_error or 'ok'}"
            )
    finally:
        stop_process(mock)
        stop_process(process)

    success = error is None and end_activity in seen and "working" in seen
    result = {
        "kind": "scenario",
        "scenario": scenario.get("name", args.target),
        "description": scenario.get("description"),
        "target": args.target,
        "chooser": chooser,
        "confidence": confidence if chooser == "jev" else None,
        "jev_model": args.jev_model if chooser == "jev" else None,
        "ok": success,
        "error": error,
        "expected": {"activities": wanted, "end": end_activity},
        "seen": seen,
        "timeline": timeline,
        "observations": observations,
        "actions": history,
        "mock_error": mock_error,
        "claude_desktop_version": desktop_version(),
        "agenttail_bin": str(args.agenttail),
        "agents": agents,
        "home": str(home),
        "app_log_tail": tail(out / f"{args.target}.log", 3000),
    }
    write_json(out / "score.json", result)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    scenario_path = args.scenario
    if scenario_path is None and args.target == "standin" and not args.probe_only:
        scenario_path = DEFAULT_SCENARIO
    if scenario_path is not None and not scenario_path.is_absolute():
        scenario_path = (HERE / scenario_path).resolve()
    try:
        scenario = load_scenario(scenario_path)
    except DriverError as error:
        print(f"desktop-xvfb: {error}", file=sys.stderr)
        return 2
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    out = args.out / f"{scenario.get('name', args.target)}-{stamp}"
    out.mkdir(parents=True, exist_ok=True)
    write_json(out / "run.json", {"argv": sys.argv, "target": args.target, "chooser": args.chooser, "time": stamp})
    args.home.mkdir(parents=True, exist_ok=True)
    try:
        if args.probe_only:
            result = run_probe(args, scenario, out, args.home)
            ok = result["found"]
            print(json.dumps({key: result[key] for key in ("kind", "target", "found", "candidate_count", "error")}, sort_keys=True))
        else:
            result = run_scenario(args, scenario, out, args.home)
            ok = bool(result["ok"])
            print(
                json.dumps(
                    {
                        "kind": result["kind"],
                        "scenario": result["scenario"],
                        "target": result["target"],
                        "chooser": result["chooser"],
                        "ok": result["ok"],
                        "seen": result["seen"],
                        "error": result["error"],
                    },
                    sort_keys=True,
                )
            )
    except DriverError as error:
        write_json(out / "blocker.json", {"error": str(error), "traceback": traceback.format_exc(), "target": args.target})
        print(f"desktop-xvfb: {error}", file=sys.stderr)
        print(f"evidence: {out}", file=sys.stderr)
        return 3
    print(f"evidence: {out}")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
