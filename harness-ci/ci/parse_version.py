#!/usr/bin/env python3
"""Extract a dotted three-part version from a CLI's --version output.

Copilot prints a line like ``GitHub Copilot CLI 1.0.103.``. Splitting that with
``awk '{print $4}'`` keeps the trailing punctuation, and stripping dots with
``tr -d '.'`` would turn ``1.0.103`` into ``10103``. Match the version itself so
punctuation is ignored and the internal dots are preserved.
"""

from __future__ import annotations

import re
import sys

VERSION_RE = re.compile(r"(?<![\d.])(\d+\.\d+\.\d+)(?!\d|\.\d)")


def parse_version(text: str) -> str:
    """Return the first three-component dotted version in *text*."""
    match = VERSION_RE.search(text)
    if match is None:
        raise ValueError(f"no three-component version found in {text!r}")
    return match.group(1)


def main() -> int:
    try:
        print(parse_version(sys.stdin.read()))
    except ValueError as error:
        print(f"parse_version: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
