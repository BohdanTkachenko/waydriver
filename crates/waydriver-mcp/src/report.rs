//! Per-session reporting: event log + live HTML viewer + query rendering.
//!
//! Each session under report mode writes to `{report_dir}/{session_id}/`:
//!
//! - `events.jsonl` — durable, append-only log of every tool call.
//! - `events.js` — atomically-rewritten JS payload that the viewer reloads
//!   every 2s via a `<script src=...>` swap (Chrome blocks `fetch()` over
//!   `file://` but not `<script>`).
//! - `index.html` — the static viewer shell rendered by [`render_index_html`].
//!
//! The `query` tool also lives here (well, its result-shaping function does)
//! since [`render_matches`] is what the user-facing JSON looks like.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use waydriver::atspi::ElementInfo;

/// Append one event to `{report_dir}/{session_id}/events.jsonl` and rewrite
/// `{report_dir}/{session_id}/events.js` atomically. Returns the assigned
/// 1-based sequence number.
///
/// `events` is the in-memory mirror of the on-disk log; the same lock guards
/// both files so concurrent calls never interleave.
#[allow(clippy::too_many_arguments)]
pub async fn append_event(
    report_dir: &std::path::Path,
    session_id: &str,
    events: &Mutex<Vec<serde_json::Value>>,
    action: &'static str,
    params: serde_json::Value,
    outcome: Result<&str, &str>,
    screenshot: Option<&str>,
) -> std::io::Result<u32> {
    let mut guard = events.lock().await;
    let seq = guard.len() as u32 + 1;
    let ts_ms = now_ms();
    let (status, message) = match outcome {
        Ok(msg) => ("ok", msg),
        Err(msg) => ("err", msg),
    };
    let mut event = serde_json::json!({
        "seq": seq,
        "ts_ms": ts_ms,
        "action": action,
        "params": params,
        "status": status,
        "message": message,
    });
    if let Some(name) = screenshot {
        event["screenshot"] = serde_json::Value::String(name.to_string());
    }

    // 1. Append to events.jsonl (durable source of truth).
    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');
    let session_dir = report_dir.join(session_id);
    let jsonl_path = session_dir.join("events.jsonl");
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&jsonl_path)
        .await?;
    file.write_all(&line).await?;
    file.flush().await?;

    // 2. Push into in-memory vec.
    guard.push(event);

    // 3. Rewrite events.js atomically (tempfile + rename on same filesystem).
    // The viewer HTML swaps in a fresh <script src="events.js?v=..."> every 2s,
    // which triggers window.__events_update with the full array.
    let json_array = serde_json::to_string(&*guard)?;
    let js_body = format!("window.__events_update({json_array});\n");
    let tmp_path = session_dir.join(".events.js.tmp");
    tokio::fs::write(&tmp_path, js_body.as_bytes()).await?;
    tokio::fs::rename(&tmp_path, session_dir.join("events.js")).await?;

    Ok(seq)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Render the static viewer shell written once per session. The shell fetches
/// `events.jsonl` at load time (and on an interval) and renders each entry as
/// a styled card. If `video_file` is `Some`, a `<video>` element is embedded
/// at the top of the page pointing at that filename (relative to the session
/// dir).
///
/// The HTML/CSS/JS skeleton lives in `viewer.html` and is included at compile
/// time; this function fills in five `__SENTINEL__` placeholders with the
/// per-session values. Sentinels are used (rather than `format!`) so the
/// embedded JavaScript can use real `{`/`}` braces without having to escape
/// them as `{{`/`}}`.
pub fn render_index_html(
    session_id: &str,
    app_name: &str,
    started_at_ms: u64,
    video_file: Option<&str>,
) -> String {
    const TEMPLATE: &str = include_str!("viewer.html");

    let video_block = match video_file {
        Some(name) => format!(
            r#"<video controls preload="metadata" class="w-full rounded-lg border border-slate-200 shadow-sm bg-black mb-6" src="{}"></video>"#,
            html_escape(name)
        ),
        None => String::new(),
    };
    let sid_json = serde_json::Value::String(session_id.to_string()).to_string();

    TEMPLATE
        .replace("__SID__", &html_escape(session_id))
        .replace("__APP__", &html_escape(app_name))
        .replace("__VIDEO_BLOCK__", &video_block)
        .replace("__STARTED_AT_MS__", &started_at_ms.to_string())
        .replace("__SID_JSON__", &sid_json)
}

/// Serialize the matches from `Locator::inspect_all` into the JSON array
/// returned by the `query` tool. Each entry carries a pinned XPath that
/// targets that specific ordinal match on future tool calls.
pub fn render_matches(xpath: &str, matches: &[ElementInfo]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = matches
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let pinned = format!("({xpath})[{}]", i + 1);
            let mut obj = serde_json::json!({
                "xpath": pinned,
                "role": m.role,
                "name": m.name,
                "attributes": m.attributes,
                "states": m.states,
            });
            if let Some(raw) = &m.role_raw {
                obj["role_raw"] = serde_json::Value::String(raw.clone());
            }
            if let Some(b) = m.bounds {
                obj["bounds"] = serde_json::json!({
                    "x": b.x,
                    "y": b.y,
                    "width": b.width,
                    "height": b.height,
                });
            }
            obj
        })
        .collect();
    serde_json::Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(role: &str, name: Option<&str>) -> ElementInfo {
        ElementInfo {
            ref_: ("bus".to_string(), "/p".to_string()),
            role: role.to_string(),
            role_raw: None,
            name: name.map(str::to_string),
            attributes: std::collections::HashMap::new(),
            states: Vec::new(),
            bounds: None,
        }
    }

    #[test]
    fn render_matches_pins_each_entry_by_one_indexed_ordinal() {
        let base = "//PushButton";
        let ms = vec![
            info("PushButton", Some("A")),
            info("PushButton", Some("B")),
            info("PushButton", Some("C")),
        ];
        let arr = render_matches(base, &ms);
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["xpath"], "(//PushButton)[1]");
        assert_eq!(arr[1]["xpath"], "(//PushButton)[2]");
        assert_eq!(arr[2]["xpath"], "(//PushButton)[3]");
        assert_eq!(arr[0]["role"], "PushButton");
        assert_eq!(arr[0]["name"], "A");
    }

    #[test]
    fn render_matches_empty_returns_empty_array() {
        let arr = render_matches("//Missing", &[]);
        assert_eq!(arr.as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_matches_serializes_attributes_and_states() {
        let mut m = info("PushButton", Some("OK"));
        m.attributes.insert("id".to_string(), "btn-ok".to_string());
        m.states.push("showing".to_string());
        m.states.push("enabled".to_string());
        let arr = render_matches("//PushButton", &[m]);
        let entry = &arr.as_array().unwrap()[0];
        assert_eq!(entry["attributes"]["id"], "btn-ok");
        let states = entry["states"].as_array().unwrap();
        let state_names: Vec<&str> = states.iter().filter_map(|v| v.as_str()).collect();
        assert!(state_names.contains(&"showing"));
        assert!(state_names.contains(&"enabled"));
    }

    #[test]
    fn render_matches_includes_role_raw_when_present() {
        // Node-fallback case: role="Node" but role_raw preserves the original.
        let mut m = info("Node", Some("weird"));
        m.role_raw = Some("0weird-role".to_string());
        let arr = render_matches("//Node", &[m]);
        let entry = &arr.as_array().unwrap()[0];
        assert_eq!(entry["role"], "Node");
        assert_eq!(entry["role_raw"], "0weird-role");
    }

    #[test]
    fn render_matches_omits_role_raw_when_absent() {
        let m = info("PushButton", Some("OK"));
        let arr = render_matches("//PushButton", &[m]);
        let entry = &arr.as_array().unwrap()[0];
        assert!(
            entry.get("role_raw").is_none(),
            "role_raw should not be present on normal roles: {entry}"
        );
    }

    #[test]
    fn render_matches_includes_bounds_when_present() {
        let mut m = info("PushButton", Some("OK"));
        m.bounds = Some(waydriver::Rect {
            x: 12,
            y: 34,
            width: 100,
            height: 28,
        });
        let arr = render_matches("//PushButton", &[m]);
        let entry = &arr.as_array().unwrap()[0];
        assert_eq!(entry["bounds"]["x"], 12);
        assert_eq!(entry["bounds"]["y"], 34);
        assert_eq!(entry["bounds"]["width"], 100);
        assert_eq!(entry["bounds"]["height"], 28);
    }

    #[test]
    fn render_matches_omits_bounds_when_absent() {
        // Elements without Component (or not laid out) shouldn't surface
        // a misleading "bounds": null — just omit the key entirely.
        let m = info("PushButton", Some("OK"));
        let arr = render_matches("//PushButton", &[m]);
        let entry = &arr.as_array().unwrap()[0];
        assert!(
            entry.get("bounds").is_none(),
            "bounds should be absent when element has none: {entry}"
        );
    }

    #[test]
    fn render_matches_preserves_complex_base_xpath_in_pin() {
        // Composed selectors like (//Dialog//PushButton)[2] must wrap correctly.
        let base = "//Dialog[@name='Confirm']//PushButton";
        let arr = render_matches(base, &[info("PushButton", Some("OK"))]);
        assert_eq!(
            arr.as_array().unwrap()[0]["xpath"],
            "(//Dialog[@name='Confirm']//PushButton)[1]"
        );
    }

    #[test]
    fn render_index_html_contains_header_fields() {
        let html = render_index_html("my-sid", "gnome-calculator", 1_700_000_000_000, None);
        assert!(html.contains("my-sid"));
        assert!(html.contains("gnome-calculator"));
        assert!(html.contains("cdn.tailwindcss.com"));
        assert!(html.contains("events.js?v="));
        assert!(html.contains("window.__events_update"));
        assert!(html.contains(r#"id="events""#));
        assert!(html.contains("1700000000000"));
    }

    #[test]
    fn render_index_html_escapes_header_fields() {
        let evil = "<script>alert(1)</script>";
        let html = render_index_html("sid", evil, 0, None);
        assert!(!html.contains(evil), "raw evil string leaked into HTML");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[test]
    fn render_index_html_embeds_video_when_file_given() {
        let html = render_index_html("sid", "app", 0, Some("sid.webm"));
        assert!(
            html.contains("<video"),
            "expected <video> tag, got:\n{html}"
        );
        assert!(html.contains("src=\"sid.webm\""));
    }

    #[test]
    fn render_index_html_omits_video_when_none() {
        let html = render_index_html("sid", "app", 0, None);
        assert!(!html.contains("<video"), "unexpected <video> tag: {html}");
    }

    #[test]
    fn render_index_html_escapes_video_filename() {
        // An evil filename that tries to close the src attribute and inject
        // a new script tag must be entity-escaped so it stays inside the
        // attribute value.
        let html = render_index_html("sid", "app", 0, Some("evil\"><x>.webm"));
        assert!(
            !html.contains("src=\"evil\"><x>.webm\""),
            "raw evil filename escaped the attribute"
        );
        assert!(
            html.contains("&quot;&gt;&lt;x&gt;.webm"),
            "expected entity-escaped filename, got:\n{html}"
        );
    }
}
