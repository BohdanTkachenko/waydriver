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

use std::path::Path;
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

async fn run_calculator_test(
    command: tokio::process::Command,
    local: bool,
    report_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let transport = TokioChildProcess::new(command)?;
    let client = TestClient.serve(transport).await?;

    // List tools — verify the server exposes all expected tools
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
    let session_id = extract_kv(&text, "id").expect("id= in start_session response");
    let report_url = extract_kv(&text, "report");
    if local {
        assert!(
            report_url
                .as_deref()
                .is_some_and(|u| u.starts_with("file://")),
            "expected file:// report URL in response, got: {text}"
        );
    }

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

    // Dump tree — verify the XML snapshot has calculator buttons
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

    // Click 2 + 3 = via XPath selectors
    for name in ["2", "+", "3", "="] {
        let xpath = format!("//Button[@name='{name}']");
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
        screenshot_path.ends_with(&format!("/{session_id}/{session_id}-1.png")),
        "expected path ending in /{session_id}/{session_id}-1.png, got: {screenshot_path}"
    );
    if local {
        let metadata = tokio::fs::metadata(&screenshot_path).await?;
        assert!(metadata.len() > 1000, "screenshot file too small");
    }

    // Take a second screenshot — counter should advance to 2
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

    // List sessions — should contain our session
    let result = client
        .call_tool(CallToolRequestParams::new("list_sessions"))
        .await?;
    let text = result_text(&result);
    assert!(text.contains(&session_id), "session should be listed");
    assert!(text.contains("gnome-calculator"), "app name should appear");

    // Query by selector — look up the "5" button
    let result = client
        .call_tool(
            CallToolRequestParams::new("query").with_arguments(
                serde_json::json!({
                    "session_id": session_id,
                    "xpath": "//Button[@name='5']"
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
        "expected at least one match for //Button[@name='5'], got: {text}"
    );
    assert_eq!(matches[0]["role"], "Button");
    assert_eq!(matches[0]["name"], "5");

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

    // Event log + HTML report assertions (local only — docker run doesn't
    // expose the container's report dir or its HTTP port to the host).
    if local {
        let dir = report_dir.expect("local mode requires report_dir");
        let session_dir = dir.join(&session_id);

        let events_path = session_dir.join("events.jsonl");
        let events_raw = tokio::fs::read_to_string(&events_path).await?;
        let events: Vec<serde_json::Value> = events_raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert!(
            !events.is_empty(),
            "events.jsonl at {events_path:?} was empty"
        );
        assert_eq!(events.first().unwrap()["action"], "start_session");
        assert_eq!(events.last().unwrap()["action"], "kill_session");

        let actions: Vec<&str> = events
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        for expected in [
            "start_session",
            "press_key",
            "dump_tree",
            "click",
            "take_screenshot",
            "query",
            "type_text",
            "move_pointer",
            "pointer_click",
            "kill_session",
        ] {
            assert!(
                actions.contains(&expected),
                "expected action {expected} in event log, got: {actions:?}"
            );
        }

        let screenshot_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["action"] == "take_screenshot" && e["status"] == "ok")
            .collect();
        assert_eq!(screenshot_events.len(), 2);
        assert_eq!(
            screenshot_events[0]["screenshot"],
            format!("{session_id}-1.png")
        );
        assert_eq!(
            screenshot_events[1]["screenshot"],
            format!("{session_id}-2.png")
        );

        let index_html = tokio::fs::read_to_string(session_dir.join("index.html")).await?;
        assert!(index_html.contains(&session_id));
        assert!(index_html.contains("events.js?v="));
        assert!(index_html.contains("window.__events_update"));
        assert!(index_html.contains("cdn.tailwindcss.com"));

        // Static viewer loads events.js (not events.jsonl) via a <script src>
        // swap trick — so check that file exists and contains all seqs.
        let events_js = tokio::fs::read_to_string(session_dir.join("events.js")).await?;
        assert!(events_js.starts_with("window.__events_update("));
        for seq in 1..=events.len() {
            assert!(
                events_js.contains(&format!("\"seq\":{seq}")),
                "events.js missing seq {seq}"
            );
        }

        // WebM recording is on by default; kill_session flushes EOS + cues.
        // We don't decode the video here — a non-empty file with a WebM magic
        // header is enough to show the pipeline ran end to end.
        let webm_path = session_dir.join(format!("{session_id}.webm"));
        let webm_bytes = tokio::fs::read(&webm_path).await?;
        assert!(
            webm_bytes.len() > 1000,
            "webm at {webm_path:?} too small: {} bytes",
            webm_bytes.len()
        );
        // EBML header magic: 1A 45 DF A3
        assert_eq!(
            &webm_bytes[..4],
            &[0x1A, 0x45, 0xDF, 0xA3],
            "expected EBML magic at start of webm"
        );

        // Viewer HTML embeds the <video> element pointing at the webm file.
        assert!(
            index_html.contains(&format!("src=\"{session_id}.webm\"")),
            "index.html should reference {session_id}.webm"
        );

        // start_session should have returned a file:// URL matching the report dir.
        let expected_url = format!("file://{}/{}/index.html", dir.display(), session_id);
        assert_eq!(
            report_url.as_deref(),
            Some(expected_url.as_str()),
            "start_session report URL should be a file:// URL"
        );
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn calculator_add_via_mcp() -> anyhow::Result<()> {
    let report_dir = tempfile::tempdir()?;
    let mut cmd = tokio::process::Command::new(mcp_binary());
    cmd.args(["--report-dir", report_dir.path().to_str().unwrap()]);
    run_calculator_test(cmd, true, Some(report_dir.path())).await
}

#[tokio::test]
#[ignore = "requires pre-built waydriver-mcp-e2e docker image"]
async fn calculator_add_via_docker() -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["run", "--rm", "-i", "waydriver-mcp-e2e:latest"]);
    run_calculator_test(cmd, false, None).await
}
