# Stage 1: Build the waydriver-mcp binary
FROM fedora:42 AS builder

RUN dnf install -y \
    gcc pkg-config \
    dbus-devel at-spi2-core-devel \
    gstreamer1-devel gstreamer1-plugins-base-devel \
    pipewire-devel \
    && dnf clean all

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /src
COPY . .
RUN cargo build --release -p waydriver-mcp

# Stage 2: Runtime image
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
