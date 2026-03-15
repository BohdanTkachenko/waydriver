use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use serde::Deserialize;

use waydriver::atspi as atspi_client;
use waydriver::keysym::{char_to_keysym, key_name_to_keysym};
use waydriver::{CompositorRuntime, Session, SessionConfig};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;

// ── Parameter types ─────────────────────────────────────────────────────────

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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionIdParams {
    /// Session ID returned by start_session
    pub session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickElementParams {
    /// Session ID
    pub session_id: String,
    /// Accessible name of the element to click
    pub element_name: String,
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
pub struct FindElementParams {
    /// Session ID
    pub session_id: String,
    /// Accessible name of the element to find
    pub element_name: String,
}

// ── Server ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UiTestServer {
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Default for UiTestServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl UiTestServer {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Start a headless Wayland session with mutter and launch an application")]
    async fn start_session(
        &self,
        Parameters(params): Parameters<StartSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        let app_name = params
            .app_name
            .clone()
            .unwrap_or_else(|| params.command.clone());

        // Construct and pre-start the mutter compositor so we can pull its
        // shared Arc<MutterState> out before erasing to trait objects. Input
        // and capture are thin wrappers around that Arc, so they get cloned
        // references to the same D-Bus connection.
        let mut compositor = MutterCompositor::new();
        compositor
            .start()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let state = compositor.state();
        let input = MutterInput::new(state.clone());
        let capture = MutterCapture::new(state);

        let session = Session::start(
            Box::new(compositor),
            Box::new(input),
            Box::new(capture),
            SessionConfig {
                command: params.command,
                args: params.args,
                cwd: params.cwd,
                app_name: app_name.clone(),
            },
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let id = session.id.clone();
        let display = session.wayland_display().to_string();

        self.sessions.write().await.insert(id.clone(), session);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session started: id={}, display={}, app={}",
            id, display, app_name
        ))]))
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
        for (id, s) in sessions.iter() {
            lines.push(format!(
                "- {} (app={}, display={})",
                id,
                s.app_name,
                s.wayland_display()
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
        let session = self
            .sessions
            .write()
            .await
            .remove(&params.session_id)
            .ok_or_else(|| {
                McpError::invalid_params(format!("session not found: {}", params.session_id), None)
            })?;

        session
            .kill()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session {} killed",
            params.session_id
        ))]))
    }

    #[tool(description = "Dump the accessibility tree of the application UI")]
    async fn inspect_ui(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        let a11y = session.a11y_connection.as_ref().ok_or_else(|| {
            McpError::internal_error("no AT-SPI connection for this session".to_string(), None)
        })?;
        let tree = atspi_client::dump_app_tree(a11y, &session.app_bus_name, &session.app_path)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(tree)]))
    }

    #[tool(description = "Click a UI element by its accessible name")]
    async fn click_element(
        &self,
        Parameters(params): Parameters<ClickElementParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        let a11y = session.a11y_connection.as_ref().ok_or_else(|| {
            McpError::internal_error("no AT-SPI connection for this session".to_string(), None)
        })?;
        let result = atspi_client::click_element(
            a11y,
            &session.app_bus_name,
            &session.app_path,
            &params.element_name,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Type text into the currently focused element via keyboard input")]
    async fn type_text(
        &self,
        Parameters(params): Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        // Type each character via the input backend (goes through the compositor)
        for ch in params.text.chars() {
            let keysym = char_to_keysym(ch);
            session
                .press_keysym(keysym)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Typed '{}'",
            params.text
        ))]))
    }

    #[tool(description = "Press a keyboard key (e.g. 'Return', 'Tab', 'a')")]
    async fn press_key(
        &self,
        Parameters(params): Parameters<PressKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        let keysym = key_name_to_keysym(&params.key).ok_or_else(|| {
            McpError::invalid_params(format!("unknown key: {}", params.key), None)
        })?;

        session
            .press_keysym(keysym)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Pressed '{}'",
            params.key
        ))]))
    }

    #[tool(description = "Move the pointer by a relative offset in logical pixels")]
    async fn move_pointer(
        &self,
        Parameters(params): Parameters<MovePointerParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        session
            .pointer_motion_relative(params.dx, params.dy)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Pointer moved by ({}, {})",
            params.dx, params.dy
        ))]))
    }

    #[tool(description = "Press and release a pointer button (defaults to left click)")]
    async fn pointer_click(
        &self,
        Parameters(params): Parameters<PointerClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        let button = params.button.unwrap_or(0x110); // BTN_LEFT
        session
            .pointer_button(button)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Pointer button {:#x} clicked",
            button
        ))]))
    }

    #[tool(
        description = "Find a UI element by its accessible name and return its details (bus_name, path, role)"
    )]
    async fn find_element(
        &self,
        Parameters(params): Parameters<FindElementParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        let a11y = session.a11y_connection.as_ref().ok_or_else(|| {
            McpError::internal_error("no AT-SPI connection for this session".to_string(), None)
        })?;
        let (bus_name, path, role) = atspi_client::find_element_by_name(
            a11y,
            &session.app_bus_name,
            &session.app_path,
            &params.element_name,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Found '{}': bus_name={}, path={}, role={}",
            params.element_name, bus_name, path, role
        ))]))
    }

    #[tool(description = "Take a screenshot of the session and return the file path")]
    async fn take_screenshot(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;

        let png_bytes = session
            .take_screenshot()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let path = format!("/tmp/mcp-screenshot-{}.png", params.session_id);
        tokio::fs::write(&path, &png_bytes)
            .await
            .map_err(|e| McpError::internal_error(format!("write screenshot: {e}"), None))?;

        Ok(CallToolResult::success(vec![Content::text(path)]))
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
    // All logging must go to stderr — stdout is the MCP JSON-RPC transport
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("waydriver-mcp starting");

    let service = UiTestServer::new().serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serve error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use async_trait::async_trait;
    use waydriver::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};

    fn server() -> UiTestServer {
        UiTestServer::new()
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
        async fn start(&mut self) -> waydriver::error::Result<()> {
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
        async fn press_keysym(&self, _keysym: u32) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_motion_relative(
            &self,
            _dx: f64,
            _dy: f64,
        ) -> waydriver::error::Result<()> {
            Ok(())
        }
        async fn pointer_button(&self, button: u32) -> waydriver::error::Result<()> {
            *self.last_button.lock().unwrap() = Some(button);
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
        let session = Session::new_for_test(
            id.into(),
            app_name.into(),
            Box::new(MockInput::new()),
            Box::new(MockCapture),
            Box::new(MockCompositor {
                display: display.into(),
            }),
        );
        srv.sessions.write().await.insert(id.into(), session);
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
    async fn inspect_ui_not_found() {
        let s = server();
        let err = s.inspect_ui(session_id("bogus")).await.unwrap_err();
        assert!(err.message.contains("session not found"));
    }

    #[tokio::test]
    async fn click_element_not_found() {
        let s = server();
        let err = s
            .click_element(Parameters(ClickElementParams {
                session_id: "bogus".into(),
                element_name: "x".into(),
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
    async fn find_element_not_found() {
        let s = server();
        let err = s
            .find_element(Parameters(FindElementParams {
                session_id: "bogus".into(),
                element_name: "x".into(),
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
}
