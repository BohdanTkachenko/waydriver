#!/bin/bash
set -euo pipefail

export HOME="${HOME:-/root}"
export XDG_RUNTIME_DIR="$(mktemp -d /tmp/waydriver-rt-XXXXXX)"

# Start a container-private session D-Bus for AT-SPI activation.
# Each container gets its own bus, so apps like gnome-calculator
# don't collide across concurrent sessions.
eval "$(dbus-launch --sh-syntax)"

exec waydriver-mcp "$@"
