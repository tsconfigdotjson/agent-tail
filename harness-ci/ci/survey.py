#!/usr/bin/env python3
"""Survey and compare the *shape* of harness transcripts, and capture redacted fixtures.

The survey only records identifiers of structure: record types, field paths, content-block
types and small vocabularies. It never records ids, timestamps or free text values, so a
timestamp or id format change cannot look like drift. The nightly workflow runs `survey` on
the real CLI runs, `compare` against the committed baseline, and `capture` when a new CLI
version changes the shape.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable

SHAPE_KEYS = (
    "record_types",
    "field_paths",
    "content_types",
    "enums",
    "system_subtypes",
    "attachment_types",
    "tool_names_seen",
)
META_KEYS = {"harness", "version", "os", "generated_by", "generated_at", "surveyed", "host"}

# Keys whose values are payloads, not shape. We keep the key itself (for field_paths) but
# do not recurse into it: tool inputs, prompts, rendered system reminders and API request
# bodies all have arbitrary keys that would otherwise swamp the shape with content.
PAYLOAD_KEYS = {
    "content",
    "context",
    "description",
    "input",
    "input_schema",
    "lastPrompt",
    "aiTitle",
    "messages",
    "properties",
    "prompt",
    "rendered",
    "shape",
    "snapshot",
    "system",
    "text",
    "thinking",
    "tools",
    "toolUseResult",
    "url",
}

ENUM_PATHS = {
    "entrypoint": ("entrypoint",),
    "userType": ("userType",),
    "permissionMode": ("permissionMode",),
    "mode": ("mode",),
    "promptSource": ("promptSource",),
    "sessionKind": ("sessionKind",),
    "origin.kind": ("origin", "kind"),
    "toolDenialKind": ("toolDenialKind",),
    "permissionDecision.decision": ("permissionDecision", "decision"),
    "permissionDecision.source": ("permissionDecision", "source"),
    "permissionDecision.reasonType": ("permissionDecision", "reasonType"),
    "message.stop_reason": ("message", "stop_reason"),
    "queue-operation.operation": ("operation",),
    "status": ("status",),
    "waitingFor": ("waitingFor",),
}

ID_KEY = re.compile(r"(^id$|Id$|_id$|Uuid$|uuid$|Hash$|Digest$|digest$|Ref$|ref$)")
PATH_KEYS = {
    "cwd",
    "file_path",
    "filePath",
    "outputFile",
    "output_file",
    "path",
    "projectPath",
    "repository_url",
    "repositoryUrl",
    "messagingSocketPath",
    "transcript_path",
    "transcriptPath",
}
SECRET_KEYS = {"apiKey", "api_key", "authorization", "password", "secret", "token", "ANTHROPIC_API_KEY"}
UUID_RE = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
MOCK_ID_RE = re.compile(r"\b(?:msg_mock|toolu_mock|msg_mock_|toolu_mock_)[A-Za-z0-9_-]+")


def _safe_key(key: str) -> bool:
    # Snapshot objects use absolute file paths as keys. Payload keys already cover the
    # known ones; this second guard keeps any stray path-like key out of the baseline.
    return "/" not in key and "\\" not in key and "\n" not in key


def _walk(value: Any, path: str, fields: set[str]) -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            if not isinstance(key, str) or not _safe_key(key):
                continue
            child_path = f"{path}.{key}" if path else key
            fields.add(child_path)
            if key not in PAYLOAD_KEYS:
                _walk(child, child_path, fields)
    elif isinstance(value, list):
        fields.add(f"{path}[]")
        if path.rsplit(".", 1)[-1] not in PAYLOAD_KEYS:
            for item in value:
                _walk(item, f"{path}[]", fields)


def _get(value: dict[str, Any], path: tuple[str, ...]) -> Any:
    for key in path:
        if not isinstance(value, dict):
            return None
        value = value.get(key)  # type: ignore[assignment]
    return value


def _enum_value(value: Any) -> str | None:
    if value is None:
        return "null"
    if isinstance(value, (str, int, float, bool)):
        return str(value)
    return None


def _content_blocks(record: dict[str, Any], out: dict[str, set[str]], tool_names: set[str]) -> None:
    record_type = record.get("type")
    message = record.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    if record_type in {"assistant", "user"} and isinstance(content, list):
        for block in content:
            if not isinstance(block, dict):
                continue
            block_type = block.get("type")
            if isinstance(block_type, str):
                out[f"{record_type}.content"].add(block_type)
            if block_type == "tool_use" and isinstance(block.get("name"), str):
                tool_names.add(block["name"])
            if block_type == "tool_use" and isinstance(block.get("input"), dict):
                # Tool input keys are content-shaped by the model, not transcript shape.
                pass
            if block_type == "tool_result":
                nested = block.get("content")
                if isinstance(nested, list):
                    for item in nested:
                        if isinstance(item, dict) and isinstance(item.get("type"), str):
                            out["tool_result.content"].add(item["type"])
    attachment = record.get("attachment")
    if record_type == "attachment" and isinstance(attachment, dict) and isinstance(attachment.get("type"), str):
        out["attachment"].add(attachment["type"])


def shape_from_files(files: Iterable[Path], harness: str, version: str, os_name: str) -> dict[str, Any]:
    record_types: set[str] = set()
    field_paths: dict[str, set[str]] = defaultdict(set)
    content_types: dict[str, set[str]] = defaultdict(set)
    enums: dict[str, set[str]] = defaultdict(set)
    system_subtypes: set[str] = set()
    attachment_types: set[str] = set()
    tool_names: set[str] = set()

    for path in files:
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(record, dict):
                    continue
                record_type = record.get("type")
                record_type = record_type if isinstance(record_type, str) else "<none>"
                record_types.add(record_type)
                _walk(record, "", field_paths[record_type])
                _content_blocks(record, content_types, tool_names)
                if record_type == "system" and isinstance(record.get("subtype"), str):
                    system_subtypes.add(record["subtype"])
                attachment = record.get("attachment")
                if (
                    record_type == "attachment"
                    and isinstance(attachment, dict)
                    and isinstance(attachment.get("type"), str)
                ):
                    attachment_types.add(attachment["type"])
                for label, keys in ENUM_PATHS.items():
                    value = _enum_value(_get(record, keys))
                    if value is not None:
                        enums[label].add(value)

    return {
        "harness": harness,
        "version": version,
        "os": os_name.lower(),
        "generated_by": "harness-ci/ci/survey.py",
        "record_types": sorted(record_types),
        "field_paths": {key: sorted(value) for key, value in sorted(field_paths.items())},
        "content_types": {key: sorted(value) for key, value in sorted(content_types.items())},
        "enums": {key: sorted(value) for key, value in sorted(enums.items())},
        "system_subtypes": sorted(system_subtypes),
        "attachment_types": sorted(attachment_types),
        "tool_names_seen": sorted(tool_names),
    }


def _looks_like_run(path: Path) -> bool:
    return (
        (path / "transcripts").is_dir()
        or (path / "transcript.jsonl").is_file()
        or (path / "score.json").is_file()
    )


def find_run_dirs(root: Path) -> list[Path]:
    root = root.resolve()
    if _looks_like_run(root):
        return [root]
    found: list[Path] = []
    for child in sorted(root.iterdir()) if root.is_dir() else []:
        if child.is_dir():
            if _looks_like_run(child):
                found.append(child)
            else:
                found.extend(find_run_dirs(child))
    return found


def transcript_files(run_dir: Path) -> list[Path]:
    files: list[Path] = []
    transcripts = run_dir / "transcripts"
    if transcripts.is_dir():
        files.extend(sorted(transcripts.rglob("*.jsonl")))
    elif (run_dir / "transcript.jsonl").is_file():
        files.append(run_dir / "transcript.jsonl")
    subagents = run_dir / "subagents"
    if subagents.is_dir():
        files.extend(sorted(subagents.glob("*.jsonl")))
    seen: set[Path] = set()
    unique: list[Path] = []
    for path in files:
        resolved = path.resolve()
        if resolved not in seen:
            seen.add(resolved)
            unique.append(resolved)
    return unique


def survey_runs(runs: list[Path], harness: str, version: str, os_name: str) -> dict[str, Any]:
    files: list[Path] = []
    for run in runs:
        for run_dir in find_run_dirs(run):
            files.extend(transcript_files(run_dir))
    unique = sorted({path.resolve() for path in files})
    if not unique:
        raise SystemExit(f"survey: no transcripts found under {', '.join(str(run) for run in runs)}")
    return shape_from_files(unique, harness, version, os_name)


def _set_diff(current: set[str], baseline: set[str]) -> tuple[list[str], list[str]]:
    return sorted(current - baseline), sorted(baseline - current)


def shape_diff(baseline: dict[str, Any], current: dict[str, Any]) -> dict[str, Any]:
    diff: dict[str, Any] = {
        "added": defaultdict(list),
        "removed": defaultdict(list),
        "added_fields": defaultdict(dict),
        "removed_fields": defaultdict(dict),
    }
    for key in ("record_types", "system_subtypes", "attachment_types", "tool_names_seen"):
        added, removed = _set_diff(set(current.get(key, [])), set(baseline.get(key, [])))
        if added:
            diff["added"][key] = added
        if removed:
            diff["removed"][key] = removed
    for key in ("field_paths", "content_types", "enums"):
        old = baseline.get(key, {})
        new = current.get(key, {})
        for label in sorted(set(old) | set(new)):
            added, removed = _set_diff(set(new.get(label, [])), set(old.get(label, [])))
            if added:
                diff["added_fields"][key][label] = added
            if removed:
                diff["removed_fields"][key][label] = removed
    return {
        "added": dict(diff["added"]),
        "removed": dict(diff["removed"]),
        "added_fields": dict(diff["added_fields"]),
        "removed_fields": dict(diff["removed_fields"]),
    }


def has_drift(diff: dict[str, Any]) -> bool:
    return any(diff[key] for key in ("added", "removed", "added_fields", "removed_fields"))


def _lines(title: str, values: Iterable[str]) -> list[str]:
    values = list(values)
    if not values:
        return []
    return [f"### {title}", "", *[f"- `{value}`" for value in values], ""]


def render_report(baseline: dict[str, Any], current: dict[str, Any], diff: dict[str, Any]) -> str:
    lines = [
        f"# Harness shape drift: {current.get('harness', 'unknown')}",
        "",
        f"- Baseline: `{baseline.get('version', '?')}` ({baseline.get('os', '?')})",
        f"- Current: `{current.get('version', '?')}` ({current.get('os', '?')})",
        f"- Shape changed: **{'yes' if has_drift(diff) else 'no'}**",
        "",
        "The survey compares record types, field paths, content-block types and small",
        "vocabularies. Ids, timestamps and free-text values are deliberately not surveyed, so",
        "value-only changes are not drift.",
        "",
    ]
    for key in ("record_types", "system_subtypes", "attachment_types", "tool_names_seen"):
        lines += _lines(f"Added {key.replace('_', ' ')}", diff["added"].get(key, []))
        lines += _lines(f"Removed {key.replace('_', ' ')}", diff["removed"].get(key, []))
    for key, heading in (
        ("field_paths", "field paths"),
        ("content_types", "content types"),
        ("enums", "enum values"),
    ):
        for label, values in sorted(diff["added_fields"].get(key, {}).items()):
            lines += _lines(f"Added {heading} in `{label}`", values)
        for label, values in sorted(diff["removed_fields"].get(key, {}).items()):
            lines += _lines(f"Removed {heading} in `{label}`", values)
    if not has_drift(diff):
        lines += ["No shape changes.", ""]
    return "\n".join(lines)


def _collect_paths(records: list[dict[str, Any]]) -> list[str]:
    paths: set[str] = set()
    for record in records:
        for key in ("cwd", "file_path", "filePath", "outputFile", "path"):
            value = record.get(key)
            if isinstance(value, str) and value.startswith(("/", "~")):
                paths.add(value)
        message = record.get("message")
        if isinstance(message, dict):
            content = message.get("content")
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict):
                        for key in ("file_path", "filePath", "path"):
                            value = block.get(key)
                            if isinstance(value, str) and value.startswith(("/", "~")):
                                paths.add(value)
    return sorted(paths, key=len, reverse=True)


def _redact_string(value: str, path_map: dict[str, str], regex_map: dict[str, str]) -> str:
    for old in sorted(path_map, key=len, reverse=True):
        value = value.replace(old, path_map[old])
    value = UUID_RE.sub(lambda match: regex_map.setdefault(match.group(0), f"<uuid-{len(regex_map) + 1:04d}>"), value)
    value = MOCK_ID_RE.sub(lambda match: regex_map.setdefault(match.group(0), f"<id-{len(regex_map) + 1:04d}>"), value)
    return value


def _redact_key(key: str, path_map: dict[str, str], regex_map: dict[str, str]) -> str:
    # File-history snapshots can use absolute paths as object keys. Keep the mapping stable
    # so a later transcript path still points at the same placeholder.
    if key.startswith(("/", "~")):
        if key not in path_map:
            path_map[key] = f"<path-{len(path_map) + 1:04d}>"
        return path_map[key]
    return _redact_string(key, path_map, regex_map)


def _redact(value: Any, key: str | None, path_map: dict[str, str], id_map: dict[str, str], regex_map: dict[str, str]) -> Any:
    if isinstance(value, dict):
        return {
            _redact_key(item_key, path_map, regex_map): _redact(item_value, item_key, path_map, id_map, regex_map)
            for item_key, item_value in value.items()
        }
    if isinstance(value, list):
        return [_redact(item, key, path_map, id_map, regex_map) for item in value]
    if isinstance(value, str):
        if key in SECRET_KEYS:
            return "<redacted>"
        if key in PATH_KEYS:
            if value not in path_map:
                path_map[value] = f"<path-{len(path_map) + 1:04d}>"
            return path_map[value]
        if key and ID_KEY.search(key):
            if value not in id_map:
                id_map[value] = f"<id-{len(id_map) + 1:04d}>"
            return id_map[value]
        return _redact_string(value, path_map, regex_map)
    return value


def _toml_string(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def capture_fixture(run_dir: Path, out_root: Path, harness: str, version: str, os_name: str) -> Path | None:
    files = transcript_files(run_dir)
    if not files:
        return None
    records: list[dict[str, Any]] = []
    for path in files:
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(record, dict):
                    records.append(record)
    if not records:
        return None

    path_map = {path: f"<path-{index + 1:04d}>" for index, path in enumerate(_collect_paths(records))}
    id_map: dict[str, str] = {}
    regex_map: dict[str, str] = {}
    redacted = [_redact(record, None, path_map, id_map, regex_map) for record in records]

    scenario = run_dir.name
    target = out_root / harness.replace("_", "-") / version / os_name.lower() / f"ci-{scenario}"
    input_dir = target / "input"
    input_dir.mkdir(parents=True, exist_ok=True)
    input_path = input_dir / "session.jsonl"
    input_path.write_text(
        "".join(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n" for record in redacted),
        encoding="utf-8",
    )
    # The workflow sets AGENTTAIL_BIN so the same Rust redactor that people use locally
    # handles the nightly capture too. The local fallback above still strips ids and paths
    # for unit tests and development without a built CLI.
    binary = os.environ.get("AGENTTAIL_BIN")
    if binary:
        subprocess.run(
            [
                binary,
                "fixture",
                "redact",
                "--in-place",
                "--report",
                str(target / "redaction.json"),
                str(input_path),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
        )
    today = dt.date.today().isoformat()
    (target / "meta.toml").write_text(
        "\n".join(
            [
                f"harness = {_toml_string(harness)}",
                f"version = {_toml_string(version)}",
                f"os = {_toml_string(os_name)}",
                'capture = "ci-scripted"',
                f"date = {_toml_string(today)}",
                f'description = {_toml_string("Nightly harness capture. Ids/temp paths redacted; mock content kept.")}',
                "",
            ]
        ),
        encoding="utf-8",
    )
    return target


def version_key(version: str) -> tuple[int, ...]:
    parts = re.split(r"[.+-]", version)
    return tuple(int(part) if part.isdigit() else -1 for part in parts)


def cmd_survey(args: argparse.Namespace) -> int:
    shape = survey_runs(args.runs, args.harness, args.version, args.os)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(shape, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"survey: {len(shape['record_types'])} record types, {sum(map(len, shape['field_paths'].values()))} field paths -> {out}")
    return 0


def cmd_compare(args: argparse.Namespace) -> int:
    baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))
    current = json.loads(Path(args.current).read_text(encoding="utf-8"))
    diff = shape_diff(baseline, current)
    report = render_report(baseline, current, diff)
    if args.report:
        path = Path(args.report)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(report, encoding="utf-8")
        print(f"compare: report -> {path}")
    else:
        print(report)
    if args.exit_code and has_drift(diff):
        print("compare: shape drifted", file=sys.stderr)
        return 1
    return 0


def cmd_capture(args: argparse.Namespace) -> int:
    captured = 0
    for run in args.runs:
        for run_dir in find_run_dirs(run):
            if capture_fixture(run_dir, Path(args.out), args.harness, args.version, args.os) is not None:
                captured += 1
    print(f"capture: {captured} fixture(s) -> {args.out}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    survey = sub.add_parser("survey", help="write the shape of one or more run directories")
    survey.add_argument("--runs", nargs="+", type=Path, required=True)
    survey.add_argument("--harness", default="claude_code")
    survey.add_argument("--version", required=True)
    survey.add_argument("--os", default=sys.platform)
    survey.add_argument("--out", required=True)
    survey.set_defaults(func=cmd_survey)

    compare = sub.add_parser("compare", help="compare a current shape to a committed baseline")
    compare.add_argument("--baseline", required=True)
    compare.add_argument("--current", required=True)
    compare.add_argument("--report")
    compare.add_argument("--exit-code", action="store_true", help="exit 1 when shape drifted")
    compare.set_defaults(func=cmd_compare)

    capture = sub.add_parser("capture", help="write redacted transcript fixtures from runs")
    capture.add_argument("--runs", nargs="+", type=Path, required=True)
    capture.add_argument("--out", type=Path, required=True)
    capture.add_argument("--harness", default="claude_code")
    capture.add_argument("--version", required=True)
    capture.add_argument("--os", default=sys.platform)
    capture.set_defaults(func=cmd_capture)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
