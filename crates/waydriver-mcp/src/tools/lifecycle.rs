//! Session lifecycle tools: `start_session`, `list_sessions`, `kill_session`.
//!
//! These are the only tools that touch the server's session map directly
//! (as opposed to going through `acquire()` / `run_action()`) — hence
//! their own module. The two `start_session` helpers live here for the
//! same reason.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router, ErrorData as McpError};
use tokio::sync::Mutex;

use waydriver::{CompositorRuntime, Session, SessionConfig};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;

use crate::cli::{resolve_report_dir, resolve_resolution};
use crate::mcp_error::waydriver_to_mcp;
use crate::params::{SessionIdParams, StartSessionParams};
use crate::report::{append_event, now_ms, render_index_html};
use crate::session::ManagedSession;
use crate::UiTestServer;

/// Resolve the WebM output path for a recording-enabled session and ensure
/// its parent directory exists before GStreamer's `filesink` opens it.
///
/// Returns `None` when `record_video` is `false`. Directory-creation
/// failures are warned-and-continued — they typically clear up before
/// `Session::start` runs the recording, and a hard-fail here would
/// abort the session over a soft, recoverable IO error.
pub(crate) async fn resolve_video_output(
    record_video: bool,
    report_dir: &Path,
    compositor_id: &str,
) -> Option<PathBuf> {
    if !record_video {
        return None;
    }
    let session_dir = report_dir.join(compositor_id);
    if let Err(e) = tokio::fs::create_dir_all(&session_dir).await {
        tracing::warn!(error = %e, "create session report dir failed (pre-record)");
    }
    Some(session_dir.join(format!("{compositor_id}.webm")))
}

/// Seed `{report_dir}/{session_id}/index.html` with the viewer shell so
/// the first event always lands on an existing file. Caller decides
/// whether reporting is enabled; this helper assumes it is.
///
/// All errors are warned-and-continued: the report viewer is a
/// best-effort artifact, and refusing to start a session because
/// `index.html` couldn't be written would punish the user for a
/// disk-state problem they care less about than getting their app
/// running.
pub(crate) async fn seed_viewer(
    report_dir: &Path,
    session_id: &str,
    app_name: &str,
    started_at_ms: u64,
    video_path: Option<&Path>,
) {
    let session_dir = report_dir.join(session_id);
    if let Err(e) = tokio::fs::create_dir_all(&session_dir).await {
        tracing::warn!(error = %e, "create session report dir failed");
    }
    let video_file = video_path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let html = render_index_html(session_id, app_name, started_at_ms, video_file);
    if let Err(e) = tokio::fs::write(session_dir.join("index.html"), html).await {
        tracing::warn!(error = %e, "write index.html failed");
    }
}

#[tool_router(router = lifecycle_router, vis = "pub(crate)")]
impl UiTestServer {
    #[tool(
        description = "Start a headless Wayland session with mutter and launch an application. \
                       On success, the response includes a `report=file://...` line with the URL \
                       of the session's live viewer HTML — surface that URL directly to the user \
                       so they can watch the run in a browser. Pass `report: false` to skip \
                       writing the viewer and event log; the `report=` line is then omitted."
    )]
    pub(crate) async fn start_session(
        &self,
        Parameters(params): Parameters<StartSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        let command = params.command.clone();
        let args = params.args.clone();
        let cwd = params.cwd.clone();
        let app_name = params
            .app_name
            .clone()
            .unwrap_or_else(|| params.command.clone());

        let resolution = resolve_resolution(&self.default_resolution, params.resolution.as_deref());

        let report_dir = resolve_report_dir(&self.report_dir, params.report_dir.as_deref());
        let report_enabled = params.report;
        // Recording is tied to the report: the WebM lives alongside the
        // viewer HTML and events. Explicit opt-out via `record_video: false`
        // disables it even when reports are on.
        let record_video =
            report_enabled && params.record_video.unwrap_or(self.default_record_video);
        let resolved_bitrate = params.video_bitrate.unwrap_or(self.default_video_bitrate);

        // Construct and pre-start the mutter compositor so we can pull its
        // shared Arc<MutterState> out before erasing to trait objects. Input
        // and capture are thin wrappers around that Arc, so they get cloned
        // references to the same D-Bus connection.
        let mut compositor = MutterCompositor::new();
        compositor
            .start(Some(&resolution))
            .await
            .map_err(waydriver_to_mcp)?;
        let compositor_id = compositor.id().to_string();
        // `state()` returns `Option` post-API-tightening — but we have
        // just awaited a successful `start()`, so the state is
        // guaranteed present here. `expect` documents the invariant.
        let state = compositor
            .state()
            .expect("MutterCompositor::state must be Some immediately after start() succeeded");
        let input = MutterInput::new(state.clone());
        let capture = MutterCapture::new(state);

        let video_path = resolve_video_output(record_video, &report_dir, &compositor_id).await;

        let session = Session::start(
            Box::new(compositor),
            Box::new(input),
            Box::new(capture),
            SessionConfig {
                command: params.command,
                args: params.args,
                cwd: params.cwd,
                app_name: app_name.clone(),
                video_output: video_path.clone(),
                video_bitrate: Some(resolved_bitrate),
                video_fps: None,
            },
        )
        .await
        .map_err(waydriver_to_mcp)?;

        let id = session.id.clone();
        let display = session.wayland_display().to_string();

        let started_at_ms = now_ms();

        // Seed the per-session dir + viewer shell before we insert so the
        // first event always lands on an existing index.html.
        if report_enabled {
            seed_viewer(
                &report_dir,
                &id,
                &app_name,
                started_at_ms,
                video_path.as_deref(),
            )
            .await;
        }

        let managed = Arc::new(ManagedSession {
            session: Arc::new(session),
            report_dir: report_dir.clone(),
            screenshot_counter: AtomicU32::new(0),
            events: Mutex::new(crate::report::EventLog::new()),
            report_enabled,
            kill_lock: Arc::new(tokio::sync::RwLock::new(())),
        });

        let start_msg = format!("Session started: id={id}, display={display}, app={app_name}");
        let log_params = serde_json::json!({
            "command": command,
            "args": args,
            "cwd": cwd,
            "app_name": app_name,
            "resolution": resolution,
        });
        if let Err(e) = managed
            .log_event(&id, "start_session", log_params, Ok(&start_msg), None)
            .await
        {
            tracing::warn!(error = %e, "log_event(start_session) failed");
        }

        self.sessions.write().await.insert(id.clone(), managed);

        let text = if report_enabled {
            let url = format!("file://{}/{id}/index.html", report_dir.display());
            format!("{start_msg}\nreport={url}")
        } else {
            start_msg
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "List all active test sessions")]
    pub(crate) async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        if sessions.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No active sessions",
            )]));
        }

        let mut lines = Vec::new();
        for (id, m) in sessions.iter() {
            lines.push(format!(
                "- {} (app={}, display={})",
                id,
                m.session.app_name,
                m.session.wayland_display()
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "Kill a test session and clean up all processes")]
    pub(crate) async fn kill_session(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        // Step 1: remove from the map so no new tool call can acquire
        // this session. The map write lock is held only for the
        // remove() call; in-flight tools already hold their own
        // `Arc<ManagedSession>` clones and don't depend on the map.
        let managed_arc = self
            .sessions
            .write()
            .await
            .remove(&params.session_id)
            .ok_or_else(|| {
                McpError::invalid_params(format!("session not found: {}", params.session_id), None)
            })?;

        // Step 2: signal every in-flight tool on this session to bail.
        // Long auto-wait loops observe the token and return
        // `Error::Cancelled` in microseconds instead of waiting out
        // their natural 30s-ish timeout.
        managed_arc.session.cancel();

        // Step 3: drain. Tools hold the per-session drain lock in read
        // mode via `InFlightSession`; taking write mode waits for all
        // of them to drop. Because `InFlightSession` declares
        // `managed: Arc<ManagedSession>` before `_guard`, the Arc
        // clone drops *before* the read guard releases, so by the time
        // write_owned() resolves here we're the only holder of
        // managed_arc and `Arc::try_unwrap` is guaranteed to succeed.
        let _drain = Arc::clone(&managed_arc.kill_lock).write_owned().await;

        let managed = Arc::try_unwrap(managed_arc).unwrap_or_else(|_| {
            // Unreachable: the drain lock above guarantees we're the
            // sole Arc holder. Panic rather than silently fall back to
            // a weaker behavior — if this ever fires, the drop-order
            // invariant on InFlightSession has been broken.
            unreachable!("drain lock released while another Arc<ManagedSession> clone exists")
        });

        // Destructure so `session.kill()` can move out without
        // invalidating the remaining fields we still need for the
        // final log event.
        let ManagedSession {
            session,
            report_dir,
            events,
            report_enabled,
            ..
        } = managed;

        // Arc<Session>: the drain lock above also ensures no tool is
        // holding a session clone anymore, so this unwrap succeeds.
        let kill_result = match Arc::try_unwrap(session) {
            Ok(owned) => owned.kill().await.map_err(|e| e.to_string()),
            Err(_) => unreachable!(
                "post-drain Arc<Session> still referenced — tool leaked a clone past its scope"
            ),
        };
        let success_msg = format!("Session {} killed", params.session_id);
        let outcome = kill_result
            .as_ref()
            .map(|_| success_msg.as_str())
            .map_err(|e| e.as_str());
        if report_enabled {
            if let Err(e) = append_event(
                &report_dir,
                &params.session_id,
                &events,
                "kill_session",
                serde_json::json!({}),
                outcome,
                None,
            )
            .await
            {
                tracing::warn!(error = %e, "log_event(kill_session) failed");
            }
        }

        kill_result.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(success_msg)]))
    }
}
