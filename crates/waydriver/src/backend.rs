use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Result;

/// Direction of a discrete (notched) pointer-wheel scroll.
///
/// Encoding-free: backends translate at their boundary. The mutter
/// `NotifyPointerAxisDiscrete` D-Bus call wants `0` for vertical and
/// `1` for horizontal; libei has its own enum; X11 uses pseudo-buttons
/// 4-5/6-7. Callers and the MCP-layer JSON schema only see the named
/// directions, so adding a third (e.g. zoom or pan) is a compatible
/// extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAxis {
    /// Up/down — same direction a vertical mouse wheel uses.
    Vertical,
    /// Left/right — sideways scrolling on tilt-wheel mice or trackpads.
    Horizontal,
}

/// Pointer button.
///
/// The three named variants cover every standard mouse; `Other(code)`
/// is an escape hatch for everything else (back/forward, gaming-mouse
/// extras, side buttons) carrying the Linux `BTN_*` evdev code as a
/// raw `u32`. A typed enum at the trait boundary catches obvious
/// bugs (passing a literal `0x110` everywhere instead of `BTN_LEFT`)
/// at compile time and lets non-evdev backends translate centrally
/// in their own [`evdev_code`](Self::evdev_code) consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// Primary mouse button (`BTN_LEFT`, `0x110`).
    Left,
    /// Wheel-press button (`BTN_MIDDLE`, `0x112`).
    Middle,
    /// Secondary / context-menu button (`BTN_RIGHT`, `0x111`).
    Right,
    /// Any other button, identified by its Linux evdev `BTN_*` code.
    /// Backends that use a different transport are expected to
    /// translate; callers that already have an evdev code in hand
    /// (e.g. parsed from external input) can pass it straight through.
    Other(u32),
}

impl PointerButton {
    /// Return the Linux evdev `BTN_*` code for this button. Used by
    /// backends whose transport speaks evdev directly (mutter's
    /// RemoteDesktop, libei). Backends on other transports translate
    /// in their own pattern-match.
    pub fn evdev_code(self) -> u32 {
        match self {
            PointerButton::Left => 0x110,
            PointerButton::Middle => 0x112,
            PointerButton::Right => 0x111,
            PointerButton::Other(code) => code,
        }
    }

    /// Construct from a Linux evdev `BTN_*` code, mapping the three
    /// standard buttons onto their named variants and falling back to
    /// `Other(code)` for anything else. This is the boundary
    /// inverse of [`evdev_code`](Self::evdev_code) and is what
    /// callers receiving raw codes (MCP JSON, libinput pass-through)
    /// should use.
    pub fn from_evdev_code(code: u32) -> Self {
        match code {
            0x110 => PointerButton::Left,
            0x112 => PointerButton::Middle,
            0x111 => PointerButton::Right,
            other => PointerButton::Other(other),
        }
    }
}

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
    ///
    /// `scale` is the logical-monitor scale factor (e.g. `2.0` for a 2×
    /// HiDPI display, `1.5` for 150%). `None` — or `Some(1.0)` — means the
    /// backend's default 1:1 mapping, where `resolution` pixels are also the
    /// logical (application) size. With a scale `s`, `resolution` is the
    /// *physical* framebuffer and applications see a logical size of
    /// `resolution / s`, exactly as a real HiDPI monitor behaves. Backends
    /// that can't honour a non-1.0 scale should error rather than silently
    /// ignore it.
    async fn start(&mut self, resolution: Option<&str>, scale: Option<f64>) -> Result<()>;

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
///
/// ## Cancellation convention
///
/// Every method accepts a [`CancellationToken`]. The token is the
/// [`Session`](crate::Session)'s own cancellation handle, forwarded by
/// Session-level wrappers so individual backend calls can observe it
/// without the caller needing to plumb it themselves.
///
/// Implementations must treat cancellation as follows:
/// - **Atomic press/release gaps must complete.** Backends that press a
///   key (or button) down, wait, then release must *not* bail during
///   that gap — doing so would leave a key stuck in the compositor's
///   state. The gap is short (single-digit to tens of ms) so running
///   it to completion costs little.
/// - **Tail throttles may bail.** Backends that sleep *after* an event
///   committed (so back-to-back events don't overwhelm the app) should
///   race the sleep against `cancel.cancelled()` and return `Ok(())`
///   early on cancel — the event already succeeded and the throttle
///   is a courtesy. Higher-level loops (e.g.
///   [`Session::type_text`](crate::Session::type_text)) pick up the
///   cancellation on their next pre-flight check and return
///   [`Error::Cancelled`](crate::Error::Cancelled) to the caller.
/// - **Pre-event checks are optional** — the outer loops already
///   short-circuit, so there's no correctness requirement to re-check
///   at the backend boundary. Backends may still check if it saves
///   setup work (e.g. a D-Bus call).
#[async_trait]
pub trait InputBackend: Send + Sync {
    /// Press and release a single X11 keysym. Implementations handle any
    /// inter-event timing required by the transport. Equivalent to
    /// [`key_down`](Self::key_down) immediately followed by
    /// [`key_up`](Self::key_up).
    async fn press_keysym(&self, keysym: u32, cancel: &CancellationToken) -> Result<()>;

    /// Press a key and hold it down until a corresponding
    /// [`key_up`](Self::key_up) fires. Used to build modifier combos — hold
    /// `Ctrl` down across a target keystroke and release it afterward.
    async fn key_down(&self, keysym: u32, cancel: &CancellationToken) -> Result<()>;

    /// Release a key that was previously pressed with
    /// [`key_down`](Self::key_down). Safe to call on a key that isn't held
    /// (behavior is implementation-defined, but must not panic).
    async fn key_up(&self, keysym: u32, cancel: &CancellationToken) -> Result<()>;

    /// Move the pointer by a relative offset in logical pixels.
    async fn pointer_motion_relative(
        &self,
        dx: f64,
        dy: f64,
        cancel: &CancellationToken,
    ) -> Result<()>;

    /// Move the pointer to a screen-relative absolute position in logical
    /// pixels. Implementations route through whatever channel their
    /// compositor exposes (e.g. `NotifyPointerMotionAbsolute` on mutter's
    /// RemoteDesktop). Backends with no active capture stream to address
    /// should return `Err`.
    async fn pointer_motion_absolute(
        &self,
        x: f64,
        y: f64,
        cancel: &CancellationToken,
    ) -> Result<()>;

    /// Press a pointer button and hold it down until a corresponding
    /// [`pointer_button_up`](Self::pointer_button_up) fires. The
    /// [`PointerButton`] enum carries either one of the three named
    /// buttons or a raw Linux evdev `BTN_*` code via `Other(u32)`.
    /// Used to build drag gestures — press, move the pointer across
    /// intermediate coordinates, then release.
    async fn pointer_button_down(
        &self,
        button: PointerButton,
        cancel: &CancellationToken,
    ) -> Result<()>;

    /// Release a pointer button that was previously pressed with
    /// [`pointer_button_down`](Self::pointer_button_down). Safe to call on
    /// a button that isn't held (behavior is implementation-defined, but
    /// must not panic).
    async fn pointer_button_up(
        &self,
        button: PointerButton,
        cancel: &CancellationToken,
    ) -> Result<()>;

    /// Press and release a pointer button. Default impl composes
    /// [`pointer_button_down`](Self::pointer_button_down) and
    /// [`pointer_button_up`](Self::pointer_button_up) with a short gap so
    /// the compositor distinguishes press from release; backends with a
    /// more efficient combined path can override.
    ///
    /// The 20 ms press/release gap is *atomic*: we do not race it
    /// against the token because cancelling mid-gap would leave the
    /// button held down in the compositor's state. Cancellation
    /// observed on the tail of `pointer_button_up` (or at the outer
    /// loop's next iteration) is the designed bail-point.
    async fn pointer_button(
        &self,
        button: PointerButton,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.pointer_button_down(button, cancel).await?;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.pointer_button_up(button, cancel).await
    }

    /// Emit a discrete pointer-axis (wheel) event. `axis` selects the
    /// direction (`PointerAxis::Vertical` / `Horizontal`); `steps` is
    /// the number of wheel detents; positive scrolls down / right,
    /// negative scrolls up / left.
    ///
    /// Backends that don't support discrete wheel events may emulate
    /// via continuous axis deltas; callers shouldn't rely on step being
    /// exactly one wheel click's worth of travel, just on sign + rough
    /// magnitude. Backends translate the enum into their transport's
    /// native encoding (mutter's RemoteDesktop wants `0=vertical,
    /// 1=horizontal`, libei has its own enum).
    async fn pointer_axis_discrete(
        &self,
        axis: PointerAxis,
        steps: i32,
        cancel: &CancellationToken,
    ) -> Result<()>;
}

/// Sleep up to `dur`, waking immediately if `cancel` trips. Returns
/// unconditionally — the sleep is a courtesy throttle, not a critical
/// section, so callers don't distinguish "slept full time" from "cancel
/// cut the sleep short." Use for post-event tail delays in input
/// backends; use [`tokio::select`] + an explicit
/// `Err(crate::Error::Cancelled)` arm when cancelling must propagate.
pub async fn cancellable_tail(dur: std::time::Duration, cancel: &CancellationToken) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(dur) => {}
    }
}

/// Backend-private state attached to a [`PipeWireStream`].
///
/// Each `CaptureBackend` impl needs to remember some per-stream
/// resource — for mutter, that's the ScreenCast session's D-Bus
/// object path; another backend might carry a libei session id, a
/// wlr-screencopy frame buffer, or a wayland-protocol object. The
/// public API doesn't care which: `stop_stream` is the only
/// consumer and it always re-downcasts to whatever it stored.
///
/// Wrapping `Box<dyn Any + Send + Sync>` in a newtype rather than
/// exposing `Any` directly: keeps the public field type stable
/// across backend changes, routes construction through
/// [`StreamToken::new`] so callers don't have to spell out the
/// `Send + Sync + 'static` bound, and lets [`StreamToken::downcast`]
/// surface a typed [`Error::Screenshot`](crate::Error::Screenshot)
/// naming both the expected and stored types instead of an opaque
/// `()` mismatch.
pub struct StreamToken {
    inner: Box<dyn std::any::Any + Send + Sync + 'static>,
    /// `std::any::type_name::<T>()` captured at construction — `Any`
    /// only exposes a `TypeId` after the fact (which renders as an
    /// opaque hex blob), so we have to remember the source type name
    /// here if we want the downcast error to be readable.
    stored_type: &'static str,
}

impl StreamToken {
    /// Construct from any backend-private value. The bound matches
    /// `Box<dyn Any + Send + Sync + 'static>` so the resulting token
    /// can cross await points and live inside an `Arc<Session>`.
    pub fn new<T: std::any::Any + Send + Sync + 'static>(value: T) -> Self {
        Self {
            inner: Box::new(value),
            stored_type: std::any::type_name::<T>(),
        }
    }

    /// Recover the original concrete type. Returns
    /// [`Error::Screenshot`](crate::Error::Screenshot) naming both
    /// the requested `T` and the stored type when the cast fails,
    /// so backend bugs (storing a libei id then trying to recover
    /// a mutter object path) surface with actionable detail rather
    /// than an opaque "downcast failed."
    pub fn downcast<T: std::any::Any>(self) -> crate::error::Result<Box<T>> {
        let stored = self.stored_type;
        self.inner.downcast::<T>().map_err(|_| {
            crate::error::Error::screenshot(format!(
                "stream token type mismatch: expected {}, found {stored}",
                std::any::type_name::<T>(),
            ))
        })
    }
}

/// A live PipeWire stream the backend is keeping open on behalf of a caller.
/// Callers must explicitly call `CaptureBackend::stop_stream` — dropping does
/// not stop the stream.
pub struct PipeWireStream {
    /// PipeWire node id that a consumer (e.g. gst-launch pipewiresrc) can
    /// connect to.
    pub node_id: u32,
    /// Backend-private state (e.g. a ScreenCast session object path)
    /// that `stop_stream` needs to tear down the stream. Construct via
    /// [`StreamToken::new`]; recover via [`StreamToken::downcast`].
    pub token: StreamToken,
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

    /// Start a *dedicated* capture stream for a video recording, distinct
    /// from the interactive/keepalive stream returned by [`Self::start_stream`].
    ///
    /// The recorder must not share a node with the screenshot path. A
    /// recording pipeline is a continuous consumer; on compositors whose
    /// screencast node only emits frames on screen damage (mutter negotiates
    /// `framerate=0/1`), a screenshot consumer that attaches *after* the
    /// recorder is already draining the shared node never receives the
    /// initial frame and times out — a static app produces no further damage.
    /// Giving the recorder its own node keeps the screenshot consumer the
    /// node's first/triggering consumer, which is what makes the
    /// recording-off path reliable.
    ///
    /// Backends that don't have this hazard may use the default, which simply
    /// opens another stream via [`Self::start_stream`]. The mutter backend
    /// overrides this to open a *standalone* (non-RemoteDesktop-linked)
    /// stream so it doesn't perturb pointer routing or the active-stream path.
    async fn start_recording_stream(&self) -> Result<PipeWireStream> {
        self.start_stream().await
    }

    /// Stop a stream previously opened by [`Self::start_recording_stream`].
    /// The default delegates to [`Self::stop_stream`]; the mutter backend
    /// overrides it to avoid clearing the interactive stream's active-stream
    /// path.
    async fn stop_recording_stream(&self, stream: PipeWireStream) -> Result<()> {
        self.stop_stream(stream).await
    }

    /// Path to the PipeWire socket the shared GStreamer helper should talk
    /// to (usually `<runtime_dir>/pipewire-0`).
    fn pipewire_socket(&self) -> PathBuf;

    /// Capture a PNG from an already-running stream.
    async fn grab_screenshot(&self, stream: &PipeWireStream) -> Result<Vec<u8>> {
        crate::capture::grab_png(stream.node_id, &self.pipewire_socket()).await
    }

    /// Start a continuous WebM recording of `stream` written to
    /// `output_path` at the given `bitrate` (bits/sec) and `fps`. Returns a
    /// handle whose `stop()` must be awaited to finalize the file cleanly.
    async fn start_recording(
        &self,
        stream: &PipeWireStream,
        output_path: &Path,
        bitrate: u32,
        fps: u32,
    ) -> Result<crate::capture::VideoRecorder> {
        crate::capture::VideoRecorder::start(
            stream.node_id,
            &self.pipewire_socket(),
            output_path,
            bitrate,
            fps,
        )
        .await
    }

    /// Stop a previously-started recording, flushing the WebM seekhead/cues
    /// before returning.
    async fn stop_recording(&self, recorder: crate::capture::VideoRecorder) -> Result<()> {
        recorder.stop().await
    }
}
