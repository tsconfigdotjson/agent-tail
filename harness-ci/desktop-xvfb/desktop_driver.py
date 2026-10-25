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
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import traceback
import urllib.error
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
    "text",
    "password text",
}
EDITABLE_ROLES = {"entry", "text", "password text", "document text"}


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

    def public(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "role": self.role,
            "name": self.name,
            "description": self.description,
            "actions": self.actions,
            "bounds": self.bounds,
        }


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
    parser.add_argument("--confidence", type=float, default=float(os.environ.get("AGENTTAIL_JEV_CONFIDENCE", DEFAULT_CONFIDENCE)))
    parser.add_argument("--jev-model", default=os.environ.get("AGENTTAIL_JEV_MODEL", JEV_MODEL))
    return parser.parse_args(argv)


def scenario_value(
    scenario: dict[str, Any], args: argparse.Namespace, key: str, default: Any, cli_key: str | None = None
) -> Any:
    cli = getattr(args, cli_key or key, None)
    return cli if cli is not None else scenario.get(key, default)


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


def candidate_from_node(node: Any, index: int, prompt: str) -> Candidate | None:
    try:
        role = node.getRoleName()
        name = (node.name or "").strip()
        description = (node.description or "").strip()
    except Exception:
        return None
    if role in {"frame", "window", "application"} or name in CHROME_BUTTONS:
        return None
    if role not in ACTIONABLE_ROLES:
        return None
    if not name and role not in EDITABLE_ROLES:
        return None
    try:
        import pyatspi

        state = node.getState()
        if not state.contains(pyatspi.STATE_SHOWING):
            return None
    except ImportError:
        pass
    except Exception:
        # Older or unusual bridges can fail a state query; keep the node rather
        # than making the whole observation empty because of one widget.
        pass
    actions = node_actions(node)
    if role in EDITABLE_ROLES:
        actions = [action for action in actions if action == "click"]
        actions.append("type")
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


def observe(window_title: str, prompt: str, out: Path, step: int, budget: int = 80) -> dict[str, Any]:
    windows = find_windows(window_title)
    if not windows:
        return {"found": False, "window": None, "candidates": [], "atspi": "", "screenshot": None}
    window = windows[0]
    candidates: list[Candidate] = []
    walk_candidates(window, prompt, candidates, budget)
    shot = out / f"step-{step:02d}.png"
    shot_ok = screenshot(shot)
    observed = {
        "found": True,
        "window": window.name,
        "candidates": [candidate.public() for candidate in candidates],
        "atspi": observation_text(window),
        "screenshot": str(shot) if shot_ok else None,
    }
    (out / f"step-{step:02d}-atspi.txt").write_text(observed["atspi"] + "\n")
    return {"found": True, "window": window.name, "candidates": candidates, "atspi": observed["atspi"], "screenshot": observed["screenshot"]}


def match_candidate(candidates: list[Candidate], step: dict[str, Any]) -> Candidate | None:
    role = str(step.get("role", ""))
    role_aliases = {
        "entry": {"entry", "text", "password text"},
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
        if choice.action == "click":
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
    raw = Path(mock_scenario)
    candidates = [raw] if raw.is_absolute() else [
        (HERE / raw),
        (Path("/repo/harness-ci/desktop-xvfb") / raw),
        (Path.cwd() / raw),
    ]
    mock_path = next((path.resolve() for path in candidates if path.exists()), candidates[0].resolve())
    if not mock_path.exists():
        return None, f"mock scenario not found: {raw} (tried {', '.join(str(path) for path in candidates)})"
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
        command = [
            "claude-desktop",
            "--no-sandbox",
            "--force-renderer-accessibility",
            "--disable-gpu",
            "--enable-logging=stderr",
        ]
    return start_process(command, env, out / f"{args.target}.log")


def wait_for_window(window_title: str, timeout: float, process: subprocess.Popen[bytes], out: Path) -> tuple[dict[str, Any] | None, str | None]:
    deadline = time.time() + timeout
    last: dict[str, Any] | None = None
    while time.time() < deadline:
        if process.poll() is not None:
            return last, f"{process.args[0]} exited with {process.returncode}: {tail(out / (Path(str(process.args[0])).name + '.log'))}"
        try:
            last = observe(window_title, "", out, 0)
        except DriverError:
            last = None
        if last and last.get("found"):
            return last, None
        sleep(0.25)
    return last, f"window containing {window_title!r} did not appear within {timeout:.0f}s"


def run_probe(args: argparse.Namespace, scenario: dict[str, Any], out: Path, home: Path) -> dict[str, Any]:
    env = child_environment(home)
    mock, mock_error = (None, None)
    if args.target == "claude-desktop":
        mock, mock_error = start_mock(args, scenario, out, env)
    process = launch_target(args, scenario, out, home, env)
    window_title = args.window_title or scenario.get("window_title") or ("agenttail desktop stand-in" if args.target == "standin" else "Claude")
    startup = float(scenario_value(scenario, args, "startup_timeout_s", 45.0))
    try:
        observation, error = wait_for_window(window_title, startup, process, out)
        if observation and observation.get("found"):
            # A window can exist before the renderer has populated its AT-SPI tree.
            # Wait for the visible screen to settle, then keep the final observation
            # so probe.json and its screenshot describe the same moment.
            settle = float(scenario.get("probe_settle_s", 20.0))
            deadline = time.time() + settle
            while time.time() < deadline:
                try:
                    current = observe(window_title, "", out, 0)
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
                current = observe(window_title, prompt, out, len(observations))
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
            can_act = observation is not None and now - last_choice_at >= ACTION_GAP_SECONDS and step_index < max_steps
            if can_act:
                if chooser == "scripted":
                    choice = choose_scripted(observation, script, step_index, prompt)
                    if choice is None:
                        # Script exhausted: keep observing/scoring until the expected end or timeout.
                        can_act = False
                else:
                    choice = choose_jev(
                        observation,
                        goal,
                        prompt,
                        history,
                        args.jev_model,
                        args.confidence,
                        out / "jev.jsonl",
                    )
                if can_act and choice is not None:
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
