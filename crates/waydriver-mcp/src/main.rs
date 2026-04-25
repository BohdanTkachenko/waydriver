use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use clap::Parser;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::*;
use rmcp::transport::stdio;
use rmcp::{tool_handler, ErrorData as McpError, ServerHandler, ServiceExt};

mod cli;
mod mcp_error;
mod params;
mod report;
mod session;
mod tools;

use cli::Cli;
use mcp_error::waydriver_to_mcp;
use session::ManagedSession;

// ── Server ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UiTestServer {
    pub(crate) sessions: Arc<RwLock<HashMap<String, Arc<ManagedSession>>>>,
    pub(crate) report_dir: PathBuf,
    pub(crate) default_resolution: String,
    pub(crate) default_record_video: bool,
    pub(crate) default_video_bitrate: u32,
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
pub(crate) struct InFlightSession {
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
    pub(crate) async fn acquire(&self, session_id: &str) -> Result<InFlightSession, McpError> {
        // Phase 1: take the map read lock just long enough to clone the
        // session Arc out. After this scope exits, a concurrent
        // `kill_session` can take the map write lock immediately — it
        // will then wait on the per-session drain lock instead of the
        // whole map.
        let managed = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned().ok_or_else(|| {
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
    pub(crate) async fn run_action<F, Fut>(
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
        let log_outcome = log_view
            .as_ref()
            .map(String::as_str)
            .map_err(String::as_str);
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

impl UiTestServer {
    /// Build a fresh `UiTestServer` with an empty session map and a
    /// composed `ToolRouter` merged from every per-concern router.
    ///
    /// Each `tools::<concern>` module exposes a
    /// `#[tool_router(router = <concern>_router)]` constructor; we sum
    /// them here into the single `tool_router` field the
    /// `#[tool_handler]` below dispatches against. Adding a new tool
    /// group is: new module + new `+ Self::<group>_router()` line.
    pub fn new(
        report_dir: PathBuf,
        default_resolution: String,
        default_record_video: bool,
        default_video_bitrate: u32,
    ) -> Self {
        let tool_router = Self::lifecycle_router()
            + Self::inspection_router()
            + Self::interaction_router()
            + Self::capture_router();
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            report_dir,
            default_resolution,
            default_record_video,
            default_video_bitrate,
            tool_router,
        }
    }
}

// Default `#[tool_handler]` in rmcp 1.4 expands to `Self::tool_router()` —
// i.e. it expects a single generated function. We split the router across
// five `#[tool_router(router = <concern>_router)]` impls in `tools::*` and
// merge them in `UiTestServer::new`, so point the handler at the merged
// field instead.
#[tool_handler(router = self.tool_router)]
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use rmcp::handler::server::wrapper::Parameters;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use waydriver::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
    use waydriver::Session;

    use crate::params::{
        ClickParams, FocusParams, MovePointerParams, PointerClickParams, PressKeyParams,
        QueryParams, ReadTextParams, SelectOptionByParam, SelectOptionParams, SessionIdParams,
        SetTextParams, TypeTextParams,
    };
    use crate::tools::lifecycle::{resolve_video_output, seed_viewer};

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
        last_button: std::sync::Mutex<Option<waydriver::PointerButton>>,
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
            button: waydriver::PointerButton,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            *self.last_button.lock().unwrap() = Some(button);
            Ok(())
        }
        async fn pointer_button_up(
            &self,
            _button: waydriver::PointerButton,
            _: &tokio_util::sync::CancellationToken,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_axis_discrete(
            &self,
            _axis: waydriver::PointerAxis,
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
                events: Mutex::new(report::EventLog::new()),
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
                by: SelectOptionByParam::Label,
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
            events: Mutex::new(report::EventLog::new()),
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
        let s = UiTestServer::new(
            tmp.path().to_path_buf(),
            "1024x768".into(),
            false,
            2_000_000,
        );
        tokio::fs::create_dir_all(tmp.path().join("sid"))
            .await
            .unwrap();
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
        let s = UiTestServer::new(
            tmp.path().to_path_buf(),
            "1024x768".into(),
            false,
            2_000_000,
        );
        tokio::fs::create_dir_all(tmp.path().join("sid"))
            .await
            .unwrap();
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
            sessions
                .get("sid")
                .unwrap()
                .session
                .cancellation_token()
                .clone()
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });

        let start = std::time::Instant::now();
        let err = s
            .run_action(
                "sid",
                "slow",
                serde_json::json!({}),
                move |sess| async move {
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
                },
            )
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
