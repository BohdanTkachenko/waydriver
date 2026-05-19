//! Read-only accessibility-tree inspection: `dump_tree`, `query`, `read_text`.
//!
//! Every tool here is a thin dispatcher to `run_action`. The bodies
//! only appear to add ceremony because the `#[tool(description = ...)]`
//! attribute has to carry the full prose for MCP clients to show the
//! user.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData as McpError};

use crate::params::{
    FindTextParams, FindTextRegionsParams, ImageMatchParams, ListTextParams, QueryParams,
    ReadTextParams, SessionIdParams, WaitForStdoutLineParams,
};
use crate::report::render_matches;
use crate::UiTestServer;

/// Default XPath scope for OCR-based inspection tools (`list_text`):
/// the first element that exposes AT-SPI bounds, which is the
/// toplevel widget area in every tested toolkit. Mirrors the same
/// idiom the visual-locator e2e tests use as a "whole screen" scope.
const DEFAULT_OCR_SCOPE: &str = "//*[@bbox][1]";

fn bbox_json(r: &waydriver::Rect) -> serde_json::Value {
    serde_json::json!({
        "x": r.x,
        "y": r.y,
        "width": r.width,
        "height": r.height,
    })
}

#[tool_router(router = inspection_router, vis = "pub(crate)")]
impl UiTestServer {
    #[tool(
        description = "Dump the accessibility tree of the application UI as XML. Use this to \
                       discover selector-ready role names, attributes, and element hierarchy \
                       before writing XPath queries for `query` or `click`."
    )]
    pub(crate) async fn dump_tree(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_action(
            &params.session_id,
            "dump_tree",
            serde_json::json!({}),
            |s| async move { s.dump_tree().await },
        )
        .await
    }

    #[tool(
        description = "Query the accessibility tree with an XPath selector. Returns a JSON \
                       array of matches; each element carries a pinned `xpath` that can be \
                       passed back to `click` / `set_text` / `read_text` to target that \
                       specific ordinal match. Names are not unique, so prefer more specific \
                       selectors (role + attribute) over pure name matches."
    )]
    pub(crate) async fn query(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "query",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move {
                let matches = s.locate(&xpath).inspect_all().await?;
                // serde_json failure on a value we just constructed is
                // essentially impossible, but a typed error is cheaper
                // than a panic — wrap it as an infra failure.
                serde_json::to_string_pretty(&render_matches(&xpath, &matches))
                    .map_err(|e| waydriver::Error::process_with("serialize query result", e))
            },
        )
        .await
    }

    #[tool(
        description = "Read the text contents of an element selected by XPath. Target must \
                       implement the Text AT-SPI interface."
    )]
    pub(crate) async fn read_text(
        &self,
        Parameters(params): Parameters<ReadTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "read_text",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move { s.locate(&xpath).text().await },
        )
        .await
    }

    #[tool(
        description = "Find a single line of on-screen text via OCR and return its bounding box. \
                       Use to inspect where a label is before deciding what to do (e.g. compute a \
                       coordinate offset from it, or check whether it's painted at all). Doesn't \
                       click — pair with `click_by_text` for that. Returns JSON of shape \
                       `{found: bool, text: string, bounds: {x, y, width, height}}` on hit, or \
                       `{found: false}` otherwise. Substring match, case-insensitive. \
                       `scope_xpath` restricts the OCR region; omit to search the whole screen."
    )]
    pub(crate) async fn find_text(
        &self,
        Parameters(params): Parameters<FindTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = params.text.clone();
        let scope_xpath = params.scope_xpath.clone();
        let match_mode = params.match_mode.unwrap_or_default();
        self.run_action(
            &params.session_id,
            "find_text",
            serde_json::json!({
                "text": params.text,
                "scope_xpath": params.scope_xpath,
                "match_mode": params.match_mode,
            }),
            |s| async move {
                let locator = match &scope_xpath {
                    Some(xpath) => s.locate(xpath).find_by_text(&text).await?,
                    None => s.find_by_text(&text),
                };
                let locator = locator.with_match_mode(match_mode.to_waydriver());
                // count() short-circuits the wait loop — bounds() would
                // poll until timeout when the text is genuinely absent,
                // and "missing" is a legitimate answer here, not an error.
                let n = locator.count().await?;
                let payload = if n == 0 {
                    serde_json::json!({ "found": false })
                } else {
                    let r = locator.bounds().await?;
                    serde_json::json!({
                        "found": true,
                        "text": text,
                        "bounds": bbox_json(&r),
                    })
                };
                serde_json::to_string_pretty(&payload)
                    .map_err(|e| waydriver::Error::process_with("serialize find_text result", e))
            },
        )
        .await
    }

    #[tool(
        description = "Enumerate the full outer→inner chain of widget shapes (button pill, row, \
                       card, dialog area) that enclose an OCR'd label. Returns a JSON array of \
                       `{index, bounds, centroid: {x, y}, shape}` ordered with index 0 = \
                       outermost (parent-adjacent ring) and the last entry = innermost (tightest \
                       region around the text). \
                       Pair with `click_text_region` and pass `region_index` to click a specific \
                       level — useful when the typical innermost click hits the wrong widget \
                       (e.g. clicks the label's frame instead of the row that owns activation). \
                       `shape` is one of `Rectangle`, `Pill`, `Ellipse`, or `Irregular` (coarse \
                       classification via fill-ratio + corner-inside test)."
    )]
    pub(crate) async fn find_text_regions(
        &self,
        Parameters(params): Parameters<FindTextRegionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let scope_xpath = params.scope_xpath.clone();
        let text = params.text.clone();
        let match_mode = params.match_mode.unwrap_or_default();
        self.run_action(
            &params.session_id,
            "find_text_regions",
            serde_json::json!({
                "text": params.text,
                "scope_xpath": params.scope_xpath,
                "match_mode": params.match_mode,
            }),
            |s| async move {
                let scope = s.locate(&scope_xpath);
                let inner = scope
                    .find_by_text(&text)
                    .await?
                    .with_match_mode(match_mode.to_waydriver());
                let regions = scope.find_regions(&inner).await?;
                let payload: Vec<_> = regions
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let (cx, cy) = r.centroid();
                        serde_json::json!({
                            "index": i,
                            "bounds": bbox_json(&r.bounds()),
                            "centroid": { "x": cx, "y": cy },
                            "shape": format!("{:?}", r.shape()),
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&payload).map_err(|e| {
                    waydriver::Error::process_with("serialize find_text_regions result", e)
                })
            },
        )
        .await
    }

    #[tool(
        description = "Enumerate every line of text OCR can recognise inside the scope. Returns \
                       a JSON array of `{text, bounds}` entries (one per line; words joined with \
                       spaces, bbox is the union over all words in the line). Useful for \
                       discovery — \"what labels are visible right now?\" — before deciding \
                       which one to click. \
                       `scope_xpath` defaults to the toplevel widget area; pass a tighter \
                       selector for faster, more accurate OCR."
    )]
    pub(crate) async fn list_text(
        &self,
        Parameters(params): Parameters<ListTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let scope_xpath = params
            .scope_xpath
            .clone()
            .unwrap_or_else(|| DEFAULT_OCR_SCOPE.to_string());
        self.run_action(
            &params.session_id,
            "list_text",
            serde_json::json!({ "scope_xpath": params.scope_xpath }),
            |s| async move {
                let hits = s.locate(&scope_xpath).list_text().await?;
                let payload: Vec<_> = hits
                    .into_iter()
                    .map(|h| {
                        serde_json::json!({
                            "text": h.text,
                            "bounds": bbox_json(&h.bounds),
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&payload)
                    .map_err(|e| waydriver::Error::process_with("serialize list_text result", e))
            },
        )
        .await
    }

    #[tool(
        description = "Find a reference PNG on screen via template matching (normalized \
                       cross-correlation) and return its bounding box without clicking. Use to \
                       inspect whether an icon is present and where, before calling `click_image` \
                       or deciding on another action. Returns JSON `{found: bool, bounds: \
                       {x, y, width, height}}`. \
                       Same brittleness caveat as `click_image` (theme/DPI sensitive). Optional \
                       `scope_xpath` crops the search; `threshold` is the NCC cutoff in [0, 1] \
                       (library default 0.9)."
    )]
    pub(crate) async fn find_image(
        &self,
        Parameters(params): Parameters<ImageMatchParams>,
    ) -> Result<CallToolResult, McpError> {
        let png_path = params.png_path.clone();
        let scope_xpath = params.scope_xpath.clone();
        let threshold = params.threshold;
        self.run_action(
            &params.session_id,
            "find_image",
            serde_json::json!({
                "png_path": params.png_path,
                "scope_xpath": params.scope_xpath,
                "threshold": params.threshold,
            }),
            |s| async move {
                let png_bytes = std::fs::read(&png_path).map_err(|e| {
                    waydriver::Error::process_with(format!("read PNG {png_path:?}"), e)
                })?;
                let mut locator = match &scope_xpath {
                    Some(xpath) => s.locate(xpath).find_image(&png_bytes).await?,
                    None => s.find_image(&png_bytes)?,
                };
                if let Some(t) = threshold {
                    locator = locator.with_threshold(t);
                }
                let n = locator.count().await?;
                let payload = if n == 0 {
                    serde_json::json!({ "found": false })
                } else {
                    let r = locator.bounds().await?;
                    serde_json::json!({
                        "found": true,
                        "bounds": bbox_json(&r),
                    })
                };
                serde_json::to_string_pretty(&payload)
                    .map_err(|e| waydriver::Error::process_with("serialize find_image result", e))
            },
        )
        .await
    }

    #[tool(
        description = "Wait for a line containing `contains` to appear on the launched \
                       application's stdout. Returns the matched line. Useful as a ground-truth \
                       gate between AT-SPI events and observable application behavior — many \
                       tests want to confirm that a click handler actually ran, not just that \
                       the AT-SPI Action method returned. \
                       `timeout_ms` defaults to 5000. \
                       `after` is the buffer-position cursor used to ignore older matches: pass \
                       0 to scan from the start, omit to ignore everything currently in the \
                       buffer (only lines emitted from now on count). Captures `println!` and \
                       `eprintln!` from the child process when it flushes."
    )]
    pub(crate) async fn wait_for_stdout_line(
        &self,
        Parameters(params): Parameters<WaitForStdoutLineParams>,
    ) -> Result<CallToolResult, McpError> {
        let contains = params.contains.clone();
        let timeout = Duration::from_millis(params.timeout_ms.unwrap_or(5000));
        let after_opt = params.after;
        self.run_action(
            &params.session_id,
            "wait_for_stdout_line",
            serde_json::json!({
                "contains": params.contains,
                "timeout_ms": params.timeout_ms,
                "after": params.after,
            }),
            |s| async move {
                // None = "from now on" — snapshot the current end of buffer
                // so prior history doesn't count. The Session API requires
                // a concrete cursor, so we resolve None here rather than
                // pushing the lazy snapshot into waydriver.
                let after = after_opt.unwrap_or_else(|| s.stdout_cursor());
                let needle = contains.clone();
                s.wait_for_stdout_line(after, |line| line.contains(&needle), timeout)
                    .await
            },
        )
        .await
    }
}
