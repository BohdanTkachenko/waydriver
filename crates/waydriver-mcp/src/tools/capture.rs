//! Screen-capture tools. Just `take_screenshot` today; a separate
//! module because it doesn't fit `run_action`'s shape (log_event has
//! to stamp the screenshot filename into the event) but still wants
//! the same cancel/drain semantics via `acquire`.

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router, ErrorData as McpError};

use crate::params::SessionIdParams;
use crate::UiTestServer;

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
}
