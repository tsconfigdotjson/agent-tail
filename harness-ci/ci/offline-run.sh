#!/usr/bin/env bash
# Run a command with no network except loopback, where the host can enforce it.
#
# macOS: sandbox-exec + harness-ci/offline.sb.
# Linux: a fresh network namespace with loopback up (GitHub's runner has passwordless
# sudo). If neither is available, run normally unless HARNESS_CI_REQUIRE_OFFLINE=1.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "$(uname -s)" == "Darwin" ]] && command -v sandbox-exec >/dev/null 2>&1; then
  exec sandbox-exec -f "$here/../offline.sb" "$@"
fi

if [[ "$(uname -s)" == "Linux" ]] && command -v unshare >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  # Bring loopback up as root, then drop back to the caller. Running Claude Code as root
  # makes its `--dangerously-skip-permissions` guard refuse the scenario.
  exec sudo -n env \
    "PATH=$PATH" "HARNESS_PATH=$PATH" "HARNESS_HOME=$HOME" \
    "HARNESS_UID=$(id -u)" "HARNESS_GID=$(id -g)" \
    unshare --net -- /bin/bash -c '
      ip link set lo up
      exec setpriv --reuid="$HARNESS_UID" --regid="$HARNESS_GID" --init-groups \
        env "PATH=$HARNESS_PATH" "HOME=$HARNESS_HOME" "$@"
    ' bash "$@"
fi

if [[ "${HARNESS_CI_REQUIRE_OFFLINE:-0}" == "1" ]]; then
  echo "offline-run: no enforced offline sandbox on this host; refusing to run" >&2
  exit 3
fi
echo "offline-run: warning: no enforced offline sandbox; running without network isolation" >&2
exec "$@"
