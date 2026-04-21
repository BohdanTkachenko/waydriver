use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

use clap::Parser;
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
    /// Override report output directory for this session (replaces the server default).
    /// Reports include screenshots today; video recordings and HTML summaries planned.
    pub report_dir: Option<String>,
    /// Whether to generate the live HTML viewer and event log for this session.
    /// Defaults to true. When false, `index.html` / `events.js` / `events.jsonl`
    /// are not written and the `report=file://...` line is omitted from the
    /// start_session response. Screenshots still persist under `report_dir`.
    #[serde(default = "default_report_enabled")]
    pub report: bool,
    /// Virtual display size as "WIDTHxHEIGHT" (e.g. "1920x1080"). When unset,
    /// falls back to the server's --resolution flag (default "1024x768").
    pub resolution: Option<String>,
    /// Record a continuous WebM video of the session under
    /// `{report_dir}/{session_id}/{session_id}.webm`. When unset, falls back
    /// to the server's `--record-video` / `--no-record-video` flag (default
    /// on). Requires `report: true` — recording is written alongside the
    /// other report files.
    pub record_video: Option<bool>,
    /// VP8 target bitrate in bits/sec for the recording. Only used when
    /// recording is enabled. When unset, falls back to the server's
    /// `--video-bitrate` flag (default 2_000_000 ≈ 2 Mbps). Higher = sharper
    /// text, bigger file.
    pub video_bitrate: Option<u32>,
}

fn default_report_enabled() -> bool {
    true
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

pub struct ManagedSession {
    pub session: Session,
    pub report_dir: PathBuf,
    pub screenshot_counter: AtomicU32,
    /// In-memory event log. Guards both the on-disk `events.jsonl` (append) and
    /// the atomically-rewritten `events.js` so concurrent calls never interleave.
    pub events: Mutex<Vec<serde_json::Value>>,
    pub started_at_ms: u64,
    pub app_name: String,
    /// When false, `log_event` is a no-op and the session skips writing
    /// `index.html` / `events.js` / `events.jsonl`.
    pub report_enabled: bool,
}

impl ManagedSession {
    /// Write screenshot bytes under `{report_dir}/{session_id}/{session_id}-{n}.png`,
    /// creating the directory if needed. Increments the per-session counter.
    pub async fn persist_screenshot(
        &self,
        session_id: &str,
        png_bytes: &[u8],
    ) -> std::io::Result<PathBuf> {
        let count = self.screenshot_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let dir = self.report_dir.join(session_id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{session_id}-{count}.png"));
        tokio::fs::write(&path, png_bytes).await?;
        Ok(path)
    }

    /// Record a tool call. Appends one JSON line to `{report_dir}/{session_id}/events.jsonl`
    /// and rewrites `{report_dir}/{session_id}/events.js` atomically. Returns the
    /// assigned sequence number.
    pub async fn log_event(
        &self,
        session_id: &str,
        action: &'static str,
        params: serde_json::Value,
        outcome: Result<&str, &str>,
        screenshot: Option<&str>,
    ) -> std::io::Result<u32> {
        if !self.report_enabled {
            return Ok(0);
        }
        append_event(
            &self.report_dir,
            session_id,
            &self.events,
            action,
            params,
            outcome,
            screenshot,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn append_event(
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

fn now_ms() -> u64 {
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
pub fn render_index_html(
    session_id: &str,
    app_name: &str,
    started_at_ms: u64,
    video_file: Option<&str>,
) -> String {
    let sid = html_escape(session_id);
    let app = html_escape(app_name);
    let video_block = match video_file {
        Some(name) => format!(
            r#"<video controls preload="metadata" class="w-full rounded-lg border border-slate-200 shadow-sm bg-black mb-6" src="{}"></video>"#,
            html_escape(name)
        ),
        None => String::new(),
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>waydriver session {sid}</title>
<script src="https://cdn.tailwindcss.com"></script>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap" rel="stylesheet">
<style>
  html {{ font-family: 'Inter', system-ui, sans-serif; }}
  code, pre, .mono {{ font-family: 'JetBrains Mono', ui-monospace, monospace; }}
  .pill {{ display: inline-block; padding: 2px 8px; border-radius: 9999px; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.03em; }}
  .thumb {{ max-width: 220px; border-radius: 6px; border: 1px solid #e5e7eb; display: block; margin-top: 8px; }}
  details > summary {{ cursor: pointer; color: #475569; font-size: 13px; }}
  details[open] > summary {{ margin-bottom: 4px; }}
</style>
</head>
<body class="bg-slate-50 text-slate-800">
<header class="sticky top-0 z-10 bg-white/95 backdrop-blur border-b border-slate-200 shadow-sm">
  <div class="max-w-5xl mx-auto px-6 py-4 flex items-center gap-6">
    <div class="flex-1 min-w-0">
      <h1 class="text-lg font-semibold truncate">
        <span class="text-slate-500 mr-2">waydriver</span>
        <span class="mono">{sid}</span>
      </h1>
      <div class="text-sm text-slate-600 mt-0.5">
        app: <span class="font-medium text-slate-800">{app}</span>
        · started: <span id="started-at">—</span>
        · <span id="event-count">0</span> events
      </div>
    </div>
  </div>
</header>
<main class="max-w-5xl mx-auto px-6 py-6">
  {video_block}
  <div id="notice"></div>
  <ol id="events" class="space-y-3"></ol>
  <div id="empty" class="hidden text-center py-12 text-slate-400 text-sm">No events yet. Waiting for the first tool call…</div>
</main>
<script>
const STARTED_AT_MS = {started_at_ms};
const SESSION_ID = {sid_json};
const PILL_CLASS = {{
  start_session:   'bg-slate-200 text-slate-800',
  kill_session:    'bg-slate-200 text-slate-800',
  take_screenshot: 'bg-indigo-100 text-indigo-800',
  click_element:   'bg-blue-100 text-blue-800',
  type_text:       'bg-blue-100 text-blue-800',
  press_key:       'bg-blue-100 text-blue-800',
  move_pointer:    'bg-blue-100 text-blue-800',
  pointer_click:   'bg-blue-100 text-blue-800',
  inspect_ui:      'bg-gray-100 text-gray-700',
  find_element:    'bg-gray-100 text-gray-700',
}};

function fmtTime(ms) {{
  const d = new Date(ms);
  return d.toLocaleTimeString(undefined, {{ hour12: false }}) + '.' + String(d.getMilliseconds()).padStart(3, '0');
}}

function el(tag, attrs, children) {{
  const e = document.createElement(tag);
  if (attrs) for (const k in attrs) {{
    if (k === 'class') e.className = attrs[k];
    else if (k === 'text') e.textContent = attrs[k];
    else e.setAttribute(k, attrs[k]);
  }}
  if (children) for (const c of children) e.appendChild(c);
  return e;
}}

function renderParams(params) {{
  const entries = Object.entries(params || {{}}).filter(([_, v]) => v !== null && v !== undefined && v !== '');
  if (!entries.length) return null;
  const dl = el('dl', {{ class: 'grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm mt-1' }});
  for (const [k, v] of entries) {{
    dl.appendChild(el('dt', {{ class: 'text-slate-500', text: k }}));
    const dd = el('dd', {{ class: 'mono text-slate-800 break-all' }});
    dd.textContent = typeof v === 'string' ? v : JSON.stringify(v);
    dl.appendChild(dd);
  }}
  return dl;
}}

function renderMessage(msg) {{
  if (!msg) return null;
  if (msg.length <= 160) return el('p', {{ class: 'mono text-sm text-slate-700 mt-2 whitespace-pre-wrap break-words', text: msg }});
  const details = el('details', {{ class: 'mt-2' }});
  details.appendChild(el('summary', {{ text: `Show full output (${{msg.length}} chars)` }}));
  details.appendChild(el('pre', {{ class: 'mono text-xs text-slate-700 bg-slate-100 rounded p-3 mt-1 whitespace-pre-wrap break-words', text: msg }}));
  return details;
}}

function renderEvent(ev) {{
  const pillClass = PILL_CLASS[ev.action] || 'bg-slate-100 text-slate-700';
  const statusClass = ev.status === 'ok' ? 'bg-emerald-100 text-emerald-700' : 'bg-rose-100 text-rose-700';
  const card = el('li', {{ class: 'bg-white rounded-lg border border-slate-200 shadow-sm p-4 flex gap-4' }});

  const left = el('div', {{ class: 'w-20 shrink-0 text-right' }});
  left.appendChild(el('div', {{ class: 'text-xs text-slate-400', text: '#' + ev.seq }}));
  left.appendChild(el('div', {{ class: 'mono text-xs text-slate-600 mt-0.5', text: fmtTime(ev.ts_ms) }}));
  card.appendChild(left);

  const body = el('div', {{ class: 'flex-1 min-w-0' }});
  const head = el('div', {{ class: 'flex items-center gap-2 flex-wrap' }});
  head.appendChild(el('span', {{ class: 'pill ' + pillClass, text: ev.action }}));
  head.appendChild(el('span', {{ class: 'pill ' + statusClass, text: ev.status }}));
  body.appendChild(head);

  const params = renderParams(ev.params);
  if (params) body.appendChild(params);

  const message = renderMessage(ev.message);
  if (message) body.appendChild(message);

  if (ev.screenshot) {{
    const a = el('a', {{ href: ev.screenshot, target: '_blank', rel: 'noopener' }});
    a.appendChild(el('img', {{ src: ev.screenshot, loading: 'lazy', alt: 'screenshot', class: 'thumb' }}));
    body.appendChild(a);
  }}

  card.appendChild(body);
  return card;
}}

// Append-only render: we only ever add new events. This preserves user UI
// state (e.g. expanded <details> for inspect_ui output) across the 2-second
// refreshes. events.js is reloaded via the <script src> swap trick below —
// fetch() is blocked over file:// by Chrome, but <script src> is not.
let rendered = 0;
window.__events_update = function(events) {{
  const ol = document.getElementById('events');
  if (events.length < rendered) {{
    ol.replaceChildren();
    rendered = 0;
  }}
  for (let i = rendered; i < events.length; i++) {{
    ol.appendChild(renderEvent(events[i]));
  }}
  rendered = events.length;
  document.getElementById('event-count').textContent = rendered;
  document.getElementById('empty').classList.toggle('hidden', rendered > 0);
  document.getElementById('notice').innerHTML = '';
}};

function reload() {{
  const s = document.createElement('script');
  s.src = 'events.js?v=' + Date.now();
  s.onload = () => s.remove();
  s.onerror = () => {{
    s.remove();
    document.getElementById('notice').innerHTML = '<div class="bg-rose-50 border border-rose-300 rounded-md p-4 text-sm text-rose-900 mb-4"><div class="font-semibold mb-1">Failed to load <code class="mono">events.js</code></div><div>waydriver-mcp writes this file alongside <code class="mono">index.html</code> on every tool call. If the session directory was moved or only <code class="mono">index.html</code> was copied, reopen the full directory; otherwise the server may no longer be running.</div></div>';
  }};
  document.body.appendChild(s);
}}

document.getElementById('started-at').textContent = new Date(STARTED_AT_MS).toLocaleString();
reload();
setInterval(reload, 2000);
</script>
</body>
</html>
"#,
        sid = sid,
        app = app,
        started_at_ms = started_at_ms,
        sid_json = serde_json::Value::String(session_id.to_string()),
        video_block = video_block,
    )
}

/// Resolve the effective report dir for a new session: per-session override
/// if provided, else the server's base dir.
fn resolve_report_dir(base: &std::path::Path, override_: Option<&str>) -> PathBuf {
    override_
        .map(PathBuf::from)
        .unwrap_or_else(|| base.to_path_buf())
}

/// Resolve the effective virtual-display resolution for a new session:
/// per-session override if provided, else the server's default.
fn resolve_resolution(default: &str, override_: Option<&str>) -> String {
    override_.unwrap_or(default).to_string()
}

#[derive(Clone)]
pub struct UiTestServer {
    sessions: Arc<RwLock<HashMap<String, ManagedSession>>>,
    report_dir: PathBuf,
    default_resolution: String,
    default_record_video: bool,
    default_video_bitrate: u32,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
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

        // Compositor spawn already needs the per-session dir to exist for the
        // runtime socket; we also pre-create the report dir here so the
        // GStreamer filesink has an existing target when recording starts.
        let video_output = if record_video {
            let session_dir = report_dir.clone();
            // Actual session id isn't known until after MutterCompositor::new,
            // but MutterCompositor generates ids deterministically below — we
            // compute the path after compositor.state() gives us an id.
            Some(session_dir)
        } else {
            None
        };

        // Construct and pre-start the mutter compositor so we can pull its
        // shared Arc<MutterState> out before erasing to trait objects. Input
        // and capture are thin wrappers around that Arc, so they get cloned
        // references to the same D-Bus connection.
        let mut compositor = MutterCompositor::new();
        compositor
            .start(Some(&resolution))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let compositor_id = compositor.id().to_string();
        let state = compositor.state();
        let input = MutterInput::new(state.clone());
        let capture = MutterCapture::new(state);

        // Resolve the final WebM path + ensure the session dir exists before
        // GStreamer's filesink opens it. The session dir is also where
        // screenshots and events land, so we'd create it anyway below — doing
        // it up front means recording starts on an existing path.
        let video_path = if let Some(base) = &video_output {
            let session_dir = base.join(&compositor_id);
            if let Err(e) = tokio::fs::create_dir_all(&session_dir).await {
                tracing::warn!(error = %e, "create session report dir failed (pre-record)");
            }
            Some(session_dir.join(format!("{compositor_id}.webm")))
        } else {
            None
        };

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
            },
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let id = session.id.clone();
        let display = session.wayland_display().to_string();

        let started_at_ms = now_ms();

        // Seed the per-session dir + viewer shell before we insert so that the
        // first event always lands on an existing index.html.
        if report_enabled {
            let session_dir = report_dir.join(&id);
            if let Err(e) = tokio::fs::create_dir_all(&session_dir).await {
                tracing::warn!(error = %e, "create session report dir failed");
            }
            let video_file = video_path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str());
            let html = render_index_html(&id, &app_name, started_at_ms, video_file);
            if let Err(e) = tokio::fs::write(session_dir.join("index.html"), html).await {
                tracing::warn!(error = %e, "write index.html failed");
            }
        }

        let managed = ManagedSession {
            session,
            report_dir: report_dir.clone(),
            screenshot_counter: AtomicU32::new(0),
            events: Mutex::new(Vec::new()),
            started_at_ms,
            app_name: app_name.clone(),
            report_enabled,
        };

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
        let managed = self
            .sessions
            .write()
            .await
            .remove(&params.session_id)
            .ok_or_else(|| {
                McpError::invalid_params(format!("session not found: {}", params.session_id), None)
            })?;

        // Destructure so `session.kill()` can move out without invalidating the
        // remaining fields we still need for the final log event.
        let ManagedSession {
            session,
            report_dir,
            events,
            report_enabled,
            ..
        } = managed;

        let kill_result = session.kill().await.map_err(|e| e.to_string());
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

    #[tool(description = "Dump the accessibility tree of the application UI")]
    async fn inspect_ui(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let outcome: Result<String, String> = async {
            let a11y = session
                .a11y_connection
                .as_ref()
                .ok_or_else(|| "no AT-SPI connection for this session".to_string())?;
            atspi_client::dump_app_tree(a11y, &session.app_bus_name, &session.app_path)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str());
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "inspect_ui",
                serde_json::json!({}),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(inspect_ui) failed");
        }

        let tree = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(tree)]))
    }

    #[tool(description = "Click a UI element by its accessible name")]
    async fn click_element(
        &self,
        Parameters(params): Parameters<ClickElementParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let outcome: Result<String, String> = async {
            let a11y = session
                .a11y_connection
                .as_ref()
                .ok_or_else(|| "no AT-SPI connection for this session".to_string())?;
            atspi_client::click_element(
                a11y,
                &session.app_bus_name,
                &session.app_path,
                &params.element_name,
            )
            .await
            .map_err(|e| e.to_string())
        }
        .await;
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str());
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "click_element",
                serde_json::json!({ "element_name": params.element_name }),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(click_element) failed");
        }

        let result = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Type text into the currently focused element via keyboard input")]
    async fn type_text(
        &self,
        Parameters(params): Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let outcome: Result<String, String> = async {
            for ch in params.text.chars() {
                let keysym = char_to_keysym(ch);
                session
                    .press_keysym(keysym)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(format!("Typed '{}'", params.text))
        }
        .await;
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str());
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "type_text",
                serde_json::json!({ "text": params.text }),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(type_text) failed");
        }

        let msg = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Press a keyboard key (e.g. 'Return', 'Tab', 'a')")]
    async fn press_key(
        &self,
        Parameters(params): Parameters<PressKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        enum PressErr {
            Unknown(String),
            Internal(String),
        }
        let outcome: Result<String, PressErr> = async {
            let keysym = key_name_to_keysym(&params.key)
                .ok_or_else(|| PressErr::Unknown(format!("unknown key: {}", params.key)))?;
            session
                .press_keysym(keysym)
                .await
                .map_err(|e| PressErr::Internal(e.to_string()))?;
            Ok(format!("Pressed '{}'", params.key))
        }
        .await;
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| match e {
            PressErr::Unknown(m) => m.as_str(),
            PressErr::Internal(m) => m.as_str(),
        });
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "press_key",
                serde_json::json!({ "key": params.key }),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(press_key) failed");
        }

        let msg = outcome.map_err(|e| match e {
            PressErr::Unknown(m) => McpError::invalid_params(m, None),
            PressErr::Internal(m) => McpError::internal_error(m, None),
        })?;
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Move the pointer by a relative offset in logical pixels")]
    async fn move_pointer(
        &self,
        Parameters(params): Parameters<MovePointerParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let outcome: Result<String, String> = session
            .pointer_motion_relative(params.dx, params.dy)
            .await
            .map(|_| format!("Pointer moved by ({}, {})", params.dx, params.dy))
            .map_err(|e| e.to_string());
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str());
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "move_pointer",
                serde_json::json!({ "dx": params.dx, "dy": params.dy }),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(move_pointer) failed");
        }

        let msg = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Press and release a pointer button (defaults to left click)")]
    async fn pointer_click(
        &self,
        Parameters(params): Parameters<PointerClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let button = params.button.unwrap_or(0x110); // BTN_LEFT
        let outcome: Result<String, String> = session
            .pointer_button(button)
            .await
            .map(|_| format!("Pointer button {button:#x} clicked"))
            .map_err(|e| e.to_string());
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str());
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "pointer_click",
                serde_json::json!({ "button": button }),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(pointer_click) failed");
        }

        let msg = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(
        description = "Find a UI element by its accessible name and return its details (bus_name, path, role)"
    )]
    async fn find_element(
        &self,
        Parameters(params): Parameters<FindElementParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let outcome: Result<String, String> = async {
            let a11y = session
                .a11y_connection
                .as_ref()
                .ok_or_else(|| "no AT-SPI connection for this session".to_string())?;
            let (bus_name, path, role) = atspi_client::find_element_by_name(
                a11y,
                &session.app_bus_name,
                &session.app_path,
                &params.element_name,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(format!(
                "Found '{}': bus_name={}, path={}, role={}",
                params.element_name, bus_name, path, role
            ))
        }
        .await;
        let log_outcome = outcome.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str());
        if let Err(e) = managed
            .log_event(
                &params.session_id,
                "find_element",
                serde_json::json!({ "element_name": params.element_name }),
                log_outcome,
                None,
            )
            .await
        {
            tracing::warn!(error = %e, "log_event(find_element) failed");
        }

        let msg = outcome.map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    #[tool(description = "Take a screenshot of the session and return the file path")]
    async fn take_screenshot(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.read().await;
        let managed = sessions.get(&params.session_id).ok_or_else(|| {
            McpError::invalid_params(format!("session not found: {}", params.session_id), None)
        })?;
        let session = &managed.session;

        let outcome: Result<PathBuf, String> = async {
            let png_bytes = session.take_screenshot().await.map_err(|e| e.to_string())?;
            managed
                .persist_screenshot(&params.session_id, &png_bytes)
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
        if let Err(e) = managed
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

#[derive(Parser, Debug)]
#[command(version, about = "Headless GTK4 UI testing MCP server")]
struct Cli {
    /// Base directory for per-session report output (screenshots today;
    /// video recordings and HTML summaries planned). Each session gets a
    /// subdirectory under this path, each containing a self-contained
    /// `index.html` viewer openable directly from the filesystem.
    #[arg(long, default_value = "/tmp/waydriver", env = "WAYDRIVER_REPORT_DIR")]
    report_dir: PathBuf,
    /// Default virtual-display size ("WIDTHxHEIGHT") for sessions that don't
    /// override it via start_session's `resolution` parameter.
    #[arg(long, default_value = "1024x768", env = "WAYDRIVER_RESOLUTION")]
    resolution: String,
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
    record_video: bool,
    /// Default VP8 target bitrate in bits/sec for session recordings. Higher
    /// values produce sharper UI text at the cost of file size. Per-session
    /// override via start_session's `video_bitrate` argument.
    #[arg(long, default_value_t = 2_000_000, env = "WAYDRIVER_VIDEO_BITRATE")]
    video_bitrate: u32,
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

    use async_trait::async_trait;
    use clap::Parser;
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
            ManagedSession {
                session,
                report_dir,
                screenshot_counter: AtomicU32::new(0),
                events: Mutex::new(Vec::new()),
                started_at_ms: 0,
                app_name: app_name.into(),
                report_enabled,
            },
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

    // ── Report path / counter ──────────────────────────────────────────

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
            session,
            report_dir: dir,
            screenshot_counter: AtomicU32::new(0),
            events: Mutex::new(Vec::new()),
            started_at_ms: 0,
            app_name: "app".into(),
            report_enabled: true,
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

    #[test]
    fn start_session_params_report_defaults_to_true() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x" })).unwrap();
        assert!(params.report);
    }

    #[test]
    fn start_session_params_report_can_be_disabled() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x", "report": false })).unwrap();
        assert!(!params.report);
    }

    #[test]
    fn start_session_params_record_video_defaults_to_none() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x" })).unwrap();
        assert_eq!(params.record_video, None);
    }

    #[test]
    fn start_session_params_record_video_can_be_set() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x", "record_video": false }))
                .unwrap();
        assert_eq!(params.record_video, Some(false));
    }

    #[test]
    fn start_session_params_video_bitrate_defaults_to_none() {
        let params: StartSessionParams =
            serde_json::from_value(serde_json::json!({ "command": "x" })).unwrap();
        assert_eq!(params.video_bitrate, None);
    }

    #[test]
    fn start_session_params_video_bitrate_can_be_set() {
        let params: StartSessionParams = serde_json::from_value(
            serde_json::json!({ "command": "x", "video_bitrate": 5_000_000 }),
        )
        .unwrap();
        assert_eq!(params.video_bitrate, Some(5_000_000));
    }

    // ── CLI parsing ────────────────────────────────────────────────────

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
}
