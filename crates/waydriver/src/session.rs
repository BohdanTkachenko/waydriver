use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::atspi as atspi_client;
use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PointerAxis, PointerButton};
use crate::capture::VideoRecorder;
use crate::error::{Error, Result};
use crate::locator::Locator;

/// Fallback default timeout for auto-wait and explicit `wait_for_*` methods
/// when the `WAYDRIVER_DEFAULT_TIMEOUT_MS` env var isn't set.
const FALLBACK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on [`Session::kill`]. Past this, the future is dropped and the
/// caller gets [`Error::Timeout`] rather than waiting on a wedged D-Bus
/// call (compositor `stop()`, recording flush) or a stuck child wait.
///
/// Sized to comfortably exceed the worst-case mutter compositor shutdown
/// (~2-3s on a healthy session) plus a margin for recording-flush. With
/// AT-SPI proxies capped at the 2s `A11Y_METHOD_TIMEOUT` in `atspi.rs`,
/// a single in-flight Locator round-trip can't blow this budget on its
/// own — the cancellation token short-circuits the next iteration.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variable controlling the default wait/auto-wait timeout, in
/// milliseconds. Overridable per-session via [`Session::set_default_timeout`]
/// and per-call via [`Locator::with_timeout`](crate::Locator::with_timeout).
pub const DEFAULT_TIMEOUT_ENV_VAR: &str = "WAYDRIVER_DEFAULT_TIMEOUT_MS";

/// How long [`wait_for_app`] polls the AT-SPI registry for the target app
/// before failing. GTK4 + mutter's AT-SPI bridge typically publishes within a
/// second; the generous budget covers heavy-at-startup targets and loaded CI.
const APP_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for the AT-SPI registry walk in [`wait_for_app`] — short
/// enough to catch the app promptly without hammering D-Bus.
const APP_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Parameters for spawning the target application inside a session.
pub struct SessionConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Accessible name used to look the app up in the AT-SPI registry.
    pub app_name: String,
    /// If set, the session records a continuous WebM video of the display to
    /// this path. Recording runs on its own dedicated ScreenCast stream,
    /// opened during [`Session::start`] and torn down in [`Session::kill`]
    /// right after the encoder is flushed — kept off the keepalive node so it
    /// can't starve the screenshot path. When `None`, no recording stream or
    /// pipeline is started.
    pub video_output: Option<PathBuf>,
    /// VP8 target bitrate in bits/sec for the recording pipeline. Only
    /// consulted when `video_output` is `Some`. When `None`, falls back to
    /// [`crate::capture::DEFAULT_VIDEO_BITRATE`].
    pub video_bitrate: Option<u32>,
    /// Recording framerate in frames-per-second. Only consulted when
    /// `video_output` is `Some`. When `None`, falls back to
    /// [`crate::capture::DEFAULT_VIDEO_FPS`].
    pub video_fps: Option<u32>,
    /// When true and the `visual` Cargo feature is enabled, spawn a
    /// background task during [`Session::start`] that downloads any
    /// missing ocrs model files and loads the [`ocrs::OcrEngine`]. This
    /// keeps the first
    /// [`Session::find_by_text`](crate::Session::find_by_text) call
    /// off the test's critical path: when the test eventually reaches an
    /// OCR-based assertion, the engine is either already ready or about
    /// to be — the call awaits the prewarm without doing the load work
    /// twice.
    ///
    /// The field is always present so adding the visual feature on a
    /// downstream crate doesn't break existing callers; without the
    /// feature, the value is read and ignored.
    pub prewarm_visual: bool,
    /// Per-session tuning knobs for `Locator::find_regions` /
    /// `first_region` / `last_region` (the flood-fill-based visual
    /// region detection). The defaults work for stock Adw themes;
    /// override when flood-fill over-grows (lower `tolerance`) or
    /// under-grows (raise `tolerance`). Has no effect unless one of
    /// the region APIs is actually called.
    pub visual_region_tuning: VisualRegionTuning,

    /// Per-session tuning knobs for OCR multi-line block grouping
    /// (the geometric clustering that decides when consecutive
    /// `TextLine`s belong to the same logical label). Defaults
    /// work for stock Adw themes; override when wrapped labels are
    /// being split across blocks (raise the gap factor) or
    /// unrelated rows are being merged (lower it).
    pub visual_text_tuning: VisualTextTuning,

    /// Per-session tuning knobs for the headless-mutter cold-start
    /// pointer-routing workaround applied by
    /// [`VisualLocator::click`](crate::VisualLocator::click) and
    /// [`RegionLocator::click`](crate::RegionLocator::click).
    /// Defaults work for stock headless mutter; disable the warmup
    /// or shorten the settle times on real hardware where the race
    /// doesn't apply.
    pub visual_click_tuning: VisualClickTuning,

    /// When `true` (the default), the app is launched against the session's
    /// private per-session GSettings keyfile store (see
    /// [`crate::gsettings`]) so it starts from default state and never reads
    /// or writes the host user's dconf, and so it picks up any settings the
    /// compositor seeded (e.g. `text-scaling-factor`). When `false`, the app
    /// inherits the host's normal GSettings. Must match the isolation mode the
    /// compositor was started with — both read the same keyfile dir.
    pub gsettings_isolated: bool,

    /// When `true` (the recommended default), the app gets private
    /// `XDG_STATE_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME` under the
    /// session runtime dir, so they vanish with the session. Without this,
    /// an app that persists state via `g_get_user_state_dir()` /
    /// `user_data_dir()` writes to the host's real `~/.local/state` /
    /// `~/.local/share` — polluting the developer's environment and letting
    /// one session's saved state (e.g. a restored-window session file) leak
    /// into every later session.
    ///
    /// Set `false` only for the rare flows that genuinely need the host's
    /// persisted app state (e.g. testing against a real profile). `HOME`
    /// itself is never touched (fontconfig and friends need it); use
    /// [`extra_env`](Self::extra_env) to override it when an app reads
    /// `$HOME` directly.
    pub xdg_isolated: bool,

    /// Extra environment variables for the spawned app, applied **last** —
    /// after the session's own env (Wayland display, D-Bus address, XDG
    /// dirs), so entries here override anything the session set. Lets a
    /// harness customize the app environment without process-global
    /// `std::env::set_var` (which is unsound with concurrent sessions and
    /// leaks into everything else the harness spawns).
    pub extra_env: Vec<(String, String)>,

    /// When `true`, stand up mock D-Bus services on the app's session bus that
    /// capture "external effects" the app emits there — desktop notifications
    /// (`org.freedesktop.Notifications`) and portal open-URI requests
    /// (`org.freedesktop.portal.Desktop` `OpenURI`) — so a test can assert on
    /// them via [`Session::notifications`] / [`Session::open_uri_requests`] (and
    /// the `wait_for_*` variants).
    ///
    /// Opt-in (default `false`): the sinks own well-known names, which is only
    /// safe when nothing else owns them. On the per-session / container bus
    /// that's always the case; on a shared host bus where a real notification
    /// daemon or portal already runs, the name claim no-ops with a warning and
    /// capture for that interface stays empty. Setup is best-effort — a failure
    /// never aborts an otherwise-ready session. See [`crate::sink`].
    pub capture_external_effects: bool,
}

/// Which colour-distance metric the visual locator uses when
/// comparing pixels. See the `crate::visual::color` module docs for
/// the full tradeoffs; the short version: [`Rgb`](Self::Rgb) is
/// cheap but non-perceptual, [`LabCie76`](Self::LabCie76) (default)
/// is cheap and roughly perceptual, [`LabCie2000`](Self::LabCie2000)
/// is the perceptual gold standard at ~5× the cost.
///
/// Exposed as a top-level type (rather than living inside the
/// feature-gated `visual` module) so it can sit in
/// [`VisualRegionTuning`] / [`VisualTextTuning`] fields without
/// forcing every consumer to enable the `visual` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorDistance {
    /// Raw RGB Euclidean squared distance — cheap, not perceptual.
    Rgb,
    /// CIE Lab ΔE\*76 squared — perceptual, cheap. The default.
    #[default]
    LabCie76,
    /// CIE Lab ΔE\*00 squared — perceptual gold standard, ~5×
    /// slower than `LabCie76`.
    LabCie2000,
}

/// Knobs for the visual region detection used by
/// [`Locator::find_regions`](crate::Locator::find_regions) /
/// [`first_region`](crate::Locator::first_region) /
/// [`last_region`](crate::Locator::last_region). Set on the session
/// at [`Session::start`] via [`SessionConfig::visual_region_tuning`].
#[derive(Debug, Clone, Copy)]
pub struct VisualRegionTuning {
    /// RGB Euclidean distance threshold for "same region". Default 24.
    /// Glyph antialiasing pixels typically jump 60+; subtle gradients
    /// within a button surface stay under 20.
    pub tolerance: u8,
    /// Hard cap on the iteration chain so a high-frequency banded
    /// image can't produce thousands of regions. Default 16; the
    /// realistic widget tree depth is usually 3–5.
    pub max_regions: usize,

    /// Squared RGB distance below which the seed-pick uniformity
    /// check considers a candidate seed and its 2px-out neighbour
    /// "uniform" (i.e. safe to flood from). Default 100 → ~10 RGB
    /// units of slack. Raise on noisy backgrounds, lower for very
    /// strict uniformity.
    pub seed_uniformity_threshold_sq: u32,

    /// Minimum fill-ratio (pixel_count / bbox_area) for a flood with
    /// all four corners inside to classify as
    /// [`Shape::Rectangle`](crate::Shape). Default 0.97 — perfect
    /// rectangles score 1.0; slight antialias fringe trims a small
    /// fraction off.
    pub shape_rectangle_min_ratio: f64,

    /// Minimum fill-ratio for a flood with 0–1 corners inside to
    /// classify as [`Shape::Pill`](crate::Shape). Default 0.82 —
    /// typical Adw rounded buttons land 0.94–0.99.
    pub shape_pill_min_ratio: f64,

    /// Half-open fill-ratio range `[lo, hi)` for a flood with zero
    /// corners inside to classify as [`Shape::Ellipse`](crate::Shape).
    /// Default `(0.65, 0.83)` — a true circle scores π/4 ≈ 0.785.
    pub shape_ellipse_ratio_range: (f64, f64),

    /// Which colour-distance metric the flood-fill and seed-pick use
    /// when comparing pixels.
    /// [`ColorDistance::LabCie76`](crate::ColorDistance) (default) is
    /// roughly perceptual at low cost; [`ColorDistance::Rgb`] matches
    /// the original raw-RGB behaviour; [`ColorDistance::LabCie2000`]
    /// is the gold-standard perceptual distance, ~5× slower.
    pub color_distance: ColorDistance,
}

impl Default for VisualRegionTuning {
    fn default() -> Self {
        Self {
            tolerance: 24,
            max_regions: 16,
            seed_uniformity_threshold_sq: 100,
            shape_rectangle_min_ratio: 0.97,
            shape_pill_min_ratio: 0.82,
            shape_ellipse_ratio_range: (0.65, 0.83),
            color_distance: ColorDistance::default(),
        }
    }
}

/// Knobs for OCR text-line grouping into multi-line blocks. Set on
/// the session at [`Session::start`] via
/// [`SessionConfig::visual_text_tuning`]. Two consecutive OCR lines
/// join the same block when they are vertically close (paragraph
/// spacing) and their x-ranges overlap; the thresholds here tune
/// what "close" and "overlap" mean.
#[derive(Debug, Clone, Copy)]
pub struct VisualTextTuning {
    /// Maximum vertical gap between consecutive OCR lines for them
    /// to count as paragraph-mates, expressed as a multiple of the
    /// upper line's height. Default `0.6` — lines whose top is
    /// within 60% of the previous line's height count as part of
    /// the same block.
    ///
    /// Increase for sparse, large-line-spacing UIs; decrease for
    /// dense layouts where unrelated rows sit close together.
    pub multiline_max_gap_factor: f32,

    /// Slack (in pixels) added to the x-overlap requirement when
    /// deciding whether two lines belong to the same block. With
    /// `0`, lines must have strictly overlapping x-ranges. With
    /// positive values, lines that almost-but-not-quite overlap
    /// (a centred line whose ends differ slightly from a left-
    /// aligned line above) still join. Default `4` — generous
    /// enough for typical font metrics, tight enough to keep
    /// separate columns apart.
    pub multiline_x_slack_px: i32,

    /// RGB Euclidean distance threshold for "same background
    /// colour" when deciding whether the gap between two candidate-
    /// merge lines crosses a visual boundary. Default 24, matching
    /// the region-tolerance default. Set to `u8::MAX` to disable
    /// the background-colour check entirely.
    pub background_color_tolerance: u8,

    /// When true (default), the grouper scans the gap between two
    /// candidate-merge lines for a horizontal or vertical divider
    /// stripe — a row or column of pixels whose colour differs
    /// from both surrounding backgrounds. A divider veto'es the
    /// merge. Disable on themes where shadow rasters or anti-
    /// aliased streaks trip the heuristic.
    pub divider_detection_enabled: bool,

    /// Padding (in pixels) added to all four sides of the crop fed
    /// to ocrs. The padding gives the recognition head visual
    /// context that disambiguates small/low-contrast glyphs — without
    /// it, e.g. `lazy-button` can read as `lazv-button`. Hits whose
    /// bbox falls entirely inside the padding ring are filtered out
    /// at the call site. Default 32. Set to 0 only when the caller
    /// is explicitly scoping to text known to read cleanly without
    /// context.
    pub ocr_context_padding_px: i32,

    /// Session-wide default integer factor the cropped image is upscaled by
    /// (Lanczos3) *before* it's fed to ocrs, with detected coordinates scaled
    /// back to screen space. Makes very small UI text (≈11px row titles that
    /// the detector can miss at native size) legible. Default `1` (no
    /// upscaling — no behavior change).
    ///
    /// This applies to *every* OCR search in the session. When only a specific
    /// label is too small, leave this at `1` and upscale just that search with
    /// [`VisualLocator::with_upscale`](crate::VisualLocator::with_upscale)
    /// instead — upscaling the full frame for every lookup is wasteful. Either
    /// way, set `2`/`3` when small text reads as 0 and verify with
    /// [`Session::recognized_text`]; the benefit is rendering-dependent (clean
    /// text already reads fine), so pair upscaling with a scoped `within(...)`
    /// crop.
    pub ocr_upscale_factor: u32,

    /// Sample density per axis for the boundary-detection divider
    /// scan. Each row in the gap between two candidate-merge OCR
    /// lines is sampled at this many evenly-spaced x positions
    /// (horizontal scan); each column gets this many y samples
    /// (vertical scan). Default 16 — increase on very wide gaps
    /// where a narrow divider could slip between sparse samples.
    pub boundary_samples_per_axis: usize,

    /// Fraction of samples that must be colour-different from BOTH
    /// surrounding backgrounds for a gap row or column to count as
    /// a divider. Default 0.8 — increase to require a more solid
    /// stripe, decrease to be more lenient with broken/dotted
    /// dividers.
    pub boundary_majority_threshold: f32,

    /// Radius (in pixels) of the square window averaged when
    /// computing the "background colour" at a sample position
    /// inside the boundary-detection check. A radius of `r`
    /// averages a `(2r+1) × (2r+1)` window. Default 2 (5×5
    /// window) — averaging smooths over single antialias-fringe
    /// pixels that would skew a single-pixel sample. Set to 0
    /// to fall back to a single-pixel sample.
    pub background_sample_radius: u32,

    /// Which colour-distance metric the boundary-detection check
    /// uses when comparing background samples and divider-scan
    /// pixels. [`ColorDistance::LabCie76`](crate::ColorDistance)
    /// (default) is roughly perceptual at low cost; switch to
    /// [`ColorDistance::Rgb`] to match the original raw-RGB
    /// behaviour, or [`ColorDistance::LabCie2000`] for the
    /// gold-standard perceptual distance (~5× slower).
    pub color_distance: ColorDistance,

    /// When true, the grouper runs a bounded flood-fill in the gap
    /// between two candidate-merge OCR lines and refuses to merge
    /// if the flood from the prev-line's background can't reach the
    /// next-line's background. Catches "two cards on the same bg
    /// colour, no visible divider, but each is boxed in by a thin
    /// border" cases that the colour and divider checks miss.
    /// Default `false` (off) — the most expensive of the boundary
    /// checks; the cheap checks cover the common cases.
    pub connectivity_check_enabled: bool,

    /// Maximum pixels the connectivity-check flood-fill will visit
    /// before giving up and vetoing the merge. Default 4096 — large
    /// enough that a typical gap-region fits, small enough that a
    /// pathological flood (seeded on the whole-screen background)
    /// terminates quickly. Only consulted when
    /// `connectivity_check_enabled == true`.
    pub max_connectivity_pixels: usize,
}

impl Default for VisualTextTuning {
    fn default() -> Self {
        Self {
            multiline_max_gap_factor: 0.6,
            multiline_x_slack_px: 4,
            background_color_tolerance: 24,
            divider_detection_enabled: true,
            ocr_context_padding_px: 32,
            ocr_upscale_factor: 1,
            boundary_samples_per_axis: 16,
            boundary_majority_threshold: 0.8,
            background_sample_radius: 2,
            color_distance: ColorDistance::default(),
            connectivity_check_enabled: false,
            max_connectivity_pixels: 4096,
        }
    }
}

/// Knobs for the headless-mutter cold-start pointer-routing
/// workaround applied by
/// [`VisualLocator::click`](crate::VisualLocator::click) and
/// [`RegionLocator::click`](crate::RegionLocator::click).
///
/// Headless mutter has a documented cold-start race: the first
/// `pointer_motion_absolute` after a fresh session start can be
/// delivered before the compositor has bound pointer focus to the
/// target surface, so a single motion+click pair silently does
/// nothing. The workaround sends a warmup motion to a point just
/// off the target, settles, motions to the target, settles, then
/// presses the button (with its own settle between down and up).
///
/// Defaults match what the visual-locator first shipped with —
/// known to defeat the race on stock headless mutter. On real
/// hardware where the race doesn't apply, disable the warmup
/// (`cold_start_warmup_enabled = false`) for a single
/// motion + button-press round trip.
#[derive(Debug, Clone, Copy)]
pub struct VisualClickTuning {
    /// When true (default), the warmup-motion-then-click sequence
    /// runs before every visual click. Set to false to fall through
    /// to a single motion + button-press, skipping all settles.
    pub cold_start_warmup_enabled: bool,
    /// Distance (in pixels) of the warmup motion from the target.
    /// Far enough that mutter sees an actual motion delta on the
    /// subsequent call, close enough that any pointer-enter event
    /// fires for the right widget hierarchy. Default 4.
    pub cold_start_warmup_offset_px: f64,
    /// Sleep after each `pointer_motion_absolute` call in the
    /// warmup sequence. Default 60ms — empirically reliable on
    /// headless mutter; tighten on faster hosts.
    pub cold_start_motion_settle: Duration,
    /// Sleep between `pointer_button_down` and `pointer_button_up`.
    /// Default 50ms — gives toolkits time to register a press as
    /// a click rather than a stuck button.
    pub cold_start_press_settle: Duration,
}

impl Default for VisualClickTuning {
    fn default() -> Self {
        Self {
            cold_start_warmup_enabled: true,
            cold_start_warmup_offset_px: 4.0,
            cold_start_motion_settle: Duration::from_millis(60),
            cold_start_press_settle: Duration::from_millis(50),
        }
    }
}

/// Buffer of lines emitted on the target app's stdout, with a Notify the
/// reader task pokes on every append so [`Session::wait_for_stdout_line`]
/// can wake and rescan.
#[derive(Default)]
struct AppStdout {
    lines: Mutex<Vec<String>>,
    notify: Notify,
}

/// Everything needed to relaunch the session's app as a *secondary* instance in
/// the same environment (same Wayland display, D-Bus bus, XDG dirs) so a
/// single-instance `GApplication` forwards the secondary's command line to the
/// already-running primary. Captured at [`Session::start`].
struct SecondaryLaunchSpec {
    command: String,
    env: Vec<(String, String)>,
    cwd: Option<String>,
}

/// Result of [`Session::launch_secondary`]: the secondary process's exit code
/// (`None` if it was killed by a signal) plus whatever it printed before
/// exiting. The *primary* instance's reaction to the forwarded command line is
/// observed separately, via [`Session::wait_for_stdout_line`].
#[derive(Debug, Clone)]
pub struct SecondaryInstance {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// How long [`Session::launch_secondary`] waits for the secondary to exit. A
/// single-instance app forwards its command line and exits near-instantly; this
/// only bounds a misbehaving binary.
const SECONDARY_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

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
    pub a11y_connection: Option<zbus::Connection>,
    /// Mock D-Bus sinks capturing the app's external effects (notifications,
    /// portal open-URI). `Some` only when [`SessionConfig::capture_external_effects`]
    /// was set and setup succeeded. This is a connection to the *app* (host /
    /// per-session) bus, independent of the compositor's private bus, so it is
    /// **not** part of the load-bearing shutdown ordering below — `kill` drops
    /// it up front to release the owned names.
    external_sinks: Option<crate::sink::ExternalSinks>,
    /// Captured spec for relaunching the app as a secondary instance in the
    /// same environment, used by [`Session::launch_secondary`] to exercise
    /// single-instance `GApplication` command-line forwarding.
    secondary_spec: SecondaryLaunchSpec,
    /// Default timeout (in nanoseconds) applied to auto-wait on Locator
    /// actions and explicit `wait_for_*` calls. Stored as AtomicU64 so
    /// [`set_default_timeout`] can mutate it behind an `Arc<Session>`
    /// without requiring interior-mutability gymnastics on every field.
    default_timeout_ns: AtomicU64,
    /// Whether this session runs against the isolated per-session GSettings
    /// keyfile store, copied from [`SessionConfig::gsettings_isolated`] at
    /// start. [`set_setting`](Self::set_setting) requires it: a live keyfile
    /// write only reaches the app through GIO's keyfile backend, never the
    /// host's shared dconf.
    gsettings_isolated: bool,
    /// Cooperative cancellation signal. Long-running auto-wait loops in
    /// [`Locator`] race this against their backoff sleep so a caller
    /// (typically `kill_session` in the MCP layer) can bail out of a
    /// stuck wait in milliseconds instead of waiting for the natural
    /// timeout. Cloning is cheap — internally an `Arc<AtomicBool>`.
    cancellation: CancellationToken,
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
    /// Dedicated ScreenCast stream backing [`video_recorder`], separate from
    /// `keepalive_stream`. The recorder must not share the keepalive node with
    /// the screenshot path: a recording pipeline is a continuous consumer, and
    /// on mutter's on-damage (`framerate=0/1`) screencast node a screenshot
    /// consumer that attaches after the recorder never receives a frame for a
    /// static app and times out. Its own node keeps the screenshot consumer
    /// the keepalive node's first/triggering consumer. `Some` only while
    /// recording; torn down in [`Session::kill`] right after the recorder is
    /// flushed.
    recorder_stream: Option<crate::backend::PipeWireStream>,
    /// Optional long-lived WebM recording that reads from `recorder_stream`.
    /// Declared after the streams so implicit drop order matches the explicit
    /// shutdown sequence in [`Session::kill`]: flush the recording before
    /// releasing the ScreenCast tokens.
    video_recorder: Option<VideoRecorder>,
    input: Box<dyn InputBackend>,
    capture: Box<dyn CaptureBackend>,
    compositor: Box<dyn CompositorRuntime>,
    /// Captured lines from the app process's stdout. A background task
    /// reads from the child pipe and pushes each line here, notifying
    /// waiters so they can rescan the buffer. Lines persist for the
    /// session's lifetime (no ring-buffer eviction yet).
    stdout: Arc<AppStdout>,
    /// Handle to the background stdout reader so [`Session::kill`]
    /// can abort it deterministically rather than waiting for the
    /// child's stdout pipe to close. That pipe stays open whenever
    /// a leaked grandchild has inherited it (browser launchers,
    /// electron preloads, anything that double-forks), which would
    /// otherwise pin the reader — and the `Arc<AppStdout>` it
    /// closes over — for the lifetime of the waydriver process.
    ///
    /// `Option` so `kill` can `.take()` and call `abort()` without
    /// leaving a stale handle behind; the reader also exits on its
    /// cancellation token, which is the cooperative path.
    stdout_reader: Option<JoinHandle<()>>,
    /// Lazily-initialized ocrs `OcrEngine`, shared across all
    /// [`VisualLocator`](crate::VisualLocator) calls in this session.
    /// First caller (either the prewarm task or
    /// [`Session::find_by_text`]) runs the init; concurrent callers
    /// await the same `OnceCell` rather than duplicating work. The
    /// stored value is a `Result` so a failed load short-circuits all
    /// subsequent OCR attempts with the original error rather than
    /// re-trying the load on every call.
    #[cfg(feature = "visual")]
    visual_engine: Arc<tokio::sync::OnceCell<crate::visual::EngineResult>>,
    /// Per-frame OCR memo: the last captured frame's hash plus the OCR result
    /// for each region scoped on it. Repeated `find_by_text` / asserts on an
    /// unchanged screen (and the auto-wait retry loop) reuse one OCR pass
    /// instead of re-running the ~tens-of-seconds pipeline. Invalidated
    /// wholesale when a new frame hash arrives.
    #[cfg(feature = "visual")]
    visual_ocr_cache: std::sync::Mutex<crate::visual::OcrCache>,
    /// Region-detection tuning copied from [`SessionConfig`] at
    /// session start. Read on every `Locator::find_regions` etc. call
    /// so a runtime setter (future) can re-tune without invalidating
    /// in-flight locators.
    pub(crate) visual_region_tuning: VisualRegionTuning,
    /// OCR block-grouping tuning copied from [`SessionConfig`] at
    /// session start. Read every time `Substring`-mode matching
    /// runs and every time `list_text` / `list_labelled_regions`
    /// builds blocks.
    pub(crate) visual_text_tuning: VisualTextTuning,
    /// Visual click tuning copied from [`SessionConfig`] at session
    /// start. Read by the cold-start warmup paths in
    /// `VisualLocator::click` and `RegionLocator::click`.
    pub(crate) visual_click_tuning: VisualClickTuning,
    /// Cached virtual-monitor pixel size, decoded lazily from the first
    /// screenshot. Constant for a session (the headless monitor never
    /// resizes), so it's memoised to keep [`window_origin`](Self::window_origin)
    /// from grabbing a frame on every pointer action.
    screen_size: std::sync::OnceLock<(i32, i32)>,
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
        // Build the app env once and reuse it for the captured secondary-launch
        // spec, so a secondary instance lands on the same bus / display / dirs.
        let app_env = app_env_pairs(
            &cfg,
            compositor.wayland_display(),
            compositor.runtime_dir(),
            &dbus_address,
        );
        let secondary_spec = SecondaryLaunchSpec {
            command: cfg.command.clone(),
            env: app_env.clone(),
            cwd: cfg.cwd.clone(),
        };
        let mut app = spawn_app(&cfg, &app_env)?;
        tracing::debug!(id, app_name = %cfg.app_name, "app spawned");

        let stdout = Arc::new(AppStdout::default());
        // Local cancellation token cloned into the reader task so
        // `Session::kill` can drop the task even if a leaked
        // grandchild keeps the child stdout pipe open after the app
        // exits. The same token is moved into the `Session` below.
        let cancellation = CancellationToken::new();
        let stdout_reader = app.stdout.take().map(|child_stdout| {
            let captured = stdout.clone();
            let id_for_task = id.clone();
            let cancel_for_task = cancellation.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(child_stdout).lines();
                loop {
                    tokio::select! {
                        // Cooperative exit. `Session::kill` cancels
                        // the token before aborting the join handle,
                        // so a well-behaved reader exits here without
                        // touching the abort path.
                        _ = cancel_for_task.cancelled() => break,
                        line = reader.next_line() => match line {
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
                }
            })
        });

        let a11y_connection = atspi_client::connect_a11y(&dbus_address).await?;
        let (app_bus_name, app_path) = wait_for_app(&a11y_connection, &cfg.app_name).await?;
        tracing::info!(id, app_name = %cfg.app_name, %app_bus_name, "session ready");

        // Optionally stand up the mock external-effect sinks on the app's
        // session bus. Best-effort: a failure here (bad address, registration)
        // must not fail an otherwise-ready session — capture simply stays off.
        let external_sinks = if cfg.capture_external_effects {
            match crate::sink::ExternalSinks::start(&dbus_address).await {
                Ok(sinks) => Some(sinks),
                Err(e) => {
                    tracing::warn!(
                        id,
                        error = %e,
                        "external-effect capture setup failed; continuing without it"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Start a keepalive ScreenCast stream. In headless mutter the
        // compositor only delivers Wayland frame callbacks while it is
        // actively compositing, and it only composites when a ScreenCast
        // consumer is pulling frames. Without this stream, GTK4 apps
        // render their first frame but never repaint because the frame
        // clock never ticks.
        let keepalive_stream = capture.start_stream().await?;

        // Prime mutter's keyboard-focus assignment. On a fresh headless
        // mutter session, the first `NotifyKeyboardKeysym` arrives
        // before mutter has bound keyboard focus to the newly-mapped
        // toplevel — the event is delivered to no client and silently
        // dropped, even though the AT-SPI bridge is already serving
        // queries against the app's tree. Subsequent keypresses land
        // because the first one wakes mutter into assigning focus.
        //
        // Pumping one priming press up-front absorbs that consumed-by-
        // focus-assignment event so the caller's first real keystroke
        // is reliably the second one mutter sees. `Shift_L` is chosen
        // deliberately: a bare modifier press generates no character,
        // doesn't trigger button/menu accelerators on any GTK widget,
        // and `press_keysym` always emits its matching release — so
        // there's no risk of leaving the modifier stuck down.
        //
        // Pointer events have the same cold-start race in headless
        // mutter — the `fixture_locator_pointer_actions` e2e test
        // documents the symptom — but no single priming pointer call
        // (motion, click, relative or absolute) reliably bootstraps
        // surface focus from a freshly-mapped toplevel. Tests that
        // depend on pointer events still need an ad-hoc warmup; this
        // session-startup prime covers the keyboard path only.
        //
        // Best-effort: a backend that returns an error here shouldn't
        // fail session startup, which is otherwise fully ready. The
        // `Locator` paths that don't depend on keyboard input
        // continue to work.
        //
        // See the `cold_first_keypress_lands_without_warmup` test in
        // `waydriver-e2e` for the bare repro.
        const SHIFT_L_KEYSYM: u32 = 0xffe1;
        if let Err(e) = input.press_keysym(SHIFT_L_KEYSYM, &cancellation).await {
            tracing::warn!(
                id,
                error = %e,
                "keyboard focus prime failed; first user keypress may be dropped"
            );
        }

        // If the caller requested a recording, open a *dedicated* ScreenCast
        // stream for it and run the encoder against that node — never the
        // keepalive node the screenshot path uses. Sharing the node makes the
        // recorder a continuous consumer that starves a later-attaching
        // screenshot consumer on mutter's on-damage stream (see
        // `recorder_stream` / `CaptureBackend::start_recording_stream`).
        // Failure here aborts session startup: the caller explicitly opted in,
        // so silently skipping would be surprising.
        let (recorder_stream, video_recorder) = if let Some(ref path) = cfg.video_output {
            let bitrate = cfg
                .video_bitrate
                .unwrap_or(crate::capture::DEFAULT_VIDEO_BITRATE);
            let fps = cfg.video_fps.unwrap_or(crate::capture::DEFAULT_VIDEO_FPS);
            let stream = capture.start_recording_stream().await?;
            let recorder = capture.start_recording(&stream, path, bitrate, fps).await?;
            (Some(stream), Some(recorder))
        } else {
            (None, None)
        };

        #[cfg(feature = "visual")]
        let visual_engine = Arc::new(tokio::sync::OnceCell::new());

        // Kick off the OCR engine load in the background if the caller
        // opted in via `prewarm_visual`. The future is fire-and-forget;
        // its result lands in `visual_engine` and the on-demand path in
        // `find_by_text` reuses it. If the prewarm hasn't completed by
        // the time `find_by_text` is called, the call awaits the same
        // `OnceCell` initializer rather than starting a second load.
        #[cfg(feature = "visual")]
        if cfg.prewarm_visual {
            let cell = visual_engine.clone();
            tokio::spawn(async move {
                let _ = cell.get_or_init(crate::visual::ensure_engine).await;
            });
        }

        let session = Session {
            id,
            app_name: cfg.app_name,
            app_bus_name,
            app_path,
            a11y_connection: Some(a11y_connection),
            external_sinks,
            secondary_spec,
            default_timeout_ns: AtomicU64::new(resolve_default_timeout().as_nanos() as u64),
            gsettings_isolated: cfg.gsettings_isolated,
            cancellation,
            app,
            keepalive_stream: Some(keepalive_stream),
            recorder_stream,
            video_recorder,
            input,
            capture,
            compositor,
            stdout,
            stdout_reader,
            #[cfg(feature = "visual")]
            visual_engine,
            #[cfg(feature = "visual")]
            visual_ocr_cache: std::sync::Mutex::new(crate::visual::OcrCache::default()),
            visual_region_tuning: cfg.visual_region_tuning,
            visual_text_tuning: cfg.visual_text_tuning,
            visual_click_tuning: cfg.visual_click_tuning,
            screen_size: std::sync::OnceLock::new(),
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
        let id = self.id.clone();
        tracing::info!(id = %id, "killing session");

        // Cancel the token *before* arming the outer timeout so any
        // tool currently inside `poll_with_retry` short-circuits at
        // its next iteration rather than racing the kill budget.
        self.cancellation.cancel();

        // Bound the whole shutdown sequence so a wedged D-Bus call
        // (compositor stop, recording flush) or a child stuck in
        // uninterruptible state can't pin the caller indefinitely.
        // Past KILL_TIMEOUT we surface Error::Timeout; the in-flight
        // futures are dropped, which for tokio process / D-Bus
        // primitives means cancellation rather than detached work.
        let inner = async move {
            // Release the mock-sink bus names/objects up front. They live on the
            // app's session bus, independent of the compositor's private bus, so
            // this has no ordering relationship to the steps below.
            let _ = self.external_sinks.take();

            if let Some(handle) = self.stdout_reader.take() {
                // Cooperative path runs first via the token; the
                // abort here is a hard fallback for the case where
                // the reader is wedged inside a syscall that doesn't
                // observe the select.
                handle.abort();
                let _ = handle.await;
            }

            let _ = self.app.kill().await;
            let _ = self.app.wait().await;

            // Finalize the recording before tearing down its ScreenCast
            // stream so the muxer still has a live PipeWire node to
            // flush through. Errors are logged but don't block teardown.
            if let Some(recorder) = self.video_recorder.take() {
                if let Err(e) = self.capture.stop_recording(recorder).await {
                    tracing::warn!(error = %e, "stop_recording failed");
                }
            }

            // Tear down the recorder's dedicated stream now that the encoder
            // has flushed. Done before the keepalive stream for symmetry; the
            // two are independent ScreenCast sessions so order is not
            // load-bearing between them.
            if let Some(stream) = self.recorder_stream.take() {
                let _ = self.capture.stop_recording_stream(stream).await;
            }

            // Stop the keepalive ScreenCast stream before dropping backends.
            if let Some(stream) = self.keepalive_stream.take() {
                let _ = self.capture.stop_stream(stream).await;
            }

            self.compositor.stop().await?;

            // self drops here: Drop sees an already-dead app and
            // already-stopped compositor, then input/capture release
            // their Arc refs harmlessly.
            Result::<()>::Ok(())
        };

        match tokio::time::timeout(KILL_TIMEOUT, inner).await {
            Ok(res) => res,
            Err(_) => {
                tracing::warn!(
                    id = %id,
                    timeout_ms = KILL_TIMEOUT.as_millis(),
                    "kill exceeded budget; abandoning shutdown"
                );
                Err(Error::Timeout(format!(
                    "session {id} kill exceeded {}s budget",
                    KILL_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Send a key press + release for the given X11 keysym.
    pub async fn press_keysym(&self, keysym: u32) -> Result<()> {
        self.input.press_keysym(keysym, &self.cancellation).await
    }

    /// Press a chord like `"Ctrl+Shift+A"` — modifiers are held in order,
    /// the target key is pressed and released, then modifiers are released
    /// in reverse order.
    ///
    /// Accepts single key names (`"Return"`, `"a"`) as chords with no
    /// modifiers. See [`crate::keysym::parse_chord`] for the full grammar.
    /// Returns an error if the chord can't be parsed.
    pub async fn press_chord(&self, chord: &str) -> Result<()> {
        // Pre-flight cancellation check: if kill fired before we started,
        // bail without pressing anything. Checks *inside* the modifier
        // loop would leave keys stuck down — the existing unwind always
        // runs so any modifiers already pressed get released cleanly.
        if self.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let parsed = crate::keysym::parse_chord(chord)
            .ok_or_else(|| Error::process(format!("invalid chord: {chord:?}")))?;
        // Press all modifiers in order.
        for m in &parsed.modifiers {
            self.input.key_down(*m, &self.cancellation).await?;
        }
        // Press + release the target key while modifiers are held.
        let target_result = self
            .input
            .press_keysym(parsed.key, &self.cancellation)
            .await;
        // Release modifiers in reverse order, even if the target press
        // failed — leaving modifiers stuck down would break subsequent
        // keyboard input.
        for m in parsed.modifiers.iter().rev() {
            if let Err(e) = self.input.key_up(*m, &self.cancellation).await {
                tracing::warn!(error = %e, keysym = m, "key_up failed during chord unwind");
            }
        }
        target_result
    }

    /// Press a key and hold it down until a matching [`key_up`](Self::key_up)
    /// fires. Unlike [`press_keysym`](Self::press_keysym) (which presses *and*
    /// releases) this leaves the key held, so a caller can bracket another
    /// input event with a modifier — hold `Ctrl`, scroll the wheel, then
    /// release `Ctrl` for Ctrl+scroll-to-zoom; hold `Shift` across a click for
    /// range-select; and similar held-modifier pointer gestures.
    ///
    /// The caller owns the release: a key left down by `key_down` stays held
    /// in the compositor until a matching `key_up`. When the whole press is a
    /// modifier-name chord around a single keystroke, prefer
    /// [`press_chord`](Self::press_chord), which releases for you (and unwinds
    /// even on error). `keysym` is an X11 keysym; modifier keysyms come from
    /// [`crate::keysym::modifier_name_to_keysym`] (e.g. `"Ctrl"` → `0xffe3`).
    pub async fn key_down(&self, keysym: u32) -> Result<()> {
        self.input.key_down(keysym, &self.cancellation).await
    }

    /// Release a key previously pressed with [`key_down`](Self::key_down).
    /// Safe to call on a key that isn't held — the backend tolerates a stray
    /// release rather than erroring.
    pub async fn key_up(&self, keysym: u32) -> Result<()> {
        self.input.key_up(keysym, &self.cancellation).await
    }

    /// Move the pointer by a relative offset in logical pixels.
    pub async fn pointer_motion_relative(&self, dx: f64, dy: f64) -> Result<()> {
        self.input
            .pointer_motion_relative(dx, dy, &self.cancellation)
            .await
    }

    /// Move the pointer to a screen-relative absolute position in logical
    /// pixels. Requires an active capture stream on backends that route
    /// through the compositor's ScreenCast pipeline (mutter).
    pub async fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<()> {
        self.input
            .pointer_motion_absolute(x, y, &self.cancellation)
            .await
    }

    /// Press and release a pointer button.
    pub async fn pointer_button(&self, button: PointerButton) -> Result<()> {
        self.input.pointer_button(button, &self.cancellation).await
    }

    /// Hold a pointer button down until a matching [`pointer_button_up`](Self::pointer_button_up)
    /// fires. Used to build drag gestures — press, move across intermediate
    /// coordinates, then release.
    pub async fn pointer_button_down(&self, button: PointerButton) -> Result<()> {
        self.input
            .pointer_button_down(button, &self.cancellation)
            .await
    }

    /// Release a pointer button previously pressed with
    /// [`pointer_button_down`](Self::pointer_button_down).
    pub async fn pointer_button_up(&self, button: PointerButton) -> Result<()> {
        self.input
            .pointer_button_up(button, &self.cancellation)
            .await
    }

    /// Type a string as keyboard input, one X11 keysym per `char`. Latin-1
    /// characters map directly; other Unicode uses the `0x01000000 + codepoint`
    /// encoding (see [`crate::keysym::char_to_keysym`]). Does not manage
    /// focus — call [`crate::Locator::focus`] or click the target widget
    /// first.
    ///
    /// Observes the session's cancellation token between characters so a
    /// long typed string bails promptly on `kill_session` instead of
    /// typing every remaining character before noticing. Cancellation
    /// latency is capped at one keystroke (~50ms backend-internal
    /// sleep); mid-keystroke cancel would require plumbing the token
    /// through the [`InputBackend`](crate::backend::InputBackend) trait.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if self.cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.press_keysym(crate::keysym::char_to_keysym(ch)).await?;
        }
        Ok(())
    }

    /// Emit a discrete pointer-axis (wheel) event. `axis` selects
    /// vertical or horizontal; `steps` is the number of wheel detents
    /// — positive scrolls down/right, negative scrolls up/left.
    pub async fn pointer_axis_discrete(&self, axis: PointerAxis, steps: i32) -> Result<()> {
        self.input
            .pointer_axis_discrete(axis, steps, &self.cancellation)
            .await
    }

    /// Change a GSettings key on the **already-running** app, live.
    ///
    /// Rewrites the session's isolated keyfile in place (read-modify-write,
    /// preserving every other key — including the compositor's seeds). GIO's
    /// keyfile backend watches that file, so the app re-emits its GSettings
    /// `changed` signal and re-applies the value without a restart: cursor
    /// theme, font scaling, color scheme, bell, scrollback, and the like
    /// update live. Where seeding via [`SessionConfig`] only sets a key
    /// *before* launch, this flips it *after*, exercising the app's live
    /// change-handler. `value` is GVariant text form — the same syntax
    /// `gsettings set` and the launch seeds use (numbers bare, strings
    /// single-quoted, arrays bracketed).
    ///
    /// The app observes the change **asynchronously**: there's no
    /// acknowledgement that it has re-applied. Drive the assertion off the
    /// resulting effect — a [`Locator`](crate::Locator) auto-wait on the
    /// changed UI, or [`wait_for_stdout_line`](Self::wait_for_stdout_line) when
    /// the app logs its handler.
    ///
    /// Requires GSettings isolation ([`SessionConfig::gsettings_isolated`], the
    /// default). Without it the session reads the host's dconf, which this
    /// write deliberately never touches — so it returns [`Error::Process`]
    /// rather than silently no-op. Also returns [`Error::Process`] if the
    /// keyfile rewrite fails.
    pub async fn set_setting(&self, schema: &str, key: &str, value: &str) -> Result<()> {
        if !self.gsettings_isolated {
            return Err(Error::process(
                "set_setting requires gsettings isolation; start with \
                 SessionConfig.gsettings_isolated = true",
            ));
        }
        let entry = crate::gsettings::GSettingEntry::new(schema, key, value);
        crate::gsettings::live_write(self.compositor.runtime_dir(), &entry)
            .map_err(|e| Error::process_with("set_setting: rewrite keyfile", e))
    }

    // ── External-effect capture (notifications / portal open-URI) ──────────

    /// Whether external-effect capture is active for this session — i.e.
    /// [`SessionConfig::capture_external_effects`] was set and the mock sinks
    /// were started. The readback methods below error when this is `false`.
    pub fn external_effects_enabled(&self) -> bool {
        self.external_sinks.is_some()
    }

    fn sinks(&self) -> Result<&crate::sink::ExternalSinks> {
        self.external_sinks.as_ref().ok_or_else(|| {
            Error::process(
                "external-effect capture is not enabled for this session; start it with \
                 SessionConfig.capture_external_effects = true",
            )
        })
    }

    /// Snapshot of every desktop notification (`org.freedesktop.Notifications.Notify`)
    /// the app has posted so far. Requires [`SessionConfig::capture_external_effects`].
    pub fn notifications(&self) -> Result<Vec<crate::sink::CapturedNotification>> {
        Ok(self.sinks()?.notifications())
    }

    /// Snapshot of every portal open-URI request the app has made so far.
    /// Requires [`SessionConfig::capture_external_effects`].
    pub fn open_uri_requests(&self) -> Result<Vec<crate::sink::CapturedOpenUri>> {
        Ok(self.sinks()?.open_uri_requests())
    }

    /// Current notification-log length — a high-water mark to pass as `after`
    /// to [`wait_for_notification`](Self::wait_for_notification) so a wait only
    /// matches notifications posted after this point.
    pub fn notification_cursor(&self) -> Result<usize> {
        Ok(self.sinks()?.notification_count())
    }

    /// Current open-URI-log length — a high-water mark for
    /// [`wait_for_open_uri`](Self::wait_for_open_uri).
    pub fn open_uri_cursor(&self) -> Result<usize> {
        Ok(self.sinks()?.open_uri_count())
    }

    /// Wait for a captured notification at or after index `after` matching
    /// `pred`. Returns it on success, [`Error::Timeout`] if none arrives in
    /// time, or [`Error::Cancelled`] if the session is killed while waiting.
    pub async fn wait_for_notification<F>(
        &self,
        after: usize,
        pred: F,
        timeout: Duration,
    ) -> Result<crate::sink::CapturedNotification>
    where
        F: Fn(&crate::sink::CapturedNotification) -> bool,
    {
        self.sinks()?
            .wait_for_notification(after, pred, timeout, &self.cancellation)
            .await
    }

    /// Wait for a captured open-URI request at or after index `after` matching
    /// `pred`. Same outcome semantics as
    /// [`wait_for_notification`](Self::wait_for_notification).
    pub async fn wait_for_open_uri<F>(
        &self,
        after: usize,
        pred: F,
        timeout: Duration,
    ) -> Result<crate::sink::CapturedOpenUri>
    where
        F: Fn(&crate::sink::CapturedOpenUri) -> bool,
    {
        self.sinks()?
            .wait_for_open_uri(after, pred, timeout, &self.cancellation)
            .await
    }

    // ── Single-instance CLI forwarding ────────────────────────────────────

    /// Relaunch the session's app as a **secondary instance** with `args`, in
    /// the same environment as the primary (same Wayland display, D-Bus bus, XDG
    /// dirs). For a single-instance `GApplication`, the secondary detects the
    /// already-running primary on the session bus and **forwards** its command
    /// line to it instead of opening a new window, then exits.
    ///
    /// Returns the secondary process's own exit/output (usually exit 0, empty
    /// output). Observe what the *primary* did with the forwarded command line
    /// via [`wait_for_stdout_line`](Self::wait_for_stdout_line) (or the AT-SPI
    /// tree) — that's where the effect lands.
    ///
    /// Bounded by an internal timeout; a secondary that doesn't exit (e.g. it
    /// became the primary because none was running) surfaces as
    /// [`Error::Timeout`].
    pub async fn launch_secondary(&self, args: Vec<String>) -> Result<SecondaryInstance> {
        self.launch_secondary_with_timeout(args, SECONDARY_LAUNCH_TIMEOUT)
            .await
    }

    /// [`launch_secondary`](Self::launch_secondary) with an explicit exit
    /// timeout.
    pub async fn launch_secondary_with_timeout(
        &self,
        args: Vec<String>,
        timeout: Duration,
    ) -> Result<SecondaryInstance> {
        if self.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let spec = &self.secondary_spec;
        let mut cmd = Command::new(&spec.command);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        set_pdeathsig(&mut cmd);

        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| {
                Error::Timeout(format!(
                    "secondary instance '{}' did not exit within {timeout:?}",
                    spec.command
                ))
            })?
            .map_err(|e| Error::process_with(format!("launch secondary '{}'", spec.command), e))?;

        Ok(SecondaryInstance {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Pointer "click" using the cold-start warmup recipe shared by the
    /// visual locator and [`Locator::pointer_click`](crate::Locator::pointer_click).
    /// `cx`/`cy` are **screen-absolute** logical pixels. When the warmup is
    /// enabled (see [`VisualClickTuning`]) it sends an approach motion +
    /// settle — so the first motion after a fresh session binds pointer focus
    /// before the press — then a separate press/settle/release of `button`.
    /// With the warmup disabled it falls through to a single motion + click.
    pub(crate) async fn cold_start_click(
        &self,
        cx: f64,
        cy: f64,
        button: PointerButton,
    ) -> Result<()> {
        let t = self.visual_click_tuning;
        if t.cold_start_warmup_enabled {
            let warmup_x = (cx - t.cold_start_warmup_offset_px).max(0.0);
            let warmup_y = (cy - t.cold_start_warmup_offset_px).max(0.0);
            self.pointer_motion_absolute(warmup_x, warmup_y).await?;
            tokio::time::sleep(t.cold_start_motion_settle).await;
            self.pointer_motion_absolute(cx, cy).await?;
            tokio::time::sleep(t.cold_start_motion_settle).await;
            self.pointer_button_down(button).await?;
            tokio::time::sleep(t.cold_start_press_settle).await;
            self.pointer_button_up(button).await?;
        } else {
            self.pointer_motion_absolute(cx, cy).await?;
            self.pointer_button(button).await?;
        }
        Ok(())
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
            .ok_or_else(|| Error::screenshot("no keepalive stream"))?;
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

    /// Cancellation token observed by long-running auto-wait loops in
    /// [`Locator`]. Returned as a reference because the internal handle
    /// is already cheap to clone (`Arc<AtomicBool>` under the hood);
    /// callers that need to stash a copy can call `.clone()` on the
    /// returned ref.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Trigger the session's cancellation token. Idempotent — cancelling
    /// an already-cancelled token is a no-op. After calling this, any
    /// in-flight auto-wait will resolve promptly with [`Error::Cancelled`]
    /// so the caller can shut the session down cleanly.
    pub fn cancel(&self) {
        self.cancellation.cancel();
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
    /// `after` in the buffer. Returns the matched line on success,
    /// `Error::Timeout` if no matching line arrives before the deadline,
    /// or `Error::Cancelled` if the session's cancellation token trips
    /// while waiting (typically because `kill_session` fired).
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
            // Race three things: the `Notified` future (new line appended),
            // the deadline (via tokio::time::sleep), and the session's
            // cancellation token. A raced cancel surfaces as
            // `Error::Cancelled` so callers can distinguish "kill fired"
            // from "deadline elapsed without a match."
            tokio::select! {
                _ = &mut notified => {
                    // Woken by a new line; loop and re-scan.
                }
                _ = tokio::time::sleep(remaining) => {
                    return Err(Error::Timeout(format!(
                        "no stdout line matched within {timeout:?} (buffer had {} line(s) after cursor {after})",
                        self.stdout.lines.lock().unwrap().len().saturating_sub(after),
                    )));
                }
                _ = self.cancellation.cancelled() => {
                    return Err(Error::Cancelled);
                }
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
            .ok_or_else(|| Error::atspi("session has no AT-SPI connection"))?;
        atspi_client::snapshot_tree(a11y, &self.app_bus_name, &self.app_path).await
    }

    /// Walk keyboard focus through the app by pressing **Tab** `steps`
    /// times (with a short settle between presses).
    ///
    /// The point is the side effect: focusing a widget force-realizes its
    /// AT-SPI context (GTK's focus path realizes unconditionally), which is
    /// the only client-side trigger that surfaces GTK4/libadwaita's
    /// lazily-realized widgets — content revealed after the toplevel was
    /// presented (hidden→shown `AdwPreferencesGroup`, non-initial
    /// `AdwPreferencesDialog` pages) that never enters the `GetChildren`
    /// tree. After a walk, the realized widgets and their ancestor chains
    /// are readable via [`hidden_accessibles`](Self::hidden_accessibles).
    ///
    /// **Destructive:** this moves real keyboard focus. Focus wraps within
    /// the active window's Tab cycle, so a `steps` larger than the number
    /// of focusable widgets simply loops — covering every reachable widget.
    pub async fn focus_walk(&self, steps: u32) -> Result<()> {
        for _ in 0..steps {
            if self.cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.press_chord("Tab").await?;
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        // One extra beat so the last focus event's realization lands in the
        // app cache before the caller re-reads it.
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(())
    }

    /// Read the application's AT-SPI cache (`Cache.GetItems`) — every
    /// accessible whose context the toolkit has realized, including ones
    /// the `GetChildren` tree (and therefore [`dump_tree`](Self::dump_tree)
    /// / XPath locators) cannot reach.
    pub async fn cached_accessibles(&self) -> Result<Vec<atspi_client::CachedAccessible>> {
        let a11y = self
            .a11y_connection
            .as_ref()
            .ok_or_else(|| Error::atspi("session has no AT-SPI connection"))?;
        atspi_client::cache_items(a11y, &self.app_bus_name).await
    }

    /// The accessibles that exist in the app's AT-SPI cache but are
    /// **missing from the snapshot tree** — i.e. widgets XPath locators
    /// cannot see. For GTK4/libadwaita lazily-realized content this is the
    /// only AT-SPI read path: run [`focus_walk`](Self::focus_walk) first to
    /// realize the hidden widgets, then call this to discover and inspect
    /// them (role, name, states, `(bus, path)` reference).
    ///
    /// Returns an empty list when the cache holds nothing beyond the tree —
    /// the healthy case. Note the limits established for these widgets:
    /// their `Component` bounds are unreliable and they expose no `Action`,
    /// so use this for discovery/assertions, and actuate via keyboard
    /// (focus + Space/Enter) or the OCR visual locator.
    pub async fn hidden_accessibles(&self) -> Result<Vec<atspi_client::CachedAccessible>> {
        let a11y = self
            .a11y_connection
            .as_ref()
            .ok_or_else(|| Error::atspi("session has no AT-SPI connection"))?;
        let xml = atspi_client::snapshot_tree(a11y, &self.app_bus_name, &self.app_path).await?;
        let tree_paths: std::collections::HashSet<String> =
            atspi_client::evaluate_xpath_detailed(&xml, "//*")?
                .into_iter()
                .map(|e| e.ref_.1)
                .collect();
        let cached = atspi_client::cache_items(a11y, &self.app_bus_name).await?;
        Ok(cached
            .into_iter()
            .filter(|c| !tree_paths.contains(&c.ref_.1))
            .collect())
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

    /// Translate a *window-relative* AT-SPI rectangle (as returned by
    /// [`Locator::bounds`](crate::Locator::bounds)) into the *screen-absolute*
    /// pixel space the pointer API
    /// ([`pointer_motion_absolute`](Self::pointer_motion_absolute)) consumes.
    ///
    /// AT-SPI extents under headless mutter are window-relative — `atspi.rs`
    /// reads `CoordType::Window` because mutter reports `CoordType::Screen`
    /// as `(0, 0)` for every widget. Feeding `bounds()` straight to the
    /// pointer therefore misses the widget by the toplevel's on-screen
    /// origin. Mutter centers the single toplevel on the virtual monitor, so
    /// that origin is derived as `((screen − window-content) / 2)` (verified
    /// to within 1px). [`Locator::pointer_click`](crate::Locator::pointer_click),
    /// `hover`, `double_click`, `right_click`, and `drag_to` all route through
    /// this, so callers rarely need it directly — reach for it when you read a
    /// rect via `bounds()` and want to drive the pointer there yourself.
    ///
    /// **Caveat:** assumes mutter's single centered toplevel. Widgets inside a
    /// *separate* OS-level dialog window (rare in modern libadwaita, which
    /// renders dialogs in-window) center against their own window, so the
    /// main-window origin computed here won't match — use the visual locator
    /// ([`Session::find_by_text`](Self::find_by_text)), which works in screen
    /// space, for those.
    pub async fn to_screen_bounds(
        self: &Arc<Self>,
        window_rel: crate::atspi::Rect,
    ) -> Result<crate::atspi::Rect> {
        let (ox, oy) = self.window_origin().await?;
        Ok(crate::atspi::Rect {
            x: window_rel.x + ox,
            y: window_rel.y + oy,
            width: window_rel.width,
            height: window_rel.height,
        })
    }

    /// On-screen origin `(x, y)` of the app's toplevel under headless mutter:
    /// the offset that maps a window-relative AT-SPI rect to screen pixels.
    /// See [`to_screen_bounds`](Self::to_screen_bounds) for the derivation
    /// and caveats.
    pub async fn window_origin(self: &Arc<Self>) -> Result<(i32, i32)> {
        let (sw, sh) = self.screen_size().await?;
        let els = self.locate("//*").inspect_all().await?;
        centered_window_origin(els.into_iter().filter_map(|e| e.bounds), sw, sh).ok_or_else(|| {
            Error::atspi(
                "window_origin: AT-SPI tree has no on-screen-sized elements to size the toplevel"
                    .to_string(),
            )
        })
    }

    /// Virtual-monitor pixel size, memoised. Decoded from a screenshot the
    /// first time so it's correct regardless of the configured resolution.
    async fn screen_size(self: &Arc<Self>) -> Result<(i32, i32)> {
        if let Some(sz) = self.screen_size.get() {
            return Ok(*sz);
        }
        let shot = self.take_screenshot().await?;
        let img = crate::locator::decode_screenshot_png(&shot)?;
        let sz = (img.width() as i32, img.height() as i32);
        let _ = self.screen_size.set(sz);
        Ok(sz)
    }

    /// Locator matching any element whose accessible name equals `name`.
    pub fn find_by_name(self: &Arc<Self>, name: &str) -> Locator {
        self.locate(&find_by_name_xpath(name))
    }

    /// OCR-backed visual locator. Use when a widget is drawn on screen
    /// but absent from the AT-SPI tree (libadwaita's hidden-then-shown
    /// `PreferencesGroup` inside an `AdwPreferencesPage` is the
    /// motivating example) — *not* as a general fallback when an
    /// XPath-based locator doesn't match.
    ///
    /// First call in a session is expensive (model load + inference,
    /// hundreds of milliseconds to a few seconds). See
    /// [`SessionConfig::prewarm_visual`] to move the load off the test's
    /// critical path, and the [`crate::visual`] module docs for full
    /// cost expectations.
    #[cfg(feature = "visual")]
    pub fn find_by_text(self: &Arc<Self>, text: &str) -> crate::visual::VisualLocator {
        crate::visual::VisualLocator::new(self.clone(), text)
    }

    /// OCR the entire current frame and return every recognised text block —
    /// its text plus screen-coordinate bounds — in reading order.
    ///
    /// Where [`find_by_text`](Self::find_by_text) searches for one string,
    /// this dumps *everything* OCR saw. It's the diagnostic for a
    /// `find_by_text` that returned 0: you can see whether the target was
    /// mis-recognised (e.g. "Cursor" read as "Cursar"), folded into a
    /// different block, or never detected — instead of a bare 0 that's
    /// indistinguishable from "not on screen."
    ///
    /// Runs a full-frame OCR pass, so it costs as much as one *unscoped*
    /// `find_by_text`. Requires the `visual` Cargo feature. (No confidence
    /// score — the underlying `ocrs` engine doesn't expose one.)
    #[cfg(feature = "visual")]
    pub async fn recognized_text(self: &Arc<Self>) -> Result<Vec<crate::visual::TextHit>> {
        let png = self.take_screenshot().await?;
        crate::visual::__recognized_text(self, png).await
    }

    /// Find a reference image inside the current screen via classical
    /// normalized cross-correlation (template matching). Returns an
    /// [`ImageLocator`](crate::visual::ImageLocator); call `.click()`,
    /// `.bounds()`, etc. on it.
    ///
    /// Use this for icon-only buttons that have no on-screen text
    /// (the OCR-based [`find_by_text`](Self::find_by_text) won't find
    /// them). The `png_bytes` are the contents of a reference PNG
    /// captured against a screenshot of the same app — same DPI,
    /// theme, antialias settings — committed alongside the test.
    ///
    /// Template matching is brittle: a theme swap or DPI change can
    /// invalidate the reference. When AT-SPI exposes the widget, or
    /// it has searchable text, prefer those paths.
    ///
    /// Requires the `visual` Cargo feature. The PNG is decoded once
    /// at construction time and the screenshot is taken anew on each
    /// terminal-method call, so the locator survives between calls.
    #[cfg(feature = "visual")]
    pub fn find_image(self: &Arc<Self>, png_bytes: &[u8]) -> Result<crate::visual::ImageLocator> {
        crate::visual::ImageLocator::new(self.clone(), png_bytes, None)
    }

    /// Perceptual diff of two PNG buffers — a captured crop against a
    /// committed reference — returning a [`BaselineComparison`] score.
    ///
    /// This is a **data primitive, not an assertion**: it never errors
    /// on a visual mismatch (that's reported via
    /// [`BaselineComparison::matched`] / `score`); it errors only on a
    /// decode failure or a dimension mismatch. Storing reference images,
    /// choosing a tolerance, and deciding pass/fail are the caller's
    /// job — waydriver is not a test framework.
    ///
    /// See [`crate::visual::compare_to_baseline`] for the scoring model.
    /// The work is CPU-bound and synchronous; wrap it in
    /// `tokio::task::spawn_blocking` when comparing large crops from an
    /// async context. [`crate::Locator::compare_to_baseline`] is the
    /// element-scoped counterpart that captures the crop for you.
    ///
    /// Requires the `visual` Cargo feature.
    #[cfg(feature = "visual")]
    pub fn compare_to_baseline(
        &self,
        actual_png: &[u8],
        baseline_png: &[u8],
        tolerance: f64,
    ) -> Result<crate::visual::BaselineComparison> {
        crate::visual::compare_to_baseline(actual_png, baseline_png, tolerance)
    }

    /// Find the visual region containing the screen pixel `(x, y)`.
    ///
    /// Lowest-level entry point in the visual stack — no OCR, no
    /// AT-SPI parent, just a flood-fill from the supplied pixel.
    /// Useful when you already have coordinates (from a debugger,
    /// a prior screenshot, a hard-coded layout assumption) and want
    /// to address the widget at those coordinates rather than the
    /// exact pixel.
    ///
    /// The pixel doesn't need to be the centre of the target region:
    /// flood-fill is a BFS that recovers the same bbox / centroid /
    /// shape from any starting point inside the region. The
    /// constraint is just that `(x, y)` lands inside the region you
    /// want — anywhere on a button's fill is fine, but a pixel on a
    /// text glyph or on the gap between widgets gives you that
    /// glyph's region or the gap's background.
    ///
    /// Requires the `visual` Cargo feature. Tolerance comes from
    /// [`SessionConfig::visual_region_tuning`].
    #[cfg(feature = "visual")]
    pub async fn region_at(
        self: &Arc<Self>,
        x: i32,
        y: i32,
    ) -> Result<crate::visual::RegionLocator> {
        let png = self.take_screenshot().await?;
        crate::visual::__region_at_seed(self, (x, y), &png, self.visual_region_tuning)
    }

    /// Internal accessor for the shared ocrs engine cell. Used by
    /// [`VisualLocator`](crate::VisualLocator) to fetch (and lazily
    /// initialize) the engine.
    #[cfg(feature = "visual")]
    pub(crate) fn visual_engine(&self) -> &Arc<tokio::sync::OnceCell<crate::visual::EngineResult>> {
        &self.visual_engine
    }

    /// The per-frame OCR memo (see the field docs). Used by
    /// `crate::visual::ocr_lines` to skip re-OCRing an unchanged frame.
    #[cfg(feature = "visual")]
    pub(crate) fn visual_ocr_cache(&self) -> &std::sync::Mutex<crate::visual::OcrCache> {
        &self.visual_ocr_cache
    }

    /// Locator matching an element by PascalCase role and accessible name.
    /// For example, `find_by_role_name("PushButton", "OK")` compiles to
    /// `//PushButton[@name='OK']`.
    pub fn find_by_role_name(self: &Arc<Self>, role: &str, name: &str) -> Locator {
        self.locate(&find_by_role_name_xpath(role, name))
    }
}

/// Center-derive the toplevel's on-screen origin `(x, y)` from the
/// window-relative AT-SPI bounds in the tree and the monitor size `sw`×`sh`.
///
/// Headless mutter centers the single toplevel on the virtual monitor, so the
/// origin is `((screen − window-content) / 2)`. The content box is taken as the
/// **largest bbox that fits within the screen**: a centered window never
/// exceeds the monitor, but a scrollable child can report a logical bbox
/// taller/wider than the viewport (observed: a 696×800 list inside a 720×640
/// window on a 768px-tall screen), which would otherwise be mistaken for the
/// window. Returns `None` when no bbox fits the screen.
fn centered_window_origin(
    bounds: impl IntoIterator<Item = crate::atspi::Rect>,
    sw: i32,
    sh: i32,
) -> Option<(i32, i32)> {
    let content = bounds
        .into_iter()
        .filter(|b| b.width <= sw && b.height <= sh)
        .max_by_key(|b| b.width as i64 * b.height as i64)?;
    Some((
        ((sw - content.width) / 2).max(0),
        ((sh - content.height) / 2).max(0),
    ))
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
            // Mock sessions have no real session bus, so external-effect
            // capture is unavailable (the readback methods return the
            // capture-disabled error).
            external_sinks: None,
            secondary_spec: SecondaryLaunchSpec {
                command: "sleep".to_string(),
                env: Vec::new(),
                cwd: None,
            },
            default_timeout_ns: AtomicU64::new(FALLBACK_DEFAULT_TIMEOUT.as_nanos() as u64),
            // Mock sessions have no real isolated keyfile store, so live
            // `set_setting` is unavailable (it returns the isolation-required
            // error); tests that need it construct a real session.
            gsettings_isolated: false,
            cancellation: CancellationToken::new(),
            app,
            keepalive_stream: None,
            recorder_stream: None,
            video_recorder: None,
            input,
            capture,
            compositor,
            stdout: Arc::new(AppStdout::default()),
            stdout_reader: None,
            #[cfg(feature = "visual")]
            visual_engine: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "visual")]
            visual_ocr_cache: std::sync::Mutex::new(crate::visual::OcrCache::default()),
            visual_region_tuning: VisualRegionTuning::default(),
            visual_text_tuning: VisualTextTuning::default(),
            visual_click_tuning: VisualClickTuning::default(),
            screen_size: std::sync::OnceLock::new(),
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
        // app → keepalive_stream → recorder_stream → video_recorder → input →
        // capture → compositor. A video_recorder dropped without explicit
        // stop() leaves a truncated WebM (no seekhead) — see
        // VideoRecorder::Drop.
        // Cancel the token so a still-running stdout reader exits
        // cooperatively; abort the JoinHandle as a hard fallback for
        // the leaked-grandchild case where the read syscall never
        // observes the cancellation. Drop is sync so we can't await
        // the abort — the runtime tears the task down on its next
        // poll.
        self.cancellation.cancel();
        if let Some(handle) = self.stdout_reader.take() {
            handle.abort();
        }
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

/// The per-session XDG base-dir overrides applied under
/// [`SessionConfig::gsettings_isolated`]: state, data, and cache homes,
/// each a subdirectory of the session runtime dir so they vanish with the
/// session. Config home is handled separately (it carries the GSettings
/// keyfile — see [`crate::gsettings::config_dir`]).
fn isolated_xdg_env(runtime_dir: &Path) -> [(&'static str, PathBuf); 3] {
    [
        ("XDG_STATE_HOME", runtime_dir.join("xdg-state")),
        ("XDG_DATA_HOME", runtime_dir.join("xdg-data")),
        ("XDG_CACHE_HOME", runtime_dir.join("xdg-cache")),
    ]
}

/// Build the environment the target app is launched with, as owned key/value
/// pairs. Shared by [`spawn_app`] and the [`SecondaryLaunchSpec`] captured for
/// [`Session::launch_secondary`], so a secondary instance lands on the exact
/// same Wayland display, D-Bus bus, XDG dirs, and GSettings backend as the
/// primary — which is what lets a single-instance `GApplication` recognize it
/// and forward its command line.
///
/// Has the side effect of creating the isolated config / state / data / cache
/// directories (idempotent), matching the original inline behavior.
fn app_env_pairs(
    cfg: &SessionConfig,
    wayland_display: &str,
    runtime_dir: &Path,
    dbus_address: &str,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("WAYLAND_DISPLAY".into(), wayland_display.to_string()),
        ("DBUS_SESSION_BUS_ADDRESS".into(), dbus_address.to_string()),
        (
            "XDG_RUNTIME_DIR".into(),
            runtime_dir.to_string_lossy().into_owned(),
        ),
        ("NO_AT_BRIDGE".into(), "0".into()),
        ("GTK_A11Y".into(), "atspi".into()),
    ];

    // When isolation is on, point the app at the session's private keyfile
    // GSettings store (see `crate::gsettings`) so it starts from default
    // state, never touches the host's dconf, and picks up any settings the
    // compositor seeded into the same keyfile (the compositor is the sole
    // writer; we only read here). The keyfile backend bypasses the dconf
    // daemon entirely, unlike GSETTINGS_BACKEND=memory which the host daemon
    // ignores. When off, the app inherits the host's normal GSettings.
    if cfg.gsettings_isolated {
        let config_dir = crate::gsettings::config_dir(runtime_dir);
        let _ = std::fs::create_dir_all(&config_dir);
        env.push((
            "XDG_CONFIG_HOME".into(),
            config_dir.to_string_lossy().into_owned(),
        ));
        env.push((
            "GSETTINGS_BACKEND".into(),
            crate::gsettings::KEYFILE_BACKEND.to_string(),
        ));
    }
    if cfg.xdg_isolated {
        // Private state/data/cache base dirs. Config-only isolation lets
        // app state (g_get_user_state_dir() / user_data_dir()) escape to
        // the host's ~/.local/{state,share} — both polluting the host and
        // letting one session's persisted state (e.g. a saved-session
        // file) poison every later session.
        for (key, dir) in isolated_xdg_env(runtime_dir) {
            let _ = std::fs::create_dir_all(&dir);
            env.push((key.to_string(), dir.to_string_lossy().into_owned()));
        }
    }
    // Caller-supplied env last, so it can override anything above.
    for (key, value) in &cfg.extra_env {
        env.push((key.clone(), value.clone()));
    }
    env
}

fn spawn_app(cfg: &SessionConfig, env: &[(String, String)]) -> Result<Child> {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Kill the app if its `Child` is dropped without an explicit
        // kill — e.g. when `Session::start` is aborted mid-construction
        // (a setup timeout / client cancellation in the MCP layer) before
        // the child is owned by a `Session` whose `Drop` would SIGKILL it.
        // Harmless on the normal path, where `kill()` runs first.
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    if let Some(dir) = &cfg.cwd {
        cmd.current_dir(dir);
    }
    set_pdeathsig(&mut cmd);
    cmd.spawn()
        .map_err(|e| Error::process_with(format!("app '{}'", cfg.command), e))
}

/// Linux parent-death protection: SIGKILL the app the moment the spawning
/// thread dies, so a hard-killed controlling process (`SIGKILL`, panic=abort,
/// OOM, CI timeout) — which bypasses `Session::Drop`/`kill_on_drop` — can't
/// orphan the app. Mirrors the compositor's protection of its daemon quartet.
/// The `getppid` check covers the fork/exec parent-death race.
fn set_pdeathsig(cmd: &mut Command) {
    // Capture our PID in the parent; the child bails only if its parent is no
    // longer us (we died and it was reparented). A hard-coded `getppid() == 1`
    // check is wrong in a container, where the controlling process is often
    // PID 1 and every legitimately-parented child would be killed at exec.
    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and calls only
    // async-signal-safe libc functions (prctl, getppid, _exit).
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent as i32 {
                libc::_exit(0);
            }
            Ok(())
        });
    }
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

async fn wait_for_app(conn: &zbus::Connection, app_name: &str) -> Result<(String, String)> {
    let total_polls =
        (APP_DISCOVERY_TIMEOUT.as_millis() / APP_DISCOVERY_POLL_INTERVAL.as_millis()) as usize;
    // Log the registry snapshot ~5 times over the wait so a stuck
    // discovery is visible in logs without spamming on every poll.
    let log_every = (total_polls / 5).max(1);

    for i in 0..total_polls {
        if let Ok(root) = atspi_client::get_registry_root(conn).await {
            if let Ok(children) = root.get_children().await {
                let mut found_names = Vec::new();
                for child_ref in &children {
                    let Some(bus_name) = child_ref.name_as_str() else {
                        continue;
                    };
                    let path = child_ref.path_as_str();

                    if let Ok(child) = atspi_client::build_accessible(conn, bus_name, path).await {
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

                if i % log_every == 0 {
                    tracing::debug!(
                        "AT-SPI registry has {} apps: {:?} (looking for '{}')",
                        found_names.len(),
                        found_names,
                        app_name
                    );
                }
            }
        }

        tokio::time::sleep(APP_DISCOVERY_POLL_INTERVAL).await;
    }
    Err(Error::Timeout(format!(
        "app '{}' did not appear in AT-SPI registry within {}s",
        app_name,
        APP_DISCOVERY_TIMEOUT.as_secs()
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

    fn rect(x: i32, y: i32, w: i32, h: i32) -> crate::atspi::Rect {
        crate::atspi::Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn centered_window_origin_centers_the_largest_fitting_box() {
        // A 720×640 window on a 1024×768 monitor → ((1024-720)/2, (768-640)/2).
        let boxes = [rect(0, 0, 720, 640), rect(12, 420, 696, 32)];
        assert_eq!(centered_window_origin(boxes, 1024, 768), Some((152, 64)));
    }

    #[test]
    fn centered_window_origin_ignores_overflowing_scroll_child() {
        // The 696×800 child is taller than the 768px screen and has the
        // largest area — but a centered window can't exceed the monitor, so
        // it must be filtered out in favour of the real 720×640 window.
        let boxes = [
            rect(12, 360, 696, 800), // overflowing scroll content (area 556800)
            rect(0, 0, 720, 640),    // the real window (area 460800)
        ];
        assert_eq!(centered_window_origin(boxes, 1024, 768), Some((152, 64)));
    }

    #[test]
    fn centered_window_origin_clamps_negative_to_zero() {
        // A window as large as the screen sits at the origin, not a negative
        // coordinate.
        let boxes = [rect(0, 0, 1024, 768)];
        assert_eq!(centered_window_origin(boxes, 1024, 768), Some((0, 0)));
    }

    #[test]
    fn centered_window_origin_none_when_nothing_fits() {
        // Every bbox overflows the screen → no usable content box.
        let boxes = [rect(0, 0, 2000, 2000)];
        assert_eq!(centered_window_origin(boxes, 1024, 768), None);
    }

    #[test]
    fn test_get_host_session_bus_fallback() {
        let result = get_host_session_bus_inner(None);
        assert!(
            result.contains("/run/user/"),
            "expected /run/user/ path, got: {result}"
        );
    }

    fn minimal_cfg() -> SessionConfig {
        SessionConfig {
            command: "true".into(),
            args: vec![],
            cwd: None,
            app_name: "x".into(),
            video_output: None,
            video_bitrate: None,
            video_fps: None,
            prewarm_visual: false,
            visual_region_tuning: Default::default(),
            visual_text_tuning: Default::default(),
            visual_click_tuning: Default::default(),
            gsettings_isolated: true,
            xdg_isolated: true,
            extra_env: vec![("CUSTOM".into(), "1".into())],
            capture_external_effects: false,
        }
    }

    #[test]
    fn app_env_pairs_includes_core_isolation_and_extra_env() {
        let cfg = minimal_cfg();
        let dir = tempfile::tempdir().expect("tempdir");
        let env = app_env_pairs(&cfg, "wayland-9", dir.path(), "unix:path=/tmp/bus");
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("WAYLAND_DISPLAY"), Some("wayland-9"));
        assert_eq!(get("DBUS_SESSION_BUS_ADDRESS"), Some("unix:path=/tmp/bus"));
        assert_eq!(get("GTK_A11Y"), Some("atspi"));
        // gsettings isolation on → keyfile backend + private config home.
        assert!(get("XDG_CONFIG_HOME").is_some());
        assert_eq!(
            get("GSETTINGS_BACKEND"),
            Some(crate::gsettings::KEYFILE_BACKEND)
        );
        // xdg isolation on → private state dir.
        assert!(get("XDG_STATE_HOME").is_some());
        // Caller-supplied env is carried through (applied last).
        assert_eq!(get("CUSTOM"), Some("1"));
    }

    #[test]
    fn app_env_pairs_skips_isolation_dirs_when_disabled() {
        let mut cfg = minimal_cfg();
        cfg.gsettings_isolated = false;
        cfg.xdg_isolated = false;
        let dir = tempfile::tempdir().expect("tempdir");
        let env = app_env_pairs(&cfg, "wayland-9", dir.path(), "unix:path=/tmp/bus");
        let has = |k: &str| env.iter().any(|(key, _)| key == k);
        assert!(!has("GSETTINGS_BACKEND"));
        assert!(!has("XDG_CONFIG_HOME"));
        assert!(!has("XDG_STATE_HOME"));
        // Core env is still present.
        assert!(has("WAYLAND_DISPLAY"));
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
            async fn start(&mut self, _r: Option<&str>, _scale: Option<f64>) -> Result<()> {
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
            async fn press_keysym(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                _: crate::backend::PointerAxis,
                _: i32,
                _: &CancellationToken,
            ) -> Result<()> {
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
            async fn press_keysym(&self, k: u32, _: &CancellationToken) -> Result<()> {
                self.0.lock().unwrap().push(Event::Press(k));
                Ok(())
            }
            async fn key_down(&self, k: u32, _: &CancellationToken) -> Result<()> {
                self.0.lock().unwrap().push(Event::Down(k));
                Ok(())
            }
            async fn key_up(&self, k: u32, _: &CancellationToken) -> Result<()> {
                self.0.lock().unwrap().push(Event::Up(k));
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                _: crate::backend::PointerAxis,
                _: i32,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
        }

        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>, _: Option<f64>) -> Result<()> {
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

    /// The held-modifier pointer-gesture primitive from the original report:
    /// `key_down` / `key_up` must be able to *bracket* another input event so
    /// the app sees e.g. Ctrl+scroll-to-zoom rather than a plain scroll. This
    /// drives the issue's exact recipe — hold Ctrl, wheel up, release Ctrl —
    /// and asserts the wheel event lands *between* the press and the release,
    /// which `press_chord` (press + release as one atom) can't express.
    #[tokio::test]
    async fn key_down_up_brackets_wheel_scroll_with_held_modifier() {
        use crate::backend::{
            CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream, PointerAxis,
        };
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};
        use std::sync::Mutex;

        /// Records keyboard holds and wheel events in dispatch order so the
        /// test can assert the scroll is sandwiched by the held modifier.
        #[derive(Debug, PartialEq, Eq)]
        enum Event {
            Down(u32),
            Up(u32),
            Axis(PointerAxis, i32),
        }

        struct RecordingInput(Arc<Mutex<Vec<Event>>>);
        #[async_trait]
        impl InputBackend for RecordingInput {
            async fn press_keysym(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, k: u32, _: &CancellationToken) -> Result<()> {
                self.0.lock().unwrap().push(Event::Down(k));
                Ok(())
            }
            async fn key_up(&self, k: u32, _: &CancellationToken) -> Result<()> {
                self.0.lock().unwrap().push(Event::Up(k));
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                axis: PointerAxis,
                steps: i32,
                _: &CancellationToken,
            ) -> Result<()> {
                self.0.lock().unwrap().push(Event::Axis(axis, steps));
                Ok(())
            }
        }

        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>, _: Option<f64>) -> Result<()> {
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

        // The issue's recipe: hold Ctrl, wheel up one detent, release Ctrl.
        let ctrl = crate::keysym::modifier_name_to_keysym("Ctrl").unwrap();
        s.key_down(ctrl).await.unwrap();
        s.pointer_axis_discrete(PointerAxis::Vertical, -1)
            .await
            .unwrap();
        s.key_up(ctrl).await.unwrap();

        let got: Vec<Event> = std::mem::take(&mut *events.lock().unwrap());
        assert_eq!(
            got,
            vec![
                Event::Down(ctrl),
                Event::Axis(PointerAxis::Vertical, -1),
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
            async fn start(&mut self, _: Option<&str>, _: Option<f64>) -> Result<()> {
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
            async fn press_keysym(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                _: crate::backend::PointerAxis,
                _: i32,
                _: &CancellationToken,
            ) -> Result<()> {
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
            matches!(err, Error::Process { ref message, .. } if message.contains("invalid chord")),
            "expected process:invalid chord, got {err:?}"
        );
    }

    #[tokio::test]
    async fn type_text_bails_when_cancelled_mid_string() {
        use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        // Input backend that counts keystrokes and fires the session's
        // cancellation token after the Nth one. Driving cancellation
        // from inside the backend (rather than a concurrent task +
        // sleep) makes the test deterministic — no wall-clock race.
        struct CountAndCancelInput {
            count: Arc<AtomicUsize>,
            cancel_after: usize,
            token: CancellationToken,
        }
        #[async_trait]
        impl InputBackend for CountAndCancelInput {
            async fn press_keysym(&self, _: u32, _: &CancellationToken) -> Result<()> {
                let n = self.count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                if n == self.cancel_after {
                    self.token.cancel();
                }
                Ok(())
            }
            async fn key_down(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                _: crate::backend::PointerAxis,
                _: i32,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
        }
        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>, _: Option<f64>) -> Result<()> {
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

        // Build the session first so we can clone its real cancellation
        // token into the backend. (new_for_test instantiates a fresh
        // token internally; we share a handle to it.)
        let count = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::new();
        let backend_token = token.clone();
        let mut s = Session::new_for_test(
            "t".into(),
            "a".into(),
            Box::new(CountAndCancelInput {
                count: Arc::clone(&count),
                cancel_after: 3,
                token: backend_token,
            }),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        );
        // Swap the session's default-constructed token for the shared
        // one so `self.cancellation.is_cancelled()` in type_text sees
        // the cancel that the backend triggers.
        s.cancellation = token;

        let err = s.type_text("abcdefghijklmnopqrstuvwxyz").await.unwrap_err();
        assert!(
            matches!(err, Error::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        let typed = count.load(AtomicOrdering::SeqCst);
        // The loop checks the token *before* each press_keysym, so the
        // backend can consume iteration N, cancel, and iteration N+1
        // will bail. Expected: exactly `cancel_after` presses.
        assert_eq!(
            typed, 3,
            "loop should bail on the iteration after cancel; typed = {typed}"
        );
    }

    #[tokio::test]
    async fn press_chord_bails_when_already_cancelled() {
        use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
        use async_trait::async_trait;
        use std::path::{Path, PathBuf};

        struct RejectInput;
        #[async_trait]
        impl InputBackend for RejectInput {
            async fn press_keysym(&self, _: u32, _: &CancellationToken) -> Result<()> {
                panic!("press_keysym should not run on a cancelled session")
            }
            async fn key_down(&self, _: u32, _: &CancellationToken) -> Result<()> {
                panic!("key_down should not run on a cancelled session")
            }
            async fn key_up(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                _: crate::backend::PointerAxis,
                _: i32,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
        }
        struct StubCompositor;
        #[async_trait]
        impl CompositorRuntime for StubCompositor {
            async fn start(&mut self, _: Option<&str>, _: Option<f64>) -> Result<()> {
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

        let s = Session::new_for_test(
            "t".into(),
            "a".into(),
            Box::new(RejectInput),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        );
        s.cancel();

        let err = s.press_chord("Ctrl+A").await.unwrap_err();
        assert!(
            matches!(err, Error::Cancelled),
            "expected Cancelled, got {err:?}"
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
            async fn start(&mut self, _: Option<&str>, _: Option<f64>) -> Result<()> {
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
            async fn press_keysym(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_down(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn key_up(&self, _: u32, _: &CancellationToken) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_relative(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_motion_absolute(
                &self,
                _: f64,
                _: f64,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_down(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_button_up(
                &self,
                _: crate::backend::PointerButton,
                _: &CancellationToken,
            ) -> Result<()> {
                Ok(())
            }
            async fn pointer_axis_discrete(
                &self,
                _: crate::backend::PointerAxis,
                _: i32,
                _: &CancellationToken,
            ) -> Result<()> {
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
    async fn set_setting_errors_without_gsettings_isolation() {
        // make_test_session builds a non-isolated mock session, so set_setting
        // must refuse up front rather than attempt a keyfile write.
        let s = make_test_session();
        let err = s
            .set_setting("org.gnome.desktop.interface", "text-scaling-factor", "1.5")
            .await
            .expect_err("set_setting must error when the session isn't gsettings-isolated");
        assert!(
            matches!(err, Error::Process { ref message, .. } if message.contains("isolation")),
            "expected an isolation-required Process error, got: {err:?}"
        );
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

    #[tokio::test]
    async fn wait_for_stdout_line_bails_when_cancelled() {
        // kill_session firing during a long stdout wait should surface
        // as Error::Cancelled in milliseconds, not wait out the deadline.
        let s = Arc::new(make_test_session());
        let s_for_cancel = s.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            s_for_cancel.cancel();
        });

        let start = std::time::Instant::now();
        // Deadline is 5s so any quick return is attributable to cancel,
        // not timeout.
        let err = s
            .wait_for_stdout_line(0, |l| l == "never", Duration::from_secs(5))
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        assert!(matches!(err, Error::Cancelled), "got: {err:?}");
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel should wake the wait promptly; elapsed = {elapsed:?}"
        );
    }

    /// The XDG sandbox dirs must all live under the session runtime dir
    /// (so they vanish with the session) and be distinct from each other —
    /// state escaping to a shared path is exactly the cross-session
    /// poisoning this isolation exists to prevent.
    #[test]
    fn isolated_xdg_env_dirs_are_private_and_distinct() {
        let runtime = Path::new("/tmp/wd-session-test");
        let dirs = isolated_xdg_env(runtime);
        let keys: Vec<&str> = dirs.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"]);
        let mut paths = std::collections::HashSet::new();
        for (key, path) in &dirs {
            assert!(
                path.starts_with(runtime),
                "{key} must live under the runtime dir, got {path:?}"
            );
            assert!(paths.insert(path.clone()), "{key} collides: {path:?}");
        }
    }
}
