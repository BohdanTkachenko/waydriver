//! Screen-capture tools. Just `take_screenshot` today; a separate
//! module because it doesn't fit `run_action`'s shape (log_event has
//! to stamp the screenshot filename into the event) but still wants
//! the same cancel/drain semantics via `acquire`.

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router, ErrorData as McpError};

use crate::params::{BaselineCompareParams, ElementScreenshotParams, SessionIdParams};
use crate::UiTestServer;

/// Differing-pixel fraction allowed when `compare_element_to_baseline`
/// is called without an explicit `tolerance`. Small but non-zero so
/// antialias jitter on a faithful re-render doesn't read as a change.
const DEFAULT_BASELINE_TOLERANCE: f64 = 0.01;

#[tool_router(router = capture_router, vis = "pub(crate)")]
impl UiTestServer {
    #[tool(description = "Take a screenshot of the session and return the file path")]
    pub(crate) async fn take_screenshot(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        // take_screenshot doesn't fit run_action because log_event needs
        // the screenshot filename (for the viewer thumbnail). Use the
        // same acquire() primitive so we still get cancel/drain semantics.
        let held = self.acquire(&params.session_id).await?;

        let outcome: Result<PathBuf, String> = async {
            let png_bytes = held
                .session
                .take_screenshot()
                .await
                .map_err(|e| e.to_string())?;
            held.persist_screenshot(&params.session_id, &png_bytes)
                .await
                .map_err(|e| format!("persist screenshot: {e}"))
        }
        .await;
        let ok_display = outcome.as_ref().ok().map(|p| p.display().to_string());
        let screenshot_name = outcome
            .as_ref()
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string));
        let log_outcome = match (&ok_display, &outcome) {
            (Some(s), _) => Ok(s.as_str()),
            (None, Err(e)) => Err(e.as_str()),
            (None, Ok(_)) => unreachable!(),
        };
        if let Err(e) = held
            .log_event(
                &params.session_id,
                "take_screenshot",
                serde_json::json!({}),
                log_outcome,
                screenshot_name.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(take_screenshot) failed");
        }

        let path = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(
            path.display().to_string(),
        )]))
    }

    #[tool(
        description = "Screenshot just the element selected by XPath. Captures a full frame and \
                       crops to the element's AT-SPI bounds — useful for inspecting one widget \
                       at a time without the surrounding noise. The target must expose bounds \
                       (most widgets do; some pure-Generic accessibles don't)."
    )]
    pub(crate) async fn take_element_screenshot(
        &self,
        Parameters(params): Parameters<ElementScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        // Same shape as take_screenshot: bypass run_action so log_event
        // can stamp the cropped PNG's filename into the event for the
        // viewer thumbnail.
        let held = self.acquire(&params.session_id).await?;

        let xpath = params.xpath.clone();
        let outcome: Result<PathBuf, String> = async {
            let png_bytes = held
                .session
                .locate(&xpath)
                .screenshot()
                .await
                .map_err(|e| e.to_string())?;
            held.persist_screenshot(&params.session_id, &png_bytes)
                .await
                .map_err(|e| format!("persist screenshot: {e}"))
        }
        .await;
        let ok_display = outcome.as_ref().ok().map(|p| p.display().to_string());
        let screenshot_name = outcome
            .as_ref()
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string));
        let log_outcome = match (&ok_display, &outcome) {
            (Some(s), _) => Ok(s.as_str()),
            (None, Err(e)) => Err(e.as_str()),
            (None, Ok(_)) => unreachable!(),
        };
        if let Err(e) = held
            .log_event(
                &params.session_id,
                "take_element_screenshot",
                serde_json::json!({ "xpath": params.xpath }),
                log_outcome,
                screenshot_name.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(take_element_screenshot) failed");
        }

        let path = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(
            path.display().to_string(),
        )]))
    }

    #[tool(
        description = "Compare an element's current pixels against a committed reference PNG and \
                       return a perceptual diff SCORE — not a pass/fail verdict. Crops the element \
                       selected by XPath to its AT-SPI bounds and diffs it against `baseline_path` \
                       using per-pixel CIEDE2000 colour distance. Returns JSON with `score` (the \
                       fraction of perceptibly-differing pixels, [0,1]), `matched` (`score <= \
                       tolerance`, default tolerance 0.01), plus `mean_delta_e`, `max_delta_e`, \
                       `ncc`, `diff_pixels`, `total_pixels`, `width`, `height`. On a mismatch a \
                       diff image (changed pixels in red) is written next to the captured crop and \
                       returned as `diff_path`. waydriver never asserts, stores, or updates \
                       baselines — committing the reference and deciding pass/fail is the caller's \
                       job. Use `//Window` to compare the whole window. Same theme/DPI brittleness \
                       as `find_image`."
    )]
    pub(crate) async fn compare_element_to_baseline(
        &self,
        Parameters(params): Parameters<BaselineCompareParams>,
    ) -> Result<CallToolResult, McpError> {
        // Like take_element_screenshot, this captures + persists a PNG, so it
        // bypasses run_action to stamp the crop's filename into the event.
        let held = self.acquire(&params.session_id).await?;

        let xpath = params.xpath.clone();
        let baseline_path = params.baseline_path.clone();
        let tolerance = params.tolerance.unwrap_or(DEFAULT_BASELINE_TOLERANCE);

        let outcome: Result<(PathBuf, String), String> = async {
            let crop = held
                .session
                .locate(&xpath)
                .screenshot()
                .await
                .map_err(|e| e.to_string())?;
            let crop_path = held
                .persist_screenshot(&params.session_id, &crop)
                .await
                .map_err(|e| format!("persist screenshot: {e}"))?;

            let baseline = tokio::fs::read(&baseline_path)
                .await
                .map_err(|e| format!("read baseline {baseline_path:?}: {e}"))?;

            // Per-pixel CIEDE2000 is CPU-bound — keep it off the runtime.
            let cmp = {
                let (crop, baseline) = (crop.clone(), baseline.clone());
                tokio::task::spawn_blocking(move || {
                    waydriver::visual::compare_to_baseline(&crop, &baseline, tolerance)
                })
                .await
                .map_err(|e| format!("compare task panicked: {e}"))?
                .map_err(|e| e.to_string())?
            };

            let mut payload = serde_json::json!({
                "matched": cmp.matched,
                "score": cmp.score,
                "mean_delta_e": cmp.mean_delta_e,
                "max_delta_e": cmp.max_delta_e,
                "ncc": cmp.ncc,
                "diff_pixels": cmp.diff_pixels,
                "total_pixels": cmp.total_pixels,
                "width": cmp.width,
                "height": cmp.height,
                "tolerance": cmp.tolerance,
            });

            if !cmp.matched {
                // Best-effort diff artifact next to the crop; a failure to
                // render or write it must not fail the comparison itself.
                let diff = tokio::task::spawn_blocking(move || {
                    waydriver::visual::diff_to_baseline(&crop, &baseline)
                })
                .await;
                match diff {
                    Ok(Ok(diff_png)) => {
                        let stem = crop_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("crop");
                        let diff_path = crop_path.with_file_name(format!("{stem}-diff.png"));
                        match tokio::fs::write(&diff_path, &diff_png).await {
                            Ok(()) => {
                                payload["diff_path"] =
                                    serde_json::Value::String(diff_path.display().to_string());
                            }
                            Err(e) => tracing::warn!(error = %e, "write diff image failed"),
                        }
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "render diff image failed"),
                    Err(e) => tracing::warn!(error = %e, "diff task panicked"),
                }
            }

            let json = serde_json::to_string_pretty(&payload)
                .map_err(|e| format!("serialize result: {e}"))?;
            Ok((crop_path, json))
        }
        .await;

        let screenshot_name = outcome
            .as_ref()
            .ok()
            .and_then(|(p, _)| p.file_name().and_then(|n| n.to_str()).map(str::to_string));
        let log_outcome = match &outcome {
            Ok((_, json)) => Ok(json.as_str()),
            Err(e) => Err(e.as_str()),
        };
        if let Err(e) = held
            .log_event(
                &params.session_id,
                "compare_element_to_baseline",
                serde_json::json!({
                    "xpath": params.xpath,
                    "baseline_path": params.baseline_path,
                    "tolerance": tolerance,
                }),
                log_outcome,
                screenshot_name.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(compare_element_to_baseline) failed");
        }

        let (_, json) = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}
