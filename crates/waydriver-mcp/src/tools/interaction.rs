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

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData as McpError};

use waydriver::keysym::parse_chord;

use crate::params::{
    ClickParams, DoubleClickParams, DragToParams, FillParams, FocusParams, HoverParams,
    MovePointerParams, PointerClickParams, PressKeyParams, RightClickParams, SelectOptionParams,
    SetTextParams, TypeTextParams,
};
use crate::UiTestServer;

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
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "click",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move { s.locate(&xpath).click().await.map(|_| format!("Clicked {xpath}")) },
        )
        .await
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
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "focus",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move { s.locate(&xpath).focus().await.map(|_| format!("Focused {xpath}")) },
        )
        .await
    }

    #[tool(
        description = "Move the pointer to the centre of the element selected by XPath without \
                       clicking. Use to reveal hover-only UI like tooltips or slide-out menus."
    )]
    pub(crate) async fn hover(
        &self,
        Parameters(params): Parameters<HoverParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "hover",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move { s.locate(&xpath).hover().await.map(|_| format!("Hovered {xpath}")) },
        )
        .await
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
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "double_click",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move {
                s.locate(&xpath)
                    .double_click()
                    .await
                    .map(|_| format!("Double-clicked {xpath}"))
            },
        )
        .await
    }

    #[tool(
        description = "Right-click the element selected by XPath, typically opening the widget's \
                       context menu."
    )]
    pub(crate) async fn right_click(
        &self,
        Parameters(params): Parameters<RightClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "right_click",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move {
                s.locate(&xpath)
                    .right_click()
                    .await
                    .map(|_| format!("Right-clicked {xpath}"))
            },
        )
        .await
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
                       \"select_all\" (Ctrl+A — faster when the app honors it)."
    )]
    pub(crate) async fn fill(
        &self,
        Parameters(params): Parameters<FillParams>,
    ) -> Result<CallToolResult, McpError> {
        // Validate mode up front: it's a caller-input problem, not a
        // runtime failure, so it shouldn't get logged as an action error.
        let mode = match params.mode.as_deref() {
            None | Some("caret_nav") => waydriver::FillMode::CaretNav,
            Some("select_all") => waydriver::FillMode::SelectAll,
            Some(other) => {
                return Err(McpError::invalid_params(
                    format!(
                        "invalid fill mode {other:?}; expected \"caret_nav\" or \"select_all\""
                    ),
                    None,
                ));
            }
        };

        let xpath = params.xpath.clone();
        let text = params.text.clone();
        self.run_action(
            &params.session_id,
            "fill",
            serde_json::json!({
                "xpath": params.xpath,
                "text": params.text,
                "mode": params.mode,
            }),
            |s| async move {
                s.locate(&xpath)
                    .fill_with_opts(&text, mode)
                    .await
                    .map(|_| format!("Filled {xpath}"))
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
        // Parse by/value up front into an owned discriminant so the
        // closure can reconstruct `SelectBy` (which borrows). Bad input
        // here is caller error, not infrastructure failure.
        enum ParsedBy {
            Label(String),
            Index(usize),
        }
        let parsed = match params.by.as_str() {
            "label" => ParsedBy::Label(params.value.clone()),
            "index" => params
                .value
                .parse::<usize>()
                .map(ParsedBy::Index)
                .map_err(|e| {
                    McpError::invalid_params(
                        format!("invalid index {:?}: {e}", params.value),
                        None,
                    )
                })?,
            other => {
                return Err(McpError::invalid_params(
                    format!("invalid `by` {other:?}; expected \"label\" or \"index\""),
                    None,
                ));
            }
        };

        let xpath = params.xpath.clone();
        let by = params.by.clone();
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
            |s| async move { s.press_chord(&key).await.map(|_| format!("Pressed '{key}'")) },
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

    #[tool(description = "Press and release a pointer button (defaults to left click)")]
    pub(crate) async fn pointer_click(
        &self,
        Parameters(params): Parameters<PointerClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let button = params.button.unwrap_or(0x110); // BTN_LEFT
        self.run_action(
            &params.session_id,
            "pointer_click",
            serde_json::json!({ "button": button }),
            |s| async move {
                s.pointer_button(button)
                    .await
                    .map(|_| format!("Pointer button {button:#x} clicked"))
            },
        )
        .await
    }
}
