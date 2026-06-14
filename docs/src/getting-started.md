# Getting Started

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

## Add WayDriver to your project

Add the core library plus the Mutter backend crates:

```sh
cargo add waydriver waydriver-compositor-mutter waydriver-input-mutter waydriver-capture-mutter
```

WayDriver's API is async, so you'll also want a Tokio runtime:

```sh
cargo add tokio --features full
```

## Usage

```rust
use std::sync::Arc;
use waydriver::{Session, SessionConfig, CompositorRuntime};
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;
use waydriver_capture_mutter::MutterCapture;

let mut compositor = MutterCompositor::new();
compositor.start(None).await?;
// `state()` is `Option`; immediately after a successful `start()` it is
// always `Some` — `expect` documents that invariant locally.
let state = compositor.state().expect("state available after start");
let input = MutterInput::new(state.clone());
let capture = MutterCapture::new(state);

let session = Arc::new(Session::start(
    Box::new(compositor),
    Box::new(input),
    Box::new(capture),
    SessionConfig {
        command: "your-gtk-app".into(),
        args: vec![],
        cwd: None,
        app_name: "your-gtk-app".into(),
        // Record the entire session to a WebM file. Set to `None` to skip.
        video_output: Some("/tmp/session.webm".into()),
        video_bitrate: None, // defaults to waydriver::capture::DEFAULT_VIDEO_BITRATE (2 Mbps)
        video_fps: None,     // defaults to waydriver::capture::DEFAULT_VIDEO_FPS (15)
    },
).await?);

// Take a screenshot (returns PNG bytes).
let png = session.take_screenshot().await?;

// Target widgets with XPath selectors over the AT-SPI tree. Actions
// auto-wait for the element to be visible + enabled before firing.
session.locate("//Button[@name='primary-button']").click().await?;
session.locate("//Text[@name='search']").set_text("hello").await?;

// Keyboard input with modifier chords.
session.press_chord("Ctrl+Shift+S").await?;

// Explicit waits when auto-wait isn't enough — e.g. an item appearing
// after some async work.
session.locate("//Label[@name='status']")
    .wait_for_text(|t| t == "ready")
    .await?;

// Inspect the tree while debugging selectors.
let xml = session.dump_tree().await?;
println!("{xml}");

Arc::try_unwrap(session).unwrap().kill().await?;
```

Next: the [Locator API](./guide/locators.md) reference covers the full action surface, and the [MCP Server](./guide/mcp-server.md) chapter shows how to drive apps from an AI assistant without writing Rust.
