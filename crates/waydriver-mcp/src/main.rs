use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use clap::Parser;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};

use waydriver::keysym::parse_chord;
use waydriver::{CompositorRuntime, Session, SessionConfig};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;

mod cli;
mod mcp_error;
mod params;
mod report;
mod session;

use cli::{resolve_report_dir, resolve_resolution, Cli};
use mcp_error::waydriver_to_mcp;
use params::{
    ClickParams, DoubleClickParams, DragToParams, FillParams, FocusParams, HoverParams,
    MovePointerParams, PointerClickParams, PressKeyParams, QueryParams, ReadTextParams,
    RightClickParams, SelectOptionParams, SessionIdParams, SetTextParams, StartSessionParams,
    TypeTextParams,
};
use report::{append_event, now_ms, render_index_html, render_matches};
use session::ManagedSession;

// ── start_session helpers ───────────────────────────────────────────────────

/// Resolve the WebM output path for a recording-enabled session and ensure
/// its parent directory exists before GStreamer's `filesink` opens it.
///
/// Returns `None` when `record_video` is `false`. Directory-creation
/// failures are warned-and-continued — they typically clear up before
/// `Session::start` runs the recording, and a hard-fail here would
/// abort the session over a soft, recoverable IO error.
async fn resolve_video_output(
    record_video: bool,
    report_dir: &std::path::Path,
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
async fn seed_viewer(
    report_dir: &std::path::Path,
    session_id: &str,
    app_name: &str,
    started_at_ms: u64,
    video_path: Option<&std::path::Path>,
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

// ── Server ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UiTestServer {
    sessions: Arc<RwLock<HashMap<String, Arc<ManagedSession>>>>,
    report_dir: PathBuf,
    default_resolution: String,
    default_record_video: bool,
    default_video_bitrate: u32,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

/// Scope handle held for the duration of a single tool call against one
/// session. Bundles the session `Arc` with the drain-lock read guard so
/// `kill_session` can wait for every in-flight tool to drop its ref
/// *before* trying to unwrap the session.
///
/// **Drop-order is load-bearing.** Struct fields drop in declaration
/// order, so `managed` (the `Arc<ManagedSession>`) drops *before*
/// `_guard` (the read lock). That ordering guarantees that when
/// `kill_session`'s `write_owned()` acquires the drain lock, the
/// corresponding tool's `Arc<ManagedSession>` has already been dropped —
/// so `Arc::try_unwrap` in `kill_session` deterministically succeeds.
/// Reversing the fields breaks this invariant.
struct InFlightSession {
    managed: Arc<ManagedSession>,
    _guard: tokio::sync::OwnedRwLockReadGuard<()>,
}

impl std::ops::Deref for InFlightSession {
    type Target = ManagedSession;
    fn deref(&self) -> &ManagedSession {
        &self.managed
    }
}

impl UiTestServer {
    /// Look up a session by id and acquire its drain lock in read mode.
    /// The returned handle holds both the session `Arc` and the guard;
    /// drop it to release the lock and let any pending `kill_session`
    /// proceed.
    async fn acquire(&self, session_id: &str) -> Result<InFlightSession, McpError> {
        // Phase 1: take the map read lock just long enough to clone the
        // session Arc out. After this scope exits, a concurrent
        // `kill_session` can take the map write lock immediately — it
        // will then wait on the per-session drain lock instead of the
        // whole map.
        let managed = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id)
                .cloned()
                .ok_or_else(|| {
                    McpError::invalid_params(format!("session not found: {session_id}"), None)
                })?
        };
        // Phase 2: take the per-session drain lock in read mode. This is
        // what `kill_session`'s `write_owned().await` waits on to learn
        // that every in-flight tool has finished.
        let guard = Arc::clone(&managed.kill_lock).read_owned().await;
        Ok(InFlightSession {
            managed,
            _guard: guard,
        })
    }

    /// Boilerplate every action tool would otherwise repeat: look up the
    /// session, run a closure against its `Arc<Session>`, log the outcome
    /// to the per-session event log, and shape success/failure into a
    /// `CallToolResult` / `McpError`.
    ///
    /// The closure returns `Result<String, waydriver::Error>` — the
    /// success string becomes the MCP text response and the event-log
    /// `message`; the typed error is chain-walked into both the log
    /// message and the MCP error so D-Bus / GStreamer / IO sources don't
    /// get flattened away by `to_string`.
    ///
    /// **Concurrency:** the map read lock is held only for the moment
    /// it takes to clone out an `Arc<ManagedSession>`. Tool work runs
    /// against the cloned `Arc` with the per-session drain lock held in
    /// read mode — so tools on other sessions never block each other
    /// and `kill_session` on other sessions is instant. A cancel on
    /// *this* session (via `kill_session`) wakes auto-wait loops via
    /// the session's `CancellationToken`, so even a stuck wait resolves
    /// promptly as `Error::Cancelled`.
    async fn run_action<F, Fut>(
        &self,
        session_id: &str,
        action: &'static str,
        log_params: serde_json::Value,
        work: F,
    ) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(Arc<waydriver::Session>) -> Fut,
        Fut: std::future::Future<Output = Result<String, waydriver::Error>>,
    {
        let held = self.acquire(session_id).await?;
        let result = work(Arc::clone(&held.session)).await;

        // Materialize a stringy view for log_event while still holding
        // the typed error, so we can route the typed error through
        // waydriver_to_mcp below (which discriminates locator-shape
        // errors to invalid_params instead of internal_error).
        let log_view: Result<String, String> = match &result {
            Ok(msg) => Ok(msg.clone()),
            Err(e) => Err(mcp_error::format_chain(e)),
        };
        let log_outcome = log_view.as_ref().map(String::as_str).map_err(String::as_str);
        if let Err(e) = held
            .log_event(session_id, action, log_params, log_outcome, None)
            .await
        {
            tracing::warn!(error = %e, %action, "log_event failed");
        }

        let success = result.map_err(waydriver_to_mcp)?;
        Ok(CallToolResult::success(vec![Content::text(success)]))
    }
}

#[tool_router]
impl UiTestServer {
    pub fn new(
        report_dir: PathBuf,
        default_resolution: String,
        default_record_video: bool,
        default_video_bitrate: u32,
    ) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            report_dir,
            default_resolution,
            default_record_video,
            default_video_bitrate,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Start a headless Wayland session with mutter and launch an application. \
                       On success, the response includes a `report=file://...` line with the URL \
                       of the session's live viewer HTML — surface that URL directly to the user \
                       so they can watch the run in a browser. Pass `report: false` to skip \
                       writing the viewer and event log; the `report=` line is then omitted."
    )]
    async fn start_session(
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
        let state = compositor.state();
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
            events: Mutex::new(Vec::new()),
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
    async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
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
    async fn kill_session(
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

    #[tool(
        description = "Dump the accessibility tree of the application UI as XML. Use this to \
                       discover selector-ready role names, attributes, and element hierarchy \
                       before writing XPath queries for `query` or `click`."
    )]
    async fn dump_tree(
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
    async fn query(
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
        description = "Click a UI element selected by XPath. The selector must resolve to \
                       exactly one element; if it matches multiple, use `query` first and \
                       pass the pinned `xpath` back, or refine the selector."
    )]
    async fn click(
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
    async fn focus(
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
    async fn hover(
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
    async fn double_click(
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
    async fn right_click(
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
    async fn drag_to(
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
    async fn set_text(
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
    async fn fill(
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
    async fn select_option(
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
            "index" => params.value.parse::<usize>().map(ParsedBy::Index).map_err(|e| {
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

    #[tool(
        description = "Read the text contents of an element selected by XPath. Target must \
                       implement the Text AT-SPI interface."
    )]
    async fn read_text(
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

    #[tool(description = "Type text into the currently focused element via keyboard input")]
    async fn type_text(
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
    async fn press_key(
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
    async fn move_pointer(
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
    async fn pointer_click(
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

    #[tool(description = "Take a screenshot of the session and return the file path")]
    async fn take_screenshot(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        // take_screenshot doesn't fit run_action because log_event needs
        // the screenshot filename (for the viewer thumbnail). Use the
        // same acquire() primitive so we still get cancel/drain semantics.
        let held = self.acquire(&params.session_id).await?;

        let outcome: Result<PathBuf, String> = async {
            let png_bytes = held.session.take_screenshot().await.map_err(|e| e.to_string())?;
            held.persist_screenshot(&params.session_id, &png_bytes)
                .await
                .map_err(|e| format!("persist screenshot: {e}"))
        }
        .await;
        let ok_display = outcome.as_ref().ok().map(|p| p.display().to_string());
        let screenshot_name = outcome
            .as_ref()
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string));
        let log_outcome = match (&ok_display, &outcome) {
            (Some(s), _) => Ok(s.as_str()),
            (None, Err(e)) => Err(e.as_str()),
            (None, Ok(_)) => unreachable!(),
        };
        if let Err(e) = held
            .log_event(
                &params.session_id,
                "take_screenshot",
                serde_json::json!({}),
                log_outcome,
                screenshot_name.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(take_screenshot) failed");
        }

        let path = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(
            path.display().to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for UiTestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Headless GTK4 UI testing server. Start a session, interact with elements, take screenshots.".to_string(),
            )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // All logging must go to stderr — stdout is the MCP JSON-RPC transport
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // Create the base report dir up front so per-session dirs land under an
    // existing parent.
    if let Err(e) = tokio::fs::create_dir_all(&cli.report_dir).await {
        tracing::warn!(error = %e, "create report_dir failed");
    }

    tracing::info!(
        report_dir = %cli.report_dir.display(),
        resolution = %cli.resolution,
        record_video = cli.record_video,
        video_bitrate = cli.video_bitrate,
        "waydriver-mcp starting"
    );

    let service = UiTestServer::new(
        cli.report_dir,
        cli.resolution,
        cli.record_video,
        cli.video_bitrate,
    )
    .serve(stdio())
    .await
    .inspect_err(|e| {
        tracing::error!("serve error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use async_trait::async_trait;
    use tempfile::TempDir;
    use waydriver::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};

    fn server() -> UiTestServer {
        UiTestServer::new(
            PathBuf::from("/tmp/waydriver-test"),
            "1024x768".into(),
            false,
            2_000_000,
        )
    }

    fn session_id(id: &str) -> Parameters<SessionIdParams> {
        Parameters(SessionIdParams {
            session_id: id.into(),
        })
    }

    // ── Mock backends ──────────────────────────────────────────────────

    struct MockCompositor {
        display: String,
    }

    #[async_trait]
    impl CompositorRuntime for MockCompositor {
        async fn start(&mut self, _resolution: Option<&str>) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> waydriver::error::Result<()> {
            Ok(())
        }
        fn id(&self) -> &str {
            "mock"
        }
        fn wayland_display(&self) -> &str {
            &self.display
        }
        fn runtime_dir(&self) -> &Path {
            Path::new("/tmp")
        }
    }

    struct MockInput {
        last_button: std::sync::Mutex<Option<u32>>,
    }

    impl MockInput {
        fn new() -> Self {
            Self {
                last_button: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl InputBackend for MockInput {
        async fn press_keysym(
            &self,
            _keysym: u32,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn key_down(
            &self,
            _keysym: u32,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn key_up(
            &self,
            _keysym: u32,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_motion_relative(
            &self,
            _dx: f64,
            _dy: f64,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_motion_absolute(
            &self,
            _x: f64,
            _y: f64,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_button_down(
            &self,
            button: u32,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            *self.last_button.lock().unwrap() = Some(button);
            Ok(())
        }
        async fn pointer_button_up(
            &self,
            _button: u32,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_axis_discrete(
            &self,
            _axis: u32,
            _steps: i32,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
    }

    struct MockCapture;

    #[async_trait]
    impl CaptureBackend for MockCapture {
        async fn start_stream(&self) -> waydriver::error::Result<PipeWireStream> {
            unimplemented!()
        }
        async fn stop_stream(&self, _stream: PipeWireStream) -> waydriver::error::Result<()> {
            Ok(())
        }
        fn pipewire_socket(&self) -> PathBuf {
            PathBuf::from("/tmp/test-pw")
        }
    }

    async fn insert_test_session(srv: &UiTestServer, id: &str, app_name: &str, display: &str) {
        insert_test_session_with(srv, id, app_name, display, true).await;
    }

    async fn insert_test_session_with(
        srv: &UiTestServer,
        id: &str,
        app_name: &str,
        display: &str,
        report_enabled: bool,
    ) {
        let session = Session::new_for_test(
            id.into(),
            app_name.into(),
            Box::new(MockInput::new()),
            Box::new(MockCapture),
            Box::new(MockCompositor {
                display: display.into(),
            }),
        );
        let report_dir = srv.report_dir.clone();
        srv.sessions.write().await.insert(
            id.into(),
            Arc::new(ManagedSession {
                session: Arc::new(session),
                report_dir,
                screenshot_counter: AtomicU32::new(0),
                events: Mutex::new(Vec::new()),
                report_enabled,
                kill_lock: Arc::new(tokio::sync::RwLock::new(())),
            }),
        );
    }

    // ── Error-path tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn list_sessions_empty() {
        let s = server();
        let result = s.list_sessions().await.unwrap();
        let text = content_text(&result);
        assert_eq!(text, "No active sessions");
    }

    #[tokio::test]
    async fn kill_session_not_found() {
        let s = server();
        let err = s.kill_session(session_id("bogus")).await.unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn dump_tree_not_found() {
        let s = server();
        let err = s.dump_tree(session_id("bogus")).await.unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn click_not_found() {
        let s = server();
        let err = s
            .click(Parameters(ClickParams {
                session_id: "bogus".into(),
                xpath: "//PushButton".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn focus_not_found() {
        let s = server();
        let err = s
            .focus(Parameters(FocusParams {
                session_id: "bogus".into(),
                xpath: "//TextBox".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn query_not_found() {
        let s = server();
        let err = s
            .query(Parameters(QueryParams {
                session_id: "bogus".into(),
                xpath: "//PushButton".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn set_text_not_found() {
        let s = server();
        let err = s
            .set_text(Parameters(SetTextParams {
                session_id: "bogus".into(),
                xpath: "//Text".into(),
                text: "hi".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn select_option_not_found() {
        let s = server();
        let err = s
            .select_option(Parameters(SelectOptionParams {
                session_id: "bogus".into(),
                xpath: "//ComboBox".into(),
                by: "label".into(),
                value: "Small".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn read_text_not_found() {
        let s = server();
        let err = s
            .read_text(Parameters(ReadTextParams {
                session_id: "bogus".into(),
                xpath: "//Label".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn type_text_not_found() {
        let s = server();
        let err = s
            .type_text(Parameters(TypeTextParams {
                session_id: "bogus".into(),
                text: "hello".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn press_key_not_found() {
        let s = server();
        let err = s
            .press_key(Parameters(PressKeyParams {
                session_id: "bogus".into(),
                key: "Return".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn move_pointer_not_found() {
        let s = server();
        let err = s
            .move_pointer(Parameters(MovePointerParams {
                session_id: "bogus".into(),
                dx: 10.0,
                dy: 20.0,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn pointer_click_not_found() {
        let s = server();
        let err = s
            .pointer_click(Parameters(PointerClickParams {
                session_id: "bogus".into(),
                button: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn take_screenshot_not_found() {
        let s = server();
        let err = s.take_screenshot(session_id("bogus")).await.unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn press_key_unknown_key() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;
        let err = s
            .press_key(Parameters(PressKeyParams {
                session_id: "test-1".into(),
                key: "NoSuchKey".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown key"));
    }

    // ── Success-path tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn list_sessions_with_entries() {
        let s = server();
        insert_test_session(&s, "abc", "calculator", "wayland-wd-abc").await;
        insert_test_session(&s, "def", "text-editor", "wayland-wd-def").await;

        let result = s.list_sessions().await.unwrap();
        let text = content_text(&result);

        assert!(text.contains("abc"));
        assert!(text.contains("calculator"));
        assert!(text.contains("wayland-wd-abc"));
        assert!(text.contains("def"));
        assert!(text.contains("text-editor"));
        assert!(text.contains("wayland-wd-def"));
    }

    #[tokio::test]
    async fn type_text_success() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;

        let result = s
            .type_text(Parameters(TypeTextParams {
                session_id: "test-1".into(),
                text: "hello".into(),
            }))
            .await
            .unwrap();
        let text = content_text(&result);
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn press_key_success() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;

        let result = s
            .press_key(Parameters(PressKeyParams {
                session_id: "test-1".into(),
                key: "Return".into(),
            }))
            .await
            .unwrap();
        let text = content_text(&result);
        assert!(text.contains("Return"));
    }

    #[tokio::test]
    async fn move_pointer_success() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;

        let result = s
            .move_pointer(Parameters(MovePointerParams {
                session_id: "test-1".into(),
                dx: 10.0,
                dy: -5.0,
            }))
            .await
            .unwrap();
        let text = content_text(&result);
        assert!(text.contains("10"));
        assert!(text.contains("-5"));
    }

    #[tokio::test]
    async fn pointer_click_default_button() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;

        let result = s
            .pointer_click(Parameters(PointerClickParams {
                session_id: "test-1".into(),
                button: None,
            }))
            .await
            .unwrap();
        let text = content_text(&result);
        // BTN_LEFT = 0x110
        assert!(
            text.contains("0x110"),
            "expected BTN_LEFT default, got: {text}"
        );
    }

    #[tokio::test]
    async fn pointer_click_explicit_button() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;

        let result = s
            .pointer_click(Parameters(PointerClickParams {
                session_id: "test-1".into(),
                button: Some(0x111), // BTN_RIGHT
            }))
            .await
            .unwrap();
        let text = content_text(&result);
        assert!(text.contains("0x111"), "expected BTN_RIGHT, got: {text}");
    }

    // ── Report path / counter ──────────────────────────────────────────

    fn make_managed(dir: PathBuf) -> ManagedSession {
        let session = Session::new_for_test(
            "sid".into(),
            "app".into(),
            Box::new(MockInput::new()),
            Box::new(MockCapture),
            Box::new(MockCompositor {
                display: "wayland-x".into(),
            }),
        );
        ManagedSession {
            session: Arc::new(session),
            report_dir: dir,
            screenshot_counter: AtomicU32::new(0),
            events: Mutex::new(Vec::new()),
            report_enabled: true,
            kill_lock: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    #[tokio::test]
    async fn persist_screenshot_writes_first_file_with_counter_one() {
        let tmp = TempDir::new().unwrap();
        let managed = make_managed(tmp.path().to_path_buf());

        let path = managed
            .persist_screenshot("sid", b"fake-png")
            .await
            .unwrap();

        let expected = tmp.path().join("sid").join("sid-1.png");
        assert_eq!(path, expected);
        assert_eq!(tokio::fs::read(&expected).await.unwrap(), b"fake-png");
    }

    #[tokio::test]
    async fn persist_screenshot_increments_counter_across_calls() {
        let tmp = TempDir::new().unwrap();
        let managed = make_managed(tmp.path().to_path_buf());

        let p1 = managed.persist_screenshot("sid", b"a").await.unwrap();
        let p2 = managed.persist_screenshot("sid", b"b").await.unwrap();
        let p3 = managed.persist_screenshot("sid", b"c").await.unwrap();

        assert_eq!(p1.file_name().unwrap(), "sid-1.png");
        assert_eq!(p2.file_name().unwrap(), "sid-2.png");
        assert_eq!(p3.file_name().unwrap(), "sid-3.png");
        // All three files should exist with distinct contents.
        assert_eq!(tokio::fs::read(&p1).await.unwrap(), b"a");
        assert_eq!(tokio::fs::read(&p2).await.unwrap(), b"b");
        assert_eq!(tokio::fs::read(&p3).await.unwrap(), b"c");
    }

    #[tokio::test]
    async fn persist_screenshot_creates_nested_missing_dirs() {
        let tmp = TempDir::new().unwrap();
        // Point at a dir that does not yet exist under the tempdir.
        let nested = tmp.path().join("does").join("not").join("exist");
        let managed = make_managed(nested.clone());

        let path = managed.persist_screenshot("sid", b"x").await.unwrap();
        assert!(path.starts_with(&nested));
        assert!(tokio::fs::metadata(&path).await.is_ok());
    }

    #[tokio::test]
    async fn persist_screenshot_honors_per_session_override_dir() {
        let base = TempDir::new().unwrap();
        let override_dir = TempDir::new().unwrap();
        // A session constructed with an override dir (as start_session would do)
        // writes there, not under the server base.
        let managed = make_managed(override_dir.path().to_path_buf());

        let path = managed.persist_screenshot("sid", b"png").await.unwrap();

        assert!(path.starts_with(override_dir.path()));
        assert!(!path.starts_with(base.path()));
    }

    #[tokio::test]
    async fn log_event_is_noop_when_report_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut managed = make_managed(tmp.path().to_path_buf());
        managed.report_enabled = false;

        let seq = managed
            .log_event(
                "sid",
                "click_element",
                serde_json::json!({ "element_name": "2" }),
                Ok("Clicked"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(seq, 0);
        assert!(
            tokio::fs::metadata(tmp.path().join("sid").join("events.jsonl"))
                .await
                .is_err()
        );
        assert!(
            tokio::fs::metadata(tmp.path().join("sid").join("events.js"))
                .await
                .is_err()
        );
        assert!(managed.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn kill_session_skips_event_write_when_report_disabled() {
        let tmp = TempDir::new().unwrap();
        let s = UiTestServer::new(
            tmp.path().to_path_buf(),
            "1024x768".into(),
            false,
            2_000_000,
        );
        insert_test_session_with(&s, "test-1", "calculator", "wayland-test-1", false).await;

        let result = s.kill_session(session_id("test-1")).await.unwrap();
        assert!(content_text(&result).contains("killed"));
        assert!(
            tokio::fs::metadata(tmp.path().join("test-1").join("events.jsonl"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn server_stores_report_base_dir() {
        let s = UiTestServer::new(
            PathBuf::from("/tmp/custom-out"),
            "1024x768".into(),
            false,
            2_000_000,
        );
        assert_eq!(s.report_dir, PathBuf::from("/tmp/custom-out"));
    }

    // ── Event log + HTML renderer ──────────────────────────────────────

    #[tokio::test]
    async fn log_event_appends_jsonl_line() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("sid"))
            .await
            .unwrap();
        let managed = make_managed(tmp.path().to_path_buf());

        managed
            .log_event(
                "sid",
                "click_element",
                serde_json::json!({ "element_name": "2" }),
                Ok("Clicked '2'"),
                None,
            )
            .await
            .unwrap();
        managed
            .log_event(
                "sid",
                "take_screenshot",
                serde_json::json!({}),
                Err("no keepalive stream"),
                None,
            )
            .await
            .unwrap();

        let contents = tokio::fs::read_to_string(tmp.path().join("sid").join("events.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let e1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(e1["seq"], 1);
        assert_eq!(e1["action"], "click_element");
        assert_eq!(e1["status"], "ok");
        assert_eq!(e1["params"]["element_name"], "2");

        let e2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(e2["seq"], 2);
        assert_eq!(e2["action"], "take_screenshot");
        assert_eq!(e2["status"], "err");
        assert_eq!(e2["message"], "no keepalive stream");

        // events.js is written atomically alongside events.jsonl and should
        // contain exactly the same events wrapped in a window.__events_update(...) call.
        let js = tokio::fs::read_to_string(tmp.path().join("sid").join("events.js"))
            .await
            .unwrap();
        assert!(js.starts_with("window.__events_update("));
        assert!(js.trim_end().ends_with(");"));
        assert!(js.contains("\"seq\":1"));
        assert!(js.contains("\"seq\":2"));
        assert!(js.contains("\"action\":\"click_element\""));
        assert!(js.contains("\"action\":\"take_screenshot\""));
    }

    #[tokio::test]
    async fn log_event_concurrent_writes_are_serialized() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("sid"))
            .await
            .unwrap();
        let managed = Arc::new(make_managed(tmp.path().to_path_buf()));

        let mut handles = Vec::new();
        for i in 0..25 {
            let m = Arc::clone(&managed);
            handles.push(tokio::spawn(async move {
                m.log_event(
                    "sid",
                    "press_key",
                    serde_json::json!({ "key": format!("k{i}") }),
                    Ok("ok"),
                    None,
                )
                .await
                .unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let contents = tokio::fs::read_to_string(tmp.path().join("sid").join("events.jsonl"))
            .await
            .unwrap();
        let mut seqs: Vec<u64> = contents
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=25).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn inserted_session_inherits_base_report_dir() {
        let s = UiTestServer::new(
            PathBuf::from("/tmp/base-out"),
            "1024x768".into(),
            false,
            2_000_000,
        );
        insert_test_session(&s, "abc", "calc", "wayland-abc").await;
        let sessions = s.sessions.read().await;
        let managed = sessions.get("abc").unwrap();
        assert_eq!(managed.report_dir, PathBuf::from("/tmp/base-out"));
        assert_eq!(managed.screenshot_counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn kill_session_success() {
        let s = server();
        insert_test_session(&s, "test-1", "calculator", "wayland-test-1").await;

        let result = s.kill_session(session_id("test-1")).await.unwrap();
        let text = content_text(&result);
        assert!(text.contains("test-1"));
        assert!(text.contains("killed"));

        // Verify session is gone
        assert!(s.sessions.read().await.is_empty());
    }

    /// Helper to extract the text content from a CallToolResult.
    fn content_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match &c.raw {
                RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    // ── start_session helpers ──────────────────────────────────────────

    #[tokio::test]
    async fn resolve_video_output_returns_none_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let path = resolve_video_output(false, tmp.path(), "sid-x").await;
        assert!(path.is_none());
        // Disabled means no side effects either — the session dir
        // shouldn't have been created.
        assert!(tokio::fs::metadata(tmp.path().join("sid-x")).await.is_err());
    }

    #[tokio::test]
    async fn resolve_video_output_creates_dir_and_returns_webm_path() {
        let tmp = TempDir::new().unwrap();
        let path = resolve_video_output(true, tmp.path(), "sid-x").await;
        let path = path.expect("expected Some path when recording is enabled");
        assert_eq!(path, tmp.path().join("sid-x").join("sid-x.webm"));
        assert!(tokio::fs::metadata(tmp.path().join("sid-x")).await.is_ok());
    }

    #[tokio::test]
    async fn seed_viewer_writes_index_html() {
        let tmp = TempDir::new().unwrap();
        seed_viewer(tmp.path(), "sid", "calculator", 1_700_000_000_000, None).await;
        let html = tokio::fs::read_to_string(tmp.path().join("sid").join("index.html"))
            .await
            .unwrap();
        assert!(html.contains("sid"));
        assert!(html.contains("calculator"));
    }

    #[tokio::test]
    async fn seed_viewer_embeds_video_filename_when_path_given() {
        let tmp = TempDir::new().unwrap();
        let video_path = tmp.path().join("sid").join("sid.webm");
        seed_viewer(tmp.path(), "sid", "app", 0, Some(&video_path)).await;
        let html = tokio::fs::read_to_string(tmp.path().join("sid").join("index.html"))
            .await
            .unwrap();
        assert!(html.contains("sid.webm"));
        assert!(html.contains("<video"));
    }

    // ── run_action helper ──────────────────────────────────────────────

    #[tokio::test]
    async fn run_action_returns_invalid_params_when_session_missing() {
        let s = server();
        let err = s
            .run_action("ghost", "test", serde_json::json!({}), |_| async move {
                Ok::<_, waydriver::Error>("unreachable".to_string())
            })
            .await
            .unwrap_err();
        assert_eq!(err.code.0, -32602, "expected invalid_params, got {err:?}");
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn run_action_propagates_locator_error_as_invalid_params() {
        // ElementNotFound is a "your selector didn't match" caller error,
        // not an infrastructure failure — must surface as invalid_params.
        let s = server();
        insert_test_session(&s, "sid", "app", "wayland-x").await;
        let err = s
            .run_action("sid", "test", serde_json::json!({}), |_| async move {
                Err::<String, _>(waydriver::Error::ElementNotFound {
                    xpath: "//Missing".into(),
                })
            })
            .await
            .unwrap_err();
        assert_eq!(err.code.0, -32602);
        assert!(err.message.contains("//Missing"));
    }

    #[tokio::test]
    async fn run_action_propagates_infra_error_as_internal_error() {
        let s = server();
        insert_test_session(&s, "sid", "app", "wayland-x").await;
        let err = s
            .run_action("sid", "test", serde_json::json!({}), |_| async move {
                Err::<String, _>(waydriver::Error::process("dbus dropped"))
            })
            .await
            .unwrap_err();
        assert_eq!(err.code.0, -32603);
        assert!(err.message.contains("dbus dropped"));
    }

    #[tokio::test]
    async fn run_action_logs_event_on_success() {
        let tmp = TempDir::new().unwrap();
        let s = UiTestServer::new(tmp.path().to_path_buf(), "1024x768".into(), false, 2_000_000);
        tokio::fs::create_dir_all(tmp.path().join("sid")).await.unwrap();
        insert_test_session(&s, "sid", "app", "wayland-x").await;

        let result = s
            .run_action(
                "sid",
                "demo",
                serde_json::json!({ "k": "v" }),
                |_| async move { Ok::<_, waydriver::Error>("did the thing".to_string()) },
            )
            .await
            .unwrap();
        assert_eq!(content_text(&result), "did the thing");

        let jsonl = tokio::fs::read_to_string(tmp.path().join("sid").join("events.jsonl"))
            .await
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(event["action"], "demo");
        assert_eq!(event["status"], "ok");
        assert_eq!(event["message"], "did the thing");
        assert_eq!(event["params"]["k"], "v");
    }

    #[tokio::test]
    async fn run_action_logs_chain_walked_error() {
        // The whole point of the new error plumbing: log_event sees the
        // same chain-serialized message the MCP response carries.
        let tmp = TempDir::new().unwrap();
        let s = UiTestServer::new(tmp.path().to_path_buf(), "1024x768".into(), false, 2_000_000);
        tokio::fs::create_dir_all(tmp.path().join("sid")).await.unwrap();
        insert_test_session(&s, "sid", "app", "wayland-x").await;

        let _ = s
            .run_action("sid", "demo", serde_json::json!({}), |_| async move {
                let io_err = std::io::Error::other("disk full");
                Err::<String, _>(waydriver::Error::screenshot_with("write png", io_err))
            })
            .await
            .unwrap_err();

        let jsonl = tokio::fs::read_to_string(tmp.path().join("sid").join("events.jsonl"))
            .await
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(event["status"], "err");
        let msg = event["message"].as_str().unwrap();
        assert!(msg.contains("write png"), "missing operation: {msg}");
        assert!(msg.contains("disk full"), "missing source: {msg}");
        assert!(msg.contains(" | "), "missing chain separator: {msg}");
    }

    // ── Cooperative cancellation ───────────────────────────────────────

    #[tokio::test]
    async fn run_action_bails_fast_when_session_cancelled_mid_work() {
        // A tool whose closure is stuck in a long sleep should be
        // interrupted as soon as the session's cancellation token trips —
        // it exits via the token-aware select in its own body (or, for
        // real tools, via poll_with_retry). Here we prove the wiring:
        // the closure yields to the token, and run_action returns
        // quickly with Err(Cancelled-derived).
        let s = server();
        insert_test_session(&s, "sid", "app", "wayland-x").await;

        // Get a handle to the session's token so the spawned task can
        // signal cancellation after a delay.
        let token = {
            let sessions = s.sessions.read().await;
            sessions.get("sid").unwrap().session.cancellation_token().clone()
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });

        let start = std::time::Instant::now();
        let err = s
            .run_action("sid", "slow", serde_json::json!({}), move |sess| async move {
                // Race the token against a long sleep, same pattern
                // poll_with_retry uses internally.
                tokio::select! {
                    _ = sess.cancellation_token().cancelled() => {
                        Err(waydriver::Error::Cancelled)
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        Ok("slept 30s".to_string())
                    }
                }
            })
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        assert!(err.message.contains("cancelled"), "got: {}", err.message);
        assert!(
            elapsed < Duration::from_millis(500),
            "run_action should return promptly on cancel; elapsed = {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn kill_session_interrupts_in_flight_tool_call() {
        // The whole point of the cooperative-cancellation refactor:
        // calling kill_session on a session with an in-flight tool
        // should *interrupt* the tool (via the cancellation token) AND
        // then deterministically unwrap the session for teardown —
        // end-to-end, in milliseconds, not the 30s natural timeout.
        let tmp = TempDir::new().unwrap();
        let s = UiTestServer::new(
            tmp.path().to_path_buf(),
            "1024x768".into(),
            false,
            2_000_000,
        );
        insert_test_session(&s, "sid", "app", "wayland-x").await;

        // Spawn a tool that would otherwise sleep for 30s. It races
        // the cancellation token against its sleep, same as real tools
        // do via poll_with_retry.
        let s_for_tool = s.clone();
        let tool = tokio::spawn(async move {
            s_for_tool
                .run_action("sid", "slow", serde_json::json!({}), |sess| async move {
                    tokio::select! {
                        _ = sess.cancellation_token().cancelled() => {
                            Err(waydriver::Error::Cancelled)
                        }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            Ok("slept 30s".to_string())
                        }
                    }
                })
                .await
        });

        // Give the tool a moment to get past acquire() and into its
        // work so kill really races against an in-flight call, not a
        // not-yet-started one.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let start = std::time::Instant::now();
        let kill_result = s.kill_session(session_id("sid")).await.unwrap();
        let elapsed = start.elapsed();

        assert!(content_text(&kill_result).contains("killed"));
        assert!(
            elapsed < Duration::from_secs(2),
            "kill_session must not wait for the tool's natural timeout; elapsed = {elapsed:?}"
        );

        // The tool itself should have seen the cancellation and returned
        // an error (it was told to bail).
        let tool_outcome = tool.await.unwrap();
        assert!(tool_outcome.is_err(), "tool should surface cancellation");

        // Session is fully removed from the map.
        assert!(s.sessions.read().await.is_empty());
    }

    #[tokio::test]
    async fn tools_on_different_sessions_do_not_block_each_other() {
        // With the per-session drain lock, a slow tool on session A
        // must not delay a tool on session B. The old map-wide RwLock
        // would have serialized them.
        let s = server();
        insert_test_session(&s, "a", "app", "wayland-a").await;
        insert_test_session(&s, "b", "app", "wayland-b").await;

        let s_for_a = s.clone();
        let slow = tokio::spawn(async move {
            s_for_a
                .run_action("a", "slow", serde_json::json!({}), |_sess| async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok("done".to_string())
                })
                .await
        });

        // Give slow a moment to acquire its session.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let start = std::time::Instant::now();
        let fast = s
            .run_action("b", "fast", serde_json::json!({}), |_sess| async move {
                Ok::<_, waydriver::Error>("quick".to_string())
            })
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(content_text(&fast), "quick");
        assert!(
            elapsed < Duration::from_millis(200),
            "tool on session B blocked on session A's work; elapsed = {elapsed:?}"
        );

        // Cleanup so the slow task doesn't leak past the test.
        slow.await.unwrap().unwrap();
    }
}
