# Stage 1: Publishable builder base image
# Fedora 42 matches the runtime image, so binaries built here are ABI-compatible
# with the runtime. Users extend this image to build their own GTK4 apps for
# testing against waydriver-mcp.
FROM fedora:42 AS builder-base

RUN dnf install -y \
    gcc g++ make pkg-config meson ninja-build cmake \
    dbus-devel at-spi2-core-devel \
    gstreamer1-devel gstreamer1-plugins-base-devel \
    pipewire-devel \
    gtk4-devel glib2-devel libadwaita-devel \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# Stage 2: Build the MCP server and the e2e fixture. Both end up in the
# builder image; downstream stages pick which binaries to carry forward.
FROM builder-base AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p waydriver-mcp -p waydriver-fixture-gtk \
    && cargo build --release -p waydriver-examples --example gnome_calculator

# Stage 3: Base runtime image — shared pieces for both production and e2e.
FROM fedora:42 AS runtime-base

RUN dnf install -y \
    dbus dbus-x11 at-spi2-core \
    mutter pipewire wireplumber pipewire-gstreamer \
    gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-good \
    gsettings-desktop-schemas \
    && dnf clean all

COPY --from=builder /src/target/release/waydriver-mcp /usr/local/bin/
COPY docker-entrypoint.sh /usr/local/bin/

ENTRYPOINT ["docker-entrypoint.sh"]

# Stage 4a (default): production image — just waydriver-mcp. Users bring
# their own app binaries via bind-mount or a derived image.
FROM runtime-base AS runtime

# Stage 4b: e2e image — adds the GTK4 fixture binary and its runtime
# dependencies. Drives `cargo test -p waydriver-mcp --test e2e -- --ignored`
# in CI and when reproducing e2e locally via `docker build --target
# runtime-e2e -t waydriver-mcp-e2e .`.
FROM runtime-base AS runtime-e2e

RUN dnf install -y gtk4 libadwaita && dnf clean all
COPY --from=builder /src/target/release/waydriver-fixture-gtk /usr/local/bin/

# Stage 4c: examples image — adds gnome-calculator and the example
# binaries from `crates/waydriver-examples`. The CI `examples` job
# runs the example end-to-end against this image; locally reproduce
# via `docker build --target runtime-examples -t waydriver-examples .`
# and `docker run --rm waydriver-examples gnome_calculator`.
FROM runtime-base AS runtime-examples

RUN dnf install -y gtk4 libadwaita gnome-calculator && dnf clean all
COPY --from=builder /src/target/release/examples/gnome_calculator /usr/local/bin/
