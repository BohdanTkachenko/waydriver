//! User-interaction tools: every way the MCP server sends keyboard or
//! pointer events to the running application.
//!
//! Two shapes live here:
//!
//! - **Locator-driven** (click, focus, hover, double_click, right_click,
//!   drag_to, set_text, fill, select_option) resolve an XPath selector
//!   first, then act on the matched element. These auto-wait on the
//!   element via `Locator`'s polling layer.
//! - **Direct** (type_text, press_key, move_pointer, pointer_click)
//!   send events without a selector, targeting whatever already has
//!   focus or is under the pointer. Use these after setting focus or
//!   when the target is implicit.
//!
//! The distinction is an implementation detail from the caller's
//! perspective — both groups are "send input" — so they share one
//! router. Read-only inspection lives in [`super::inspection`] and
//! screen capture in [`super::capture`].

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData as McpError};

use waydriver::keysym::parse_chord;

use crate::params::{
    ClickByTextParams, ClickParams, ClickTextRegionParams, DoubleClickParams, DragToParams,
    FillParams, FocusParams, HoverParams, ImageMatchParams, LaunchSecondaryParams,
    MovePointerAbsoluteParams, MovePointerParams, PointerClickParams, PressKeyParams,
    RightClickParams, ScrollParams, SelectOptionByParam, SelectOptionParams, SetTextParams,
    TypeTextParams,
};
use crate::UiTestServer;

/// Boilerplate body for the five isomorphic single-XPath locator tools
/// (click/focus/hover/double_click/right_click). All follow the same
/// shape — clone the xpath, route through `run_action`, call one method
/// on the resolved `Locator`, and format a past-tense success message.
/// Expressing it once as a macro keeps each tool method to a single
/// line and removes the chance of one drifting from the others.
macro_rules! single_xpath_action {
    (
        $self:ident, $params:ident,
        action = $action:literal,
        verb = $verb:literal,
        call = $method:ident
    ) => {{
        let xpath = $params.xpath.clone();
        $self.run_action(
            &$params.session_id,
            $action,
            serde_json::json!({ "xpath": $params.xpath }),
            |s| async move {
                s.locate(&xpath)
                    .$method()
                    .await
                    .map(|_| format!(concat!($verb, " {}"), xpath))
            },
        )
        .await
    }};
}

#[tool_router(router = interaction_router, vis = "pub(crate)")]
impl UiTestServer {
    // ── Locator-driven ─────────────────────────────────────────────────

    #[tool(
        description = "Click a UI element selected by XPath. The selector must resolve to \
                       exactly one element; if it matches multiple, use `query` first and \
                       pass the pinned `xpath` back, or refine the selector."
    )]
    pub(crate) async fn click(
        &self,
        Parameters(params): Parameters<ClickParams>,
    ) -> Result<CallToolResult, McpError> {
        single_xpath_action!(
            self,
            params,
            action = "click",
            verb = "Clicked",
            call = click
        )
    }

    #[tool(
        description = "Give keyboard focus to the element selected by XPath. The selector must \
                       resolve to exactly one focusable element. Use this before sending \
                       keyboard input via `type_text` or `press_key` when you need the input \
                       to land on a specific widget."
    )]
    pub(crate) async fn focus(
        &self,
        Parameters(params): Parameters<FocusParams>,
    ) -> Result<CallToolResult, McpError> {
        single_xpath_action!(
            self,
            params,
            action = "focus",
            verb = "Focused",
            call = focus
        )
    }

    #[tool(
        description = "Move the pointer to the centre of the element selected by XPath without \
                       clicking. Use to reveal hover-only UI like tooltips or slide-out menus."
    )]
    pub(crate) async fn hover(
        &self,
        Parameters(params): Parameters<HoverParams>,
    ) -> Result<CallToolResult, McpError> {
        single_xpath_action!(
            self,
            params,
            action = "hover",
            verb = "Hovered",
            call = hover
        )
    }

    #[tool(
        description = "Double-click the element selected by XPath with the primary mouse button. \
                       Synthesizes two rapid pointer clicks at the element's centre so toolkits \
                       see a real double-click (unlike `click`, which routes through AT-SPI)."
    )]
    pub(crate) async fn double_click(
        &self,
        Parameters(params): Parameters<DoubleClickParams>,
    ) -> Result<CallToolResult, McpError> {
        single_xpath_action!(
            self,
            params,
            action = "double_click",
            verb = "Double-clicked",
            call = double_click
        )
    }

    #[tool(
        description = "Right-click the element selected by XPath, typically opening the widget's \
                       context menu."
    )]
    pub(crate) async fn right_click(
        &self,
        Parameters(params): Parameters<RightClickParams>,
    ) -> Result<CallToolResult, McpError> {
        single_xpath_action!(
            self,
            params,
            action = "right_click",
            verb = "Right-clicked",
            call = right_click
        )
    }

    #[tool(
        description = "Drag the element selected by `source_xpath` onto the element selected by \
                       `target_xpath` with the primary mouse button held. Both selectors must \
                       resolve to exactly one element."
    )]
    pub(crate) async fn drag_to(
        &self,
        Parameters(params): Parameters<DragToParams>,
    ) -> Result<CallToolResult, McpError> {
        let source_xpath = params.source_xpath.clone();
        let target_xpath = params.target_xpath.clone();
        self.run_action(
            &params.session_id,
            "drag_to",
            serde_json::json!({
                "source_xpath": params.source_xpath,
                "target_xpath": params.target_xpath,
            }),
            |s| async move {
                let source = s.locate(&source_xpath);
                let target = s.locate(&target_xpath);
                source
                    .drag_to(&target)
                    .await
                    .map(|_| format!("Dragged {source_xpath} to {target_xpath}"))
            },
        )
        .await
    }

    #[tool(
        description = "Replace the editable-text contents of an element selected by XPath. \
                       Target must implement the EditableText AT-SPI interface."
    )]
    pub(crate) async fn set_text(
        &self,
        Parameters(params): Parameters<SetTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        let text = params.text.clone();
        self.run_action(
            &params.session_id,
            "set_text",
            serde_json::json!({ "xpath": params.xpath, "text": params.text }),
            |s| async move {
                s.locate(&xpath)
                    .set_text(&text)
                    .await
                    .map(|_| format!("Set text on {xpath}"))
            },
        )
        .await
    }

    #[tool(
        description = "Replace text contents by simulating keyboard input: focus the element, \
                       clear existing content, then type. Works on any standard text widget \
                       — including GtkTextView and others that don't implement EditableText. \
                       Prefer set_text when the target supports it (one D-Bus call); use \
                       fill as the compatibility fallback. \
                       `mode`: \"caret_nav\" (default; Ctrl+Home then Ctrl+Shift+End) or \
                       \"select_all\" (Ctrl+A — faster when the app honors it). \
                       `assume_focused`: skip the AT-SPI focus call and trust the caller to \
                       have focused the widget already (via a prior click/focus). Required \
                       for GTK4 text widgets that don't implement the Component interface \
                       and would otherwise error with NotSupported."
    )]
    pub(crate) async fn fill(
        &self,
        Parameters(params): Parameters<FillParams>,
    ) -> Result<CallToolResult, McpError> {
        // Mode validation has moved into the JSON Schema: `FillModeParam`
        // is a serde-typed enum, so a request with an unknown string
        // is rejected at deserialise time before this body ever runs.
        // Default to the documented `caret_nav` when the caller omits
        // the field.
        let mode = params.mode.unwrap_or_default().to_waydriver();
        let assume_focused = params.assume_focused;

        let xpath = params.xpath.clone();
        let text = params.text.clone();
        self.run_action(
            &params.session_id,
            "fill",
            serde_json::json!({
                "xpath": params.xpath,
                "text": params.text,
                "mode": params.mode,
                "assume_focused": params.assume_focused,
            }),
            |s| async move {
                let locator = s.locate(&xpath);
                let result = if assume_focused {
                    locator.fill_assume_focused(&text, mode).await
                } else {
                    locator.fill_with_opts(&text, mode).await
                };
                result.map(|_| format!("Filled {xpath}"))
            },
        )
        .await
    }

    #[tool(
        description = "Pick an option in a combobox, dropdown, or other AT-SPI Selection \
                       container. Calls Selection::select_child on the located element — much \
                       faster and less flaky than clicking the widget open and clicking the \
                       item. `by`: \"label\" (matches the option's accessible name) or \
                       \"index\" (parses `value` as a 0-indexed integer). Container must \
                       implement the Selection interface."
    )]
    pub(crate) async fn select_option(
        &self,
        Parameters(params): Parameters<SelectOptionParams>,
    ) -> Result<CallToolResult, McpError> {
        // `by` is now a serde-typed enum (`SelectOptionByParam`), so an
        // unknown discriminator is rejected at JSON-Schema validation.
        // What's left is the index-string path: when by == Index, the
        // accompanying `value` must parse as a non-negative integer.
        // That's still caller error, not infra failure, so it returns
        // `invalid_params` rather than running through `run_action`.
        enum ParsedBy {
            Label(String),
            Index(usize),
        }
        let parsed = match params.by {
            SelectOptionByParam::Label => ParsedBy::Label(params.value.clone()),
            SelectOptionByParam::Index => params
                .value
                .parse::<usize>()
                .map(ParsedBy::Index)
                .map_err(|e| {
                    McpError::invalid_params(format!("invalid index {:?}: {e}", params.value), None)
                })?,
        };

        let xpath = params.xpath.clone();
        let by = params.by;
        let value = params.value.clone();
        self.run_action(
            &params.session_id,
            "select_option",
            serde_json::json!({
                "xpath": params.xpath,
                "by": params.by,
                "value": params.value,
            }),
            |s| async move {
                let selector = match &parsed {
                    ParsedBy::Label(name) => waydriver::SelectBy::Label(name.as_str()),
                    ParsedBy::Index(i) => waydriver::SelectBy::Index(*i),
                };
                s.locate(&xpath)
                    .select_option(selector)
                    .await
                    .map(|_| format!("Selected {by}={value:?} on {xpath}"))
            },
        )
        .await
    }

    #[tool(
        description = "Scroll a located area by mouse-wheel detents. Parks the pointer over the \
                       centre of the element selected by `xpath`, then emits `steps` wheel \
                       notches along `axis` (\"vertical\" default, or \"horizontal\"). Positive \
                       `steps` scroll down / right, negative up / left. Drive a scroll area or \
                       list with it and pair with `read_value` on the scroll bar to confirm the \
                       offset moved; over-scroll (more detents than the content is long) to park \
                       at an edge. For keyboard scrollback (e.g. a terminal's Shift+Page_Up) use \
                       `press_key` instead — this sends real wheel input over the widget."
    )]
    pub(crate) async fn scroll(
        &self,
        Parameters(params): Parameters<ScrollParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        let axis = params.axis;
        let steps = params.steps;
        self.run_action(
            &params.session_id,
            "scroll",
            serde_json::json!({
                "xpath": params.xpath,
                "axis": params.axis,
                "steps": params.steps,
            }),
            |s| async move {
                s.locate(&xpath)
                    .scroll(axis.to_waydriver(), steps)
                    .await
                    .map(|_| format!("Scrolled {xpath} by {steps} {axis:?} detent(s)"))
            },
        )
        .await
    }

    // ── Direct (selector-less) ─────────────────────────────────────────

    #[tool(description = "Type text into the currently focused element via keyboard input")]
    pub(crate) async fn type_text(
        &self,
        Parameters(params): Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = params.text.clone();
        self.run_action(
            &params.session_id,
            "type_text",
            serde_json::json!({ "text": params.text }),
            |s| async move { s.type_text(&text).await.map(|_| format!("Typed '{text}'")) },
        )
        .await
    }

    #[tool(
        description = "Press a keyboard key or chord. Accepts either a single-key name \
                       ('Return', 'Tab', 'a') or a modifier combo ('Ctrl+A', 'Shift+Tab', \
                       'Ctrl+Shift+Alt+F1'). Modifier aliases: Ctrl=Control, Super=Meta=Win=Cmd. \
                       Separator can be '+' or '-'. Case-insensitive."
    )]
    pub(crate) async fn press_key(
        &self,
        Parameters(params): Parameters<PressKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        // Validate the chord string up front so an unparseable input
        // surfaces as invalid_params (caller error), not internal_error.
        // press_chord would also reject it but with a less specific code.
        if parse_chord(&params.key).is_none() {
            return Err(McpError::invalid_params(
                format!("unknown key: {}", params.key),
                None,
            ));
        }

        let key = params.key.clone();
        self.run_action(
            &params.session_id,
            "press_key",
            serde_json::json!({ "key": params.key }),
            |s| async move {
                s.press_chord(&key)
                    .await
                    .map(|_| format!("Pressed '{key}'"))
            },
        )
        .await
    }

    #[tool(description = "Move the pointer by a relative offset in logical pixels")]
    pub(crate) async fn move_pointer(
        &self,
        Parameters(params): Parameters<MovePointerParams>,
    ) -> Result<CallToolResult, McpError> {
        let dx = params.dx;
        let dy = params.dy;
        self.run_action(
            &params.session_id,
            "move_pointer",
            serde_json::json!({ "dx": params.dx, "dy": params.dy }),
            |s| async move {
                s.pointer_motion_relative(dx, dy)
                    .await
                    .map(|_| format!("Pointer moved by ({dx}, {dy})"))
            },
        )
        .await
    }

    #[tool(
        description = "Move the pointer to an absolute screen coordinate. Use when you have a \
                       pre-computed pixel (e.g. from a screenshot inspection) and want to point \
                       at it directly rather than offsetting from the current position. \
                       Coordinates are screen-relative logical pixels matching the session's \
                       resolution."
    )]
    pub(crate) async fn move_pointer_absolute(
        &self,
        Parameters(params): Parameters<MovePointerAbsoluteParams>,
    ) -> Result<CallToolResult, McpError> {
        let x = params.x;
        let y = params.y;
        self.run_action(
            &params.session_id,
            "move_pointer_absolute",
            serde_json::json!({ "x": params.x, "y": params.y }),
            |s| async move {
                s.pointer_motion_absolute(x, y)
                    .await
                    .map(|_| format!("Pointer moved to ({x}, {y})"))
            },
        )
        .await
    }

    #[tool(
        description = "Click a UI element by its visible on-screen text via OCR. Use as a \
                       fallback when the target widget isn't in the AT-SPI tree but its label is \
                       rendered (e.g. libadwaita lazy-realization bugs, custom-painted toolkits). \
                       Slower than AT-SPI `click` (~200-500ms for OCR + screenshot, plus a one- \
                       time ~1-2s ocrs model load on first call) — reach for it only when AT-SPI \
                       can't reach the widget. \
                       Optional `scope_xpath` restricts OCR to that element's AT-SPI bounds — \
                       smaller crop = faster + fewer false positives. Whole row of an \
                       AdwSwitchRow / AdwButtonRow activates on any-point click, so OCR-clicking \
                       the title text reliably toggles/triggers the row. \
                       Requires the `visual` Cargo feature (always enabled in this server build)."
    )]
    pub(crate) async fn click_by_text(
        &self,
        Parameters(params): Parameters<ClickByTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = params.text.clone();
        let scope_xpath = params.scope_xpath.clone();
        let match_mode = params.match_mode.unwrap_or_default();
        self.run_action(
            &params.session_id,
            "click_by_text",
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
                locator
                    .with_match_mode(match_mode.to_waydriver())
                    .click()
                    .await?;
                Ok(match scope_xpath {
                    Some(xpath) => format!("Clicked text {text:?} inside {xpath}"),
                    None => format!("Clicked text {text:?}"),
                })
            },
        )
        .await
    }

    #[tool(
        description = "Find on-screen text via OCR, then flood-fill the enclosing widget shape \
                       (button pill, row background, card frame) and click its centroid — not \
                       the text glyphs. More reliable than `click_by_text` for widgets that \
                       route activation to the row/pill rather than the label, e.g. AdwButtonRow \
                       or AdwSwitchRow. Requires `scope_xpath` (a parent element with AT-SPI \
                       bounds) because the flood-fill needs a known starting region — typically \
                       the surrounding dialog or pane. \
                       Heavier than `click_by_text` (one extra flood-fill per call); reach for \
                       it when text-bbox clicking doesn't activate the widget. \
                       `region_index` picks a specific level of the outer→inner enclosing-region \
                       chain (call `find_text_regions` first to see what's available). Omit to \
                       click the innermost region — the typical row/pill activation pattern."
    )]
    pub(crate) async fn click_text_region(
        &self,
        Parameters(params): Parameters<ClickTextRegionParams>,
    ) -> Result<CallToolResult, McpError> {
        let scope_xpath = params.scope_xpath.clone();
        let text = params.text.clone();
        let region_index = params.region_index;
        let match_mode = params.match_mode.unwrap_or_default();
        self.run_action(
            &params.session_id,
            "click_text_region",
            serde_json::json!({
                "text": params.text,
                "scope_xpath": params.scope_xpath,
                "region_index": params.region_index,
                "match_mode": params.match_mode,
            }),
            |s| async move {
                let scope = s.locate(&scope_xpath);
                let inner = scope
                    .find_by_text(&text)
                    .await?
                    .with_match_mode(match_mode.to_waydriver());
                match region_index {
                    None => {
                        // Default: innermost (last_region — cheap, one flood-fill).
                        scope.last_region(&inner).await?.click().await?;
                        Ok(format!(
                            "Clicked innermost region of text {text:?} inside {scope_xpath}"
                        ))
                    }
                    Some(i) => {
                        // Indexed: full chain walk needed.
                        let regions = scope.find_regions(&inner).await?;
                        let total = regions.len();
                        let chosen = regions.into_iter().nth(i).ok_or_else(|| {
                            waydriver::Error::visual(format!(
                                "region_index {i} out of range — chain has {total} region(s); \
                                 call find_text_regions to inspect"
                            ))
                        })?;
                        chosen.click().await?;
                        Ok(format!(
                            "Clicked region {i}/{total} of text {text:?} inside {scope_xpath}"
                        ))
                    }
                }
            },
        )
        .await
    }

    #[tool(
        description = "Click a UI element by matching a reference PNG against the current \
                       screenshot (template matching via normalized cross-correlation). Use for \
                       icon-only buttons that have no on-screen text and aren't queryable via \
                       AT-SPI. The reference image is read from `png_path` and matched at full \
                       size — capture it from a screenshot of the same app, with the same theme \
                       and DPI, and commit it next to the test. Brittle to theme/DPI changes; \
                       prefer AT-SPI or `click_by_text` when either is available. \
                       Optional `scope_xpath` crops the search to that element's bounds (faster, \
                       fewer false positives). `threshold` is the NCC cutoff in [0, 1]; higher \
                       is stricter (library default 0.9)."
    )]
    pub(crate) async fn click_image(
        &self,
        Parameters(params): Parameters<ImageMatchParams>,
    ) -> Result<CallToolResult, McpError> {
        let png_path = params.png_path.clone();
        let scope_xpath = params.scope_xpath.clone();
        let threshold = params.threshold;
        self.run_action(
            &params.session_id,
            "click_image",
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
                locator.click().await?;
                Ok(match scope_xpath {
                    Some(xpath) => format!("Clicked image {png_path:?} inside {xpath}"),
                    None => format!("Clicked image {png_path:?}"),
                })
            },
        )
        .await
    }

    #[tool(description = "Press and release a pointer button (defaults to left click)")]
    pub(crate) async fn pointer_click(
        &self,
        Parameters(params): Parameters<PointerClickParams>,
    ) -> Result<CallToolResult, McpError> {
        // The MCP layer accepts an evdev `BTN_*` code as `u32` for
        // backwards compatibility (`0x110` = BTN_LEFT is the
        // documented default). `PointerButton::from_evdev_code` maps
        // the three standard codes onto the named variants and falls
        // through to `Other(code)` for the rest, so a future caller
        // passing e.g. BTN_BACK (0x116) still works without us
        // having to teach the JSON schema about every button.
        let button_code = params.button.unwrap_or(0x110);
        let button = waydriver::PointerButton::from_evdev_code(button_code);
        self.run_action(
            &params.session_id,
            "pointer_click",
            serde_json::json!({ "button": button_code }),
            |s| async move {
                s.pointer_button(button)
                    .await
                    .map(|_| format!("Pointer button {button_code:#x} clicked"))
            },
        )
        .await
    }

    #[tool(
        description = "Launch a second instance of the session's app with the given args, in the \
                       same environment as the primary. For a single-instance GApplication, the \
                       second invocation detects the running primary on the session bus and \
                       forwards its command line to it (instead of opening a new window), then \
                       exits. Returns the secondary process's own exit code and output (usually \
                       empty). Observe what the *primary* did with the forwarded args via \
                       `wait_for_stdout_line` or `query`. `timeout_secs` bounds how long to wait \
                       for the secondary to exit (default 15)."
    )]
    pub(crate) async fn launch_secondary_instance(
        &self,
        Parameters(params): Parameters<LaunchSecondaryParams>,
    ) -> Result<CallToolResult, McpError> {
        let args = params.args.clone();
        let timeout = params.timeout_secs.map(Duration::from_secs);
        self.run_action(
            &params.session_id,
            "launch_secondary_instance",
            serde_json::json!({ "args": params.args, "timeout_secs": params.timeout_secs }),
            |s| async move {
                let outcome = match timeout {
                    Some(t) => s.launch_secondary_with_timeout(args, t).await?,
                    None => s.launch_secondary(args).await?,
                };
                Ok(format!(
                    "Secondary instance exited (code={:?}).\nstdout:\n{}\nstderr:\n{}",
                    outcome.exit_code, outcome.stdout, outcome.stderr
                ))
            },
        )
        .await
    }
}
