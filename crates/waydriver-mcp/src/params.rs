//! Tool-input parameter structs for the MCP server.
//!
//! Each `#[derive(Deserialize, JsonSchema)]` here corresponds to one
//! `#[tool]` method on `UiTestServer`. They live in their own module so
//! `main.rs` can stay focused on the tool-router impl.
//!
//! `pub` here is `pub(crate)`-equivalent — this is a binary crate, no
//! external API surface — but the derives need the structs visible from
//! the macro-expanded code, so we use plain `pub`.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartSessionParams {
    /// Command to launch (e.g. "gnome-calculator")
    pub command: String,
    /// Arguments for the command
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory
    pub cwd: Option<String>,
    /// Application name for AT-SPI lookup (defaults to command name)
    pub app_name: Option<String>,
    /// Override report output directory for this session (replaces the server default).
    /// Reports include screenshots today; video recordings and HTML summaries planned.
    pub report_dir: Option<String>,
    /// Whether to generate the live HTML viewer and event log for this session.
    /// Defaults to true. When false, `index.html` / `events.js` / `events.jsonl`
    /// are not written and the `report=file://...` line is omitted from the
    /// start_session response. Screenshots still persist under `report_dir`.
    #[serde(default = "default_report_enabled")]
    pub report: bool,
    /// Virtual display size as "WIDTHxHEIGHT" (e.g. "1920x1080"). When unset,
    /// falls back to the server's --resolution flag (default "1024x768").
    pub resolution: Option<String>,
    /// Record a continuous WebM video of the session under
    /// `{report_dir}/{session_id}/{session_id}.webm`. When unset, falls back
    /// to the server's `--record-video` / `--no-record-video` flag (default
    /// on). Requires `report: true` — recording is written alongside the
    /// other report files.
    pub record_video: Option<bool>,
    /// VP8 target bitrate in bits/sec for the recording. Only used when
    /// recording is enabled. When unset, falls back to the server's
    /// `--video-bitrate` flag (default 2_000_000 ≈ 2 Mbps). Higher = sharper
    /// text, bigger file.
    pub video_bitrate: Option<u32>,
}

fn default_report_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionIdParams {
    /// Session ID returned by start_session
    pub session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector evaluated against the accessibility tree snapshot
    /// (e.g. `//PushButton[@name='OK']`, `//Dialog[@name='Confirm']//PushButton`).
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one element at click time.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FocusParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one focusable element.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HoverParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one element at hover time.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DoubleClickParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one element.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RightClickParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one element.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DragToParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector for the drag source; must resolve to exactly one element.
    pub source_xpath: String,
    /// XPath selector for the drop target; must resolve to exactly one element.
    pub target_xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetTextParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one editable-text element.
    pub xpath: String,
    /// Text to write to the element (replaces existing contents).
    pub text: String,
}

/// How to clear existing content before typing in `fill`.
///
/// Modelled as a real enum on the JSON Schema so the MCP layer
/// rejects unknown values during `serde::Deserialize` instead of at a
/// hand-rolled `match params.mode.as_deref()` check inside the tool
/// body. Variants render to lower-snake-case strings (`"caret_nav"`,
/// `"select_all"`), matching the documented protocol.
#[derive(
    Debug, Default, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum FillModeParam {
    /// `Ctrl+Home` then `Ctrl+Shift+End` — works on any single-line
    /// or multi-line widget.
    #[default]
    CaretNav,
    /// `Ctrl+A` — faster, but depends on the app honouring the
    /// binding.
    SelectAll,
}

impl FillModeParam {
    /// Translate the wire-level enum into the library-level
    /// [`waydriver::FillMode`]. Kept as a method so the mapping has a
    /// single home — adding a third mode is one variant + one arm.
    pub fn to_waydriver(self) -> waydriver::FillMode {
        match self {
            FillModeParam::CaretNav => waydriver::FillMode::CaretNav,
            FillModeParam::SelectAll => waydriver::FillMode::SelectAll,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FillParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one text-accepting element.
    pub xpath: String,
    /// Text to type into the element (replaces existing contents).
    pub text: String,
    /// How to clear existing content before typing.
    #[serde(default)]
    pub mode: Option<FillModeParam>,
    /// Skip the AT-SPI `grab_focus` call and assume the element is
    /// already focused. Use when a prior `click`/`focus` has already
    /// landed focus on the target — necessary for GTK4 text widgets
    /// that don't expose the Component interface and would otherwise
    /// error with `NotSupported`. Defaults to false: `fill` calls
    /// `grab_focus` and propagates any error.
    #[serde(default)]
    pub assume_focused: bool,
}

/// Discriminator for [`SelectOptionParams::by`].
///
/// Modelled as an enum so unknown discriminators are rejected at
/// JSON-Schema validation time instead of with a hand-rolled `match
/// params.by.as_str()` inside the tool body. Variants render to
/// lower-snake-case strings (`"label"`, `"index"`).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectOptionByParam {
    /// Match the option whose AT-SPI accessible *name* equals `value`.
    Label,
    /// Parse `value` as a 0-indexed integer and pass it straight to
    /// `Selection::select_child`.
    Index,
}

impl std::fmt::Display for SelectOptionByParam {
    /// Snake-case wire form, matching the JSON discriminator. The
    /// `select_option` tool emits its outcome message using this
    /// form (e.g. `"Selected label=…"`) so MCP clients and log
    /// scrapers can pattern-match on the same string the request
    /// used.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SelectOptionByParam::Label => "label",
            SelectOptionByParam::Index => "index",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SelectOptionParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one container
    /// implementing the AT-SPI Selection interface (combobox,
    /// dropdown, listbox, etc.).
    pub xpath: String,
    /// Discriminator. `"label"` picks the child whose accessible
    /// name matches `value`; `"index"` parses `value` as a 0-indexed
    /// integer and passes it to `Selection::select_child` directly.
    pub by: SelectOptionByParam,
    /// Either the accessible name to match (when `by == "label"`) or
    /// a decimal integer (when `by == "index"`).
    pub value: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadTextParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one element supporting the Text interface.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeTextParams {
    /// Session ID
    pub session_id: String,
    /// Text to type
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PressKeyParams {
    /// Session ID
    pub session_id: String,
    /// Key name: "Return", "Tab", "Escape", "a", "1", etc.
    pub key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MovePointerParams {
    /// Session ID
    pub session_id: String,
    /// Horizontal offset in logical pixels (positive = right)
    pub dx: f64,
    /// Vertical offset in logical pixels (positive = down)
    pub dy: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PointerClickParams {
    /// Session ID
    pub session_id: String,
    /// Linux evdev button code (default: 0x110 = BTN_LEFT)
    pub button: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MovePointerAbsoluteParams {
    /// Session ID
    pub session_id: String,
    /// Absolute screen X in logical pixels
    pub x: f64,
    /// Absolute screen Y in logical pixels
    pub y: f64,
}

/// How OCR-recognised words are matched against the search text.
///
/// Wire form is snake_case so the JSON discriminator is rejected at
/// serde-deserialise time instead of inside the tool body.
#[derive(
    Debug, Default, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum MatchModeParam {
    /// Case-insensitive substring (also accent-stripped). The default —
    /// tolerates OCR noise better than `exact`.
    #[default]
    Substring,
    /// Case-sensitive full-string equality on the recognised word.
    /// Use when overlapping labels would substring-match (e.g. "Save"
    /// matching both "Save" and "Save As").
    Exact,
}

impl MatchModeParam {
    pub fn to_waydriver(self) -> waydriver::MatchMode {
        match self {
            MatchModeParam::Substring => waydriver::MatchMode::Substring,
            MatchModeParam::Exact => waydriver::MatchMode::Exact,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickByTextParams {
    /// Session ID
    pub session_id: String,
    /// Text to find on screen via OCR. Matched as a case-insensitive
    /// substring against words/phrases the recogniser extracted from
    /// the screenshot.
    pub text: String,
    /// Optional XPath of a parent element whose AT-SPI bounds restrict
    /// the OCR search region. Faster (smaller crop) and more accurate
    /// (no off-screen text confusing the recogniser). When omitted, the
    /// whole screen is searched.
    pub scope_xpath: Option<String>,
    /// How OCR words are matched against `text`. Defaults to
    /// `substring` (case-insensitive); pass `exact` when overlapping
    /// labels in the same scope would substring-match each other.
    #[serde(default)]
    pub match_mode: Option<MatchModeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickTextRegionParams {
    /// Session ID
    pub session_id: String,
    /// Text to find on screen via OCR.
    pub text: String,
    /// XPath of the surrounding container (a parent element with AT-SPI
    /// bounds). Required because flood-fill needs a bounded region.
    pub scope_xpath: String,
    /// Which level of the enclosing-region chain to click. The chain is
    /// ordered outermost-first (`0` = parent-adjacent ring, larger
    /// indices = tighter regions around the text). Omit to click the
    /// innermost region (typical for AdwButtonRow / AdwSwitchRow row
    /// activation). Use a specific index after a `find_text_regions`
    /// call has shown the chain.
    #[serde(default)]
    pub region_index: Option<usize>,
    /// OCR match mode — see `click_by_text`. Defaults to `substring`.
    #[serde(default)]
    pub match_mode: Option<MatchModeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindTextParams {
    /// Session ID
    pub session_id: String,
    /// Text to find on screen via OCR. Matched as a case-insensitive
    /// substring against words/phrases the recogniser extracted.
    pub text: String,
    /// Optional XPath of a parent element whose AT-SPI bounds restrict
    /// the OCR search region. Faster (smaller crop) and more accurate
    /// (no off-screen text). When omitted, the whole screen is searched.
    pub scope_xpath: Option<String>,
    /// OCR match mode — see `click_by_text`. Defaults to `substring`.
    #[serde(default)]
    pub match_mode: Option<MatchModeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindTextRegionsParams {
    /// Session ID
    pub session_id: String,
    /// Text to find on screen via OCR.
    pub text: String,
    /// XPath of the surrounding container (must expose AT-SPI bounds).
    /// Required because the region chain walk needs a bounded scope.
    pub scope_xpath: String,
    /// OCR match mode — see `click_by_text`. Defaults to `substring`.
    #[serde(default)]
    pub match_mode: Option<MatchModeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListTextParams {
    /// Session ID
    pub session_id: String,
    /// XPath of a parent element whose AT-SPI bounds restrict the OCR
    /// region. Defaults to the first element with bounds (the toplevel
    /// widget area) when omitted.
    pub scope_xpath: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ElementScreenshotParams {
    /// Session ID
    pub session_id: String,
    /// XPath of the element to screenshot. The full-frame capture is
    /// cropped to the element's AT-SPI bounds.
    pub xpath: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImageMatchParams {
    /// Session ID
    pub session_id: String,
    /// Filesystem path to a reference PNG. The image is read once at
    /// call time and matched against the current screenshot using
    /// normalized cross-correlation. Paths are resolved relative to
    /// the MCP server's working directory.
    pub png_path: String,
    /// Optional XPath of a parent element whose AT-SPI bounds restrict
    /// the template-match region. Faster and more accurate when set.
    pub scope_xpath: Option<String>,
    /// NCC match threshold in [0, 1]. Higher = stricter. Library
    /// default is 0.9 when omitted.
    pub threshold: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitForStdoutLineParams {
    /// Session ID
    pub session_id: String,
    /// Substring the matching line must contain (case-sensitive).
    pub contains: String,
    /// How long to wait before giving up, in milliseconds. Defaults to 5000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Buffer-position cursor returned by a prior `wait_for_stdout_line` call
    /// (or 0 to scan from the start). Use to skip lines emitted before the
    /// action you're waiting on, so a repeated event-string can't match its
    /// own past occurrence. Omitted = start from the current end of the
    /// buffer (only new lines count).
    #[serde(default)]
    pub after: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_session_params_report_defaults_to_true() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x" })).unwrap();
        assert!(params.report);
    }

    #[test]
    fn start_session_params_report_can_be_disabled() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x", "report": false })).unwrap();
        assert!(!params.report);
    }

    #[test]
    fn start_session_params_record_video_defaults_to_none() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x" })).unwrap();
        assert_eq!(params.record_video, None);
    }

    #[test]
    fn start_session_params_record_video_can_be_set() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x", "record_video": false }))
                .unwrap();
        assert_eq!(params.record_video, Some(false));
    }

    #[test]
    fn start_session_params_video_bitrate_defaults_to_none() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x" })).unwrap();
        assert_eq!(params.video_bitrate, None);
    }

    #[test]
    fn start_session_params_video_bitrate_can_be_set() {
        let params: StartSessionParams = serde_json::from_value(
            serde_json::json!({ "command": "x", "video_bitrate": 5_000_000 }),
        )
        .unwrap();
        assert_eq!(params.video_bitrate, Some(5_000_000));
    }
}
