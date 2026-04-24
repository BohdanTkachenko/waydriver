use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::Result;

/// Lifecycle of a headless compositor instance. A backend owns its compositor's
/// child processes (the compositor binary itself plus any supporting daemons
/// like pipewire) and exposes the Wayland display name and runtime dir that
/// client applications and sibling backends use.
#[async_trait]
pub trait CompositorRuntime: Send + Sync {
    /// Spawn the compositor at the requested virtual-display size (or the
    /// backend default when `None`) and wait for it to be ready. After this
    /// returns successfully, `wayland_display()` and `runtime_dir()` must
    /// point at a live Wayland socket.
    async fn start(&mut self, resolution: Option<&str>) -> Result<()>;

    /// Stop the compositor, tearing down all child processes and cleaning up
    /// the runtime directory. Safe to call on an un-started or
    /// already-stopped backend.
    async fn stop(&mut self) -> Result<()>;

    /// Session identifier, used in log fields and default output paths.
    fn id(&self) -> &str;

    /// Wayland display socket name (e.g. `wayland-wd-abc12345`).
    fn wayland_display(&self) -> &str;

    /// Per-session XDG_RUNTIME_DIR, holding the Wayland socket and any
    /// supporting sockets like pipewire.
    fn runtime_dir(&self) -> &Path;
}

/// Keyboard and pointer injection. Decoupled from the compositor trait so
/// alternative implementations (e.g. libei) can drive the same compositor
/// alongside a mutter/KWin/wlroots backend.
#[async_trait]
pub trait InputBackend: Send + Sync {
    /// Press and release a single X11 keysym. Implementations handle any
    /// inter-event timing required by the transport. Equivalent to
    /// [`key_down`](Self::key_down) immediately followed by
    /// [`key_up`](Self::key_up).
    async fn press_keysym(&self, keysym: u32) -> Result<()>;

    /// Press a key and hold it down until a corresponding
    /// [`key_up`](Self::key_up) fires. Used to build modifier combos — hold
    /// `Ctrl` down across a target keystroke and release it afterward.
    async fn key_down(&self, keysym: u32) -> Result<()>;

    /// Release a key that was previously pressed with
    /// [`key_down`](Self::key_down). Safe to call on a key that isn't held
    /// (behavior is implementation-defined, but must not panic).
    async fn key_up(&self, keysym: u32) -> Result<()>;

    /// Move the pointer by a relative offset in logical pixels.
    async fn pointer_motion_relative(&self, dx: f64, dy: f64) -> Result<()>;

    /// Move the pointer to a screen-relative absolute position in logical
    /// pixels. Implementations route through whatever channel their
    /// compositor exposes (e.g. `NotifyPointerMotionAbsolute` on mutter's
    /// RemoteDesktop). Backends with no active capture stream to address
    /// should return `Err`.
    async fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()>;

    /// Press a pointer button and hold it down until a corresponding
    /// [`pointer_button_up`](Self::pointer_button_up) fires. `button` uses
    /// Linux evdev codes (e.g. `BTN_LEFT` = 0x110). Used to build drag
    /// gestures — press, move the pointer across intermediate coordinates,
    /// then release.
    async fn pointer_button_down(&self, button: u32) -> Result<()>;

    /// Release a pointer button that was previously pressed with
    /// [`pointer_button_down`](Self::pointer_button_down). Safe to call on
    /// a button that isn't held (behavior is implementation-defined, but
    /// must not panic).
    async fn pointer_button_up(&self, button: u32) -> Result<()>;

    /// Press and release a pointer button. `button` uses Linux evdev codes
    /// (e.g. `BTN_LEFT` = 0x110). Default impl composes
    /// [`pointer_button_down`](Self::pointer_button_down) and
    /// [`pointer_button_up`](Self::pointer_button_up) with a short gap so
    /// the compositor distinguishes press from release; backends with a
    /// more efficient combined path can override.
    async fn pointer_button(&self, button: u32) -> Result<()> {
        self.pointer_button_down(button).await?;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.pointer_button_up(button).await
    }

    /// Emit a discrete pointer-axis (wheel) event. `axis` selects the
    /// direction — `0` is vertical, `1` is horizontal — matching the
    /// `org.gnome.Mutter.RemoteDesktop.Session.NotifyPointerAxisDiscrete`
    /// convention. `steps` is the number of wheel detents; positive
    /// scrolls down / right, negative scrolls up / left.
    ///
    /// Backends that don't support discrete wheel events may emulate
    /// via continuous axis deltas; callers shouldn't rely on step being
    /// exactly one wheel click's worth of travel, just on sign + rough
    /// magnitude.
    async fn pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<()>;
}

/// A live PipeWire stream the backend is keeping open on behalf of a caller.
/// Callers must explicitly call `CaptureBackend::stop_stream` — dropping does
/// not stop the stream.
pub struct PipeWireStream {
    /// PipeWire node id that a consumer (e.g. gst-launch pipewiresrc) can
    /// connect to.
    pub node_id: u32,
    /// Opaque per-backend state (e.g. a ScreenCast session object path) that
    /// `stop_stream` needs to tear down the stream.
    pub token: Box<dyn std::any::Any + Send + Sync>,
}

/// Screen capture. Backends either return a PipeWire node id (the common path
/// on mutter/KWin) that the default `take_screenshot` pipes through GStreamer,
/// or override `take_screenshot` directly if they can produce a PNG without
/// PipeWire (e.g. a future wlr-screencopy backend).
#[async_trait]
pub trait CaptureBackend: Send + Sync {
    /// Start a PipeWire capture stream. The returned `PipeWireStream` stays
    /// alive until explicitly stopped.
    async fn start_stream(&self) -> Result<PipeWireStream>;

    /// Stop a previously started stream and release backend-side resources.
    async fn stop_stream(&self, stream: PipeWireStream) -> Result<()>;

    /// Path to the PipeWire socket the shared GStreamer helper should talk
    /// to (usually `<runtime_dir>/pipewire-0`).
    fn pipewire_socket(&self) -> PathBuf;

    /// Capture a PNG from an already-running stream.
    async fn grab_screenshot(&self, stream: &PipeWireStream) -> Result<Vec<u8>> {
        crate::capture::grab_png(stream.node_id, &self.pipewire_socket()).await
    }

    /// Start a continuous WebM recording of `stream` written to
    /// `output_path` at the given `bitrate` (bits/sec). Returns a handle
    /// whose `stop()` must be awaited to finalize the file cleanly.
    async fn start_recording(
        &self,
        stream: &PipeWireStream,
        output_path: &Path,
        bitrate: u32,
    ) -> Result<crate::capture::VideoRecorder> {
        crate::capture::VideoRecorder::start(
            stream.node_id,
            &self.pipewire_socket(),
            output_path,
            bitrate,
        )
        .await
    }

    /// Stop a previously-started recording, flushing the WebM seekhead/cues
    /// before returning.
    async fn stop_recording(&self, recorder: crate::capture::VideoRecorder) -> Result<()> {
        recorder.stop().await
    }
}
