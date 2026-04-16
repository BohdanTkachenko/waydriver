//! End-to-end tests for the MCP server via JSON-RPC over stdio.
//!
//! Each test spawns the `waydriver-mcp` binary as a child process, connects
//! an rmcp client, and exercises MCP tools against a real gnome-calculator
//! session (headless mutter, AT-SPI, PipeWire — the full stack).
//!
//! These tests share the same `#[ignore]` caveat as the library e2e tests:
//! gnome-calculator's singleton D-Bus activation causes parallel sessions
//! to interfere. Run explicitly with:
//!
//! ```sh
//! cargo test -p waydriver-mcp --test e2e -- --ignored --test-threads=1
//! ```

use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ServiceExt};

struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

fn mcp_binary() -> std::path::PathBuf {
    // cargo sets this when running integration tests
    let mut path = std::env::current_exe()
        .unwrap()
        .parent() // deps/
        .unwrap()
        .parent() // debug/
        .unwrap()
        .to_path_buf();
    path.push("waydriver-mcp");
    path
}

fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn run_calculator_test(command: tokio::process::Command, local: bool) -> anyhow::Result<()> {
    let transport = TokioChildProcess::new(command)?;
    let client = TestClient.serve(transport).await?;

    // List tools — verify the server exposes all expected tools
    let tools = client.list_all_tools().await?;
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "start_session",
        "list_sessions",
        "kill_session",
        "inspect_ui",
        "click_element",
        "type_text",
        "press_key",
        "find_element",
        "move_pointer",
        "pointer_click",
        "take_screenshot",
    ] {
        assert!(
            tool_names.iter().any(|n| n == expected),
            "missing tool: {expected}, got: {tool_names:?}"
        );
    }

    // Start a calculator session
    let result = client
        .call_tool(
            CallToolRequestParams::new("start_session").with_arguments(
                serde_json::json!({"command": "gnome-calculator"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    let text = result_text(&result);
    assert!(text.contains("Session started"), "unexpected: {text}");

    // Extract session id from "Session started: id=XXXX, ..."
    let session_id = text
        .split("id=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_string();

    // Let the app render
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Dismiss any startup dialog
    client
        .call_tool(
            CallToolRequestParams::new("press_key").with_arguments(
                serde_json::json!({"session_id": session_id, "key": "Escape"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Inspect UI — verify accessibility tree has calculator buttons
    let result = client
        .call_tool(
            CallToolRequestParams::new("inspect_ui").with_arguments(
                serde_json::json!({"session_id": session_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    let tree = result_text(&result);
    assert!(tree.contains("[button]"), "tree should contain buttons");

    // Click 2 + 3 = via MCP tools
    for name in ["2", "+", "3", "="] {
        let result = client
            .call_tool(
                CallToolRequestParams::new("click_element").with_arguments(
                    serde_json::json!({"session_id": session_id, "element_name": name})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await?;
        assert!(
            !result.is_error.unwrap_or(false),
            "click_element({name}) failed: {}",
            result_text(&result)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Take a screenshot — verify file is created
    let result = client
        .call_tool(
            CallToolRequestParams::new("take_screenshot").with_arguments(
                serde_json::json!({"session_id": session_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    let screenshot_path = result_text(&result);
    assert!(
        screenshot_path.ends_with(".png"),
        "expected png path, got: {screenshot_path}"
    );
    if local {
        let metadata = tokio::fs::metadata(&screenshot_path).await?;
        assert!(metadata.len() > 1000, "screenshot file too small");
    }

    // List sessions — should contain our session
    let result = client
        .call_tool(CallToolRequestParams::new("list_sessions"))
        .await?;
    let text = result_text(&result);
    assert!(text.contains(&session_id), "session should be listed");
    assert!(text.contains("gnome-calculator"), "app name should appear");

    // Find element — look up the "5" button
    let result = client
        .call_tool(
            CallToolRequestParams::new("find_element").with_arguments(
                serde_json::json!({"session_id": session_id, "element_name": "5"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    let text = result_text(&result);
    assert!(text.contains("Found '5'"), "should find button 5: {text}");

    // Type text
    let result = client
        .call_tool(
            CallToolRequestParams::new("type_text").with_arguments(
                serde_json::json!({"session_id": session_id, "text": "10"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false), "type_text failed");

    // Move pointer
    let result = client
        .call_tool(
            CallToolRequestParams::new("move_pointer").with_arguments(
                serde_json::json!({"session_id": session_id, "dx": 50.0, "dy": 50.0})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false), "move_pointer failed");

    // Pointer click (default BTN_LEFT)
    let result = client
        .call_tool(
            CallToolRequestParams::new("pointer_click").with_arguments(
                serde_json::json!({"session_id": session_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false), "pointer_click failed");

    // Kill session
    let result = client
        .call_tool(
            CallToolRequestParams::new("kill_session").with_arguments(
                serde_json::json!({"session_id": session_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    let text = result_text(&result);
    assert!(text.contains("killed"), "session should be killed: {text}");

    // Verify session is gone
    let result = client
        .call_tool(CallToolRequestParams::new("list_sessions"))
        .await?;
    let text = result_text(&result);
    assert_eq!(text, "No active sessions");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn calculator_add_via_mcp() -> anyhow::Result<()> {
    run_calculator_test(tokio::process::Command::new(mcp_binary()), true).await
}

#[tokio::test]
#[ignore = "requires pre-built waydriver-mcp-e2e docker image"]
async fn calculator_add_via_docker() -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["run", "--rm", "-i", "waydriver-mcp-e2e:latest"]);
    run_calculator_test(cmd, false).await
}
