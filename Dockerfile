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
    gtk4-devel glib2-devel \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# Stage 2: Build waydriver-mcp specifically (not published; feeds the runtime)
FROM builder-base AS builder

WORKDIR /src
COPY . .
RUN cargo build --release -p waydriver-mcp

# Stage 3: Runtime image
FROM fedora:42

RUN dnf install -y \
    dbus dbus-x11 at-spi2-core \
    mutter pipewire wireplumber pipewire-gstreamer \
    gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-good \
    gsettings-desktop-schemas \
    && dnf clean all

# Optional: install gnome-calculator for e2e testing
ARG INSTALL_CALCULATOR=false
RUN if [ "$INSTALL_CALCULATOR" = "true" ]; then \
    dnf install -y gnome-calculator && dnf clean all; \
    fi

COPY --from=builder /src/target/release/waydriver-mcp /usr/local/bin/
COPY docker-entrypoint.sh /usr/local/bin/

ENTRYPOINT ["docker-entrypoint.sh"]
