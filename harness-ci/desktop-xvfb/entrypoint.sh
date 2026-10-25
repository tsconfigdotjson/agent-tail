#!/bin/bash
# Container entrypoint: set up a virtual X server, a session D-Bus and AT-SPI,
# then run the driver as the unprivileged `claude` user. The Desktop 3P policy
# file must be root-owned, so it is written here before dropping privileges.
set -euo pipefail

MODE="${1:-standin}"
if [[ "$#" -gt 0 ]]; then shift; fi

OUT="${AGENTTAIL_OUT:-/out}"
DISPLAY="${DISPLAY:-:99}"
export OUT DISPLAY
mkdir -p "$OUT" /home/claude/work /tmp/agenttail-xvfb/runtime /tmp/agenttail-xvfb/home
chmod 777 "$OUT" /tmp/agenttail-xvfb /tmp/agenttail-xvfb/home
chown -R claude:claude "$OUT" /home/claude /tmp/agenttail-xvfb
chmod 700 /tmp/agenttail-xvfb/runtime

desktop_3p=0
case "$MODE" in
  desktop|desktop-probe) desktop_3p=1 ;;
esac

if [[ "$desktop_3p" == "1" ]]; then
  mkdir -p /etc/claude-desktop
  python3 - "${AGENTTAIL_MOCK_URL:-http://127.0.0.1:8765}" > /etc/claude-desktop/managed-settings.json <<'PY'
import json, sys
url = sys.argv[1].rstrip("/")
print(json.dumps({
    "inferenceProvider": "gateway",
    "inferenceGatewayBaseUrl": url,
    "inferenceGatewayApiKey": "agenttail-mock-key",
    "inferenceGatewayAuthScheme": "bearer",
    "inferenceModels": [{"name": "claude-sonnet-4-5", "labelOverride": "Mock Sonnet"}],
    "disableDeploymentModeChooser": True,
    "deploymentOrganizationUuid": "00000000-0000-4000-8000-000000000080",
    "disableEssentialTelemetry": True,
    "disableNonessentialTelemetry": True,
    "modelCatalogEnabled": False,
    "disableAutoUpdates": True,
    "coworkTabEnabled": True,
    "chatTabEnabled": True,
    "isClaudeCodeForDesktopEnabled": True,
    "allowedWorkspaceFolders": ["/home/claude/work"],
    "coworkEgressAllowedHosts": ["127.0.0.1", "localhost"],
    "developerModeEnabled": True,
}, indent=2, sort_keys=True) + "\n")
PY
  chown root:root /etc/claude-desktop /etc/claude-desktop/managed-settings.json
  chmod 755 /etc/claude-desktop
  chmod 644 /etc/claude-desktop/managed-settings.json
else
  rm -f /etc/claude-desktop/managed-settings.json
fi

case "$MODE" in
  standin)
    set -- --target standin --scenario /opt/agenttail-desktop/scenarios/standin_plain_qa.json "$@"
    ;;
  probe)
    set -- --target claude-desktop --probe-only "$@"
    ;;
  desktop-probe)
    set -- --target claude-desktop --probe-only "$@"
    ;;
  desktop)
    set -- --target claude-desktop --chooser jev --scenario /opt/agenttail-desktop/scenarios/claude_desktop_plain_qa.json "$@"
    ;;
  driver)
    ;;
  *)
    echo "unknown mode $MODE (use standin, probe, desktop-probe, desktop, or driver)" >&2
    exit 2
    ;;
esac

Xvfb "$DISPLAY" -screen 0 "${AGENTTAIL_SCREEN:-1440x900x24}" -nolisten tcp > "$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
cleanup() {
  kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT
for _ in $(seq 1 50); do
  if xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
  echo "Xvfb did not start; see $OUT/xvfb.log" >&2
  exit 1
fi

# Run the session bus and every GUI process as the same unprivileged user.
# Starting the bus as root and the app as claude makes D-Bus EXTERNAL auth fail.
exec runuser -u claude --preserve-environment -- env \
  HOME=/home/claude \
  USER=claude \
  LOGNAME=claude \
  DISPLAY="$DISPLAY" \
  XDG_RUNTIME_DIR=/tmp/agenttail-xvfb/runtime \
  dbus-run-session -- /opt/agenttail-desktop/session.sh "$@"
