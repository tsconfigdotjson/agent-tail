#!/bin/bash
# Runs inside a `dbus-run-session` owned by the unprivileged Desktop user.
# Starts the window manager and AT-SPI bus, then runs the observe/act driver.
set -euo pipefail

OUT="${AGENT_TAIL_OUT:-/out}"
DISPLAY="${DISPLAY:-:99}"
export DISPLAY
export GTK_MODULES=gail:atk-bridge
export NO_AT_BRIDGE=0
export ACCESSIBILITY_ENABLED=1
export XDG_RUNTIME_DIR=/tmp/agent-tail-xvfb/runtime

openbox > "$OUT/openbox.log" 2>&1 &
OPENBOX_PID=$!
# at-spi-bus-launcher is in /usr/libexec, not on PATH on Ubuntu 24.04.
ATSPI_LAUNCHER=$(command -v at-spi-bus-launcher || true)
if [[ -z "$ATSPI_LAUNCHER" ]]; then ATSPI_LAUNCHER=/usr/libexec/at-spi-bus-launcher; fi
"$ATSPI_LAUNCHER" --launch-immediately > "$OUT/atspi-bus.log" 2>&1 &
ATSPI_PID=$!
cleanup() {
  kill "$OPENBOX_PID" "$ATSPI_PID" 2>/dev/null || true
}
trap cleanup EXIT
for _ in $(seq 1 30); do
  if dbus-send --session --dest=org.freedesktop.DBus --type=method_call --print-reply \
      /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner string:org.a11y.Bus 2>/dev/null | grep -q true; then
    break
  fi
  sleep 0.1
done

python3 /opt/agent-tail-desktop/desktop_driver.py "$@"
