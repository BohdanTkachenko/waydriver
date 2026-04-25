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
use serde::Deserialize;

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FillParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one text-accepting element.
    pub xpath: String,
    /// Text to type into the element (replaces existing contents).
    pub text: String,
    /// How to clear existing content before typing. `"caret_nav"`
    /// (default) uses `Ctrl+Home` then `Ctrl+Shift+End` — works on any
    /// single-line or multi-line widget. `"select_all"` uses `Ctrl+A`
    /// — faster, but depends on the app honoring the binding.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SelectOptionParams {
    /// Session ID
    pub session_id: String,
    /// XPath selector; must resolve to exactly one container
    /// implementing the AT-SPI Selection interface (combobox,
    /// dropdown, listbox, etc.).
    pub xpath: String,
    /// Discriminator: `"label"` picks the child whose accessible
    /// name matches `value`; `"index"` parses `value` as a 0-indexed
    /// integer and passes it to `Selection::select_child` directly.
    pub by: String,
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

