# WayDriver

[![CI](https://github.com/BohdanTkachenko/waydriver/actions/workflows/ci.yml/badge.svg)](https://github.com/BohdanTkachenko/waydriver/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/waydriver.svg)](https://crates.io/crates/waydriver)
[![docs.rs](https://docs.rs/waydriver/badge.svg)](https://docs.rs/waydriver)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A Rust library for headless GUI application testing on Wayland. Launches apps in isolated compositor sessions, interacts with them via AT-SPI accessibility APIs, and captures screenshots via PipeWire.

The repo also contains `waydriver-mcp`, a standalone MCP server binary built on top of the library that lets AI assistants drive GTK4 apps directly — see [MCP server](#mcp-server) below.

## How it works

Each test session creates an isolated environment with a headless compositor, input injection, and screen capture:

```mermaid
graph TD
    subgraph Session["Per-session processes"]
        dbus["dbus-daemon (private)"]
        dbus --- mutter["Mutter --headless --wayland"]
        mutter --- screencast["ScreenCast API (screenshots)"]
        mutter --- remotedesktop["RemoteDesktop API (input)"]
        dbus --- pipewire["PipeWire (frame capture)"]
        dbus --- wireplumber["WirePlumber (PipeWire graph manager)"]

        app["Your app (on Mutter's Wayland display)"]
        app --- atspi["AT-SPI (accessibility tree, actions)"]
    end
```

The library is backend-agnostic. Three traits define the interface:

- **`CompositorRuntime`** — lifecycle of a headless compositor (start, stop, expose Wayland display)
- **`InputBackend`** — keyboard and pointer injection
- **`CaptureBackend`** — screen capture (start/stop PipeWire streams, grab PNG frames)

Concrete implementations are separate crates. The trait-based design allows backends to be added as sibling crates without changing the core.

## Backend support

| Feature                        | Mutter                      | KWin | Sway |
| ------------------------------ | --------------------------- | ---- | ---- |
| Headless compositor            | Yes                         | —    | —    |
| Keyboard input                 | Yes (RemoteDesktop)         | —    | —    |
| Pointer input                  | Yes (RemoteDesktop)         | —    | —    |
| Screenshots                    | Yes (ScreenCast + PipeWire) | —    | —    |
| AT-SPI (UI inspection, clicks) | Yes                         | —    | —    |

Currently only Mutter is implemented (`waydriver-compositor-mutter`, `waydriver-input-mutter`, `waydriver-capture-mutter`). Each compositor has its own APIs (Mutter uses `org.gnome.Mutter.*` D-Bus interfaces, KWin has `org.kde.KWin.*`, Sway uses wlroots Wayland protocols), so each would need its own set of backend crates.

## Crate structure

| Crate                         | Purpose                                                                                      |
| ----------------------------- | -------------------------------------------------------------------------------------------- |
| `waydriver`                   | Trait definitions, `Session`, AT-SPI client, keysym helpers, shared GStreamer capture helper |
| `waydriver-compositor-mutter` | `CompositorRuntime` impl — manages Mutter, PipeWire, WirePlumber, private D-Bus              |
| `waydriver-input-mutter`      | `InputBackend` impl — keyboard/pointer via Mutter RemoteDesktop                              |
| `waydriver-capture-mutter`    | `CaptureBackend` impl — screenshots via Mutter ScreenCast + PipeWire                         |
| `waydriver-mcp`               | Binary — MCP JSON-RPC server over stdio that exposes the library to AI assistants            |

## Usage

```rust
use waydriver::{Session, SessionConfig, CompositorRuntime};
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;
use waydriver_capture_mutter::MutterCapture;

let mut compositor = MutterCompositor::new();
compositor.start().await?;
let state = compositor.state();
let input = MutterInput::new(state.clone());
let capture = MutterCapture::new(state);

let session = Session::start(
    Box::new(compositor),
    Box::new(input),
    Box::new(capture),
    SessionConfig {
        command: "gnome-calculator".into(),
        args: vec![],
        cwd: None,
        app_name: "gnome-calculator".into(),
    },
).await?;

// Take a screenshot (returns PNG bytes)
let png = session.take_screenshot().await?;

// Interact via AT-SPI
waydriver::atspi::click_element(
    &session.a11y_connection,
    &session.app_bus_name,
    &session.app_path,
    "5",
).await?;

session.kill().await?;
```

## MCP server

`waydriver-mcp` is a standalone binary that exposes the library over the [Model Context Protocol](https://modelcontextprotocol.io), letting AI assistants (Claude Desktop, Claude Code, etc.) drive GTK4 apps in isolated headless sessions. It speaks JSON-RPC over stdio and constructs the Mutter backends internally — clients only see the high-level tools below.

| Tool              | Purpose                                                               |
| ----------------- | --------------------------------------------------------------------- |
| `start_session`   | Spawn a headless Mutter session and launch a command inside it        |
| `list_sessions`   | List active session ids, app names, and Wayland displays              |
| `kill_session`    | Tear down a session and clean up all child processes                  |
| `inspect_ui`      | Dump the AT-SPI accessibility tree of the running app                 |
| `click_element`   | Click a widget by its accessible name (via AT-SPI action)             |
| `type_text`       | Type a string into a focused element through the input backend        |
| `press_key`       | Press a named key (`Return`, `Tab`, `Escape`, letters, …)             |
| `find_element`    | Find a widget by accessible name and return its role and path         |
| `move_pointer`    | Move the pointer by a relative offset in logical pixels               |
| `pointer_click`   | Press and release a pointer button (defaults to left click)           |
| `take_screenshot` | Capture a PNG via the keepalive ScreenCast stream and return its path |

### Why Docker?

waydriver-mcp needs ~8 system services at runtime (mutter, pipewire, wireplumber, dbus, AT-SPI, gstreamer). Installing these manually is fragile and distro-specific. Docker solves four problems:

- **Security** — the MCP server spawns arbitrary processes, interacts with them via D-Bus, and captures their screen. Running this on your host session gives it access to everything your user can do. Inside a container, it only sees what you explicitly mount — no access to your files, browser sessions, or credentials. Add `--network none` to block network access entirely
- **Zero-setup distribution** — `docker pull` and you're running, no system packages to install
- **D-Bus isolation** — each container gets its own dbus-daemon, so apps like gnome-calculator don't interfere across concurrent test sessions (the singleton D-Bus activation problem)
- **ABI compatibility** — apps built inside the container are guaranteed to link against the same libraries the MCP runtime uses

### Running with Docker (recommended)

Prebuilt images are published to [GitHub Container Registry](https://github.com/BohdanTkachenko/waydriver/pkgs/container/waydriver-mcp) for each release:

| Image                                           | Purpose                                                                        |
| ----------------------------------------------- | ------------------------------------------------------------------------------ |
| `ghcr.io/bohdantkachenko/waydriver-mcp`         | Runtime — MCP server with all system deps                                      |
| `ghcr.io/bohdantkachenko/waydriver-mcp-builder` | Build env — Fedora 42 + Rust + gcc/g++ + meson + cmake + GTK4/GLib dev headers |

```sh
docker pull ghcr.io/bohdantkachenko/waydriver-mcp:latest
docker pull ghcr.io/bohdantkachenko/waydriver-mcp-builder:latest
```

Use the builder image to compile your app in a Fedora environment that matches the runtime. The resulting binary is ABI-compatible with the runtime image. See [Testing your app](#testing-your-app-with-waydriver-mcp) below for language-specific build examples.

MCP client config (e.g. `.mcp.json` for Claude Code):

```json
{
  "mcpServers": {
    "waydriver-mcp": {
      "command": "sh",
      "args": ["-c", "docker run --rm -i --network none -v \"$PWD:/workspace:ro\" -v /tmp/waydriver:/tmp ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
    }
  }
}
```

- `$PWD:/workspace:ro` — mounts the project directory so the MCP can launch your app binaries from `/workspace/`
- `/tmp/waydriver:/tmp` — makes screenshots accessible on the host at `/tmp/waydriver/`
- `--network none` — the MCP server doesn't need internet access

For NixOS users, also mount the Nix store so Nix-built binaries work inside the container:

```json
{
  "mcpServers": {
    "waydriver-mcp": {
      "command": "sh",
      "args": ["-c", "docker run --rm -i --network none -v /nix/store:/nix/store:ro -v \"$PWD:/workspace:ro\" -v /tmp/waydriver:/tmp ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
    }
  }
}
```

Or build from source:

```sh
docker build -t waydriver-mcp .
```

### Testing your app with waydriver-mcp

The MCP server is persistent — it stays up for the entire AI assistant session. You rebuild your app independently, and each `start_session` call picks up the latest binary from the volume. No MCP restart needed between iterations.

**Rust apps** — build with the builder image, volume-mount the binary:

```sh
docker run --rm -v "$PWD:/src:ro" -v "$PWD/build:/out" \
  ghcr.io/bohdantkachenko/waydriver-mcp-builder:latest \
  sh -c "cp -r /src /tmp/build && cd /tmp/build && cargo build --release && cp target/release/myapp /out/"
```

```json
{
  "mcpServers": {
    "waydriver-mcp": {
      "command": "docker",
      "args": ["run", "--rm", "-i",
        "-v", "/path/to/myapp/build:/workspace:ro",
        "ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
    }
  }
}
```

Then call `start_session` with `command: "/workspace/myapp"`.

**C/C++ apps** — the builder image includes gcc, g++, meson, ninja-build, cmake, and GTK4/GLib dev headers:

```sh
docker run --rm -v "$PWD:/src:ro" -v "$PWD/build:/out" \
  ghcr.io/bohdantkachenko/waydriver-mcp-builder:latest \
  sh -c "cp -r /src /tmp/build && cd /tmp/build && meson setup _build && meson compile -C _build && cp _build/myapp /out/"
```

For extra deps (e.g. `libadwaita-devel`), extend the builder:

```dockerfile
FROM ghcr.io/bohdantkachenko/waydriver-mcp-builder:latest
RUN dnf install -y libadwaita-devel
```

**Node/Python apps** — extend the runtime image to add the interpreter, use a named volume for deps:

```dockerfile
FROM ghcr.io/bohdantkachenko/waydriver-mcp:latest
RUN dnf install -y nodejs && dnf clean all
```

Install deps into a named volume (re-run only when lockfile changes):

```sh
docker volume create myapp-nodemods
docker run --rm \
  -v "$PWD/package.json:/app/package.json:ro" \
  -v "$PWD/package-lock.json:/app/package-lock.json:ro" \
  -v "myapp-nodemods:/app/node_modules" \
  -w /app \
  ghcr.io/bohdantkachenko/waydriver-mcp-builder:latest \
  sh -c "dnf install -y nodejs npm && npm ci --omit=dev"
```

Mount source + deps — edit source freely, MCP picks up changes on next `start_session`:

```json
"args": ["run", "--rm", "-i",
  "-v", "/path/to/myapp/src:/app/src:ro",
  "-v", "myapp-nodemods:/app/node_modules:ro",
  "myapp-mcp:latest"]
```

**NixOS users** — mount `/nix/store` so Nix-built binaries just work:

```json
"args": ["run", "--rm", "-i",
  "-v", "/nix/store:/nix/store:ro",
  "-v", "/path/to/myapp:/workspace:ro",
  "ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
```

### Running with Nix

For local development without Docker, the Nix app wraps the binary with the required runtime env vars:

```sh
nix run .#mcp
```

Sessions are kept in an in-memory `HashMap` keyed by id, so multiple apps can run concurrently within one server process.

## Requirements

All dependencies are provided by the Nix flake (`nix develop`). If not using Nix, you need the following system packages.

### Build dependencies

| Debian/Ubuntu                      | Fedora                          | Arch               |
| ---------------------------------- | ------------------------------- | ------------------ |
| `pkg-config`                       | `pkg-config`                    | `pkg-config`       |
| `libglib2.0-dev`                   | `glib2-devel`                   | `glib2`            |
| `libgstreamer1.0-dev`              | `gstreamer1-devel`              | `gstreamer`        |
| `libgstreamer-plugins-base1.0-dev` | `gstreamer1-plugins-base-devel` | `gst-plugins-base` |

### Runtime dependencies

| Debian/Ubuntu               | Fedora                        | Arch                  |
| --------------------------- | ----------------------------- | --------------------- |
| `mutter`                    | `mutter`                      | `mutter`              |
| `pipewire`                  | `pipewire`                    | `pipewire`            |
| `wireplumber`               | `wireplumber`                 | `wireplumber`         |
| `gstreamer1.0-plugins-base` | `gstreamer1-plugins-base`     | `gst-plugins-base`    |
| `gstreamer1.0-plugins-good` | `gstreamer1-plugins-good`     | `gst-plugins-good`    |
| `gstreamer1.0-pipewire`     | `gstreamer1-plugins-pipewire` | `gst-plugin-pipewire` |
| `at-spi2-core`              | `at-spi2-core`                | `at-spi2-core`        |
| `dbus`                      | `dbus`                        | `dbus`                |

**Quick install:**

```sh
# Debian/Ubuntu
sudo apt install pkg-config libglib2.0-dev libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev mutter pipewire wireplumber \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-pipewire at-spi2-core dbus

# Fedora
sudo dnf install pkg-config glib2-devel gstreamer1-devel \
  gstreamer1-plugins-base-devel mutter pipewire wireplumber \
  gstreamer1-plugins-base gstreamer1-plugins-good \
  gstreamer1-plugins-pipewire at-spi2-core dbus

# Arch
sudo pacman -S pkg-config glib2 gstreamer gst-plugins-base \
  gst-plugins-good gst-plugin-pipewire mutter pipewire \
  wireplumber at-spi2-core dbus
```

## Architecture notes

### Keepalive ScreenCast stream

In headless mode, Mutter only composites (and delivers Wayland frame callbacks) when a ScreenCast consumer is pulling frames. Without an active stream, GTK4 apps render their first frame but never repaint — the frame clock never ticks.

`Session::start` opens a persistent ScreenCast stream that stays alive for the session's lifetime. This keeps Mutter compositing continuously so frame callbacks flow and GTK4 apps repaint normally.

### Input: RemoteDesktop vs AT-SPI

Two input paths are available, with different trade-offs:

- **RemoteDesktop keyboard/pointer** (`press_keysym`, `pointer_button`) — events go through the full Wayland input pipeline (Mutter -> Wayland protocol -> GDK -> GTK event loop). GTK4 processes them normally and repaints. Use this for interactions that need to produce visible changes.

- **AT-SPI actions** (`click_element`) — directly invoke widget signal handlers by accessible name. Accurate and name-based, but they update GTK4's internal model without triggering compositor redraws. Useful for reading the accessibility tree and programmatic activation, but screenshots taken after AT-SPI-only interactions may show stale frames.

### App isolation

Apps are launched with `GSETTINGS_BACKEND=keyfile` and `XDG_CONFIG_HOME` pointing to the per-session runtime directory. This bypasses the host dconf daemon entirely, so each session starts with default app state and never reads or writes the user's settings.

### Dual D-Bus

GTK4's built-in AT-SPI backend only registers on the host session bus — it ignores custom `DBUS_SESSION_BUS_ADDRESS`. So each session uses two D-Bus connections:

- **Host session bus**: AT-SPI communication with the app
- **Private D-Bus**: Mutter's ScreenCast and RemoteDesktop APIs (isolated from the host compositor)

```mermaid
graph LR
    subgraph Host
        host_dbus["Host session bus"]
    end

    subgraph Session["Per-session"]
        private_dbus["Private D-Bus"]
        mutter["Mutter"]
        app["Your app"]
        waydriver["WayDriver"]
    end

    waydriver -- "AT-SPI" --> host_dbus
    app -- "AT-SPI register" --> host_dbus
    waydriver -- "ScreenCast\nRemoteDesktop" --> private_dbus
    mutter -- "org.gnome.Mutter.*" --> private_dbus
```

### Screenshot pipeline

```mermaid
graph LR
    screencast["Mutter ScreenCast API"]
    monitor["RecordMonitor\n(virtual monitor)"]
    pipewire["PipeWire stream\n(keepalive)"]
    gst["GStreamer pipeline\n(in-process)"]
    png["PNG bytes"]

    screencast --> monitor --> pipewire --> gst --> png
```

The keepalive stream doubles as the capture source — `take_screenshot` reads frames directly from it via the GStreamer Rust bindings (`gstreamer` + `gstreamer-app` crates).
