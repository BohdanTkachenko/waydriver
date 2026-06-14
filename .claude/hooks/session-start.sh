#!/bin/bash
# SessionStart hook for Claude Code on the web (remote, non-Nix environment).
#
# Locally this repo is developed via the Nix flake (`nix develop` / direnv
# `use flake`), but the Claude Code cloud execution environment is plain
# Ubuntu 24.04 with apt — there is no Nix here. This script installs the same
# system dependencies the flake/README provide, so that `cargo build`,
# `cargo fmt`, `cargo clippy` and `cargo test` work in the remote session.
#
# Limitation (by design): the `waydriver-fixture-gtk` crate needs the gtk-rs
# `libadwaita` `v1_6` feature, which requires the C libadwaita >= 1.6. Ubuntu
# 24.04 only ships libadwaita 1.5, so that single crate cannot be built here.
# `--exclude waydriver-fixture-gtk` keeps the everyday loop fast — but if you
# change the fixture OR `waydriver-e2e`, do NOT defer verification to CI: build
# and run them in the Fedora dev-container (`scripts/dev-container.sh`, which
# auto-starts dockerd here). CI's `e2e` job runs only `fixture_via_docker`, so
# the `waydriver-e2e --ignored` suite runs nowhere but that container.
set -euo pipefail

# Only act in the remote (Claude Code on the web) environment. On a local Nix
# machine this is a clean no-op — `nix develop` provides the deps there.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

# Resolve the project dir whether invoked as a registered hook (CLAUDE_PROJECT_DIR
# is set) or run manually for validation (derive it from this script's location).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(cd "$SCRIPT_DIR/../.." && pwd)}"

# Keep the verbose apt/cargo output out of the session context; surface only on
# failure (printed by the ERR trap below).
LOG="/tmp/waydriver-session-start.log"
: > "$LOG"
trap 'echo "[session-start] FAILED — last 40 lines of $LOG:"; tail -n 40 "$LOG" 2>/dev/null || true' ERR

export DEBIAN_FRONTEND=noninteractive

echo "[session-start] Installing waydriver system dependencies via apt (log: $LOG)…"
apt-get update -qq >>"$LOG" 2>&1

# --no-install-recommends keeps the closure lean and predictable. Package names
# mirror the README "If not using Nix" table (Debian/Ubuntu) plus the extra
# -dev packages the CI Ubuntu job installs for compilation.
apt-get install -y --no-install-recommends \
  build-essential pkg-config \
  libglib2.0-dev libdbus-1-dev libatspi2.0-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libpipewire-0.3-dev \
  mutter pipewire wireplumber \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-pipewire \
  at-spi2-core dbus dbus-x11 gsettings-desktop-schemas \
  >>"$LOG" 2>&1

# Rust is preinstalled in the base image; just ensure the lint components exist.
rustup component add rustfmt clippy >>"$LOG" 2>&1 || true

# Warm the crate cache so the first in-session build is faster (non-fatal).
if [ -f "$PROJECT_DIR/Cargo.lock" ]; then
  echo "[session-start] Pre-fetching cargo crate cache…"
  (cd "$PROJECT_DIR" && cargo fetch --locked >>"$LOG" 2>&1) \
    || echo "[session-start] note: cargo fetch failed; crates will download on first build"
fi

echo "[session-start] System dependencies ready (Ubuntu/apt, non-Nix cloud env)."
echo "[session-start] 'waydriver-fixture-gtk' is unbuildable here (needs libadwaita>=1.6; Ubuntu 24.04 ships 1.5)."
echo "[session-start] Fast loop for the rest: cargo <build|clippy|test> --workspace --exclude waydriver-fixture-gtk"
echo "[session-start] Changed the fixture or waydriver-e2e? Verify in Fedora — don't defer to CI:"
echo "[session-start]   scripts/dev-container.sh bash -lc 'cargo build -p waydriver-fixture-gtk && dbus-run-session -- cargo test -p waydriver-e2e -- --ignored --test-threads=1'"
echo "[session-start]   (CI's e2e job runs ONLY fixture_via_docker — the waydriver-e2e suite runs nowhere but that container.)"
