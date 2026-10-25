#!/usr/bin/env python3
"""Build a pinned + latest GitHub Actions matrix for one harness lane.

The pinned versions are the known-good releases used for reproduction. `latest` is resolved
by the caller (npm or the vendor installer) immediately before the matrix is expanded, so the
job names and installs all refer to the same concrete release. A pinned version that is also
current is intentionally kept as a separate entry: the pinned job is the reproduction lane and
the latest job is the autoupdate alarm.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterable

PINNED = {
    "claude-code": ("2.1.281", "2.1.310"),
    "pi": ("1.4.0",),
    "codex": ("0.160.1",),
    "opencode": ("1.19.4",),
    "cursor": ("2026.10.01-e373342",),
    "gemini": ("0.65.0",),
    "copilot": ("1.0.103",),
}
DEFAULT_OSES = ("ubuntu-latest", "macos-latest")


def build_matrix(harness: str, latest: str, oses: Iterable[str] = DEFAULT_OSES) -> dict[str, list[dict[str, str]]]:
    """Return an Actions matrix with one job per pinned/latest version and OS."""
    latest = latest.strip()
    if not latest:
        raise ValueError("latest version is empty")
    versions = [(version, version) for version in PINNED[harness]]
    versions.append(("latest", latest))
    entries = [
        {"label": label, "version": version, "os": os_name}
        for label, version in versions
        for os_name in oses
    ]
    return {"include": entries}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", choices=sorted(PINNED))
    parser.add_argument("--latest", required=True, help="the concrete version resolved for the latest lane")
    parser.add_argument(
        "--github-output",
        action="store_true",
        help="prefix the JSON with value= for a GITHUB_OUTPUT file",
    )
    args = parser.parse_args(argv)
    try:
        matrix = build_matrix(args.harness, args.latest)
    except ValueError as error:
        parser.error(str(error))
    text = json.dumps(matrix, separators=(",", ":"))
    if args.github_output:
        text = f"value={text}"
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
