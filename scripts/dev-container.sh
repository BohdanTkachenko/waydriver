#!/usr/bin/env bash
# Fedora dev / bug-reproduction container for waydriver.
#
# Why this exists: locally this repo builds via the Nix flake; in the Claude
# Code cloud env it's plain Ubuntu 24.04, where the GTK fixture can't build
# (system libadwaita is 1.5, but the gtk-rs `v1_6` feature needs >= 1.6) and
# so the native e2e suite can't run. Fedora 42 ships libadwaita >= 1.6 and
# Mesa at standard paths -- exactly why the project's Dockerfile and CI use
# it. This script drops you into a Fedora 42 shell with the full build +
# runtime stack and YOUR working tree bind-mounted, so you can build the
# fixture and run the native e2e suite (crates/waydriver-e2e) to reproduce and
# debug integration bugs with fast, native-speed iteration -- no image rebuild
# per source edit (unlike the production Dockerfile, which bakes the binary in).
#
# Usage:
#   scripts/dev-container.sh                 # interactive shell in /src
#   scripts/dev-container.sh <cmd...>        # run a command in the container
#   scripts/dev-container.sh --rebuild       # force-rebuild the dev image first
#
# Inside the container, run the native e2e suite with:
#   cargo build -p waydriver-fixture-gtk     # the e2e tests don't rebuild it
#   dbus-run-session -- cargo test -p waydriver-e2e -- --ignored --test-threads=1
set -euo pipefail

# Non-Hub Fedora registry -- avoids Docker Hub's anonymous pull rate limit
# (the bare `fedora:42` in the Dockerfile pulls from Hub, which is throttled).
FEDORA_IMAGE="registry.fedoraproject.org/fedora:42"
BUILDER_TAG="waydriver-builder-base:local"
DEV_TAG="waydriver-dev:local"

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

REBUILD=0
if [ "${1:-}" = "--rebuild" ]; then REBUILD=1; shift; fi

# --- ensure the Docker daemon is reachable (the cloud env doesn't auto-start it) ---
ensure_docker() {
  if docker info >/dev/null 2>&1; then return 0; fi
  echo "[dev-container] Docker daemon not running; attempting to start dockerd…"
  if [ "$(id -u)" -ne 0 ]; then
    echo "[dev-container] ERROR: need root to start dockerd. Start Docker and retry." >&2
    exit 1
  fi
  dockerd >/tmp/dockerd.log 2>&1 &
  for _ in $(seq 1 15); do docker info >/dev/null 2>&1 && break; sleep 1; done
  docker info >/dev/null 2>&1 || {
    echo "[dev-container] ERROR: dockerd failed to start; see /tmp/dockerd.log" >&2
    tail -n 15 /tmp/dockerd.log >&2 || true
    exit 1
  }
}

# --- provide a local `fedora:42` that (a) avoids the Docker Hub anon pull rate
# limit and (b) trusts this environment's egress-gateway proxy CA so that
# in-container dnf / rustup / cargo HTTPS works. The cloud env transparently
# MITMs TLS with a private CA that fresh containers don't trust; the host's
# copies live in /usr/local/share/ca-certificates. On a normal machine (no
# such certs) this just pulls + tags fedora. The result is tagged `fedora:42`
# so the project's Dockerfile (`FROM fedora:42`) inherits it unchanged.
ensure_fedora_base() {
  local certdir="/usr/local/share/ca-certificates"
  if ls "$certdir"/*.crt >/dev/null 2>&1; then
    echo "[dev-container] Building proxy-CA-trusted fedora:42 base (egress gateway TLS interception)…"
    docker build -t fedora:42 -f - "$certdir" <<'DOCKERFILE'
FROM registry.fedoraproject.org/fedora:42
COPY *.crt /etc/pki/ca-trust/source/anchors/
RUN update-ca-trust
DOCKERFILE
  elif ! docker image inspect fedora:42 >/dev/null 2>&1; then
    echo "[dev-container] Pulling $FEDORA_IMAGE and tagging it locally as fedora:42…"
    docker pull "$FEDORA_IMAGE"
    docker tag "$FEDORA_IMAGE" fedora:42
  fi
}

build_images() {
  # Reuse the project's own builder-base stage so the build-dependency list and
  # rustup install stay single-sourced in the Dockerfile (no duplication here).
  echo "[dev-container] Building builder-base (build deps + rustup) from Dockerfile…"
  docker build --target builder-base -t "$BUILDER_TAG" "$PROJECT_DIR"

  # Derive the dev image: add the runtime stack the native e2e suite needs to
  # actually run (headless mutter + pipewire + AT-SPI + gst plugins).
  # NOTE: keep this list in sync with the runtime-base stage in ../Dockerfile.
  echo "[dev-container] Building dev image (adds runtime stack)…"
  docker build -t "$DEV_TAG" - <<EOF
FROM $BUILDER_TAG
RUN dnf install -y \
    dbus dbus-x11 at-spi2-core \
    mutter pipewire wireplumber pipewire-gstreamer \
    gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-good \
    gsettings-desktop-schemas \
 && dnf clean all
WORKDIR /src
EOF
}

ensure_docker
ensure_fedora_base
if [ "$REBUILD" = "1" ] || ! docker image inspect "$DEV_TAG" >/dev/null 2>&1; then
  build_images
fi

# Use a TTY only when attached to one, so `scripts/dev-container.sh cargo …`
# still works non-interactively (e.g. in CI or from another script).
TTY_FLAGS="-i"
[ -t 0 ] && [ -t 1 ] && TTY_FLAGS="-it"

# Named volumes keep the Fedora build cache (a different ABI than the host's
# ./target) and the crate cache out of the working tree and persistent across
# runs. The source bind-mount is read-write so edits and Cargo.lock updates
# flow back to the host.
echo "[dev-container] Entering $DEV_TAG (your working tree is at /src)…"
exec docker run --rm $TTY_FLAGS \
  -v "$PROJECT_DIR":/src \
  -v waydriver-dev-target:/src/target \
  -v waydriver-dev-cargo:/root/.cargo/registry \
  -e CARGO_TERM_COLOR=always \
  -e LIBGL_ALWAYS_SOFTWARE=1 \
  --entrypoint /bin/bash \
  "$DEV_TAG" -lc '
    # A private XDG_RUNTIME_DIR so mutter/pipewire/AT-SPI sockets have a home,
    # mirroring docker-entrypoint.sh.
    export XDG_RUNTIME_DIR="$(mktemp -d /tmp/waydriver-rt-XXXXXX)"
    if [ "$#" -eq 0 ]; then
      echo "waydriver Fedora dev shell — libadwaita $(pkg-config --modversion libadwaita-1), $(rustc --version)"
      echo "  build fixture:  cargo build -p waydriver-fixture-gtk"
      echo "  native e2e:     dbus-run-session -- cargo test -p waydriver-e2e -- --ignored --test-threads=1"
      exec bash
    fi
    exec "$@"
  ' _ "$@"
