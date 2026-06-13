//! Loading the shared `OcrEngine`. Single entry point used by both the
//! prewarm task spawned at session start and the on-demand path inside
//! `VisualLocator::resolve` — `tokio::sync::OnceCell` ensures exactly
//! one initializer runs no matter how many callers arrive concurrently.

use std::sync::Arc;

use ocrs::{OcrEngine, OcrEngineParams};
use rten::Model;

use crate::error::{Error, Result};

/// The cached engine state. `Result` is captured (not just `OcrEngine`)
/// so that a failed load doesn't get retried forever — every subsequent
/// `find_by_text` caller sees the same error and can act on it.
///
/// `String` rather than the full error chain because `OnceCell` only
/// supports `Clone`-able values, and the underlying errors are mostly
/// stringified anyway.
pub(crate) type EngineResult = std::result::Result<Arc<OcrEngine>, String>;

/// Build an `OcrEngine` from the two `.rten` files at the given paths.
/// CPU-bound (model parsing); call from `spawn_blocking`.
pub(super) fn load_engine_blocking(
    detection_path: std::path::PathBuf,
    recognition_path: std::path::PathBuf,
) -> Result<Arc<OcrEngine>> {
    // OCR cost is dominated by rten inference, and an unoptimized (dev
    // profile) rten is ~30x slower — minutes per full-frame pass instead of
    // seconds. Consumers hit this by running `cargo test` without dependency
    // opt-level overrides and then watching every `find_by_text` blow past
    // the 120s visual auto-wait budget (which is calibrated for optimized
    // builds). Surface the fix at the one place every visual call funnels
    // through. Keyed on this crate's own debug_assertions because rten's
    // opt-level isn't observable at runtime — note the override does NOT
    // clear debug_assertions, so the message tells already-fixed consumers
    // to ignore it.
    if cfg!(debug_assertions) {
        tracing::warn!(
            "visual: waydriver was compiled in a dev/debug profile — rten OCR inference \
             is ~30x slower unoptimized (minutes per full-frame pass instead of seconds), \
             and the 120s visual auto-wait default assumes an optimized build. If OCR is \
             slow, add this to your WORKSPACE ROOT Cargo.toml (optimizes dependencies \
             only; your own code stays fast to build):\n\n\
             [profile.dev.package.\"*\"]\n\
             opt-level = 3\n\n\
             Already applied an override like that? Then OCR is fast and this warning is \
             a false positive (opt-level overrides don't clear cfg(debug_assertions)) — \
             ignore it."
        );
    }

    let started = std::time::Instant::now();
    tracing::info!(
        detection = %detection_path.display(),
        recognition = %recognition_path.display(),
        "visual: loading ocrs engine"
    );

    let detection_model = Model::load_file(&detection_path).map_err(|e| {
        Error::visual(format!(
            "failed to load detection model {}: {e}",
            detection_path.display()
        ))
    })?;
    let recognition_model = Model::load_file(&recognition_path).map_err(|e| {
        Error::visual(format!(
            "failed to load recognition model {}: {e}",
            recognition_path.display()
        ))
    })?;

    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection_model),
        recognition_model: Some(recognition_model),
        ..Default::default()
    })
    .map_err(|e| Error::visual(format!("failed to construct ocrs engine: {e}")))?;

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "visual: ocrs engine ready"
    );
    Ok(Arc::new(engine))
}

/// Full happy-path init: resolve model paths (downloading if needed),
/// then build the engine. Runs in a `spawn_blocking` task because both
/// halves are blocking. Stringifies the error so the result is
/// `Clone`-friendly for `OnceCell` storage.
pub(crate) async fn ensure_engine() -> EngineResult {
    tokio::task::spawn_blocking(move || -> Result<Arc<OcrEngine>> {
        let (det, rec) = super::models::ensure_models()?;
        load_engine_blocking(det, rec)
    })
    .await
    .map_err(|join_err| format!("ocrs engine init task panicked: {join_err}"))?
    .map_err(|e| e.to_string())
}
