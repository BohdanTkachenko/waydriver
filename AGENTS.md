# AGENTS.md

This file provides guidance to AI coding assistants working with code in this repository.

## Development Environment

This project uses a Nix flake with a devShell (`flake.nix`) and direnv (`.envrc`). The devShell uses the `.nix-profile` symlink pattern: `refresh` builds `packages.x86_64-linux.dev-profile` (a `buildEnv` over the `devPackages` list) and links it at `./.nix-profile`, whose `bin/` is prepended to `PATH` in the `shellHook`.

To add a new tool, add it to `devPackages` in `flake.nix` and run `refresh`. Do not use `nix run` or `nix shell` for project tooling — keep everything in the devShell. Use `nix run` only for one-off commands that don't belong in the devShell permanently.

The shellHook also sets `GST_PLUGIN_PATH`, `XDG_DATA_DIRS`, and prepends `at-spi2-core/libexec` to `PATH` — these cannot come from `buildEnv` alone because GStreamer plugin discovery and the `at-spi-bus-launcher` in `libexec` need explicit env vars.

## Build and test

With direnv allowed, tools are on `PATH` automatically inside the project directory. Otherwise, use `nix develop --command`:

```sh
cargo build --workspace
cargo test --workspace
cargo test -p waydriver                # single crate
cargo test -p waydriver keysym         # single test module within a crate
cargo clippy --workspace
cargo fmt --all
```

### Build and run (MCP binary)

```sh
nix build           # builds ./result/bin/waydriver (unwrapped)
nix run .#mcp       # runs the wrapper with runtime deps injected (see flake.nix apps)
```

The `nix run` wrapper is the only way the server will function at runtime — it injects `GST_PLUGIN_PATH`, `XDG_DATA_DIRS`, and the `at-spi2-core/libexec` path. Running the raw binary from `target/debug` will fail to launch subprocess dependencies.

### Docker

The `Dockerfile` (Fedora 42, multi-stage) produces two publishable images from the same file:

| Image | Dockerfile target | Contents |
|-------|-------------------|----------|
| `waydriver-mcp` | final (default) | Runtime: mutter, pipewire, gstreamer, AT-SPI, dbus + waydriver-mcp binary |
| `waydriver-mcp-builder` | `builder-base` | Build env: Fedora 42 + Rust (rustup) + gcc + GTK4/GLib/GStreamer/PipeWire dev headers |

Both are published to `ghcr.io/bohdantkachenko/` on each release and on push to main.

```sh
docker build -t waydriver-mcp .                                        # runtime image
docker build --target builder-base -t waydriver-mcp-builder .          # builder image
docker build --build-arg INSTALL_CALCULATOR=true -t waydriver-mcp-e2e . # runtime + gnome-calculator
```

Or via Nix convenience apps:
```sh
nix run .#docker-build       # runtime
nix run .#docker-build-e2e   # runtime + gnome-calculator
```

#### Container isolation

`docker-entrypoint.sh` launches a container-private dbus-daemon before exec-ing waydriver-mcp. Each container gets its own D-Bus session bus, so:
- gnome-calculator's singleton D-Bus activation is scoped to that container
- AT-SPI registry is per-container
- No interference between concurrent test sessions

This is why the Docker-based e2e test (`calculator_add_via_docker` in `crates/waydriver-mcp/tests/e2e.rs`) works reliably — unlike the host-based test which needs `--test-threads=1`.

#### User dev workflow — bringing app binaries into the MCP container

The MCP server is persistent (started by the MCP client, stays up for the session). Users rebuild their app independently and the MCP picks up the new binary on the next `start_session` call.

**Rust apps** — volume-mount the built binary:
```json
{
  "waydriver-mcp": {
    "command": "docker",
    "args": ["run", "--rm", "-i",
      "-v", "/home/user/myapp/build:/workspace:ro",
      "ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
  }
}
```
Build with the builder image for ABI compatibility:
```sh
docker run --rm -v "$PWD:/src:ro" -v "$PWD/build:/out" \
  ghcr.io/bohdantkachenko/waydriver-mcp-builder:latest \
  sh -c "cp -r /src /tmp/build && cd /tmp/build && cargo build --release && cp target/release/myapp /out/"
```
Then `start_session` with `command: "/workspace/myapp"`.

**C/C++ apps** — same volume-mount pattern. The builder image includes `gcc`, `g++`, `meson`, `ninja-build`, `cmake`, `pkg-config`, `gtk4-devel`, and `glib2-devel`:
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

**NixOS users** — mount `/nix/store` so Nix-built binaries just work:
```json
"args": ["run", "--rm", "-i",
  "-v", "/nix/store:/nix/store:ro",
  "-v", "/home/user/myapp:/workspace:ro",
  "ghcr.io/bohdantkachenko/waydriver-mcp:latest"]
```

**Node/Python apps** — extend the runtime image to add the interpreter, use a named volume for deps:
```dockerfile
FROM ghcr.io/bohdantkachenko/waydriver-mcp:latest
RUN dnf install -y nodejs && dnf clean all
```
Install deps into a named volume (re-run when lockfile changes):
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
Mount source + deps volume in `.mcp.json`:
```json
"args": ["run", "--rm", "-i",
  "-v", "/home/user/myapp/src:/app/src:ro",
  "-v", "myapp-nodemods:/app/node_modules:ro",
  "myapp-mcp:latest"]
```
Edit source freely — MCP picks up changes on next `start_session`. No MCP restart needed.

## Architecture

### Workspace layout — five crates

The project is a Cargo workspace under `crates/` with the naming convention `waydriver-<role>-<backend>` for concrete implementations. Trait definitions live in the umbrella library (`waydriver`); each concrete implementation of a trait is a separate sibling crate. Future backends (`waydriver-input-libei`, `waydriver-capture-wlr`, `waydriver-compositor-sway`) slot in as additive siblings.

| Crate                             | Purpose                                                                                                                                                                                                                                                       |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`waydriver`**                   | Umbrella library. Trait definitions (`CompositorRuntime`, `InputBackend`, `CaptureBackend`), `Session` (holds three `Box<dyn Trait>` fields), AT-SPI client, keysym helpers, shared `grab_png` GStreamer helper, `Error`/`Result`. Zero mutter-specific code. |
| **`waydriver-compositor-mutter`** | `MutterCompositor` impl of `CompositorRuntime`. Owns mutter/pipewire/wireplumber child processes + private D-Bus. Exposes `Arc<MutterState>` via `state()` after `start()`.                                                                                   |
| **`waydriver-input-mutter`**      | `MutterInput` impl of `InputBackend`. Wraps `Arc<MutterState>`, calls `org.gnome.Mutter.RemoteDesktop.Session.NotifyKeyboardKeysym` / `NotifyPointerMotionRelative`.                                                                                          |
| **`waydriver-capture-mutter`**    | `MutterCapture` impl of `CaptureBackend`. Wraps `Arc<MutterState>`, creates ScreenCast sessions, returns PipeWire node ids.                                                                                                                                   |
| **`waydriver-mcp`**               | Binary. MCP JSON-RPC server over stdio. Constructs the three mutter impls, wires them into `Session::start`. Only crate depending on `rmcp`/`schemars`.                                                                                                       |

**Dependency DAG** (must hold strictly — verify via `cargo tree -e normal -p <crate>`):
- `waydriver` has no deps on any `waydriver-*-mutter` crate or `rmcp`.
- `waydriver-compositor-mutter` depends on `waydriver` only.
- `waydriver-input-mutter` and `waydriver-capture-mutter` depend on `waydriver` + `waydriver-compositor-mutter` (for `MutterState`).
- `waydriver-mcp` depends on all four library crates.

### Session lifecycle

`Session::start` takes three pre-constructed trait objects plus a `SessionConfig`. The caller is responsible for starting the compositor first and constructing input/capture from whatever state it exposes. For mutter:

```rust
let mut compositor = MutterCompositor::new();
compositor.start().await?;              // spawns dbus, pipewire, wireplumber, mutter
let state = compositor.state();         // Arc<MutterState> — D-Bus conn + RD path + runtime dir
let input = MutterInput::new(state.clone());
let capture = MutterCapture::new(state);
let session = Session::start(Box::new(compositor), Box::new(input), Box::new(capture), cfg).await?;
```

`Session::start` then spawns the target app (on the **host** D-Bus for AT-SPI, on mutter's Wayland display), connects to the host AT-SPI bus, and waits for the app to appear in the AT-SPI registry.

### `Session::kill` drop ordering — load-bearing

`Session::kill` destructures `self` and shuts down in this order:

1. Kill the **app** first — its Wayland connection holds a reference into mutter.
2. **Drop input and capture** — for mutter, this releases `Arc<MutterState>` strong refs before the D-Bus connection is torn down.
3. Call **`compositor.stop()`** — kills mutter/pipewire/wireplumber, terminates the private dbus-daemon, removes the runtime dir.

The `Session` struct's field declaration order mirrors this sequence so implicit `Drop` is also safe. **Do not reorder the fields.**

### `Arc<MutterState>` sharing invariant

`MutterCompositor::start()` constructs an `Arc<MutterState>` containing the private D-Bus connection, the RemoteDesktop session path, and the runtime dir. `MutterInput` and `MutterCapture` hold cloned `Arc`s. While any `Arc<MutterState>` exists, the compositor's child processes and D-Bus connection **must** remain alive. `Session::kill` enforces this; direct `MutterCompositor::stop()` callers must drop input/capture first too.

### Dual D-Bus — the core constraint

GTK4's built-in AT-SPI backend hard-codes to the host session bus and ignores custom `DBUS_SESSION_BUS_ADDRESS`. So the code holds two D-Bus connections per session:

- `a11y_connection` (in `Session`) — host session bus → AT-SPI registry → the app's accessible tree.
- `MutterState::conn` — private bus → mutter's ScreenCast and RemoteDesktop interfaces.

When editing session setup, keep this invariant: the **app** gets the host bus; **mutter/pipewire/wireplumber** get the private bus. Mixing them will silently break either accessibility or input/screencast.

### Click = AT-SPI action + input wake

After AT-SPI `do_action(0)` (in `waydriver::atspi::click_element`), the caller should send a harmless Shift_L press/release through the input backend. This wakes GTK4's GLib main loop, flushing pending widget invalidations so the framebuffer reflects the click. Without this, screenshots after clicks are stale.

### Text input vs. key press

- `press_key` and `type_text` both go through `Session::press_keysym` → the input backend. Text input sends each `char` individually via `char_to_keysym` (Latin-1 maps directly, other Unicode uses the `0x01000000 + codepoint` keysym encoding).
- `key_name_to_keysym` in `waydriver::keysym` handles named keys (Return, Tab, F1–F12, arrows, etc.). Modifier-only keys (`ctrl`, `shift`, `alt`, `super`) intentionally return `None` — the API currently has no modifier/chord support.

### Screenshot pipeline

The flow is: `MutterCapture::start_stream` → `ScreenCast.CreateSession → Session.RecordMonitor → subscribe to PipeWireStreamAdded → Session.Start → receive node_id` → `CaptureBackend::grab_screenshot` → `waydriver::capture::grab_png` (shared GStreamer helper: `pipewiresrc ! videoconvert ! pngenc snapshot=true ! appsink`) → `MutterCapture::stop_stream`.

The signal subscription must happen **before** `Session.Start` — mutter emits `PipeWireStreamAdded` synchronously during `Start`, and subscribing after misses it. GStreamer uses `num-buffers=5 ... pngenc snapshot=true` to grab a recent frame rather than the first (often-blank) one.

`Session::take_screenshot` uses the keepalive stream (started during `Session::start`) via `CaptureBackend::grab_screenshot`, avoiding per-screenshot stream setup/teardown overhead.

### MCP server scaffolding

- `crates/waydriver-mcp/src/main.rs` wires `UiTestServer` to stdio via `rmcp`. **All logging must go to stderr** — stdout is the JSON-RPC transport. `tracing_subscriber` is configured with `with_writer(std::io::stderr)`; don't `println!` anywhere.
- Uses `rmcp`'s `#[tool_router]` / `#[tool]` macros. Each tool method takes `Parameters<T>` where `T` derives `Deserialize + JsonSchema` — the schema is what the MCP client (Claude) sees.
- Session state lives in `Arc<RwLock<HashMap<String, ManagedSession>>>`, where `ManagedSession` wraps the underlying `Session` plus a per-session `report_dir`, an atomic screenshot counter, and an `events: Mutex<Vec<serde_json::Value>>` that guards both the on-disk `events.jsonl` (append) and the atomically-rewritten `events.js` (replace). Read-only tools take a `.read()` lock, `start_session`/`kill_session` take `.write()`.
- Report output path (screenshots today; video/HTML planned) is configurable via the `--report-dir` CLI flag / `WAYDRIVER_REPORT_DIR` env var (default `/tmp/waydriver`), or per-session via `start_session`'s optional `report_dir` argument. Screenshots land at `{dir}/{session_id}/{session_id}-{n}.png`.
- Every session-scoped tool handler **must call `ManagedSession::log_event`** (or `append_event` for `kill_session`, which has to destructure first). That single call appends to `events.jsonl` and atomically rewrites `events.js` (tempfile + rename) so the static viewer sees new events on its next `<script src>` reload. Logging errors are swallowed via `tracing::warn!` and never mask the real tool result.
- No HTTP server. The viewer is a static HTML file that reloads `events.js` every 2 s via a `<script src>` swap — which works over `file://` where `fetch()` would hit CORS. `start_session` returns a `file://` URL. Multiple MCP instances side-by-side just work (no port conflicts).

### Error model

`waydriver::error` defines a single `Error` enum (`thiserror`); `Result<T>` is an alias.

Inside the MCP binary, errors are mapped to `rmcp::ErrorData` via `McpError::internal_error(e.to_string(), None)` or `McpError::invalid_params` for missing-session lookups. Keep that distinction — user-visible "you passed a bad session id" is `invalid_params`, everything else is `internal_error`.

## Testing notes

Unit tests live inside `#[cfg(test)] mod tests` blocks:
- `waydriver::error::tests` — error display and From impls.
- `waydriver::keysym::tests` — keysym mapping tables.
- `waydriver::session::tests` — app name normalization and matching.
- `waydriver::capture::tests` — pipeline string building and path validation.
- `waydriver_compositor_mutter::tests` — D-Bus output parsing, Wayland socket wait, constructor properties.
- `waydriver_mcp::tests` — MCP tool error paths + success paths with mock backends.

These tests are pure — they don't spawn mutter or touch D-Bus — so `cargo test --workspace` runs fast and works without the full runtime stack.

End-to-end tests exercise the full stack (mutter, pipewire, AT-SPI, gnome-calculator):

**Library e2e** (`crates/waydriver/tests/e2e.rs`):
- `calculator_screenshots_change` — keyboard input + screenshot comparison.
- `accessibility_tree_inspection` — AT-SPI tree dump, element search, ElementNotFound error.
- `click_element_changes_display` — AT-SPI click_element + screenshot verification.
- `pointer_input_operations` — pointer motion and button input.
- `#[ignore]` — host D-Bus singleton issue. Run with `--ignored --test-threads=1`.

**MCP e2e** (`crates/waydriver-mcp/tests/e2e.rs`):
- `calculator_add_via_mcp` — spawns local waydriver-mcp binary, exercises all 11 tools via JSON-RPC. `#[ignore]` — same host D-Bus issue.
- `calculator_add_via_docker` — spawns `docker run -i waydriver-mcp-e2e:latest`, same test flow. `#[ignore]` — requires pre-built Docker image. **Runs in CI** via the `e2e` job in `.github/workflows/ci.yml`.

### CI pipeline

CI (`.github/workflows/ci.yml`) does not use Nix — it uses standard `apt-get` + `dtolnay/rust-toolchain`:

| Job | What it does |
|-----|-------------|
| `fmt` | `cargo fmt --check` |
| `clippy` | `cargo clippy -- -D warnings` (needs system dev headers) |
| `test` | `cargo test --workspace` (unit tests only, no `--ignored`) |
| `e2e` | Builds `waydriver-mcp-e2e` Docker image, runs `calculator_add_via_docker` |

Docker images are published to ghcr.io via `.github/workflows/publish-docker.yml` on push to main (`:main` tag) and on release tags (`:latest`, `:<version>`, `:<minor>`, `:<major>`).

## Commit messages

**Always** use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages (e.g. `feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `test:`). Include a scope when it clarifies the change (e.g. `feat(capture): ...`).

## Track sessions

When a `.session/` symlink exists in your workspace, this workspace
is a development track. Read `.session/Spec.md` for the objective
and list `.session/Todos/` for the per-TODO files — each contains
its own plan and notes. Use the `/todo` and `/track` skills to
manage them.
