#!/bin/bash
set -euo pipefail

export HOME="${HOME:-/root}"
export XDG_RUNTIME_DIR="$(mktemp -d /tmp/waydriver-rt-XXXXXX)"

# Start a container-private session D-Bus for AT-SPI activation.
# Each container gets its own bus, so apps that use D-Bus singletons
# don't collide across concurrent sessions.
eval "$(dbus-launch --sh-syntax)"

# Default command is waydriver-mcp; if the first arg starts with `-` it's
# treated as a flag for waydriver-mcp, otherwise it's exec'd as the
# command to run (used by the examples image to launch a different
# binary while still benefiting from the dbus/runtime-dir setup above).
if [ "$#" -eq 0 ] || [ "${1:0:1}" = "-" ]; then
    exec waydriver-mcp "$@"
else
    exec "$@"
fi
