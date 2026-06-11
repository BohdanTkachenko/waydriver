use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

use crate::error::{Error, Result};

/// Serializes `grab_png_sync` calls so concurrent sessions don't race on the
/// process-wide `PIPEWIRE_REMOTE` / `XDG_RUNTIME_DIR` env vars that
/// `pipewiresrc` reads during pipeline startup.
static GRAB_PNG_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that sets process-wide env vars and restores their prior values
/// on drop.
///
/// `pipewiresrc` only reads `PIPEWIRE_REMOTE` / `XDG_RUNTIME_DIR` from the
/// environment, so the capture paths have to set them process-wide before the
/// pipeline connects. Leaving them set, though, is the root cause of the
/// session-dir nesting overflow: `XDG_RUNTIME_DIR` would stay pointed at the
/// live session's runtime dir for the rest of the server's life, so any later
/// consumer that re-derived a path from it would nest one level deeper per
/// session until the AF_UNIX `sun_path` limit wedged pipewire (a restart was
/// the only cure). Restoring on drop keeps the parent env pristine, so the
/// leak — and the nesting it caused — can't accumulate across a server
/// lifetime regardless of init order.
///
/// All construction/restoration happens under [`GRAB_PNG_LOCK`], so there's no
/// concurrent mutation of these process-wide keys.
struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set(vars: &[(&'static str, &std::ffi::OsStr)]) -> Self {
        let saved = vars
            .iter()
            .map(|(key, val)| {
                let prev = std::env::var_os(key);
                // Safe: callers hold GRAB_PNG_LOCK, which serializes every
                // read/write of these keys across the process.
                unsafe { std::env::set_var(key, val) };
                (*key, prev)
            })
            .collect();
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in &self.saved {
            // Safe: still under GRAB_PNG_LOCK (the guard drops before the lock
            // guard, which is declared first and so drops last).
            unsafe {
                match prev {
                    Some(val) => std::env::set_var(key, val),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Capture a PNG from a PipeWire node using an in-process GStreamer pipeline.
///
/// Builds `pipewiresrc ! videoconvert ! pngenc snapshot=true ! appsink` and
/// pulls the encoded PNG bytes directly from the appsink buffer — no subprocess,
/// no stdout piping.
///
/// `pipewiresrc` reads `PIPEWIRE_REMOTE` and `XDG_RUNTIME_DIR` from the
/// environment. Calls are serialized via [`GRAB_PNG_LOCK`] so concurrent
/// sessions don't race on these process-wide env vars.
fn validate_pipewire_socket(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        Error::screenshot(format!(
            "pipewire socket path has no parent: {}",
            path.display()
        ))
    })
}

fn build_pipeline_str(node_id: u32) -> String {
    format!(
        "pipewiresrc path={node_id} always-copy=true do-timestamp=true num-buffers=5 \
         ! videoconvert \
         ! pngenc snapshot=true \
         ! appsink name=sink"
    )
}

/// Capture a single PNG frame from a PipeWire stream via GStreamer.
///
/// Connects to the PipeWire node identified by `node_id` through the given
/// `pipewire_socket`, grabs one video frame, and returns it as PNG bytes.
pub async fn grab_png(node_id: u32, pipewire_socket: &Path) -> Result<Vec<u8>> {
    let runtime_dir = validate_pipewire_socket(pipewire_socket)?;

    let socket = pipewire_socket.to_path_buf();
    let runtime = runtime_dir.to_path_buf();

    // GStreamer pipeline ops are blocking — run on a blocking thread.
    tokio::task::spawn_blocking(move || grab_png_sync(node_id, &socket, &runtime))
        .await
        .map_err(|e| Error::screenshot_with("spawn_blocking failed", e))?
}

fn grab_png_sync(node_id: u32, pipewire_socket: &Path, runtime_dir: &Path) -> Result<Vec<u8>> {
    let _guard = GRAB_PNG_LOCK
        .lock()
        .map_err(|e| Error::screenshot(format!("grab_png lock poisoned: {e}")))?;

    gst::init().map_err(|e| Error::screenshot_with("gstreamer init failed", e))?;

    // pipewiresrc reads these from the environment. The guard restores their
    // prior values when this function returns (on every path, including the
    // `?` errors below), so the parent process is never left with
    // `XDG_RUNTIME_DIR` pointed at a session dir. The synchronous frame pull
    // below keeps them set for as long as pipewiresrc needs them.
    let _env = EnvGuard::set(&[
        ("PIPEWIRE_REMOTE", pipewire_socket.as_os_str()),
        ("XDG_RUNTIME_DIR", runtime_dir.as_os_str()),
    ]);

    let pipeline_str = build_pipeline_str(node_id);

    let pipeline = gst::parse::launch(&pipeline_str)
        .map_err(|e| Error::screenshot_with("pipeline parse failed", e))?;

    let pipeline = pipeline
        .dynamic_cast::<gst::Pipeline>()
        .map_err(|_| Error::screenshot("parsed element is not a Pipeline"))?;

    let sink = pipeline
        .by_name("sink")
        .ok_or_else(|| Error::screenshot("appsink not found in pipeline"))?;
    let appsink = sink
        .dynamic_cast::<AppSink>()
        .map_err(|_| Error::screenshot("element 'sink' is not an AppSink"))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| Error::screenshot_with("failed to start pipeline", e))?;

    // Pull a sample with a timeout.
    let sample = appsink
        .try_pull_sample(gst::ClockTime::from_seconds(10))
        .ok_or_else(|| Error::screenshot("timed out waiting for PNG frame"))?;

    let buffer = sample
        .buffer()
        .ok_or_else(|| Error::screenshot("sample has no buffer"))?;

    let map = buffer
        .map_readable()
        .map_err(|e| Error::screenshot_with("failed to map buffer", e))?;

    let png_bytes = map.as_slice().to_vec();

    pipeline
        .set_state(gst::State::Null)
        .map_err(|e| Error::screenshot_with("failed to stop pipeline", e))?;

    tracing::info!(bytes = png_bytes.len(), "screenshot captured");
    Ok(png_bytes)
}

// ── Video recording ─────────────────────────────────────────────────────────

/// Default VP8 target bitrate for session recordings, in bits per second.
///
/// 2 Mbps is a sensible budget for screen content at typical UI-test display
/// sizes (SVGA through FHD) at [`DEFAULT_VIDEO_FPS`]: enough bits to keep text
/// edges crisp during redraw spikes, while staying well under the CPU budget
/// of a headless run. VP8's own default of 256 kbps was visibly soft on UI
/// text. Callers recording at 4K+ should raise this, since the same bit
/// budget has to cover ~8× as many pixels.
pub const DEFAULT_VIDEO_BITRATE: u32 = 2_000_000;

/// Default recording framerate in frames-per-second.
///
/// 15 fps is plenty for UI testing artifacts (you're looking at state
/// transitions, not smooth animation) and keeps the encode budget low on
/// mutter's bursty headless frame delivery. Callers wanting smoother playback
/// of animated UI can raise this via [`SessionConfig::video_fps`].
pub const DEFAULT_VIDEO_FPS: u32 = 15;

/// Build the GStreamer pipeline string for a long-lived WebM recording.
///
/// `pipewiresrc` feeds raw frames through `videoconvert` + `videorate` (capped
/// at `fps` — mutter's headless frame delivery is bursty, so videorate smooths
/// timestamps), VP8-encodes them, muxes into WebM, and writes directly to
/// `output_path`.
///
/// `bitrate` is passed to `vp8enc` as `target-bitrate` in bits/sec. The
/// encoder is also configured with `min-quantizer=4 max-quantizer=30` so
/// individual frames can't be starved — screen content has long static
/// stretches punctuated by sudden changes, and VP8's default max-quantizer
/// of 56 produces visibly smeared text during those changes.
/// `keyframe-max-dist = fps * 2` (a keyframe every ~2 s) keeps random-access
/// seeking responsive without inflating the file much.
fn build_recording_pipeline_str(
    node_id: u32,
    output_path: &Path,
    bitrate: u32,
    fps: u32,
) -> String {
    // GStreamer's gst_parse_launch tolerates paths with forward slashes but
    // would choke on unescaped spaces or quotes. Session IDs are hex-only so
    // in practice the path is safe; we still guard by debug-asserting no
    // spaces, matching how the screenshot pipeline treats `node_id` as
    // already-validated input from the backend.
    debug_assert!(
        !output_path.to_string_lossy().contains(char::is_whitespace),
        "recording output path must not contain whitespace: {}",
        output_path.display()
    );
    let keyframe_max_dist = fps * 2;
    format!(
        "pipewiresrc path={node_id} always-copy=true do-timestamp=true \
         ! videoconvert \
         ! videorate \
         ! video/x-raw,framerate={fps}/1 \
         ! vp8enc deadline=1 cpu-used=4 \
           target-bitrate={bitrate} \
           min-quantizer=4 max-quantizer=30 \
           keyframe-max-dist={keyframe_max_dist} \
         ! webmmux \
         ! filesink location={path}",
        path = output_path.display()
    )
}

/// Handle to a running WebM recording pipeline. Callers must call
/// [`VideoRecorder::stop`] to finalize the file — dropping without stopping
/// flushes best-effort to NULL state, which produces a truncated WebM without
/// a seekhead.
pub struct VideoRecorder {
    /// `Some` while the pipeline is live; `None` once `stop` has consumed it
    /// and finished EOS. `Drop` treats `Some` as the "never stopped cleanly"
    /// case and falls back to a plain state-change to NULL.
    pipeline: Option<gst::Pipeline>,
    output_path: PathBuf,
}

impl VideoRecorder {
    /// Start a WebM recording that reads from the given PipeWire node and
    /// writes to `output_path` at the given `bitrate` (bits/sec) and `fps`.
    /// Returns once the pipeline is in PLAYING state.
    pub async fn start(
        node_id: u32,
        pipewire_socket: &Path,
        output_path: &Path,
        bitrate: u32,
        fps: u32,
    ) -> Result<VideoRecorder> {
        let socket = pipewire_socket.to_path_buf();
        let runtime = validate_pipewire_socket(pipewire_socket)?.to_path_buf();
        let output = output_path.to_path_buf();

        tokio::task::spawn_blocking(move || {
            start_recording_sync(node_id, &socket, &runtime, output, bitrate, fps)
        })
        .await
        .map_err(|e| Error::screenshot_with("spawn_blocking failed", e))?
    }

    /// Send EOS, wait for the muxer to flush cues, then set the pipeline to
    /// NULL. This is the only shutdown path that produces a seekable WebM.
    pub async fn stop(mut self) -> Result<()> {
        let pipeline = self
            .pipeline
            .take()
            .ok_or_else(|| Error::screenshot("recording already stopped"))?;
        tokio::task::spawn_blocking(move || stop_recording_sync(&pipeline))
            .await
            .map_err(|e| Error::screenshot_with("spawn_blocking failed", e))?
    }

    /// Path the WebM is being written to.
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }
}

impl Drop for VideoRecorder {
    fn drop(&mut self) {
        let Some(pipeline) = self.pipeline.take() else {
            return;
        };
        tracing::warn!(
            path = %self.output_path.display(),
            "VideoRecorder dropped without stop(); WebM will be truncated (no seekhead/cues)"
        );
        let _ = pipeline.set_state(gst::State::Null);
    }
}

fn start_recording_sync(
    node_id: u32,
    pipewire_socket: &Path,
    runtime_dir: &Path,
    output_path: PathBuf,
    bitrate: u32,
    fps: u32,
) -> Result<VideoRecorder> {
    let _guard = GRAB_PNG_LOCK
        .lock()
        .map_err(|e| Error::screenshot(format!("grab_png lock poisoned: {e}")))?;

    gst::init().map_err(|e| Error::screenshot_with("gstreamer init failed", e))?;

    // pipewiresrc reads these from the environment during the state transition
    // below. Unlike the screenshot path, the pipeline outlives this function,
    // so we must keep the env set until pipewiresrc has actually connected —
    // hence the explicit wait for PLAYING before the guard restores the prior
    // values on return. Restoring is what stops `XDG_RUNTIME_DIR` from leaking
    // a session dir into the parent for the rest of the server's life.
    let _env = EnvGuard::set(&[
        ("PIPEWIRE_REMOTE", pipewire_socket.as_os_str()),
        ("XDG_RUNTIME_DIR", runtime_dir.as_os_str()),
    ]);

    let pipeline_str = build_recording_pipeline_str(node_id, &output_path, bitrate, fps);

    let pipeline = gst::parse::launch(&pipeline_str)
        .map_err(|e| Error::screenshot_with("recording pipeline parse failed", e))?;
    let pipeline = pipeline
        .dynamic_cast::<gst::Pipeline>()
        .map_err(|_| Error::screenshot("parsed element is not a Pipeline"))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| Error::screenshot_with("failed to start recording pipeline", e))?;

    // Block until the pipeline reaches PLAYING so pipewiresrc has connected to
    // the daemon and read PIPEWIRE_REMOTE/XDG_RUNTIME_DIR before `_env` drops
    // and restores the parent's prior values. Without this the async state
    // change could still be pending when we restore, and pipewiresrc would
    // read a stale socket path.
    let (res, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(10));
    res.map_err(|e| Error::screenshot_with("recording pipeline failed to reach PLAYING", e))?;

    tracing::info!(path = %output_path.display(), node_id, "video recording started");

    Ok(VideoRecorder {
        pipeline: Some(pipeline),
        output_path,
    })
}

fn stop_recording_sync(pipeline: &gst::Pipeline) -> Result<()> {
    // Sending EOS is load-bearing: webmmux only writes the cues/seekhead on
    // EOS. Without it the file is playable linearly but has no index, which
    // breaks seeking in browsers.
    pipeline.send_event(gst::event::Eos::new());

    let bus = pipeline
        .bus()
        .ok_or_else(|| Error::screenshot("recording pipeline has no bus"))?;

    // Wait up to 10s for the EOS to propagate through the encoder + muxer.
    let timeout = gst::ClockTime::from_seconds(10);
    if let Some(msg) =
        bus.timed_pop_filtered(timeout, &[gst::MessageType::Eos, gst::MessageType::Error])
    {
        if let gst::MessageView::Error(err) = msg.view() {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(Error::screenshot(format!(
                "recording pipeline error before EOS: {} ({:?})",
                err.error(),
                err.debug()
            )));
        }
    } else {
        tracing::warn!("recording EOS did not arrive within 10s; file may be truncated");
    }

    pipeline
        .set_state(gst::State::Null)
        .map_err(|e| Error::screenshot_with("failed to stop recording pipeline", e))?;

    tracing::info!("video recording stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_pipeline_str_contains_node_id() {
        let s = build_pipeline_str(42);
        assert!(s.contains("path=42"), "expected path=42, got: {s}");
    }

    #[test]
    fn test_build_pipeline_str_contains_appsink() {
        let s = build_pipeline_str(0);
        assert!(s.contains("appsink name=sink"));
    }

    #[test]
    fn test_build_pipeline_str_contains_pngenc() {
        let s = build_pipeline_str(1);
        assert!(s.contains("pngenc snapshot=true"));
    }

    #[test]
    fn test_build_pipeline_str_max_node_id() {
        let s = build_pipeline_str(u32::MAX);
        assert!(s.contains("path=4294967295"));
    }

    #[test]
    fn test_build_recording_pipeline_str_contains_node_id() {
        let s = build_recording_pipeline_str(
            42,
            Path::new("/tmp/out.webm"),
            DEFAULT_VIDEO_BITRATE,
            DEFAULT_VIDEO_FPS,
        );
        assert!(s.contains("path=42"));
    }

    #[test]
    fn test_build_recording_pipeline_str_contains_output_path() {
        let s = build_recording_pipeline_str(
            1,
            Path::new("/tmp/abc/abc.webm"),
            DEFAULT_VIDEO_BITRATE,
            DEFAULT_VIDEO_FPS,
        );
        assert!(
            s.contains("location=/tmp/abc/abc.webm"),
            "expected filesink location=..., got: {s}"
        );
    }

    #[test]
    fn test_build_recording_pipeline_str_uses_vp8_webm() {
        let s = build_recording_pipeline_str(
            0,
            Path::new("/tmp/x.webm"),
            DEFAULT_VIDEO_BITRATE,
            DEFAULT_VIDEO_FPS,
        );
        assert!(s.contains("vp8enc"), "expected vp8enc: {s}");
        assert!(s.contains("webmmux"), "expected webmmux: {s}");
    }

    #[test]
    fn test_build_recording_pipeline_str_uses_default_fps() {
        let s = build_recording_pipeline_str(
            0,
            Path::new("/tmp/x.webm"),
            DEFAULT_VIDEO_BITRATE,
            DEFAULT_VIDEO_FPS,
        );
        assert!(
            s.contains(&format!("framerate={DEFAULT_VIDEO_FPS}/1")),
            "expected framerate={DEFAULT_VIDEO_FPS}/1: {s}"
        );
    }

    #[test]
    fn test_build_recording_pipeline_str_honors_custom_fps() {
        let s =
            build_recording_pipeline_str(0, Path::new("/tmp/x.webm"), DEFAULT_VIDEO_BITRATE, 30);
        assert!(s.contains("framerate=30/1"), "expected framerate=30/1: {s}");
    }

    #[test]
    fn test_build_recording_pipeline_str_keyframe_max_dist_scales_with_fps() {
        let s =
            build_recording_pipeline_str(0, Path::new("/tmp/x.webm"), DEFAULT_VIDEO_BITRATE, 30);
        assert!(
            s.contains("keyframe-max-dist=60"),
            "expected keyframe-max-dist=60 at 30 fps: {s}"
        );
    }

    #[test]
    fn test_build_recording_pipeline_str_embeds_bitrate() {
        let s =
            build_recording_pipeline_str(0, Path::new("/tmp/x.webm"), 1_500_000, DEFAULT_VIDEO_FPS);
        assert!(
            s.contains("target-bitrate=1500000"),
            "expected target-bitrate=1500000, got: {s}"
        );
    }

    #[test]
    fn test_build_recording_pipeline_str_caps_quantizer() {
        let s = build_recording_pipeline_str(
            0,
            Path::new("/tmp/x.webm"),
            DEFAULT_VIDEO_BITRATE,
            DEFAULT_VIDEO_FPS,
        );
        assert!(s.contains("max-quantizer=30"));
        assert!(s.contains("min-quantizer=4"));
    }

    #[test]
    fn default_video_bitrate_is_two_mbps() {
        assert_eq!(DEFAULT_VIDEO_BITRATE, 2_000_000);
    }

    #[test]
    fn default_video_fps_is_fifteen() {
        assert_eq!(DEFAULT_VIDEO_FPS, 15);
    }

    #[test]
    fn test_validate_pipewire_socket_valid() {
        let parent = validate_pipewire_socket(Path::new("/run/user/1000/pipewire-0")).unwrap();
        assert_eq!(parent, Path::new("/run/user/1000"));
    }

    #[test]
    fn test_validate_pipewire_socket_root() {
        let parent = validate_pipewire_socket(Path::new("/pipewire-0")).unwrap();
        assert_eq!(parent, Path::new("/"));
    }

    #[test]
    fn test_validate_pipewire_socket_no_parent() {
        assert!(validate_pipewire_socket(Path::new("")).is_err());
    }

    /// The whole nesting fix hinges on capture leaving the process env exactly
    /// as it found it. Both env tests take `GRAB_PNG_LOCK` so they serialize
    /// with each other (and mirror how the real capture paths guard these keys).
    #[test]
    fn env_guard_restores_prior_value_on_drop() {
        let _lock = GRAB_PNG_LOCK.lock().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/tmp/wd-test-root") };
        {
            let _g = EnvGuard::set(&[(
                "XDG_RUNTIME_DIR",
                std::ffi::OsStr::new("/tmp/wd-test-root/wd-session-aaaa"),
            )]);
            assert_eq!(
                std::env::var("XDG_RUNTIME_DIR").unwrap(),
                "/tmp/wd-test-root/wd-session-aaaa"
            );
        }
        // Restored to the pre-guard value — never left pointing at the session
        // dir, so a subsequent session can't nest under it.
        assert_eq!(
            std::env::var("XDG_RUNTIME_DIR").unwrap(),
            "/tmp/wd-test-root"
        );
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    #[test]
    fn env_guard_removes_key_that_was_unset_before() {
        let _lock = GRAB_PNG_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("WD_TEST_ENVGUARD_KEY") };
        {
            let _g = EnvGuard::set(&[(
                "WD_TEST_ENVGUARD_KEY",
                std::ffi::OsStr::new("/some/session/dir"),
            )]);
            assert_eq!(
                std::env::var("WD_TEST_ENVGUARD_KEY").unwrap(),
                "/some/session/dir"
            );
        }
        // Was absent before, so it must be removed (not left as empty/stale).
        assert!(std::env::var_os("WD_TEST_ENVGUARD_KEY").is_none());
    }
}
