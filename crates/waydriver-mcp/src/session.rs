//! [`ManagedSession`] — a `waydriver::Session` plus the per-session reporting
//! state owned by `UiTestServer`.
//!
//! The struct itself lives here; tests live in `main.rs` next to the mock
//! `InputBackend` / `CaptureBackend` / `CompositorRuntime` fixtures so we
//! don't have to duplicate them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use waydriver::Session;

use crate::report;

pub struct ManagedSession {
    pub session: Arc<Session>,
    pub report_dir: PathBuf,
    pub screenshot_counter: AtomicU32,
    /// In-memory event log. Guards both the on-disk `events.jsonl` (append) and
    /// the atomically-rewritten `events.js` so concurrent calls never interleave.
    pub events: Mutex<Vec<serde_json::Value>>,
    /// When false, `log_event` is a no-op and the session skips writing
    /// `index.html` / `events.js` / `events.jsonl`.
    pub report_enabled: bool,
    /// Per-session drain lock. Tool calls hold this in read mode
    /// (`kill_lock.clone().read_owned()`) for the full duration of their
    /// work, including `log_event`. `kill_session` acquires write mode
    /// to wait for all in-flight tools to release before tearing down
    /// the session, so `Arc::try_unwrap(session)` deterministically
    /// succeeds. The `Arc` wrapper is required because
    /// `RwLock::clone().read_owned()` needs `Arc<RwLock<_>>`.
    pub kill_lock: Arc<RwLock<()>>,
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
    /// assigned sequence number, or 0 when reporting is disabled for this session.
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
        report::append_event(
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
