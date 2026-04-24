//! End-to-end test for the MCP server via JSON-RPC over stdio.
//!
//! A single test drives the `waydriver-mcp:e2e` Docker image against
//! the project's own `waydriver-fixture-gtk` binary through the full
//! stack (headless mutter, AT-SPI, PipeWire). Docker runs it in CI.
//! Library-level coverage for the equivalent flows — click, dump_tree,
//! waits, keyboard input, etc. — lives in `crates/waydriver/tests/e2e.rs`
//! against the same fixture.
//!
//! Run explicitly with:
//!
//! ```sh
//! nix run .#docker-build-e2e  # build the image first
//! cargo test -p waydriver-mcp --test e2e -- --ignored
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

fn extract_kv(text: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    Some(
        rest.split([',', '\n'])
            .next()
            .unwrap_or(rest)
            .trim()
            .to_string(),
    )
}

async fn run_fixture_test(command: tokio::process::Command) -> anyhow::Result<()> {
    let transport = TokioChildProcess::new(command)?;
    let client = TestClient.serve(transport).await?;

    // List tools — verify the server exposes all expected tools.
    let tools = client.list_all_tools().await?;
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "start_session",
        "list_sessions",
        "kill_session",
        "dump_tree",
        "query",
        "click",
        "focus",
        "set_text",
        "read_text",
        "type_text",
        "press_key",
        "move_pointer",
        "pointer_click",
        "take_screenshot",
    ] {
        assert!(
            tool_names.iter().any(|n| n == expected),
            "missing tool: {expected}, got: {tool_names:?}"
        );
    }

    // Start a fixture session pinned to the gtk4 section so selectors
    // are unambiguous. The `/usr/local/bin/waydriver-fixture-gtk` path
    // matches where the Dockerfile copies the binary.
    let result = client
        .call_tool(
            CallToolRequestParams::new("start_session").with_arguments(
                serde_json::json!({
                    "command": "waydriver-fixture-gtk",
                    "args": ["--section=gtk4"]
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    let text = result_text(&result);
    assert!(text.contains("Session started"), "unexpected: {text}");

    // Extract session id from "Session started: id=XXXX, ..."
    let session_id = extract_kv(&text, "id").expect("id= in start_session response");

    // Let the app render its initial frame.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Dump tree — verify the XML snapshot has Button elements.
    let result = client
        .call_tool(
            CallToolRequestParams::new("dump_tree").with_arguments(
                serde_json::json!({"session_id": session_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    let tree = result_text(&result);
    assert!(tree.contains("<?xml"), "tree should be XML, got: {tree}");
    assert!(
        tree.contains("<Button"),
        "tree should contain Button elements, got: {tree}"
    );

    // Click the fixture's named buttons via XPath. `primary-button` fires
    // its click handler silently; `mode-toggle` toggles visual state. Both
    // exercise the MCP `click` → AT-SPI `do_action(0)` path end-to-end.
    for (role, name) in [
        ("Button", "primary-button"),
        ("ToggleButton", "mode-toggle"),
    ] {
        let xpath = format!("//{role}[@name='{name}']");
        let result = client
            .call_tool(
                CallToolRequestParams::new("click").with_arguments(
                    serde_json::json!({"session_id": session_id, "xpath": xpath})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await?;
        assert!(
            !result.is_error.unwrap_or(false),
            "click({xpath}) failed: {}",
            result_text(&result)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Take a screenshot — verify file is created.
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
        screenshot_path.ends_with(&format!("/{session_id}/{session_id}-1.png")),
        "expected path ending in /{session_id}/{session_id}-1.png, got: {screenshot_path}"
    );

    // Take a second screenshot — counter should advance to 2.
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
    let second_path = result_text(&result);
    assert!(
        second_path.ends_with(&format!("/{session_id}/{session_id}-2.png")),
        "expected second screenshot to use counter 2, got: {second_path}"
    );

    // List sessions — should contain our session and the fixture app name.
    let result = client
        .call_tool(CallToolRequestParams::new("list_sessions"))
        .await?;
    let text = result_text(&result);
    assert!(text.contains(&session_id), "session should be listed");
    assert!(
        text.contains("waydriver-fixture-gtk"),
        "app name should appear in: {text}"
    );

    // Query by selector — look up primary-button and check its shape.
    let result = client
        .call_tool(
            CallToolRequestParams::new("query").with_arguments(
                serde_json::json!({
                    "session_id": session_id,
                    "xpath": "//Button[@name='primary-button']"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    let text = result_text(&result);
    let matches: serde_json::Value = serde_json::from_str(&text)?;
    assert!(
        matches.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "expected at least one match for //Button[@name='primary-button'], got: {text}"
    );
    assert_eq!(matches[0]["role"], "Button");
    assert_eq!(matches[0]["name"], "primary-button");

    // Type text into the fixture's focused text-entry. The fixture grabs
    // focus on that widget at startup, so type_text lands directly.
    let result = client
        .call_tool(
            CallToolRequestParams::new("type_text").with_arguments(
                serde_json::json!({"session_id": session_id, "text": "hello"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert!(!result.is_error.unwrap_or(false), "type_text failed");

    // Move pointer — primitive test, just verify the call doesn't error.
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

    // Pointer click (default BTN_LEFT).
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

    // Kill session.
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

    // Verify session is gone.
    let result = client
        .call_tool(CallToolRequestParams::new("list_sessions"))
        .await?;
    let text = result_text(&result);
    assert_eq!(text, "No active sessions");

    client.cancel().await?;
    Ok(())
}

/// Drives the MCP server in the `waydriver-mcp-e2e` Docker image
/// against the fixture. The container gives us a private D-Bus session
/// bus per run, so parallel CI invocations and host-level AT-SPI state
/// don't interfere.
#[tokio::test]
#[ignore = "requires pre-built waydriver-mcp-e2e docker image"]
async fn fixture_via_docker() -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["run", "--rm", "-i", "waydriver-mcp-e2e:latest"]);
    run_fixture_test(cmd).await
}
