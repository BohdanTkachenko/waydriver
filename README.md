# WayDriver

[![CI](https://github.com/BohdanTkachenko/waydriver/actions/workflows/ci.yml/badge.svg)](https://github.com/BohdanTkachenko/waydriver/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/waydriver.svg)](https://crates.io/crates/waydriver)
[![docs.rs](https://docs.rs/waydriver/badge.svg)](https://docs.rs/waydriver)
[![Documentation](https://img.shields.io/badge/docs-waydriver.io-blue)](https://waydriver.io)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Headless GUI application testing on Wayland.** WayDriver launches GTK apps in isolated, headless compositor sessions, drives them through the AT-SPI accessibility tree, and captures screenshots and WebM video via PipeWire — no physical display required.

It comes in two forms:

- **[`waydriver`](https://crates.io/crates/waydriver)** — a Rust library for writing headless GUI tests.
- **`waydriver-mcp`** — a standalone [Model Context Protocol](https://modelcontextprotocol.io) server that lets AI assistants (Claude Code, Claude Desktop, …) drive GTK4 apps directly.

> 📖 **Full documentation — guides, API reference, and architecture notes — lives at [waydriver.io](https://waydriver.io).**

## Demo

The clip below is the full output of the [`gnome_calculator` example](crates/waydriver-examples/examples/gnome_calculator.rs) (`cargo run -p waydriver-examples --example gnome_calculator`): a session lifecycle, AT-SPI button clicks, keyboard chords, a typed unit conversion, and per-step verification via XPath locators — recorded by WayDriver itself via PipeWire.

<video src="https://github.com/BohdanTkachenko/waydriver/raw/main/docs/src/assets/demo.webm" controls width="640">Your browser can't play this video — <a href="https://github.com/BohdanTkachenko/waydriver/raw/main/docs/src/assets/demo.webm">download it here</a>.</video>

## Features

- **Headless** — runs a real Mutter compositor with no monitor attached; ideal for CI.
- **Precise** — target widgets with XPath 1.0 over the AT-SPI tree; actions auto-wait for visibility and enablement.
- **Real input** — keyboard and pointer events through the full Wayland pipeline (Mutter RemoteDesktop), plus direct AT-SPI actions.
- **Capture** — PNG screenshots and full-session VP8/WebM recordings via PipeWire.
- **AI-ready** — `waydriver-mcp` exposes the whole surface over MCP for AI assistants.
- **Isolated** — every session gets its own compositor, private D-Bus, and app settings, so tests never touch your real desktop.

## Quick start

### Write a test in Rust

```sh
cargo add waydriver waydriver-compositor-mutter waydriver-input-mutter waydriver-capture-mutter
```

Once you have a running session, drive the app with XPath locators:

```rust
// Actions auto-wait for the element to be visible + enabled before firing.
session.locate("//Button[@name='primary-button']").click().await?;
session.locate("//Text[@name='search']").set_text("hello").await?;
session.press_chord("Ctrl+Shift+S").await?;
let png = session.take_screenshot().await?;
```

→ [Getting Started](https://waydriver.io/getting-started.html) walks through system requirements and a complete, runnable example. The full action surface is in the [Locator API](https://waydriver.io/guide/locators.html) reference.

### Drive an app from an AI assistant (MCP)

Prebuilt images are published to GHCR. Point your MCP client (e.g. `.mcp.json` for Claude Code) at the runtime image:

```json
{
  "mcpServers": {
    "waydriver-mcp": {
      "command": "sh",
      "args": ["-c", "docker run --rm -i --network none -v \"$PWD:/workspace:ro\" -v /tmp/waydriver:/tmp/waydriver ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
    }
  }
}
```

→ The [MCP Server guide](https://waydriver.io/guide/mcp-server.html) covers the full tool reference, building your app in the matching container, and per-session options.

## Backend support

WayDriver is backend-agnostic — three traits (`CompositorRuntime`, `InputBackend`, `CaptureBackend`) define the interface. Today the **GNOME/Mutter** backend is implemented; KWin and Sway can be added as sibling crates without touching the core. See [Architecture](https://waydriver.io/architecture.html) for how it fits together.

## Contributing

Contributions are welcome. Development setup, build/test commands, and the architecture deep-dive live in [`AGENTS.md`](AGENTS.md) and the [Contributing guide](https://waydriver.io/contributing.html).

## License

Licensed under the [Apache License 2.0](LICENSE).
