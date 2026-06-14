# MCP Server

`waydriver-mcp` is a standalone binary that exposes the library over the [Model Context Protocol](https://modelcontextprotocol.io), letting AI assistants (Claude Desktop, Claude Code, etc.) drive GTK4 apps in isolated headless sessions. It speaks JSON-RPC over stdio and constructs the Mutter backends internally — clients only see the high-level tools below.

| Tool              | Purpose                                                               |
| ----------------- | --------------------------------------------------------------------- |
| `start_session`   | Spawn a headless Mutter session and launch a command inside it (optional `report_dir`, `resolution`, `scale`, `isolate_settings`, `gsettings`, `record_video`, `video_bitrate`, `capture_external_effects` overrides per session) |
| `list_sessions`   | List active session ids, app names, and Wayland displays              |
| `kill_session`    | Tear down a session and clean up all child processes                  |
| `set_setting`     | Change a GSettings key on the running session live — rewrites the isolated keyfile in place so the app re-applies it via its `changed` handler (cursor, fonts, color-scheme, …) without a restart |
| `dump_tree`       | Dump the AT-SPI accessibility tree as XML — each node carries a `_ref` you can target with `query`/`click`/etc. |
| `query`           | Evaluate an XPath over the tree; returns every match's role, name, attributes, and states |
| `click` / `double_click` / `right_click` | Invoke an element's primary / secondary / tertiary AT-SPI `Action`. Auto-waits for visibility + enablement. |
| `hover`           | Move the pointer to an element's center — drives a real Wayland motion event so hover-state UI repaints |
| `drag_to`         | Press, move across an element's center, release — full Wayland drag gesture |
| `drag_to_coords`  | Like `drag_to`, but release at raw screen-absolute `(x, y)` — drop onto empty space or off the source window (libadwaita tab drag-out and other "drop onto nothing" DnD) |
| `focus`           | Give keyboard focus to an element via AT-SPI `Component::grab_focus`  |
| `set_text`        | Replace an editable element's contents via `EditableText` (fast, requires the interface) |
| `fill`            | Focus + clear + type — fallback for widgets without `EditableText` (e.g. `GtkTextView`/`GtkEntry`). Tries AT-SPI `Component::grab_focus` first; widgets whose bridge doesn't expose Component (the documented GTK4 case) fall back to a pointer click at the widget's centre to drive focus through the input layer, the same way a user would. Set `assume_focused: true` to skip the whole focus step when the target is already focused. Supports `caret_nav`/`select_all` clear modes. |
| `select_option`   | Pick an entry from a Selection-interface container (combo box, list, …) by label or by index |
| `read_text`       | Read an element's text via the `Text` interface                       |
| `read_value`      | Read an element's AT-SPI `Value` (current/min/max) — a scrolled view's offset, or a slider/progress/spin value |
| `scroll`          | Scroll a located area by wheel detents along an axis (parks the pointer over it first); pair with `read_value` to confirm the offset moved |
| `type_text`       | Type a string into the currently focused element through the input backend |
| `press_key`       | Press a named key or chord (`Return`, `Ctrl+A`, `Shift+Tab`, `Escape`, …) |
| `move_pointer`    | Move the pointer by a relative offset in logical pixels               |
| `pointer_click`   | Press and release a pointer button (defaults to left click)           |
| `take_screenshot` | Capture a PNG via the keepalive ScreenCast stream and return its path |
| `compare_element_to_baseline` | Crop an element and diff it against a committed reference PNG (perceptual CIEDE2000) — returns a diff *score* (not a pass/fail verdict) and writes a red-highlighted diff image on mismatch |
| `get_captured_effects` | Read the desktop notifications and portal open-URI requests the app emitted onto the session bus (mock D-Bus sinks). Requires `capture_external_effects: true` on `start_session`; effects have no AT-SPI projection, so this is the only way to assert on them |
| `launch_secondary_instance` | Relaunch the app with extra args in the same session env — a single-instance `GApplication` forwards the command line to the running primary; observe the primary's reaction via `wait_for_stdout_line`/`query` |

Selectors use XPath 1.0 against a snapshot of the AT-SPI tree serialized to XML, with role names normalized to PascalCase (e.g. `push button` → `Button`). Example XPaths: `//Button[@name='OK']`, `//Text[@name='search']`, `//MenuItem[contains(@name, 'Mode')]`, `(//Button)[last()]`.

Each session produces output under a configurable **report directory**. Screenshots are written as `{report_dir}/{session_id}/{session_id}-{n}.png` — each session gets its own subdirectory and `n` increments per `take_screenshot` call. The base `report_dir` defaults to `/tmp/waydriver` and can be overridden with the `--report-dir <PATH>` CLI flag or the `WAYDRIVER_REPORT_DIR` environment variable. Individual `start_session` calls may also pass a `report_dir` argument to override the server default for that session.

Alongside the screenshots, each session writes:

- **`{session_id}.webm`** — full-session VP8/WebM recording of the display at 15 fps, finalized with a seekhead on `kill_session`. On by default; disable per-server with `--record-video false` / `WAYDRIVER_RECORD_VIDEO=false`, or per-session with `start_session`'s `record_video: false`. Bitrate via `--video-bitrate <bits/sec>` / `WAYDRIVER_VIDEO_BITRATE` (default `2_000_000`) or per-session `video_bitrate`.
- **`events.jsonl`** — append-only audit log of every session-scoped tool call (action, params, ok/err status, timestamp) at `{report_dir}/{session_id}/events.jsonl`.
- **`events.js`** — atomic rewrite of the same data as `window.__events_update([...])` for consumption by the viewer.
- **`index.html`** — styled viewer (Tailwind via the Play CDN) that embeds the recording in a `<video>` tag when present. Reloads `events.js` every 2 s via a `<script src>` swap (which works over `file://` unlike `fetch`), append-only rendering so expanded `<details>` stay expanded across refreshes. Written once at session start.

`start_session`'s response includes a `file://` URL to the session viewer — open it directly from the filesystem in any browser. No HTTP server, no ports, no network access required. Multiple `waydriver-mcp` instances (different Claude Code tabs / projects) can run side by side without conflict.

## Why Docker?

waydriver-mcp needs ~8 system services at runtime (mutter, pipewire, wireplumber, dbus, AT-SPI, gstreamer). Installing these manually is fragile and distro-specific. Docker solves four problems:

- **Security** — the MCP server spawns arbitrary processes, interacts with them via D-Bus, and captures their screen. Running this on your host session gives it access to everything your user can do. Inside a container, it only sees what you explicitly mount — no access to your files, browser sessions, or credentials. Add `--network none` to block network access entirely (the report viewer is purely static `file://`, so it works without any network)
- **Zero-setup distribution** — `docker pull` and you're running, no system packages to install
- **D-Bus isolation** — each container gets its own dbus-daemon, so apps with singleton D-Bus activation don't interfere across concurrent test sessions
- **ABI compatibility** — apps built inside the container are guaranteed to link against the same libraries the MCP runtime uses

## Running with Docker (recommended)

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
      "args": ["-c", "docker run --rm -i --network none -v \"$PWD:/workspace:ro\" -v /tmp/waydriver:/tmp/waydriver ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
    }
  }
}
```

- `$PWD:/workspace:ro` — mounts the project directory so the MCP can launch your app binaries from `/workspace/`
- `/tmp/waydriver:/tmp/waydriver` — makes session reports (screenshots, WebM recordings, `events.jsonl`, `index.html`) accessible on the host at `/tmp/waydriver/`. **The mount uses the same path on both sides** so the `file://` URL that `start_session` returns is openable as-is on the host
- `--network none` — safe to fully isolate: the report viewer is pure static HTML + JS loaded from your local filesystem

For NixOS users, also mount the Nix store so Nix-built binaries work inside the container:

```json
{
  "mcpServers": {
    "waydriver-mcp": {
      "command": "sh",
      "args": ["-c", "docker run --rm -i --network none -v /nix/store:/nix/store:ro -v \"$PWD:/workspace:ro\" -v /tmp/waydriver:/tmp/waydriver ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
    }
  }
}
```

Or build from source:

```sh
docker build -t waydriver-mcp .
```

## Testing your app with waydriver-mcp

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

## Running with Nix

For local development without Docker, the Nix app wraps the binary with the required runtime env vars:

```sh
nix run .#mcp
```

Sessions are kept in an in-memory `HashMap` keyed by id, so multiple apps can run concurrently within one server process.
