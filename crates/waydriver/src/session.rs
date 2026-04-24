use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;

use crate::atspi as atspi_client;
use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend};
use crate::capture::VideoRecorder;
use crate::error::{Error, Result};
use crate::locator::Locator;

/// Fallback default timeout for auto-wait and explicit `wait_for_*` methods
/// when the `WAYDRIVER_DEFAULT_TIMEOUT_MS` env var isn't set.
const FALLBACK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variable controlling the default wait/auto-wait timeout, in
/// milliseconds. Overridable per-session via [`Session::set_default_timeout`]
/// and per-call via [`Locator::with_timeout`](crate::Locator::with_timeout).
pub const DEFAULT_TIMEOUT_ENV_VAR: &str = "WAYDRIVER_DEFAULT_TIMEOUT_MS";

/// Parameters for spawning the target application inside a session.
pub struct SessionConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Accessible name used to look the app up in the AT-SPI registry.
    pub app_name: String,
    /// If set, the session records a continuous WebM video of the display to
    /// this path. Recording starts right after the keepalive stream is open
    /// and stops right before it is torn down in [`Session::kill`]. When
    /// `None`, no recording pipeline is started.
    pub video_output: Option<PathBuf>,
    /// VP8 target bitrate in bits/sec for the recording pipeline. Only
    /// consulted when `video_output` is `Some`. When `None`, falls back to
    /// [`crate::capture::DEFAULT_VIDEO_BITRATE`].
    pub video_bitrate: Option<u32>,
}

/// Buffer of lines emitted on the target app's stdout, with a Notify the
/// reader task pokes on every append so [`Session::wait_for_stdout_line`]
/// can wake and rescan.
#[derive(Default)]
struct AppStdout {
    lines: Mutex<Vec<String>>,
    notify: Notify,
}

/// A running UI test session: a compositor, input + capture backends, the
/// target application process, and an AT-SPI connection to drive it.
///
/// Construct via [`Session::start`]. Callers are responsible for pre-starting
/// the compositor (so they can wire mutually-dependent backends like
/// `waydriver-input-mutter` / `waydriver-capture-mutter`, which share state
/// with the compositor via `Arc<MutterState>`).
pub struct Session {
    pub id: String,
    pub app_name: String,
    pub app_bus_name: String,
    pub app_path: String,
    pub a11y_connection: Option<AccessibilityConnection>,
    /// Default timeout (in nanoseconds) applied to auto-wait on Locator
    /// actions and explicit `wait_for_*` calls. Stored as AtomicU64 so
    /// [`set_default_timeout`] can mutate it behind an `Arc<Session>`
    /// without requiring interior-mutability gymnastics on every field.
    default_timeout_ns: AtomicU64,
    // Field declaration order matches the required shutdown sequence (app before
    // input/capture before compositor). The Drop impl sends SIGKILL to the app;
    // implicit field drops then release input/capture Arc refs before the
    // compositor's own Drop kills its child processes.
    app: Child,
    /// A persistent ScreenCast stream kept alive so mutter composites
    /// continuously in headless mode. Without this, the compositor never
    /// sends Wayland frame callbacks and GTK4 apps cannot repaint after
    /// their initial render.
    keepalive_stream: Option<crate::backend::PipeWireStream>,
    /// Optional long-lived WebM recording that shares the keepalive
    /// ScreenCast node. Declared after `keepalive_stream` so implicit drop
    /// order matches the explicit shutdown sequence in [`Session::kill`]:
    /// flush the recording before releasing the ScreenCast token.
    video_recorder: Option<VideoRecorder>,
    input: Box<dyn InputBackend>,
    capture: Box<dyn CaptureBackend>,
    compositor: Box<dyn CompositorRuntime>,
    /// Captured lines from the app process's stdout. A background task
    /// reads from the child pipe and pushes each line here, notifying
    /// waiters so they can rescan the buffer. Lines persist for the
    /// session's lifetime (no ring-buffer eviction yet).
    stdout: Arc<AppStdout>,
}

impl Session {
    /// Build a session from a pre-started compositor plus matching input and
    /// capture backends. The caller is responsible for calling
    /// [`CompositorRuntime::start`] before passing the compositor in; this is
    /// what lets the caller construct backend-specific input/capture types
    /// from whatever state the compositor exposes after startup (for mutter,
    /// that's `waydriver_compositor_mutter::MutterCompositor::state()`).
    pub async fn start(
        compositor: Box<dyn CompositorRuntime>,
        input: Box<dyn InputBackend>,
        capture: Box<dyn CaptureBackend>,
        cfg: SessionConfig,
    ) -> Result<Self> {
        let id = compositor.id().to_string();
        tracing::info!(id, "starting session");

        let dbus_address = get_host_session_bus()?;
        let mut app = spawn_app(
            &cfg,
            compositor.wayland_display(),
            compositor.runtime_dir(),
            &dbus_address,
        )?;
        tracing::debug!(id, app_name = %cfg.app_name, "app spawned");

        let stdout = Arc::new(AppStdout::default());
        if let Some(child_stdout) = app.stdout.take() {
            let captured = stdout.clone();
            let id_for_task = id.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(child_stdout).lines();
                loop {
                    match reader.next_line().await {
                        Ok(Some(line)) => {
                            tracing::trace!(id = id_for_task, line = %line, "app stdout");
                            {
                                let mut guard = captured.lines.lock().unwrap();
                                guard.push(line);
                            }
                            captured.notify.notify_waiters();
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!(id = id_for_task, error = %e, "app stdout read error");
                            break;
                        }
                    }
                }
            });
        }

        let a11y_connection = atspi_client::connect_a11y(&dbus_address).await?;
        let (app_bus_name, app_path) = wait_for_app(&a11y_connection, &cfg.app_name).await?;
        tracing::info!(id, app_name = %cfg.app_name, %app_bus_name, "session ready");

        // Start a keepalive ScreenCast stream. In headless mutter the
        // compositor only delivers Wayland frame callbacks while it is
        // actively compositing, and it only composites when a ScreenCast
        // consumer is pulling frames. Without this stream, GTK4 apps
        // render their first frame but never repaint because the frame
        // clock never ticks.
        let keepalive_stream = capture.start_stream().await?;

        // If the caller requested a recording, start a second GStreamer
        // pipeline on the same PipeWire node. Failure here aborts session
        // startup: the caller explicitly opted in, so silently skipping
        // would be surprising.
        let video_recorder = if let Some(ref path) = cfg.video_output {
            let bitrate = cfg
                .video_bitrate
                .unwrap_or(crate::capture::DEFAULT_VIDEO_BITRATE);
            Some(
                capture
                    .start_recording(&keepalive_stream, path, bitrate)
                    .await?,
            )
        } else {
            None
        };

        let session = Session {
            id,
            app_name: cfg.app_name,
            app_bus_name,
            app_path,
            a11y_connection: Some(a11y_connection),
            default_timeout_ns: AtomicU64::new(resolve_default_timeout().as_nanos() as u64),
            app,
            keepalive_stream: Some(keepalive_stream),
            video_recorder,
            input,
            capture,
            compositor,
            stdout,
        };

        Ok(session)
    }

    /// Shut down the session in the required order.
    ///
    /// **Ordering is load-bearing:**
    /// 1. Kill the app first. Its Wayland connection holds a reference into
    ///    the compositor; killing the compositor first can make the app block
    ///    on its Wayland socket during shutdown.
    /// 2. Drop the input and capture trait objects. For backends that share
    ///    state with the compositor via `Arc` (e.g. mutter's
    ///    `Arc<MutterState>` holding the private D-Bus connection), the
    ///    strong count has to reach zero before the compositor tears the
    ///    underlying resource down.
    /// 3. Stop the compositor.
    pub async fn kill(mut self) -> Result<()> {
        tracing::info!(id = self.id, "killing session");

        let _ = self.app.kill().await;
        let _ = self.app.wait().await;

        // Finalize the recording before tearing down the ScreenCast stream so
        // the muxer still has a live PipeWire node to flush through. Errors
        // are logged but don't block session teardown.
        if let Some(recorder) = self.video_recorder.take() {
            if let Err(e) = self.capture.stop_recording(recorder).await {
                tracing::warn!(error = %e, "stop_recording failed");
            }
        }

        // Stop the keepalive ScreenCast stream before dropping backends.
        if let Some(stream) = self.keepalive_stream.take() {
            let _ = self.capture.stop_stream(stream).await;
        }

        self.compositor.stop().await?;

        // self drops here: Drop sees an already-dead app and already-stopped
        // compositor, then input/capture release their Arc refs harmlessly.
        Ok(())
    }

    /// Send a key press + release for the given X11 keysym.
    pub async fn press_keysym(&self, keysym: u32) -> Result<()> {
        self.input.press_keysym(keysym).await
    }

    /// Press a chord like `"Ctrl+Shift+A"` — modifiers are held in order,
    /// the target key is pressed and released, then modifiers are released
    /// in reverse order.
    ///
    /// Accepts single key names (`"Return"`, `"a"`) as chords with no
    /// modifiers. See [`crate::keysym::parse_chord`] for the full grammar.
    /// Returns an error if the chord can't be parsed.
    pub async fn press_chord(&self, chord: &str) -> Result<()> {
        let parsed = crate::keysym::parse_chord(chord)
            .ok_or_else(|| Error::Process(format!("invalid chord: {chord:?}")))?;
        // Press all modifiers in order.
        for m in &parsed.modifiers {
            self.input.key_down(*m).await?;
        }
        // Press + release the target key while modifiers are held.
        let target_result = self.input.press_keysym(parsed.key).await;
        // Release modifiers in reverse order, even if the target press
        // failed — leaving modifiers stuck down would break subsequent
        // keyboard input.
        for m in parsed.modifiers.iter().rev() {
            if let Err(e) = self.input.key_up(*m).await {
                tracing::warn!(error = %e, keysym = m, "key_up failed during chord unwind");
            }
        }
        target_result
    }

    /// Move the pointer by a relative offset in logical pixels.
    pub async fn pointer_motion_relative(&self, dx: f64, dy: f64) -> Result<()> {
        self.input.pointer_motion_relative(dx, dy).await
    }

    /// Move the pointer to a screen-relative absolute position in logical
    /// pixels. Requires an active capture stream on backends that route
    /// through the compositor's ScreenCast pipeline (mutter).
    pub async fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()> {
        self.input.pointer_motion_absolute(x, y).await
    }

    /// Press and release a pointer button (Linux evdev code, e.g. BTN_LEFT = 0x110).
    pub async fn pointer_button(&self, button: u32) -> Result<()> {
        self.input.pointer_button(button).await
    }

    /// Hold a pointer button down until a matching [`pointer_button_up`](Self::pointer_button_up)
    /// fires. Used to build drag gestures — press, move across intermediate
    /// coordinates, then release.
    pub async fn pointer_button_down(&self, button: u32) -> Result<()> {
        self.input.pointer_button_down(button).await
    }

    /// Release a pointer button previously pressed with
    /// [`pointer_button_down`](Self::pointer_button_down).
    pub async fn pointer_button_up(&self, button: u32) -> Result<()> {
        self.input.pointer_button_up(button).await
    }

    /// Type a string as keyboard input, one X11 keysym per `char`. Latin-1
    /// characters map directly; other Unicode uses the `0x01000000 + codepoint`
    /// encoding (see [`crate::keysym::char_to_keysym`]). Does not manage
    /// focus — call [`crate::Locator::focus`] or click the target widget
    /// first.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        for ch in text.chars() {
            self.press_keysym(crate::keysym::char_to_keysym(ch)).await?;
        }
        Ok(())
    }

    /// Emit a discrete pointer-axis (wheel) event. `axis` is 0 for
    /// vertical, 1 for horizontal. `steps` is the number of wheel
    /// detents — positive scrolls down/right, negative scrolls up/left.
    pub async fn pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<()> {
        self.input.pointer_axis_discrete(axis, steps).await
    }

    /// Wayland display socket name this session is running against.
    pub fn wayland_display(&self) -> &str {
        self.compositor.wayland_display()
    }

    /// Capture a PNG screenshot from the keepalive stream.
    pub async fn take_screenshot(&self) -> Result<Vec<u8>> {
        let stream = self
            .keepalive_stream
            .as_ref()
            .ok_or_else(|| Error::Screenshot("no keepalive stream".into()))?;
        self.capture.grab_screenshot(stream).await
    }

    /// Default timeout applied to auto-wait on action methods and to
    /// explicit `wait_for_*` calls when the locator hasn't overridden it
    /// via [`Locator::with_timeout`](crate::Locator::with_timeout).
    ///
    /// Initialized at session start from the
    /// `WAYDRIVER_DEFAULT_TIMEOUT_MS` env var (milliseconds), falling back
    /// to 5 seconds. Mutable via [`set_default_timeout`](Self::set_default_timeout).
    pub fn default_timeout(&self) -> Duration {
        Duration::from_nanos(self.default_timeout_ns.load(Ordering::Relaxed))
    }

    /// Override the default timeout for this session. Takes effect on the
    /// next wait / auto-wait call; in-flight waits keep the deadline they
    /// started with.
    pub fn set_default_timeout(&self, timeout: Duration) {
        self.default_timeout_ns
            .store(timeout.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Snapshot of every stdout line the app process has printed so far.
    ///
    /// The returned vector is a copy; later lines won't appear in it even
    /// as the app continues to emit. Combine with [`stdout_cursor`] +
    /// [`wait_for_stdout_line`] for event-driven assertions, or call this
    /// directly after a `wait_for_stdout_line` if you want the full buffer.
    ///
    /// [`stdout_cursor`]: Self::stdout_cursor
    /// [`wait_for_stdout_line`]: Self::wait_for_stdout_line
    pub fn stdout_lines(&self) -> Vec<String> {
        self.stdout.lines.lock().unwrap().clone()
    }

    /// Current length of the stdout buffer — useful as a high-water mark
    /// before an action so [`wait_for_stdout_line`] can ignore older lines
    /// from the buffer and only wait for ones emitted afterwards.
    ///
    /// ```ignore
    /// let before = session.stdout_cursor();
    /// locator.click().await?;
    /// session
    ///     .wait_for_stdout_line(before, |l| l == "fixture-event: clicked ok", Duration::from_secs(1))
    ///     .await?;
    /// ```
    ///
    /// [`wait_for_stdout_line`]: Self::wait_for_stdout_line
    pub fn stdout_cursor(&self) -> usize {
        self.stdout.lines.lock().unwrap().len()
    }

    /// Wait for a stdout line matching `pred` to appear at or after index
    /// `after` in the buffer. Returns the matched line on success, or
    /// `Error::Timeout` if no matching line arrives before the deadline.
    ///
    /// Lines already in the buffer at or after `after` count as matches —
    /// there's no "only future lines" mode. Pass `self.stdout_cursor()`
    /// before kicking off the action to exclude history.
    pub async fn wait_for_stdout_line<F>(
        &self,
        after: usize,
        pred: F,
        timeout: Duration,
    ) -> Result<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Register for notifications *before* scanning so we don't
            // miss lines appended between the scan and the wait.
            let notified = self.stdout.notify.notified();
            tokio::pin!(notified);

            {
                let guard = self.stdout.lines.lock().unwrap();
                for line in guard.iter().skip(after) {
                    if pred(line) {
                        return Ok(line.clone());
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout(format!(
                    "no stdout line matched within {timeout:?} (buffer had {} line(s) after cursor {after})",
                    self.stdout.lines.lock().unwrap().len().saturating_sub(after),
                )));
            }
            if tokio::time::timeout(remaining, &mut notified)
                .await
                .is_err()
            {
                return Err(Error::Timeout(format!(
                    "no stdout line matched within {timeout:?} (buffer had {} line(s) after cursor {after})",
                    self.stdout.lines.lock().unwrap().len().saturating_sub(after),
                )));
            }
        }
    }

    /// Serialize the live AT-SPI accessibility tree rooted at this session's
    /// application to XML. The same snapshot format XPath locators resolve
    /// against — useful for debugging selectors.
    pub async fn dump_tree(&self) -> Result<String> {
        let a11y = self
            .a11y_connection
            .as_ref()
            .ok_or_else(|| Error::Atspi("session has no AT-SPI connection".into()))?;
        atspi_client::snapshot_tree(a11y, &self.app_bus_name, &self.app_path).await
    }
}

/// XPath-based element targeting entry points. Implemented on `Arc<Session>`
/// so the returned [`Locator`] can carry a shared reference back to the
/// session for lazy resolution.
impl Session {
    /// Build a locator for the given XPath expression. Resolution is lazy —
    /// the tree is snapshotted and the selector evaluated fresh on each
    /// action or metadata read.
    pub fn locate(self: &Arc<Self>, xpath: &str) -> Locator {
        Locator::new(self.clone(), xpath.to_string())
    }

    /// Locator for the root element of the application's accessibility tree.
    pub fn root(self: &Arc<Self>) -> Locator {
        self.locate("/*")
    }

    /// Locator matching any element whose toolkit `id` attribute equals `id`.
    /// Convenience shorthand for `session.locate("//*[@id='<id>']")`.
    pub fn find_by_id(self: &Arc<Self>, id: &str) -> Locator {
        self.locate(&find_by_id_xpath(id))
    }

    /// Locator matching any element whose accessible name equals `name`.
    pub fn find_by_name(self: &Arc<Self>, name: &str) -> Locator {
        self.locate(&find_by_name_xpath(name))
    }

    /// Locator matching an element by PascalCase role and accessible name.
    /// For example, `find_by_role_name("PushButton", "OK")` compiles to
    /// `//PushButton[@name='OK']`.
    pub fn find_by_role_name(self: &Arc<Self>, role: &str, name: &str) -> Locator {
        self.locate(&find_by_role_name_xpath(role, name))
    }
}

fn find_by_id_xpath(id: &str) -> String {
    format!("//*[@id={}]", xpath_literal(id))
}

fn find_by_name_xpath(name: &str) -> String {
    format!("//*[@name={}]", xpath_literal(name))
}

fn find_by_role_name_xpath(role: &str, name: &str) -> String {
    format!("//{}[@name={}]", role, xpath_literal(name))
}

/// Render a string as an XPath 1.0 string literal, choosing quote style so
/// the literal doesn't collide with the string's contents. Falls back to
/// `concat(...)` when the value contains both `'` and `"`.
fn xpath_literal(s: &str) -> String {
    let has_single = s.contains('\'');
    let has_double = s.contains('"');
    match (has_single, has_double) {
        (false, _) => format!("'{s}'"),
        (true, false) => format!("\"{s}\""),
        (true, true) => {
            let parts: Vec<String> = s.split('\'').map(|p| format!("'{p}'")).collect::<Vec<_>>();
            format!("concat({})", parts.join(", \"'\", "))
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Session {
    /// Create a Session for testing without starting a real compositor or
    /// connecting to D-Bus. AT-SPI tools will not work on test sessions.
    pub fn new_for_test(
        id: String,
        app_name: String,
        input: Box<dyn InputBackend>,
        capture: Box<dyn CaptureBackend>,
        compositor: Box<dyn CompositorRuntime>,
    ) -> Self {
        let app = Command::new("sleep")
            .arg("86400")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn sleep for test session");

        Session {
            id,
            app_name,
            app_bus_name: String::new(),
            app_path: String::new(),
            a11y_connection: None,
            default_timeout_ns: AtomicU64::new(FALLBACK_DEFAULT_TIMEOUT.as_nanos() as u64),
            app,
            keepalive_stream: None,
            video_recorder: None,
            input,
            capture,
            compositor,
            stdout: Arc::new(AppStdout::default()),
        }
    }

    /// Push a fake stdout line into the capture buffer. Used by tests that
    /// exercise [`Session::wait_for_stdout_line`] without an actual child
    /// process.
    pub fn push_stdout_line_for_test(&self, line: impl Into<String>) {
        {
            let mut guard = self.stdout.lines.lock().unwrap();
            guard.push(line.into());
        }
        self.stdout.notify.notify_waiters();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort kill when dropped without calling kill().
        // After this returns, fields drop in declaration order:
        // app → keepalive_stream → video_recorder → input → capture →
        // compositor. A video_recorder dropped without explicit stop()
        // leaves a truncated WebM (no seekhead) — see VideoRecorder::Drop.
        let _ = self.app.start_kill();
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve the initial default timeout for a new session. Reads
/// [`DEFAULT_TIMEOUT_ENV_VAR`] as milliseconds (u64), falling back to
/// [`FALLBACK_DEFAULT_TIMEOUT`] when unset or unparseable.
fn resolve_default_timeout() -> Duration {
    std::env::var(DEFAULT_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(FALLBACK_DEFAULT_TIMEOUT)
}

fn get_host_session_bus() -> Result<String> {
    Ok(get_host_session_bus_inner(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    ))
}

fn get_host_session_bus_inner(env_addr: Option<&str>) -> String {
    if let Some(addr) = env_addr {
        return addr.to_string();
    }
    let uid = unsafe { libc::getuid() };
    format!("unix:path=/run/user/{}/bus", uid)
}

fn spawn_app(
    cfg: &SessionConfig,
    wayland_display: &str,
    runtime_dir: &Path,
    dbus_address: &str,
) -> Result<Child> {
    // Use the keyfile GSettings backend with an isolated config dir so
    // the app starts with default state and never reads or writes the
    // user's dconf database. The keyfile backend bypasses the dconf
    // daemon entirely, unlike GSETTINGS_BACKEND=memory which the host
    // daemon ignores.
    let config_dir = runtime_dir.join("config");
    let _ = std::fs::create_dir_all(&config_dir);

    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("DBUS_SESSION_BUS_ADDRESS", dbus_address)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("GSETTINGS_BACKEND", "keyfile")
        .env("NO_AT_BRIDGE", "0")
        .env("GTK_A11Y", "atspi")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = &cfg.cwd {
        cmd.current_dir(dir);
    }
    cmd.spawn()
        .map_err(|e| Error::Process(format!("app '{}': {e}", cfg.command)))
}

fn normalize_app_name(name: &str) -> String {
    name.to_lowercase().replace('-', " ")
}

fn app_name_matches(found: &str, target: &str) -> bool {
    if found.is_empty() || target.is_empty() {
        return false;
    }
    let norm_found = normalize_app_name(found);
    let norm_target = normalize_app_name(target);
    norm_found.contains(&norm_target) || norm_target.contains(&norm_found)
}

async fn wait_for_app(conn: &AccessibilityConnection, app_name: &str) -> Result<(String, String)> {
    for i in 0..100 {
        if let Ok(root) = atspi_client::get_registry_root(conn).await {
            if let Ok(children) = root.get_children().await {
                let mut found_names = Vec::new();
                for child_ref in &children {
                    let Some(bus_name) = child_ref.name_as_str() else {
                        continue;
                    };
                    let path = child_ref.path_as_str();

                    if let Ok(child) =
                        atspi_client::build_accessible(conn.connection(), bus_name, path).await
                    {
                        if let Ok(name) = child.name().await {
                            if app_name_matches(&name, app_name) {
                                tracing::info!(
                                    "found app '{}' as '{}' at {}:{}",
                                    app_name,
                                    name,
                                    bus_name,
                                    path
                                );
                                return Ok((bus_name.to_string(), path.to_string()));
                            }
                            found_names.push(name);
                        }
                    }
                }

                if i % 20 == 0 {
                    tracing::debug!(
                        "AT-SPI registry has {} apps: {:?} (looking for '{}')",
                        found_names.len(),
                        found_names,
                        app_name
                    );
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::Timeout(format!(
        "app '{}' did not appear in AT-SPI registry within 10s",
        app_name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_host_session_bus_from_env() {
        let addr = "unix:path=/run/user/1000/bus";
        let result = get_host_session_bus_inner(Some(addr));
        assert_eq!(result, addr);
    }

    #[test]
    fn test_get_host_session_bus_fallback() {
        let result = get_host_session_bus_inner(None);
        assert!(
            result.contains("/run/user/"),
            "expected /run/user/ path, got: {result}"
        );
    }

    #[test]
    fn test_normalize_app_name_lowercase() {
        assert_eq!(normalize_app_name("GNOME-Calculator"), "gnome calculator");
    }

    #[test]
    fn test_normalize_app_name_hyphens_to_spaces() {
        assert_eq!(normalize_app_name("gnome-text-editor"), "gnome text editor");
    }

    #[test]
    fn test_normalize_app_name_already_normal() {
        assert_eq!(normalize_app_name("calculator"), "calculator");
    }

    #[test]
    fn test_normalize_app_name_empty() {
        assert_eq!(normalize_app_name(""), "");
    }

    #[test]
    fn test_app_name_matches_exact() {
        assert!(app_name_matches("Calculator", "calculator"));
    }

    #[test]
    fn test_app_name_matches_target_contains_found() {
        assert!(app_name_matches("Calculator", "gnome-calculator"));
    }

    #[test]
    fn test_app_name_matches_found_contains_target() {
        assert!(app_name_matches(
            "GNOME Calculator 46.1",
            "gnome-calculator"
        ));
    }

    #[test]
    fn test_app_name_matches_no_match() {
        assert!(!app_name_matches("Firefox", "gnome-calculator"));
    }

    #[test]
    fn test_app_name_matches_hyphen_vs_space() {
        assert!(app_name_matches("gnome calculator", "gnome-calculator"));
    }

    #[test]
    fn test_app_name_matches_empty_target() {
        assert!(!app_name_matches("Calculator", ""));
    }

    #[test]
    fn test_app_name_matches_empty_found() {
        assert!(!app_name_matches("", "calculator"));
    }

    #[test]
    fn test_app_name_matches_both_empty() {
        assert!(!app_name_matches("", ""));
    }

    #[test]
    fn xpath_literal_plain() {
        assert_eq!(xpath_literal("OK"), "'OK'");
    }

    #[test]
    fn xpath_literal_with_apostrophe() {
        assert_eq!(xpath_literal("John's"), "\"John's\"");
    }

    #[test]
    fn xpath_literal_with_double_quote() {
        assert_eq!(xpath_literal("a\"b"), "'a\"b'");
    }

    #[test]
    fn xpath_literal_with_both_quotes() {
        // "a'b\"c" → concat('a', "'", 'b"c')
        let out = xpath_literal("a'b\"c");
        assert_eq!(out, "concat('a', \"'\", 'b\"c')");
    }

    #[test]
    fn find_by_id_xpath_simple() {
        assert_eq!(find_by_id_xpath("submit-btn"), "//*[@id='submit-btn']");
    }

    #[test]
    fn find_by_id_xpath_escapes_apostrophe() {
        // An id with a single quote must use double-quoted literal.
        assert_eq!(find_by_id_xpath("a'b"), "//*[@id=\"a'b\"]");
    }

    #[test]
    fn find_by_name_xpath_simple() {
        assert_eq!(find_by_name_xpath("OK"), "//*[@name='OK']");
    }

    #[test]
    fn find_by_name_xpath_with_space() {
        // Spaces are fine in XPath string literals — no special handling needed.
        assert_eq!(find_by_name_xpath("Save As"), "//*[@name='Save As']");
    }

    #[test]
    fn find_by_name_xpath_with_both_quotes_uses_concat() {
        assert_eq!(
            find_by_name_xpath("John's \"file\""),
            "//*[@name=concat('John', \"'\", 's \"file\"')]"
        );
    }

    #[test]
    fn find_by_role_name_xpath_composes_role_and_name() {
        assert_eq!(
            find_by_role_name_xpath("PushButton", "OK"),
            "//PushButton[@name='OK']"
        );
    }

    #[test]
    fn find_by_role_name_xpath_preserves_role_as_element_name() {
        // Role string is NOT escaped — it's used as the XPath node-test, so
        // callers pass PascalCase role names directly.
        assert_eq!(
            find_by_role_name_xpath("MenuItem", "File"),
            "//MenuItem[@name='File']"
        );
    }

    // ── resolve_default_timeout ────────────────────────────────────────────

    /// One test function for all three cases so they execute serially within
    /// the test thread. `std::env::set_var` is process-global, so running
    /// these as separate `#[test]`s would race under cargo's default parallel
    /// test runner and produce flaky failures.
    #[test]
    fn resolve_default_timeout_cases() {
        // Case 1: unset → fallback.
        std::env::remove_var(DEFAULT_TIMEOUT_ENV_VAR);
        assert_eq!(resolve_default_timeout(), FALLBACK_DEFAULT_TIMEOUT);

        // Case 2: valid number → parsed as milliseconds.
        std::env::set_var(DEFAULT_TIMEOUT_ENV_VAR, "750");
        assert_eq!(resolve_default_timeout(), Duration::from_millis(750));

        // Case 3: garbage → fallback.
        std::env::set_var(DEFAULT_TIMEOUT_ENV_VAR, "not-a-number");
        assert_eq!(resolve_default_timeout(), FALLBACK_DEFAULT_TIMEOUT);

        // Case 4: empty string → fallback.
        std::env::set_var(DEFAULT_TIMEOUT_ENV_VAR, "");
        assert_eq!(resolve_default_timeout(), FALLBACK_DEFAULT_TIMEOUT);

        // Restore clean state for other tests in this process.
        std::env::remove_var(DEFAULT_TIMEOUT_ENV_VAR);
    }

    #[tokio::test]
    async fn session_default_timeout_can_be_overridden() {
        use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};

        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _r: Option<&str>) -> Result<()> {
                Ok(())
            }
            async fn stop(&mut self) -> Result<()> {
                Ok(())
            }
            fn id(&self) -> &str {
                "s"
            }
            fn wayland_display(&self) -> &str {
                "d"
            }
            fn runtime_dir(&self) -> &Path {
                Path::new("/tmp")
            }
        }
        struct StubInput;
        #[async_trait]
        impl InputBackend for StubInput {
            async fn press_keysym(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(&self, _: u32, _: i32) -> Result<()> {
                Ok(())
            }
        }
        struct StubCapture;
        #[async_trait]
        impl CaptureBackend for StubCapture {
            async fn start_stream(&self) -> Result<PipeWireStream> {
                unimplemented!()
            }
            async fn stop_stream(&self, _: PipeWireStream) -> Result<()> {
                Ok(())
            }
            fn pipewire_socket(&self) -> PathBuf {
                PathBuf::from("/tmp")
            }
        }

        let s = Session::new_for_test(
            "t".into(),
            "a".into(),
            Box::new(StubInput),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        );
        // Default matches the fallback constant.
        assert_eq!(s.default_timeout(), FALLBACK_DEFAULT_TIMEOUT);
        // set_default_timeout persists.
        s.set_default_timeout(Duration::from_millis(1234));
        assert_eq!(s.default_timeout(), Duration::from_millis(1234));
    }

    #[tokio::test]
    async fn press_chord_issues_modifiers_then_target_then_releases_in_reverse() {
        use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};
        use std::sync::Mutex;

        /// What an InputBackend call was — used to assert dispatch order.
        #[derive(Debug, PartialEq, Eq)]
        enum Event {
            Down(u32),
            Up(u32),
            Press(u32),
        }

        struct RecordingInput(Arc<Mutex<Vec<Event>>>);
        #[async_trait]
        impl InputBackend for RecordingInput {
            async fn press_keysym(&self, k: u32) -> Result<()> {
                self.0.lock().unwrap().push(Event::Press(k));
                Ok(())
            }
            async fn key_down(&self, k: u32) -> Result<()> {
                self.0.lock().unwrap().push(Event::Down(k));
                Ok(())
            }
            async fn key_up(&self, k: u32) -> Result<()> {
                self.0.lock().unwrap().push(Event::Up(k));
                Ok(())
            }
            async fn pointer_motion_relative(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(&self, _: u32, _: i32) -> Result<()> {
                Ok(())
            }
        }

        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>) -> Result<()> {
                Ok(())
            }
            async fn stop(&mut self) -> Result<()> {
                Ok(())
            }
            fn id(&self) -> &str {
                "s"
            }
            fn wayland_display(&self) -> &str {
                "d"
            }
            fn runtime_dir(&self) -> &Path {
                Path::new("/tmp")
            }
        }
        struct StubCapture;
        #[async_trait]
        impl CaptureBackend for StubCapture {
            async fn start_stream(&self) -> Result<PipeWireStream> {
                unimplemented!()
            }
            async fn stop_stream(&self, _: PipeWireStream) -> Result<()> {
                Ok(())
            }
            fn pipewire_socket(&self) -> PathBuf {
                PathBuf::from("/tmp")
            }
        }

        let events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let s = Session::new_for_test(
            "t".into(),
            "a".into(),
            Box::new(RecordingInput(events.clone())),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        );

        s.press_chord("Ctrl+Shift+A").await.unwrap();

        let ctrl = 0xffe3_u32;
        let shift = 0xffe1_u32;
        let a = crate::keysym::char_to_keysym('A');
        let recorded = events.lock().unwrap().iter().collect::<Vec<_>>().len();
        let got: Vec<Event> = std::mem::take(&mut *events.lock().unwrap());
        assert_eq!(recorded, 5);
        // Expected dispatch: ctrl down, shift down, press(A), shift up, ctrl up.
        assert_eq!(
            got,
            vec![
                Event::Down(ctrl),
                Event::Down(shift),
                Event::Press(a),
                Event::Up(shift),
                Event::Up(ctrl),
            ]
        );
    }

    #[tokio::test]
    async fn press_chord_rejects_garbage() {
        use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};

        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>) -> Result<()> {
                Ok(())
            }
            async fn stop(&mut self) -> Result<()> {
                Ok(())
            }
            fn id(&self) -> &str {
                "s"
            }
            fn wayland_display(&self) -> &str {
                "d"
            }
            fn runtime_dir(&self) -> &Path {
                Path::new("/tmp")
            }
        }
        struct StubInput;
        #[async_trait]
        impl InputBackend for StubInput {
            async fn press_keysym(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(&self, _: u32, _: i32) -> Result<()> {
                Ok(())
            }
        }
        struct StubCapture;
        #[async_trait]
        impl CaptureBackend for StubCapture {
            async fn start_stream(&self) -> Result<PipeWireStream> {
                unimplemented!()
            }
            async fn stop_stream(&self, _: PipeWireStream) -> Result<()> {
                Ok(())
            }
            fn pipewire_socket(&self) -> PathBuf {
                PathBuf::from("/tmp")
            }
        }

        let s = Session::new_for_test(
            "t".into(),
            "a".into(),
            Box::new(StubInput),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        );

        let err = s.press_chord("Hyper+Nope").await.unwrap_err();
        assert!(
            matches!(err, Error::Process(ref m) if m.contains("invalid chord")),
            "expected process:invalid chord, got {err:?}"
        );
    }

    /// Build a test-only Session whose input/capture/compositor are no-op
    /// stubs — so we can exercise stdout-capture plumbing without spinning
    /// up mutter.
    fn make_test_session() -> Session {
        use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};

        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>) -> Result<()> {
                Ok(())
            }
            async fn stop(&mut self) -> Result<()> {
                Ok(())
            }
            fn id(&self) -> &str {
                "s"
            }
            fn wayland_display(&self) -> &str {
                "d"
            }
            fn runtime_dir(&self) -> &Path {
                Path::new("/tmp")
            }
        }
        struct StubInput;
        #[async_trait]
        impl InputBackend for StubInput {
            async fn press_keysym(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(&self, _: f64, _: f64) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(&self, _: u32) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(&self, _: u32, _: i32) -> Result<()> {
                Ok(())
            }
        }
        struct StubCapture;
        #[async_trait]
        impl CaptureBackend for StubCapture {
            async fn start_stream(&self) -> Result<PipeWireStream> {
                unimplemented!()
            }
            async fn stop_stream(&self, _: PipeWireStream) -> Result<()> {
                Ok(())
            }
            fn pipewire_socket(&self) -> PathBuf {
                PathBuf::from("/tmp")
            }
        }

        Session::new_for_test(
            "t".into(),
            "a".into(),
            Box::new(StubInput),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        )
    }

    #[tokio::test]
    async fn wait_for_stdout_line_returns_existing_match_immediately() {
        let s = make_test_session();
        s.push_stdout_line_for_test("fixture-event: clicked primary-button");
        let line = s
            .wait_for_stdout_line(
                0,
                |l| l.contains("clicked primary-button"),
                Duration::from_millis(100),
            )
            .await
            .expect("should match existing line");
        assert!(line.contains("clicked primary-button"));
    }

    #[tokio::test]
    async fn wait_for_stdout_line_respects_after_cursor() {
        let s = make_test_session();
        // Pre-existing noise the test should skip past.
        s.push_stdout_line_for_test("some startup chatter");
        s.push_stdout_line_for_test("fixture-event: clicked old-button");
        let cursor = s.stdout_cursor();
        assert_eq!(cursor, 2);

        // Line added after cursor — should match.
        s.push_stdout_line_for_test("fixture-event: clicked new-button");
        let line = s
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("clicked"),
                Duration::from_millis(100),
            )
            .await
            .expect("should match line after cursor");
        assert!(line.contains("new-button"), "got: {line}");
    }

    #[tokio::test]
    async fn wait_for_stdout_line_wakes_on_notify() {
        let s = Arc::new(make_test_session());
        let cursor = s.stdout_cursor();

        // Push a matching line 50ms into the wait.
        let pusher = s.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            pusher.push_stdout_line_for_test("fixture-event: clicked async-button");
        });

        let line = s
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("async-button"),
                Duration::from_secs(2),
            )
            .await
            .expect("should wake on notify");
        assert!(line.contains("async-button"));
    }

    #[tokio::test]
    async fn wait_for_stdout_line_times_out_when_no_match() {
        let s = make_test_session();
        let err = s
            .wait_for_stdout_line(0, |l| l == "never", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "got: {err:?}");
    }
}
