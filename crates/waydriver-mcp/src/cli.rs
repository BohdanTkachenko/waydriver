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
}
