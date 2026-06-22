//! CLI flag parsing for the `waydriver-mcp` binary, plus the small helpers
//! that fold a per-session override on top of the server-wide default.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Headless GTK4 UI testing MCP server")]
pub struct Cli {
    /// Base directory for per-session report output (screenshots today;
    /// video recordings and HTML summaries planned). Each session gets a
    /// subdirectory under this path, each containing a self-contained
    /// `index.html` viewer openable directly from the filesystem.
    #[arg(long, default_value = "/tmp/waydriver", env = "WAYDRIVER_REPORT_DIR")]
    pub report_dir: PathBuf,
    /// Default virtual-display size ("WIDTHxHEIGHT") for sessions that don't
    /// override it via start_session's `resolution` parameter.
    #[arg(long, default_value = "1024x768", env = "WAYDRIVER_RESOLUTION")]
    pub resolution: String,
    /// Default logical-monitor scale (HiDPI factor) for sessions that don't
    /// override it via start_session's `scale` parameter. `1.0` = 1:1; `2.0` =
    /// 200%; fractional values like `1.5` (150%) are snapped to the nearest
    /// scale mutter advertises. `--resolution` stays the *physical* framebuffer
    /// size, so apps see a logical size of resolution ÷ scale.
    #[arg(long, default_value_t = 1.0, env = "WAYDRIVER_SCALE")]
    pub scale: f64,
    /// Default GSettings isolation for sessions that don't override it via
    /// start_session's `isolate_settings` parameter. When on (default), mutter
    /// and the app run against a private per-session keyfile store instead of
    /// the host's dconf — required for fractional `--scale` and keeps sessions
    /// from touching the host's real desktop settings. Turn off to use the
    /// host's GSettings.
    #[arg(
        long,
        default_value_t = true,
        action = clap::ArgAction::Set,
        env = "WAYDRIVER_GSETTINGS_ISOLATION"
    )]
    pub gsettings_isolation: bool,
    /// Default XDG base-dir isolation for sessions that don't override it via
    /// start_session's `isolate_xdg` parameter. When on (default), the app
    /// gets private XDG state/data/cache dirs under the session runtime dir,
    /// so persisted app state can't leak to the host or poison later
    /// sessions. Turn off to run apps against the host's real state dirs.
    #[arg(
        long,
        default_value_t = true,
        action = clap::ArgAction::Set,
        env = "WAYDRIVER_XDG_ISOLATION"
    )]
    pub xdg_isolation: bool,
    /// Record a continuous WebM video of each session by default. When on,
    /// each session writes `{report_dir}/{session_id}/{session_id}.webm`
    /// alongside its screenshots and events. Per-session override via
    /// start_session's `record_video` argument. Requires reports enabled.
    #[arg(
        long,
        default_value_t = true,
        action = clap::ArgAction::Set,
        env = "WAYDRIVER_RECORD_VIDEO"
    )]
    pub record_video: bool,
    /// Default VP8 target bitrate in bits/sec for session recordings. Higher
    /// values produce sharper UI text at the cost of file size. Per-session
    /// override via start_session's `video_bitrate` argument.
    #[arg(long, default_value_t = 2_000_000, env = "WAYDRIVER_VIDEO_BITRATE")]
    pub video_bitrate: u32,
    /// Hard ceiling, in seconds, on session setup (compositor + app launch +
    /// AT-SPI settle + recording start). If setup stalls past this budget,
    /// start_session tears the partial session down and returns an error
    /// instead of hanging. Per-session override via start_session's
    /// `setup_timeout_secs` argument.
    #[arg(long, default_value_t = 90, env = "WAYDRIVER_SETUP_TIMEOUT_SECS")]
    pub setup_timeout_secs: u64,
    /// Default for external-effect capture: stand up mock D-Bus sinks on each
    /// session's bus that record the app's desktop notifications
    /// (`org.freedesktop.Notifications`) and portal open-URI requests
    /// (`org.freedesktop.portal.Desktop`), readable via `get_captured_effects`.
    /// Off by default (opt-in) — the sinks own well-known names, only safe when
    /// nothing else owns them (always true on the per-session/container bus; on
    /// a shared host bus the claim no-ops with a warning). Per-session override
    /// via start_session's `capture_external_effects` argument.
    #[arg(
        long,
        default_value_t = false,
        action = clap::ArgAction::Set,
        env = "WAYDRIVER_CAPTURE_EXTERNAL_EFFECTS"
    )]
    pub capture_external_effects: bool,
    /// Hard ceiling, in seconds, on a single session operation (screenshot,
    /// input, locator action, query, value read). If an op stalls past this
    /// budget — e.g. a wedged session whose window never composites, where a
    /// capture or D-Bus call would otherwise block forever — the call returns
    /// an `Error::Timeout` instead of hanging the MCP client. Mirrors
    /// `kill_session`'s existing 5s budget but for every other op. Wait-style
    /// tools (`wait_for_stdout_line`, `launch_secondary_instance`) extend this
    /// budget by their own caller-supplied wait, so an intentional long wait is
    /// never cut short — the op timeout only bounds the infrastructure slack on
    /// top of it. Lower it for fast-failing automated agents; raise it for slow
    /// compositors.
    #[arg(long, default_value_t = 30, env = "WAYDRIVER_OP_TIMEOUT_SECS")]
    pub op_timeout_secs: u64,
}

/// Resolve the effective report dir for a new session: per-session override
/// if provided, else the server's base dir.
pub fn resolve_report_dir(base: &std::path::Path, override_: Option<&str>) -> PathBuf {
    override_
        .map(PathBuf::from)
        .unwrap_or_else(|| base.to_path_buf())
}

/// Resolve the effective virtual-display resolution for a new session:
/// per-session override if provided, else the server's default.
pub fn resolve_resolution(default: &str, override_: Option<&str>) -> String {
    override_.unwrap_or(default).to_string()
}

/// Resolve the effective logical-monitor scale for a new session:
/// per-session override if provided, else the server's default.
pub fn resolve_scale(default: f64, override_: Option<f64>) -> f64 {
    override_.unwrap_or(default)
}

/// Resolve the effective GSettings-isolation flag for a new session:
/// per-session override if provided, else the server's default.
pub fn resolve_gsettings_isolation(default: bool, override_: Option<bool>) -> bool {
    override_.unwrap_or(default)
}

/// Resolve the effective XDG base-dir isolation flag for a new session:
/// per-session override if provided, else the server's default.
pub fn resolve_xdg_isolation(default: bool, override_: Option<bool>) -> bool {
    override_.unwrap_or(default)
}

/// Resolve the effective external-effect-capture flag for a new session:
/// per-session override if provided, else the server's default.
pub fn resolve_capture_external_effects(default: bool, override_: Option<bool>) -> bool {
    override_.unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_report_dir_defaults_to_base() {
        let base = PathBuf::from("/tmp/base");
        let resolved = resolve_report_dir(&base, None);
        assert_eq!(resolved, base);
    }

    #[test]
    fn resolve_report_dir_uses_override_when_provided() {
        let base = PathBuf::from("/tmp/base");
        let resolved = resolve_report_dir(&base, Some("/tmp/override"));
        assert_eq!(resolved, PathBuf::from("/tmp/override"));
    }

    #[test]
    fn resolve_report_dir_override_is_absolute_replacement() {
        // Relative override is taken as-is, not joined under the base.
        let base = PathBuf::from("/tmp/base");
        let resolved = resolve_report_dir(&base, Some("relative/path"));
        assert_eq!(resolved, PathBuf::from("relative/path"));
    }

    #[test]
    fn resolve_resolution_defaults_to_server_default() {
        assert_eq!(resolve_resolution("1024x768", None), "1024x768");
    }

    #[test]
    fn resolve_resolution_uses_override_when_provided() {
        assert_eq!(
            resolve_resolution("1024x768", Some("1920x1080")),
            "1920x1080"
        );
    }

    #[test]
    fn resolve_resolution_override_replaces_default_entirely() {
        // The override is taken as-is; the server default is ignored even if
        // the override is nonsensical (mutter validator catches that later).
        assert_eq!(resolve_resolution("1920x1080", Some("garbage")), "garbage");
    }

    #[test]
    fn resolve_scale_defaults_to_server_default() {
        assert_eq!(resolve_scale(1.0, None), 1.0);
    }

    #[test]
    fn resolve_scale_uses_override_when_provided() {
        assert_eq!(resolve_scale(1.0, Some(2.0)), 2.0);
    }

    #[test]
    fn cli_scale_defaults_to_one() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert_eq!(cli.scale, 1.0);
    }

    #[test]
    fn cli_accepts_scale_flag() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--scale", "1.5"]).unwrap();
        assert_eq!(cli.scale, 1.5);
    }

    #[test]
    fn resolve_gsettings_isolation_defaults_to_server_default() {
        assert!(resolve_gsettings_isolation(true, None));
        assert!(!resolve_gsettings_isolation(false, None));
    }

    #[test]
    fn resolve_gsettings_isolation_uses_override_when_provided() {
        assert!(!resolve_gsettings_isolation(true, Some(false)));
        assert!(resolve_gsettings_isolation(false, Some(true)));
    }

    #[test]
    fn resolve_xdg_isolation_defaults_to_server_default() {
        assert!(resolve_xdg_isolation(true, None));
        assert!(!resolve_xdg_isolation(false, None));
    }

    #[test]
    fn resolve_xdg_isolation_uses_override_when_provided() {
        assert!(!resolve_xdg_isolation(true, Some(false)));
        assert!(resolve_xdg_isolation(false, Some(true)));
    }

    #[test]
    fn cli_xdg_isolation_defaults_to_true() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert!(cli.xdg_isolation);
    }

    #[test]
    fn cli_xdg_isolation_can_be_disabled() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--xdg-isolation", "false"]).unwrap();
        assert!(!cli.xdg_isolation);
    }

    #[test]
    fn cli_gsettings_isolation_defaults_to_true() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert!(cli.gsettings_isolation);
    }

    #[test]
    fn cli_gsettings_isolation_can_be_disabled() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--gsettings-isolation", "false"]).unwrap();
        assert!(!cli.gsettings_isolation);
    }

    #[test]
    fn cli_defaults_to_tmp_waydriver() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert_eq!(cli.report_dir, PathBuf::from("/tmp/waydriver"));
    }

    #[test]
    fn cli_accepts_report_dir_flag() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--report-dir", "/custom/out"]).unwrap();
        assert_eq!(cli.report_dir, PathBuf::from("/custom/out"));
    }

    #[test]
    fn cli_record_video_defaults_to_true() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert!(cli.record_video);
    }

    #[test]
    fn cli_record_video_can_be_disabled() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--record-video", "false"]).unwrap();
        assert!(!cli.record_video);
    }

    #[test]
    fn cli_video_bitrate_defaults_to_two_mbps() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert_eq!(cli.video_bitrate, 2_000_000);
    }

    #[test]
    fn cli_accepts_video_bitrate_flag() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--video-bitrate", "5000000"]).unwrap();
        assert_eq!(cli.video_bitrate, 5_000_000);
    }

    #[test]
    fn cli_setup_timeout_defaults_to_ninety() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert_eq!(cli.setup_timeout_secs, 90);
    }

    #[test]
    fn cli_accepts_setup_timeout_flag() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--setup-timeout-secs", "30"]).unwrap();
        assert_eq!(cli.setup_timeout_secs, 30);
    }

    #[test]
    fn cli_op_timeout_defaults_to_thirty() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert_eq!(cli.op_timeout_secs, 30);
    }

    #[test]
    fn cli_accepts_op_timeout_flag() {
        let cli = Cli::try_parse_from(["waydriver-mcp", "--op-timeout-secs", "10"]).unwrap();
        assert_eq!(cli.op_timeout_secs, 10);
    }

    #[test]
    fn cli_capture_external_effects_defaults_to_false() {
        let cli = Cli::try_parse_from(["waydriver-mcp"]).unwrap();
        assert!(!cli.capture_external_effects);
    }

    #[test]
    fn cli_capture_external_effects_can_be_enabled() {
        let cli =
            Cli::try_parse_from(["waydriver-mcp", "--capture-external-effects", "true"]).unwrap();
        assert!(cli.capture_external_effects);
    }

    #[test]
    fn resolve_capture_external_effects_defaults_to_server_default() {
        assert!(!resolve_capture_external_effects(false, None));
        assert!(resolve_capture_external_effects(true, None));
    }

    #[test]
    fn resolve_capture_external_effects_uses_override_when_provided() {
        assert!(resolve_capture_external_effects(false, Some(true)));
        assert!(!resolve_capture_external_effects(true, Some(false)));
    }
}
