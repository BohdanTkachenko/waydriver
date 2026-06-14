# AGENTS.md

This file provides guidance to AI coding assistants working with code in this repository.

## Development Environment

This project uses a Nix flake with a devShell (`flake.nix`) and direnv (`.envrc`). The devShell uses the `.nix-profile` symlink pattern: `refresh` builds `packages.x86_64-linux.dev-profile` (a `buildEnv` over the `devPackages` list) and links it at `./.nix-profile`, whose `bin/` is prepended to `PATH` in the `shellHook`.

To add a new tool, add it to `devPackages` in `flake.nix` and run `refresh`. Do not use `nix run` or `nix shell` for project tooling — keep everything in the devShell. Use `nix run` only for one-off commands that don't belong in the devShell permanently.

The shellHook also sets `GST_PLUGIN_PATH`, `XDG_DATA_DIRS`, and prepends `at-spi2-core/libexec` to `PATH` — these cannot come from `buildEnv` alone because GStreamer plugin discovery and the `at-spi-bus-launcher` in `libexec` need explicit env vars.

### Non-Nix development (Ubuntu / other distros)

Nix is the supported path, but the repo also builds on a plain non-Nix host (e.g. the Claude Code cloud env on Ubuntu 24.04, or any Debian/Ubuntu/Fedora machine without Nix). Two helpers cover this:

- **`.claude/hooks/session-start.sh`** — a SessionStart hook that apt-installs the same GStreamer / glib / D-Bus / AT-SPI / PipeWire dev and runtime packages the flake/README provide, ensures the `rustfmt` and `clippy` rustup components exist, and pre-fetches the crate cache so `cargo build/fmt/clippy/test` work without Nix. It is guarded by `$CLAUDE_CODE_REMOTE`, so it is a no-op on a local Nix machine. To install the same packages on another distro, see the dependency tables in `README.md`.
- **`scripts/dev-container.sh`** — drops you into a Fedora 42 shell (libadwaita ≥ 1.6, Mesa at standard paths, matching the Dockerfile/CI) with the full build + runtime stack and your working tree bind-mounted. Use it to build `waydriver-fixture-gtk` and run the native e2e suite, which cannot build on Ubuntu 24.04 (it ships libadwaita 1.5, but the gtk-rs `v1_6` feature needs ≥ 1.6).

On a non-Nix host, build/test the rest of the workspace with `--exclude waydriver-fixture-gtk`; the GTK fixture and full e2e path stay container-only, as in CI. The `nix run .#mcp` wrapper is unavailable, so set the runtime env vars (`GST_PLUGIN_PATH`, `XDG_DATA_DIRS`, the `at-spi2-core/libexec` path) yourself when running the raw binary.

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
| `waydriver-examples` | `runtime-examples` | Runtime + gnome-calculator + the example binary from `crates/waydriver-examples` |

Both are published to `ghcr.io/bohdantkachenko/` on each release and on push to main.

```sh
docker build -t waydriver-mcp .                                       # runtime image
docker build --target builder-base -t waydriver-mcp-builder .         # builder image
docker build --target runtime-e2e   -t waydriver-mcp-e2e .            # runtime + waydriver-fixture-gtk
```

Or via Nix convenience apps:
```sh
nix run .#docker-build       # runtime
nix run .#docker-build-e2e   # runtime + waydriver-fixture-gtk
```

#### Container isolation

`docker-entrypoint.sh` launches a container-private dbus-daemon before exec-ing waydriver-mcp. Each container gets its own D-Bus session bus, so:
- App D-Bus activations (any app the MCP drives) are scoped to that container
- AT-SPI registry is per-container
- No interference between concurrent test sessions

The Docker-based e2e test (`fixture_via_docker` in `crates/waydriver-mcp/tests/e2e.rs`) drives the fixture through this isolated bus. Library-level host tests use the same fixture binary directly — its unique app-id means they don't collide on the host bus either.

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
compositor.start(None).await?;          // spawns dbus, pipewire, wireplumber, mutter
// state() is Option<Arc<MutterState>>; always Some immediately after a
// successful start(). Expect documents the invariant locally.
let state = compositor.state().expect("state available after start");
let input = MutterInput::new(state.clone());
let capture = MutterCapture::new(state);
let session = Session::start(Box::new(compositor), Box::new(input), Box::new(capture), cfg).await?;
```

`Session::start` then spawns the target app (on the **host** D-Bus for AT-SPI, on mutter's Wayland display), connects to the host AT-SPI bus, and waits for the app to appear in the AT-SPI registry.

### `Session::kill` drop ordering — load-bearing

`Session::kill` cancels the cooperative `CancellationToken` first, then runs the rest under a `KILL_TIMEOUT` (5s) `tokio::time::timeout` so a wedged D-Bus call surfaces as `Error::Timeout` instead of hanging the caller. Inside the budget:

1. Abort + await the **stdout reader** `JoinHandle` — drops the `Arc<AppStdout>` even when a leaked grandchild has inherited the app's stdout pipe.
2. Kill the **app** — its Wayland connection holds a reference into mutter.
3. **Stop the video recorder** (if any) — sends EOS so `webmmux` writes the seekhead before the source disappears.
4. **Stop the recorder's dedicated ScreenCast stream** (if any) — after the encoder has flushed; independent of the keepalive stream.
5. **Stop the keepalive ScreenCast stream** — must run before the backends drop, while mutter and the PipeWire node are still alive.
6. Call **`compositor.stop()`** — kills mutter/pipewire/wireplumber, terminates the private dbus-daemon, removes the runtime dir.
7. `self` drops: input/capture release their `Arc<MutterState>` refs after the compositor is already down (harmless).

The `Session` struct's field declaration order mirrors this sequence so the implicit `Drop` path (used when `kill` was never called or panicked mid-shutdown) is also safe. **Do not reorder the fields.** `Drop` itself only cancels the token, aborts the reader handle, and `start_kill`s the app — async cleanup belongs in `kill`.

### `Arc<MutterState>` sharing invariant

`MutterCompositor::start()` constructs an `Arc<MutterState>` containing the private D-Bus connection, the RemoteDesktop session path, and the runtime dir. `MutterInput` and `MutterCapture` hold cloned `Arc`s. While any `Arc<MutterState>` exists, the compositor's child processes and D-Bus connection **must** remain alive. `Session::kill` enforces this; direct `MutterCompositor::stop()` callers must drop input/capture first too.

### Dual D-Bus — the core constraint

GTK4's built-in AT-SPI backend hard-codes to the host session bus and ignores custom `DBUS_SESSION_BUS_ADDRESS`. So the code holds two D-Bus connections per session:

- `a11y_connection` (in `Session`) — host session bus → AT-SPI registry → the app's accessible tree. Built directly as a `zbus::Connection` (not the upstream `AccessibilityConnection` wrapper) so we can configure `method_timeout(A11Y_METHOD_TIMEOUT = 2s)` — this caps how long a stuck D-Bus call against a crashed app's bridge can hang `kill_session` / a Locator wait. NoReply errors at this timeout map to `Error::ElementStale` via the existing classifier, so the retry path stays unchanged.
- `MutterState::conn` — private bus → mutter's ScreenCast and RemoteDesktop interfaces. Keeps zbus' default reply timeout: compositor calls (`CreateSession`, ScreenCast negotiation) are bursty by design.

When editing session setup, keep this invariant: the **app** gets the host bus; **mutter/pipewire/wireplumber** get the private bus. Mixing them will silently break either accessibility or input/screencast.

### External-effect sinks (notifications / portal open-URI)

Some app behaviours have **no AT-SPI projection** because they leave the process onto the session bus: posting a desktop notification, or asking the portal to open a URI. `crates/waydriver/src/sink.rs` (`ExternalSinks`) mocks the daemons that would receive those calls. It opens a **dedicated connection to the app's session bus** (the same `dbus_address` the app and AT-SPI use — *not* mutter's private bus) and serves stubs that record every call:

- `org.freedesktop.Notifications` (`Notify`/`CloseNotification`/`GetCapabilities`/`GetServerInformation`) — what libnotify and the fdo path call.
- `org.freedesktop.portal.Desktop` → `org.freedesktop.portal.OpenURI` (`OpenURI`) — answers the portal Request/Response handshake (registers a `Request` object at the derived handle path and emits `Response`) so real callers like `GtkUriLauncher` complete.

This is the first **server-side** zbus code in the workspace (`#[zbus::interface]` + `object_server().at(...)` + best-effort `request_name_with_flags(.., DoNotQueue)`). It is **opt-in** via `SessionConfig::capture_external_effects` (MCP: `start_session`'s `capture_external_effects`, or the server-wide `--capture-external-effects` / `WAYDRIVER_CAPTURE_EXTERNAL_EFFECTS`, default off): owning those well-known names is only safe when nothing else owns them (always true on the per-session/container bus; on a shared host bus the claim no-ops with a warning and capture stays empty). Setup is best-effort — failure never aborts a session. Read back via `Session::notifications()` / `open_uri_requests()` (snapshots) or `wait_for_notification` / `wait_for_open_uri`; the MCP `get_captured_effects` tool returns both as JSON. The sink connection is dropped first in `Session::kill` and has no place in the compositor drop-order invariant. Out of scope: clipboard/PRIMARY readback (blocked — mutter 46.2 exposes no data-control; stopgap is paste + AT-SPI `Locator::text`), the portal `OpenFile` fd path, and the `org.freedesktop.portal.Notification` interface.

### Single-instance CLI forwarding

`Session::launch_secondary(args)` relaunches the session's app binary with the **same environment** (Wayland display, D-Bus bus, XDG dirs) captured at start (`SecondaryLaunchSpec`, built once via the shared `app_env_pairs` helper). For a single-instance `GApplication`, the second invocation detects the running primary on the session bus and forwards its command line instead of opening a new window, then exits. Observe what the *primary* did via `wait_for_stdout_line` / the AT-SPI tree. MCP tool: `launch_secondary_instance`.

### AT-SPI actions vs. real input

- `Locator::click` / `double_click` / `right_click` call AT-SPI's `Action.DoAction(n)` directly. Fast and precise but updates GTK4's internal model without a compositor redraw, so a screenshot taken immediately after may show a stale frame. Pair with `press_key` / `pointer_*` / `hover` / `drag_to` when the test needs to assert on a repainted UI.
- `Locator::hover` and `Locator::drag_to` go through the input backend's pointer motion + button primitives, so they drive a real Wayland event and the frame clock ticks.

### Text input and chords

- `press_key` and `type_text` both go through `Session::press_keysym` → the input backend. Text input sends each `char` individually via `char_to_keysym` (Latin-1 maps directly, other Unicode uses the `0x01000000 + codepoint` keysym encoding). The loop checks the session's `CancellationToken` between characters so a long string bails on `kill_session`.
- `Session::press_chord` parses strings like `"Ctrl+Shift+S"` via `keysym::parse_chord`, holds modifiers via `InputBackend::key_down`/`key_up`, and presses the target key in between. The unwind always releases held modifiers — even if the target press fails — so a panicked chord can't leave keys stuck down.
- `key_name_to_keysym` in `waydriver::keysym` handles named keys (Return, Tab, F1–F12, arrows, etc.). Modifier-only names (`ctrl`, `shift`, `alt`, `super`) map to their left-side keysyms via `parse_chord` and are not exposed as standalone `press_key` targets.

### Screenshot pipeline

The flow is: `MutterCapture::start_stream` → `ScreenCast.CreateSession → Session.RecordMonitor → subscribe to PipeWireStreamAdded → Session.Start → receive node_id` → `CaptureBackend::grab_screenshot` → `waydriver::capture::grab_png` (shared GStreamer helper: `pipewiresrc ! videoconvert ! pngenc snapshot=true ! appsink`) → `MutterCapture::stop_stream`.

The signal subscription must happen **before** `Session.Start` — mutter emits `PipeWireStreamAdded` synchronously during `Start`, and subscribing after misses it. GStreamer uses `num-buffers=5 ... pngenc snapshot=true` to grab a recent frame rather than the first (often-blank) one.

`Session::take_screenshot` uses the keepalive stream (started during `Session::start`) via `CaptureBackend::grab_screenshot`, avoiding per-screenshot stream setup/teardown overhead.

### Baseline comparison (pixel diff)

`crates/waydriver/src/visual/baseline.rs` (feature `visual`) adds a **data-returning** pixel-diff for behaviours that have no AT-SPI projection (CSS-class tints, cursor glyphs, colour/opacity, overlays). It is **not** an assertion: it returns a `BaselineComparison` score and never errors on a visual mismatch. `Locator::compare_to_baseline(baseline_png, tolerance)` captures the element crop (via `Locator::screenshot`, off-runtime in `spawn_blocking`) and diffs it against caller-supplied reference bytes; `Session::compare_to_baseline(actual_png, baseline_png, tolerance)` is the raw-bytes primitive. The headline `score` is the fraction of pixels whose CIEDE2000 ΔE (`visual::color::distance_sq`, reused from the visual locator) exceeds a JND threshold, so it catches both whole-area tints and tiny localized changes; `mean_delta_e` / `max_delta_e` / `ncc` (imageproc NCC) ride along as diagnostics. The MCP tool `compare_element_to_baseline` reads a reference PNG, returns the score JSON, and writes a red-highlighted `*-diff.png` (`visual::diff_to_baseline`) next to the captured crop on mismatch. Deliberately **out of scope** (the consumer's test harness owns these): pass/fail assertions, reference-file storage, and "update baselines" mode.

### Video recording pipeline

When `SessionConfig::video_output` is set, `Session::start` opens a **dedicated** ScreenCast stream for recording via `CaptureBackend::start_recording_stream` (a separate `recorder_stream` on `Session`, distinct from the keepalive node) and runs a long-lived GStreamer pipeline on *that* node: `pipewiresrc ! videoconvert ! videorate ! video/x-raw,framerate=15/1 ! vp8enc ! webmmux ! filesink`. The `VideoRecorder` handle lives on `Session` and is stopped by `Session::kill` **before** its stream is torn down, so the encoder/muxer still have a live source to flush through.

The recorder must **not** share the keepalive node with the screenshot path. Mutter negotiates the screencast stream at `framerate=0/1` (emit-on-damage) and only delivers the initial frame to a node's first/triggering consumer. A continuous recorder consumer on the shared node leaves a later-attaching `take_screenshot` consumer waiting for damage that a static app never produces — it times out with `screenshot: timed out waiting for PNG frame`. Giving the recorder its own stream keeps the screenshot consumer the keepalive node's first/triggering consumer (the reliable recording-off path). On mutter, the recorder's stream is **standalone** (not linked to the RemoteDesktop session) — it needs only pixels, is started via `ScreenCast.Session.Start` directly, and does not publish itself as the active-stream path used for pointer routing.

Stopping sends `EOS` on the pipeline and waits on the bus for `EOS`/`Error` (10 s timeout) before `set_state(Null)`. This is load-bearing: `webmmux` only writes the cues/seekhead on EOS, so a pipeline that's just set to `Null` produces a playable-but-unseekable WebM. `VideoRecorder::Drop` logs a warning and does the best-effort `Null` fallback — callers should always go through `Session::kill`.

VP8 tuning lives in `build_recording_pipeline_str`: `target-bitrate` is taken from `SessionConfig::video_bitrate` (default `capture::DEFAULT_VIDEO_BITRATE = 2_000_000`), `min-quantizer=4 max-quantizer=30` caps per-frame degradation, `keyframe-max-dist=30` (~2 s at 15 fps) keeps seeking responsive. Only `gst-plugins-good` is required — no `gst-plugins-bad`/`gst-plugins-ugly`.

The screenshot and recording pipelines both read `PIPEWIRE_REMOTE` and `XDG_RUNTIME_DIR` from the environment at state-transition time, so the shared `GRAB_PNG_LOCK` guards both setup paths. The lock is released once the pipeline is in `PLAYING`; the recording pipeline runs unlocked from then until `stop`.

### MCP server scaffolding

- `crates/waydriver-mcp/src/main.rs` wires `UiTestServer` to stdio via `rmcp`. **All logging must go to stderr** — stdout is the JSON-RPC transport. `tracing_subscriber` is configured with `with_writer(std::io::stderr)`; don't `println!` anywhere.
- Uses `rmcp`'s `#[tool_router]` / `#[tool]` macros. Each tool method takes `Parameters<T>` where `T` derives `Deserialize + JsonSchema` — the schema is what the MCP client (Claude) sees.
- Session state lives in `Arc<RwLock<HashMap<String, Arc<ManagedSession>>>>`. `ManagedSession` wraps the underlying `Session` plus a per-session `report_dir`, an atomic screenshot counter, an `events: Mutex<EventLog>` (a bounded ring with a 1024-event cap on the resident `VecDeque` plus a monotonic total counter — guards both the append-only `events.jsonl` and the atomically-rewritten `events.js`), and a per-session `kill_lock: Arc<RwLock<()>>` drain.
- Tool calls go through `UiTestServer::acquire(session_id)` which (1) clones the `Arc<ManagedSession>` out under the map's read lock, (2) takes `kill_lock.read_owned()` to block any concurrent `kill_session`. The returned `InFlightSession` derefs to `ManagedSession` and releases both locks on drop. `kill_session` removes from the map under write, `cancellation.cancel()`s the session, then takes `kill_lock.write_owned()` to drain in-flight tools before tearing down — so `Arc::try_unwrap` on the session is deterministically the unique reference.
- Report output path is configurable via the `--report-dir` CLI flag / `WAYDRIVER_REPORT_DIR` env var (default `/tmp/waydriver`), or per-session via `start_session`'s optional `report_dir` argument. Each session directory holds `{session_id}-{n}.png` (screenshots), `{session_id}.webm` (recording), `events.jsonl` / `events.js` (event log), and `index.html` (viewer). The virtual-monitor geometry is configurable via `--resolution` / `WAYDRIVER_RESOLUTION` (default `1024x768`), or per-session via `start_session`'s optional `resolution` argument (format `WIDTHxHEIGHT`). The HiDPI scale is configurable via `--scale` / `WAYDRIVER_SCALE` (default `1.0`), or per-session via `start_session`'s optional `scale` argument: `resolution` stays the physical framebuffer and apps see a logical size of `resolution ÷ scale`, applied through `org.gnome.Mutter.DisplayConfig` after startup. A requested scale is snapped to the nearest value mutter advertises.
- GSettings isolation: by default each session runs mutter and the app against a private per-session keyfile store (`waydriver::gsettings`, via `GSETTINGS_BACKEND=keyfile` + a `$XDG_CONFIG_HOME` under the session runtime dir) instead of the host's dconf. This is what makes fractional `scale` work — the compositor seeds `org.gnome.mutter experimental-features=['scale-monitor-framebuffer']` into the keyfile — without reading or writing the host's real desktop settings. Toggle via `--gsettings-isolation` / `WAYDRIVER_GSETTINGS_ISOLATION` (default true) or per-session `isolate_settings`; seed extra settings (e.g. `org.gnome.desktop.interface text-scaling-factor`) via `start_session`'s `gsettings` array. Mutter writes the complete keyfile before launch; `Session::set_setting` (the `set_setting` MCP tool) then rewrites it **in place** post-launch via `gsettings::live_write`, and GIO's keyfile backend re-emits `changed` so the already-running app re-applies the value live (cursor, fonts, color-scheme, …) without a restart. The app reads the same file throughout.
- Session recording is on by default. `--record-video <bool>` / `WAYDRIVER_RECORD_VIDEO` (default `true`) sets the server-wide default; `start_session`'s optional `record_video: bool` overrides it per session. Recording requires `report: true` since the `.webm` lives in the session's report dir. Bitrate is tuned via `--video-bitrate` / `WAYDRIVER_VIDEO_BITRATE` (default `2_000_000` bits/sec) or `start_session`'s optional `video_bitrate: u32`.
- Every session-scoped tool handler **must call `ManagedSession::log_event`** (or `append_event` for `kill_session`, which has to destructure first). That single call appends to `events.jsonl` and atomically rewrites `events.js` (tempfile + rename) so the static viewer sees new events on its next `<script src>` reload. Logging errors are swallowed via `tracing::warn!` and never mask the real tool result.
- No HTTP server. The viewer is a static HTML file that reloads `events.js` every 2 s via a `<script src>` swap — which works over `file://` where `fetch()` would hit CORS. `start_session` returns a `file://` URL. Multiple MCP instances side-by-side just work (no port conflicts).

### Error model

`waydriver::error` defines a single `Error` enum (`thiserror`); `Result<T>` is an alias.

Inside the MCP binary, errors are mapped to `rmcp::ErrorData` via `McpError::internal_error(e.to_string(), None)` or `McpError::invalid_params` for missing-session lookups. Keep that distinction — user-visible "you passed a bad session id" is `invalid_params`, everything else is `internal_error`.

## Testing notes

Unit tests live inside `#[cfg(test)] mod tests` blocks at the bottom of each module:
- `waydriver::error` / `keysym` / `capture` — error display and `From` impls, keysym tables, GStreamer pipeline-string assembly.
- `waydriver::atspi` — XML snapshot + XPath evaluation, role/attr sanitization, `is_stale_error_name` classifier, `Rect` / `bbox` round-trip + containment.
- `waydriver::locator` — `poll_with_retry` semantics (success, retriable swallowing, timeout, fatal-error pass-through, cancellation), state predicate composition.
- `waydriver::session` — app-name normalization/matching, `press_chord` modifier ordering + unwind, `type_text` cancellation, stdout-buffer wait and notify, default-timeout env parsing.
- `waydriver_compositor_mutter` — D-Bus output parsing, Wayland and PipeWire socket-readiness polls, resolution parsing, `state()`-before-`start()` invariant.
- `waydriver_mcp` (`main`, `report`, `params`, `mcp_error`, `cli`) — tool error paths + success with mock backends, `EventLog` ring behaviour, parameter deserialization, error mapping, CLI/env resolution.

These tests are pure — they don't spawn mutter or touch D-Bus — so `cargo test --workspace` runs fast and works without the full runtime stack.

End-to-end tests exercise the full stack (mutter, pipewire, AT-SPI, target app):

**Library e2e** (`crates/waydriver-e2e/tests/e2e.rs`) — all against the project's own `waydriver-fixture-gtk` binary. Each is `#[ignore]`-gated; run with `--ignored --test-threads=1` after `cargo build -p waydriver-fixture-gtk`. The current set covers per-section tree diagnostics (gtk4 / adw / dnd), click → stdout event, element bounds sanity, `scroll_into_view` no-op, locator-action → pixel diff, full tree+locator feature surface, keyboard chord dispatch + modifier-release, main-menu auto-wait, pointer motion (relative + absolute) and button primitives, `Locator::fill` on entry widgets, hover, double-click, right-click, drag-to, `Session::cancel` interrupting a stuck `wait_for_visible` within ~1s, `Locator::scroll` driving the fixture scroll area's offset (verified via its `scrolled` stdout event), `Locator::value` reading the fixture slider's AT-SPI `Value` range, external-effect capture (clicking the `effects` section's `fire-notification` / `open-uri` buttons → `Session::notifications` / `open_uri_requests`), and single-instance CLI forwarding (`Session::launch_secondary` → the primary's `command-line-forwarded` stdout event).

A separate gated integration test, `crates/waydriver/tests/gsettings_live_reload.rs` (`#[ignore]`), proves the live-GSettings mechanism without a GTK fixture: it spawns `gsettings monitor` (a real GIO keyfile-backend consumer) and asserts that a `gsettings::live_write` in-place rewrite makes it report the new value — the same `changed`-signal path `Session::set_setting` relies on. Run with `cargo test -p waydriver --test gsettings_live_reload -- --ignored`.

`crates/waydriver/tests/external_sinks.rs` (`#[ignore]`) similarly proves the mock external-effect sinks without GTK or mutter: it spawns a private `dbus-daemon`, starts `ExternalSinks`, and drives it as a raw zbus client — asserting `Notify` is captured (and returns an id) and `OpenURI` is captured, returns the expected request-handle path, and delivers the portal `Response` signal to a pre-subscribed caller. Run with `cargo test -p waydriver --test external_sinks -- --ignored`.

**MCP e2e** (`crates/waydriver-mcp/tests/e2e.rs`) — one Docker-based test:
- `fixture_via_docker` — spawns `docker run -i waydriver-mcp-e2e:latest` and exercises the full MCP tool surface against the `waydriver-fixture-gtk` binary running inside the container. `#[ignore]` — requires pre-built Docker image. **Runs in CI** via the `e2e` job in `.github/workflows/ci.yml`.

### CI pipeline

CI (`.github/workflows/ci.yml`) does not use Nix — it uses standard `apt-get` + `dtolnay/rust-toolchain`:

| Job | What it does |
|-----|-------------|
| `fmt` | `cargo fmt --check` |
| `clippy` | `cargo clippy -- -D warnings` (needs system dev headers) |
| `test` | `cargo test --workspace` (unit tests only, no `--ignored`) |
| `e2e` | Builds `waydriver-mcp-e2e` Docker image, runs `fixture_via_docker` |
| `examples` | Builds `waydriver-examples` Docker image, runs the `gnome_calculator` example end-to-end |

Docker images are published to ghcr.io via `.github/workflows/publish-docker.yml` on push to main (`:main` tag) and on release tags (`:latest`, `:<version>`, `:<minor>`, `:<major>`).

## Commit messages

**Always** use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages (e.g. `feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `test:`). Include a scope when it clarifies the change (e.g. `feat(capture): ...`).

## Track sessions

When a `.session/` symlink exists in your workspace, this workspace
is a development track. Read `.session/Spec.md` for the objective
and list `.session/Todos/` for the per-TODO files — each contains
its own plan and notes. Use the `/todo` and `/track` skills to
manage them.
