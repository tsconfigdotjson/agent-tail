#!/usr/bin/env python3
"""Turn downloaded nightly shape artifacts into a baseline/report/fixture PR.

Input is a directory of GitHub Actions artifacts. Each artifact directory holds one
`shape.json` and the run directories that produced it (named by the requested matrix tag,
for example `harness-latest-Linux/`). Linux is the drift authority: macOS-only registration
differences must not open a false PR, and the committed baseline is also Linux.

Exit codes: 0 means no change or a new-version drift was written; 2 means an existing
baseline no longer matches and a human should look before a PR is opened.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import survey


def choose_linux(entries: list[tuple[Path, dict]]) -> tuple[Path, dict]:
    for path, shape in entries:
        if str(shape.get("os", "")).lower() == "linux":
            return path, shape
    return entries[0]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--baseline-dir", type=Path, default=Path("harness-ci/drift/baseline/claude-code"))
    parser.add_argument("--reports-dir", type=Path, default=Path("harness-ci/drift/reports"))
    parser.add_argument("--fixtures-root", type=Path, default=Path("fixtures"))
    args = parser.parse_args(argv)

    shape_files = sorted(args.artifacts.rglob("shape.json"))
    if not shape_files:
        print("nightly: no shape artifacts downloaded", file=sys.stderr)
        return 2

    by_version: dict[str, list[tuple[Path, dict]]] = {}
    for path in shape_files:
        try:
            shape = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(f"nightly: ignoring {path}: {error}", file=sys.stderr)
            continue
        version = shape.get("version")
        if not isinstance(version, str) or not version:
            continue
        by_version.setdefault(version, []).append((path, shape))

    baseline_dir = args.baseline_dir
    baseline_dir.mkdir(parents=True, exist_ok=True)
    baselines = {
        path.stem: json.loads(path.read_text(encoding="utf-8"))
        for path in sorted(baseline_dir.glob("*.json"))
    }
    if not baselines:
        print(f"nightly: no baselines under {baseline_dir}", file=sys.stderr)
        return 2

    regressions: list[str] = []
    changes: list[str] = []
    for version in sorted(by_version, key=survey.version_key):
        entries = by_version[version]
        _path, current = choose_linux(entries)
        baseline_path = baseline_dir / f"{version}.json"
        baseline_version = version if version in baselines else max(baselines, key=survey.version_key)
        baseline = baselines[baseline_version]
        diff = survey.shape_diff(baseline, current)
        if not survey.has_drift(diff):
            print(f"nightly: {version} has no shape drift from {baseline_version}")
            continue

        report = survey.render_report(baseline, current, diff)
        if baseline_path.exists():
            # The same CLI version changed shape, or a committed baseline is stale. Do not
            # silently rewrite approved shape data; print enough for a human to decide.
            regressions.append(version)
            print(f"nightly: REGRESSION {version} differs from committed baseline {version}", file=sys.stderr)
            print(report, file=sys.stderr)
            continue

        report_path = args.reports_dir / f"claude-code-{baseline_version}-to-{version}.md"
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(report, encoding="utf-8")
        baseline_path.write_text(json.dumps(current, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        changes.append(f"{baseline_version} -> {version}")

        for artifact_path, shape in sorted(entries, key=lambda item: item[0].as_posix()):
            if str(shape.get("os", "")).lower() != "linux":
                continue
            run_root = artifact_path.parent
            for run_dir in survey.find_run_dirs(run_root):
                survey.capture_fixture(
                    run_dir,
                    args.fixtures_root,
                    "claude_code",
                    version,
                    str(shape.get("os", "linux")),
                )

    if regressions:
        return 2
    if changes:
        print("nightly: wrote drift changes for " + ", ".join(changes))
    else:
        print("nightly: no drift changes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
