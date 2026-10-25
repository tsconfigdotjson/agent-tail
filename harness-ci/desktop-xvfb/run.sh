#!/bin/bash
# Host-side wrapper: build agent-tail and the existing mock once, then run one
# Xvfb scenario. The stand-in smoke test needs no secret; it is also the nightly
# GHA lane. Desktop probes and live Jev are manual.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
MODE="${1:-standin}"
if [[ "$#" -gt 0 ]]; then shift; fi

export PATH="$HOME/.cargo/bin:$PATH"
if [[ ! -x "$ROOT/target/release/agent-tail" ]]; then
  (cd "$ROOT" && cargo build --release -p agent-tail-cli)
fi
if [[ "$MODE" == "desktop" || "$MODE" == "desktop-probe" ]]; then
  if [[ ! -x "$ROOT/harness-ci/target/release/mock-server" ]]; then
    (cd "$ROOT/harness-ci" && cargo build --release -p mock-server)
  fi
fi

mkdir -p "$HERE/runs"
chmod 777 "$HERE/runs"
export INSTALL_CLAUDE_DESKTOP="${INSTALL_CLAUDE_DESKTOP:-1}"
docker compose -f "$HERE/compose.yaml" build desktop
exec docker compose -f "$HERE/compose.yaml" run --rm desktop "$MODE" "$@"
