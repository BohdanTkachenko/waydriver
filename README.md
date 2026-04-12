# WayDriver

[![CI](https://github.com/BohdanTkachenko/waydriver/actions/workflows/ci.yml/badge.svg)](https://github.com/BohdanTkachenko/waydriver/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/waydriver.svg)](https://crates.io/crates/waydriver)
[![docs.rs](https://docs.rs/waydriver/badge.svg)](https://docs.rs/waydriver)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A Rust library for headless GUI application testing on Wayland. Launches apps in isolated compositor sessions, interacts with them via AT-SPI accessibility APIs, and captures screenshots via PipeWire.

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

## Requirements

All dependencies are provided by the Nix flake. If not using Nix, you need:

- Mutter (with `--headless` support)
- PipeWire, WirePlumber
- gstreamer, gst-plugins-base, gst-plugins-good
- at-spi2-core
- dbus

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
