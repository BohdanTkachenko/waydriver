//! Session lifecycle tools: `start_session`, `list_sessions`, `kill_session`.
//!
//! These are the only tools that touch the server's session map directly
//! (as opposed to going through `acquire()` / `run_action()`) — hence
//! their own module. The two `start_session` helpers live here for the
//! same reason.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_router, ErrorData as McpError};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Upper bound on the best-effort viewer-seed filesystem writes. Mirrors
/// `report::REPORT_IO_TIMEOUT`: seeding runs inside the start_session setup
/// budget, so a stalled write must not consume it (which would needlessly tear
/// down an otherwise-healthy session).
const SEED_IO_TIMEOUT: Duration = Duration::from_secs(10);

use waydriver::{CompositorRuntime, Session, SessionConfig};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;

use crate::cli::{
    resolve_capture_external_effects, resolve_gsettings_isolation, resolve_report_dir,
    resolve_resolution, resolve_scale, resolve_xdg_isolation,
};
use crate::mcp_error::waydriver_to_mcp;
use crate::params::{SessionIdParams, SetSettingParams, StartSessionParams};
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
    match tokio::time::timeout(SEED_IO_TIMEOUT, tokio::fs::create_dir_all(&session_dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "create session report dir failed"),
        Err(_) => tracing::warn!("create session report dir timed out"),
    }
    let video_file = video_path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let html = render_index_html(session_id, app_name, started_at_ms, video_file);
    match tokio::time::timeout(
        SEED_IO_TIMEOUT,
        tokio::fs::write(session_dir.join("index.html"), html),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "write index.html failed"),
        Err(_) => tracing::warn!("write index.html timed out"),
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
        // Per-request cancellation token, injected by rmcp. Fires when the
        // client sends a `notifications/cancelled` for this call or the
        // transport disconnects — letting us tear the half-built session down
        // instead of orphaning its compositor/app/pipewire.
        ct: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let command = params.command.clone();
        let args = params.args.clone();
        let cwd = params.cwd.clone();
        let app_name = params
            .app_name
            .clone()
            .unwrap_or_else(|| params.command.clone());

        let resolution = resolve_resolution(&self.default_resolution, params.resolution.as_deref());
        let scale = resolve_scale(self.default_scale, params.scale);
        let isolate_settings =
            resolve_gsettings_isolation(self.default_gsettings_isolation, params.isolate_settings);
        let isolate_xdg = resolve_xdg_isolation(self.default_xdg_isolation, params.isolate_xdg);
        let capture_external_effects = resolve_capture_external_effects(
            self.default_capture_external_effects,
            params.capture_external_effects,
        );
        let gsettings = waydriver::GSettingsConfig {
            isolated: isolate_settings,
            initial: params.gsettings.iter().map(|g| g.to_waydriver()).collect(),
        };

        let report_dir = resolve_report_dir(&self.report_dir, params.report_dir.as_deref());
        let report_enabled = params.report;
        // Recording is tied to the report: the WebM lives alongside the
        // viewer HTML and events. Explicit opt-out via `record_video: false`
        // disables it even when reports are on.
        let record_video =
            report_enabled && params.record_video.unwrap_or(self.default_record_video);
        let resolved_bitrate = params.video_bitrate.unwrap_or(self.default_video_bitrate);

        // Hard ceiling on the whole setup. Per-call override, else server
        // default. Guarantees the call returns even if any setup step (mutter
        // start, app launch, AT-SPI settle, recording, report I/O) stalls.
        let setup_timeout = params
            .setup_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_setup_timeout);

        // Everything that spawns processes, talks D-Bus, or touches the
        // filesystem lives in `build`. We do NOT register the session in the
        // map until `build` fully succeeds, so on timeout / cancellation /
        // setup error the future is dropped and the partially-built
        // `Session` + `MutterCompositor` run their `Drop` impls — SIGKILL'ing
        // mutter / app / pipewire — leaving nothing orphaned and nothing in
        // the map. `build` returns the ready session plus the response text.
        let build = async move {
            // Construct and pre-start the mutter compositor so we can pull its
            // shared Arc<MutterState> out before erasing to trait objects.
            // Input and capture are thin wrappers around that Arc, so they get
            // cloned references to the same D-Bus connection.
            let mut compositor = MutterCompositor::new().with_gsettings(gsettings);
            compositor
                .start(Some(&resolution), Some(scale))
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
                    // Visual tools (`click_by_text`) are exposed by this
                    // server, so kick off the ocrs model load in the
                    // background — keeps the first OCR call from paying
                    // the ~1-2s cold-start cost.
                    prewarm_visual: true,
                    visual_region_tuning: Default::default(),
                    visual_text_tuning: Default::default(),
                    visual_click_tuning: Default::default(),
                    // Must match the mode the compositor was started with —
                    // both read the same per-session keyfile dir.
                    gsettings_isolated: isolate_settings,
                    xdg_isolated: isolate_xdg,
                    extra_env: Vec::new(),
                    capture_external_effects,
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
                "scale": scale,
                "isolate_settings": isolate_settings,
                "capture_external_effects": capture_external_effects,
            });
            // Best-effort: append_event is internally bounded so a stalled
            // filesystem can't hang us here.
            if let Err(e) = managed
                .log_event(&id, "start_session", log_params, Ok(&start_msg), None)
                .await
            {
                tracing::warn!(error = %e, "log_event(start_session) failed");
            }

            let text = if report_enabled {
                let url = format!("file://{}/{id}/index.html", report_dir.display());
                format!("{start_msg}\nreport={url}")
            } else {
                start_msg
            };
            Ok::<(Arc<ManagedSession>, String, String), McpError>((managed, id, text))
        };

        let (managed, id, text) = tokio::select! {
            biased;
            _ = ct.cancelled() => {
                return Err(McpError::internal_error(
                    "start_session cancelled by client before completion; session torn down",
                    None,
                ));
            }
            r = tokio::time::timeout(setup_timeout, build) => match r {
                Ok(Ok(v)) => v,
                // Setup failed; `build` already dropped, so its partial
                // compositor/session ran Drop and cleaned up.
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(McpError::internal_error(
                        format!(
                            "start_session timed out after {}s during setup; session torn down",
                            setup_timeout.as_secs()
                        ),
                        None,
                    ));
                }
            }
        };

        // Register only after a fully successful, non-cancelled setup. This is
        // the only await left before returning and cannot block indefinitely
        // (the map lock is never held across an await elsewhere).
        self.sessions.write().await.insert(id, managed);

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Change a GSettings key on an already-running session, live. Rewrites the \
                       session's isolated keyfile in place; GIO's keyfile backend notifies the \
                       app, which re-emits its GSettings `changed` signal and re-applies the value \
                       without a restart — cursor theme, font scaling (text-scaling-factor), \
                       color-scheme, and the like update on the fly. Where the `gsettings` array \
                       on `start_session` only seeds a key *before* launch, this flips it *after*, \
                       exercising the app's live change-handler. `value` is GVariant text form \
                       (numbers bare, strings single-quoted, arrays bracketed). The app applies it \
                       asynchronously — assert on the resulting UI change (via `query` plus a \
                       wait) or on `wait_for_stdout_line`. Requires the session to use GSettings \
                       isolation (the default)."
    )]
    pub(crate) async fn set_setting(
        &self,
        Parameters(params): Parameters<SetSettingParams>,
    ) -> Result<CallToolResult, McpError> {
        let schema = params.schema.clone();
        let key = params.key.clone();
        let value = params.value.clone();
        self.run_action(
            &params.session_id,
            "set_setting",
            serde_json::json!({
                "schema": params.schema,
                "key": params.key,
                "value": params.value,
            }),
            |s| async move {
                s.set_setting(&schema, &key, &value)
                    .await
                    .map(|_| format!("Set {schema} {key} = {value}"))
            },
        )
        .await
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
