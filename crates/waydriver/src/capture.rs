use std::path::Path;
use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

use crate::error::{Error, Result};

/// Serializes `grab_png_sync` calls so concurrent sessions don't race on the
/// process-wide `PIPEWIRE_REMOTE` / `XDG_RUNTIME_DIR` env vars that
/// `pipewiresrc` reads during pipeline startup.
static GRAB_PNG_LOCK: Mutex<()> = Mutex::new(());

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
        Error::Screenshot(format!(
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
        .map_err(|e| Error::Screenshot(format!("spawn_blocking failed: {e}")))?
}

fn grab_png_sync(node_id: u32, pipewire_socket: &Path, runtime_dir: &Path) -> Result<Vec<u8>> {
    let _guard = GRAB_PNG_LOCK
        .lock()
        .map_err(|e| Error::Screenshot(format!("grab_png lock poisoned: {e}")))?;

    gst::init().map_err(|e| Error::Screenshot(format!("gstreamer init failed: {e}")))?;

    // pipewiresrc reads these from the environment. Safe because GRAB_PNG_LOCK
    // serializes all callers — no concurrent set_var/get_var on these keys.
    unsafe {
        std::env::set_var("PIPEWIRE_REMOTE", pipewire_socket);
        std::env::set_var("XDG_RUNTIME_DIR", runtime_dir);
    }

    let pipeline_str = build_pipeline_str(node_id);

    let pipeline = gst::parse::launch(&pipeline_str)
        .map_err(|e| Error::Screenshot(format!("pipeline parse failed: {e}")))?;

    let pipeline = pipeline
        .dynamic_cast::<gst::Pipeline>()
        .map_err(|_| Error::Screenshot("parsed element is not a Pipeline".into()))?;

    let sink = pipeline
        .by_name("sink")
        .ok_or_else(|| Error::Screenshot("appsink not found in pipeline".into()))?;
    let appsink = sink
        .dynamic_cast::<AppSink>()
        .map_err(|_| Error::Screenshot("element 'sink' is not an AppSink".into()))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| Error::Screenshot(format!("failed to start pipeline: {e}")))?;

    // Pull a sample with a timeout.
    let sample = appsink
        .try_pull_sample(gst::ClockTime::from_seconds(10))
        .ok_or_else(|| Error::Screenshot("timed out waiting for PNG frame".into()))?;

    let buffer = sample
        .buffer()
        .ok_or_else(|| Error::Screenshot("sample has no buffer".into()))?;

    let map = buffer
        .map_readable()
        .map_err(|e| Error::Screenshot(format!("failed to map buffer: {e}")))?;

    let png_bytes = map.as_slice().to_vec();

    pipeline
        .set_state(gst::State::Null)
        .map_err(|e| Error::Screenshot(format!("failed to stop pipeline: {e}")))?;

    tracing::info!(bytes = png_bytes.len(), "screenshot captured");
    Ok(png_bytes)
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
}
