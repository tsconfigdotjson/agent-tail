#!/usr/bin/env python3
"""Resolve the Cursor Agent release referenced by the official install script.

Cursor's installer is the public source of truth for the current CLI build and embeds the
versioned download URL. Parsing it is safer than running an unpinned shell script in CI, and
it lets the workflow turn `latest` into a concrete version before the matrix is expanded.
"""

from __future__ import annotations

import argparse
import re
import sys
import urllib.request
from pathlib import Path

INSTALLER_URL = "https://cursor.com/install"
DOWNLOAD_RE = re.compile(r"https://downloads\.cursor\.com/lab/([^/\s\"']+)/")
VERSION_RE = re.compile(r"^\d{4}\.\d{2}\.\d{2}-[0-9a-fA-F]+$")


def version_from_installer(text: str) -> str:
    """Return the single versioned Cursor download referenced by an installer script."""
    versions = list(dict.fromkeys(DOWNLOAD_RE.findall(text)))
    if not versions:
        raise ValueError("installer does not contain a versioned downloads.cursor.com URL")
    if len(versions) != 1:
        raise ValueError(f"installer references multiple Cursor versions: {', '.join(versions)}")
    version = versions[0]
    if not VERSION_RE.fullmatch(version):
        raise ValueError(f"unexpected Cursor version format: {version!r}")
    return version


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, help="read a saved installer instead of fetching it")
    parser.add_argument("--url", default=INSTALLER_URL, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    try:
        if args.installer is not None:
            text = args.installer.read_text(encoding="utf-8")
        else:
            with urllib.request.urlopen(args.url, timeout=30) as response:
                text = response.read().decode("utf-8", "replace")
        print(version_from_installer(text))
    except (OSError, ValueError) as error:
        print(f"cursor_version: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
