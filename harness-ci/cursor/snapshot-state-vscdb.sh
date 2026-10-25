#!/bin/sh
# Linux entry point for the Cursor state.vscdb approval snapshot.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$here/snapshot_state_vscdb.py" "$@"
