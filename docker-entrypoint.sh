#!/bin/bash
set -euo pipefail

export HOME="${HOME:-/root}"
export XDG_RUNTIME_DIR="$(mktemp -d /tmp/waydriver-rt-XXXXXX)"

# Start a container-private session D-Bus for AT-SPI activation.
# Each container gets its own bus, so apps that use D-Bus singletons
# don't collide across concurrent sessions.
eval "$(dbus-launch --sh-syntax)"

# Enable mutter's fractional-scaling experimental feature so non-integer
# logical-monitor scales (150%, 166%, ...) are advertised and accepted by
# DisplayConfig.ApplyMonitorsConfig. Without it mutter's native headless
# backend offers integer scales only, and waydriver's start_session `scale`
# would snap any fractional request to 1.0/2.0. The setting lands in
# ~/.config/dconf/user (read directly by every per-session mutter); the write
# needs the dconf service on the bus we just launched, hence its placement
# here. Best-effort: a missing schema/dconf shouldn't block startup — integer
# scales still work.
gsettings set org.gnome.mutter experimental-features "['scale-monitor-framebuffer']" \
    2>/dev/null || echo "warn: could not enable scale-monitor-framebuffer; fractional scales unavailable" >&2

# Default command is waydriver-mcp; if the first arg starts with `-` it's
# treated as a flag for waydriver-mcp, otherwise it's exec'd as the
# command to run (used by the examples image to launch a different
# binary while still benefiting from the dbus/runtime-dir setup above).
if [ "$#" -eq 0 ] || [ "${1:0:1}" = "-" ]; then
    exec waydriver-mcp "$@"
else
    exec "$@"
fi
