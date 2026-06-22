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
    ReadTextParams, ReadValueParams, SessionIdParams, WaitForStdoutLineParams,
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

fn notification_json(n: &waydriver::CapturedNotification) -> serde_json::Value {
    serde_json::json!({
        "seq": n.seq,
        "app_name": n.app_name,
        "replaces_id": n.replaces_id,
        "app_icon": n.app_icon,
        "summary": n.summary,
        "body": n.body,
        "actions": n.actions,
        "hints": n.hints,
        "expire_timeout": n.expire_timeout,
        "id": n.id,
    })
}

fn open_uri_json(o: &waydriver::CapturedOpenUri) -> serde_json::Value {
    serde_json::json!({
        "seq": o.seq,
        "parent_window": o.parent_window,
        "uri": o.uri,
        "options": o.options,
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "dump_tree",
            serde_json::json!({ "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, false),
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "query",
            serde_json::json!({ "xpath": params.xpath, "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, true),
            |s| async move {
                let matches = crate::tools::locate(&s, &xpath, wait).inspect_all().await?;
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "read_text",
            serde_json::json!({ "xpath": params.xpath, "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, true),
            |s| async move { crate::tools::locate(&s, &xpath, wait).text().await },
        )
        .await
    }

    #[tool(
        description = "Read the AT-SPI Value of an element selected by XPath: its current \
                       position plus the range it moves within. The headline use is reading a \
                       scrolled view's offset — which AT-SPI exposes nowhere else — by locating \
                       the scroll bar inside the scrolled window; also works for any slider, \
                       progress bar, or spin button. Returns JSON \
                       `{current, minimum, maximum, minimum_increment}`. Target must implement \
                       the Value AT-SPI interface."
    )]
    pub(crate) async fn read_value(
        &self,
        Parameters(params): Parameters<ReadValueParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "read_value",
            serde_json::json!({ "xpath": params.xpath, "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, true),
            |s| async move {
                let v = crate::tools::locate(&s, &xpath, wait).value().await?;
                serde_json::to_string_pretty(&serde_json::json!({
                    "current": v.current,
                    "minimum": v.minimum,
                    "maximum": v.maximum,
                    "minimum_increment": v.minimum_increment,
                }))
                .map_err(|e| waydriver::Error::process_with("serialize read_value result", e))
            },
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "find_text",
            serde_json::json!({
                "text": params.text,
                "scope_xpath": params.scope_xpath,
                "match_mode": params.match_mode,
                "timeout_ms": params.timeout.timeout_ms,
            }),
            self.op_budget(wait, false),
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "find_text_regions",
            serde_json::json!({
                "text": params.text,
                "scope_xpath": params.scope_xpath,
                "match_mode": params.match_mode,
                "timeout_ms": params.timeout.timeout_ms,
            }),
            self.op_budget(wait, false),
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "list_text",
            serde_json::json!({ "scope_xpath": params.scope_xpath, "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, false),
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
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "find_image",
            serde_json::json!({
                "png_path": params.png_path,
                "scope_xpath": params.scope_xpath,
                "threshold": params.threshold,
                "timeout_ms": params.timeout.timeout_ms,
            }),
            self.op_budget(wait, false),
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
        // This op intentionally waits up to `timeout_ms` (default 5000) for a
        // matching line, which can exceed the server-wide op budget. Extend the
        // budget by that wait so the backstop only bounds the surrounding
        // infrastructure, never the intended wait — the inner wait already
        // returns Error::Timeout at its own deadline.
        let op_budget = self.default_op_timeout + timeout;
        self.run_action_within(
            &params.session_id,
            "wait_for_stdout_line",
            serde_json::json!({
                "contains": params.contains,
                "timeout_ms": params.timeout_ms,
                "after": params.after,
            }),
            op_budget,
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

    #[tool(
        description = "Read the external effects the app has emitted onto the session bus — \
                       desktop notifications and portal open-URI requests — captured by mock \
                       D-Bus sinks. These have no AT-SPI projection (they leave the process to a \
                       daemon), so this is the only way to assert on them. Returns JSON \
                       `{capture_enabled, notifications: [{seq, app_name, summary, body, actions, \
                       hints, expire_timeout, replaces_id, app_icon, id}], open_uri_requests: \
                       [{seq, uri, parent_window, options}]}`. \
                       Requires the session to have been started with \
                       `capture_external_effects: true` (else `capture_enabled` is false and the \
                       arrays are empty). The app posts these asynchronously, so trigger the \
                       action, then poll this (or pair with `wait_for_stdout_line` on a log line \
                       the app emits) — entries accumulate for the session's lifetime."
    )]
    pub(crate) async fn get_captured_effects(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "get_captured_effects",
            serde_json::json!({ "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, false),
            |s| async move {
                let payload = if s.external_effects_enabled() {
                    let notifications: Vec<_> =
                        s.notifications()?.iter().map(notification_json).collect();
                    let open_uri_requests: Vec<_> =
                        s.open_uri_requests()?.iter().map(open_uri_json).collect();
                    serde_json::json!({
                        "capture_enabled": true,
                        "notifications": notifications,
                        "open_uri_requests": open_uri_requests,
                    })
                } else {
                    serde_json::json!({
                        "capture_enabled": false,
                        "notifications": [],
                        "open_uri_requests": [],
                    })
                };
                serde_json::to_string_pretty(&payload).map_err(|e| {
                    waydriver::Error::process_with("serialize get_captured_effects result", e)
                })
            },
        )
        .await
    }

    #[tool(
        description = "List the prefixed names (`app.*`, `win.*`) of every GTK GAction the app \
                       exposes over the `org.gtk.Actions` D-Bus interface. Discovery companion \
                       to `activate_action`: GActions report names but no human-readable labels, \
                       so this shows which actions exist, not which menu item each one backs. \
                       Returns a JSON array of strings."
    )]
    pub(crate) async fn list_actions(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let wait = params.timeout.timeout_ms;
        self.run_action_within(
            &params.session_id,
            "list_actions",
            serde_json::json!({ "timeout_ms": params.timeout.timeout_ms }),
            self.op_budget(wait, false),
            |s| async move {
                let actions = s.list_actions().await?;
                // serde_json failure on a Vec<String> we just built is
                // essentially impossible; map it as infra rather than panic.
                serde_json::to_string_pretty(&actions)
                    .map_err(|e| waydriver::Error::process_with("serialize actions", e))
            },
        )
        .await
    }
}
