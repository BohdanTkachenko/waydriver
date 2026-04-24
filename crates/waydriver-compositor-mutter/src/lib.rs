//! Mutter implementation of [`waydriver::CompositorRuntime`].
//!
//! Owns the private-bus `dbus-daemon`, the `pipewire` + `wireplumber` pair,
//! and a headless `mutter --wayland` instance. After [`MutterCompositor::start`]
//! returns, [`MutterCompositor::state`] exposes an `Arc<MutterState>` that
//! sibling backends (`waydriver-input-mutter`, `waydriver-capture-mutter`) use
//! to talk to the same mutter D-Bus session.
//!
//! ## Shared-state invariant
//!
//! While any `Arc<MutterState>` exists, the mutter child processes and the
//! private D-Bus connection MUST remain alive. [`waydriver::Session::kill`]
//! enforces this by dropping input and capture trait objects before calling
//! `compositor.stop().await`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::process::{Child, Command};

use waydriver::{CompositorRuntime, Error, Result};

/// Default virtual-monitor geometry passed to mutter when the caller doesn't
/// override it. Matches mutter's own implicit default.
const DEFAULT_RESOLUTION: &str = "1024x768";

/// Shared mutter-backend state consumed by `waydriver-input-mutter` and
/// `waydriver-capture-mutter`.
///
/// **Invariant:** while any `Arc<MutterState>` exists, the underlying D-Bus
/// connection and the mutter child process must remain alive. See the
/// module docs for details.
pub struct MutterState {
    /// Persistent connection to mutter's private D-Bus.
    pub conn: zbus::Connection,
    /// RemoteDesktop session path, used by input injection.
    pub rd_session_path: String,
    /// RemoteDesktop session id, read from the `SessionId` property on
    /// the RD session. `waydriver-capture-mutter` passes this as the
    /// `remote-desktop-session-id` option to `ScreenCast.CreateSession`
    /// so mutter links the two; the link is required for
    /// `NotifyPointerMotionAbsolute` to be accepted.
    pub rd_session_id: String,
    /// Whether `RemoteDesktop.Session.Start` has been called yet. Mutter
    /// requires the RD session to be *unstarted* when linking a
    /// ScreenCast session (`remote-desktop-session-id` only accepted
    /// pre-Start), so `waydriver-capture-mutter` defers `RD.Start`
    /// until after the first linked `ScreenCast.CreateSession` returns.
    /// Guarded by `Mutex` to make the check-and-set race-free.
    pub rd_started: Arc<Mutex<bool>>,
    /// Per-session XDG_RUNTIME_DIR, used by capture to locate the PipeWire socket.
    pub runtime_dir: PathBuf,
    /// ScreenCast Stream object path of the currently active stream.
    /// Set by `waydriver-capture-mutter` in `start_stream`, cleared in
    /// `stop_stream`. `waydriver-input-mutter` needs it to route
    /// `NotifyPointerMotionAbsolute` at the correct monitor. `None`
    /// when no stream is open — absolute pointer motion will error.
    pub active_stream_path: Arc<Mutex<Option<String>>>,
}

/// Headless mutter instance.
pub struct MutterCompositor {
    id: String,
    wayland_display: String,
    runtime_dir: PathBuf,
    mutter_dbus_address: String,
    mutter_dbus_pid: Option<u32>,
    mutter: Option<Child>,
    pipewire: Option<Child>,
    wireplumber: Option<Child>,
    state: Option<Arc<MutterState>>,
}

impl MutterCompositor {
    /// Construct but do not start. Generates the session id and computes
    /// where the Wayland socket and runtime dir will live. No I/O.
    pub fn new() -> Self {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let wayland_display = format!("wayland-wd-{}", id);

        let host_runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        let runtime_dir = PathBuf::from(&host_runtime).join(format!("wd-session-{}", id));

        Self {
            id,
            wayland_display,
            runtime_dir,
            mutter_dbus_address: String::new(),
            mutter_dbus_pid: None,
            mutter: None,
            pipewire: None,
            wireplumber: None,
            state: None,
        }
    }

    /// Returns the shared `Arc<MutterState>` for passing to sibling backends.
    ///
    /// # Panics
    /// Panics if called before [`CompositorRuntime::start`] has completed, or
    /// after [`CompositorRuntime::stop`]. Callers are expected to follow the
    /// fixed sequence: `new()` → `start().await?` → `state()`.
    pub fn state(&self) -> Arc<MutterState> {
        self.state
            .as_ref()
            .expect("MutterCompositor::state() called before start() or after stop()")
            .clone()
    }
}

impl Default for MutterCompositor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CompositorRuntime for MutterCompositor {
    async fn start(&mut self, resolution: Option<&str>) -> Result<()> {
        let resolution = resolution.unwrap_or(DEFAULT_RESOLUTION);
        // Validate before we start spawning subprocesses — mutter silently
        // ignores bad --virtual-monitor values and falls back to its own
        // default, which would surprise the caller.
        parse_resolution(resolution)?;

        tracing::info!(id = self.id, resolution, "starting mutter compositor");

        tokio::fs::create_dir_all(&self.runtime_dir).await?;
        let runtime_str = self.runtime_dir.to_str().unwrap().to_string();

        // Step 1: Private D-Bus for mutter (so its ScreenCast API doesn't conflict with host).
        let dbus_output = Command::new("dbus-launch")
            .arg("--sh-syntax")
            .output()
            .await?;
        if !dbus_output.status.success() {
            return Err(Error::Process(format!(
                "dbus-launch failed: {}",
                String::from_utf8_lossy(&dbus_output.stderr)
            )));
        }
        let dbus_stdout = String::from_utf8_lossy(&dbus_output.stdout);
        self.mutter_dbus_address = parse_dbus_address(&dbus_stdout)?;
        self.mutter_dbus_pid = Some(parse_dbus_pid(&dbus_stdout)?);
        tracing::debug!(id = self.id, mutter_dbus_address = %self.mutter_dbus_address, "private D-Bus for mutter");

        // Step 2: PipeWire + WirePlumber (for screenshots via ScreenCast).
        let pipewire = Command::new("pipewire")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Process(format!("pipewire: {e}")))?;
        self.pipewire = Some(pipewire);

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let wireplumber = Command::new("wireplumber")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Process(format!("wireplumber: {e}")))?;
        self.wireplumber = Some(wireplumber);

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        tracing::debug!(id = self.id, "PipeWire + WirePlumber started");

        // Step 3: mutter in headless Wayland mode (on its private D-Bus).
        let mutter = Command::new("mutter")
            .args([
                "--headless",
                "--wayland",
                "--no-x11",
                "--wayland-display",
                &self.wayland_display,
                "--virtual-monitor",
                resolution,
            ])
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Process(format!("mutter: {e}")))?;
        self.mutter = Some(mutter);
        tracing::debug!(id = self.id, wayland_display = %self.wayland_display, "mutter spawned");

        // Step 4: Wait for the Wayland socket.
        wait_for_wayland_socket(&runtime_str, &self.wayland_display).await?;
        tracing::debug!(id = self.id, "wayland socket ready");

        // Step 5: Connect to mutter's private D-Bus and start RemoteDesktop session.
        let mutter_addr: zbus::address::Address = self
            .mutter_dbus_address
            .as_str()
            .try_into()
            .map_err(|e: zbus::Error| {
                Error::Process(format!("invalid mutter dbus address: {e}"))
            })?;
        let mutter_conn = zbus::connection::Builder::address(mutter_addr)?
            .build()
            .await
            .map_err(|e| Error::Process(format!("connect to mutter dbus: {e}")))?;

        // Wait for mutter to register its D-Bus services (may take a moment after socket appears)
        let mut rd_reply = None;
        for i in 0..50 {
            match mutter_conn
                .call_method(
                    Some("org.gnome.Mutter.RemoteDesktop"),
                    "/org/gnome/Mutter/RemoteDesktop",
                    Some("org.gnome.Mutter.RemoteDesktop"),
                    "CreateSession",
                    &(),
                )
                .await
            {
                Ok(reply) => {
                    rd_reply = Some(reply);
                    break;
                }
                Err(e) if i < 49 => {
                    tracing::debug!(
                        id = self.id,
                        attempt = i,
                        "waiting for RemoteDesktop service: {e}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => {
                    return Err(Error::Process(format!("RemoteDesktop CreateSession: {e}")));
                }
            }
        }
        let rd_reply = rd_reply.unwrap();
        let rd_session_path: zbus::zvariant::OwnedObjectPath = rd_reply
            .body()
            .deserialize()
            .map_err(|e| Error::Process(format!("parse RD session path: {e}")))?;
        // Intentionally do NOT call `RemoteDesktop.Session.Start` here.
        // Mutter only accepts `remote-desktop-session-id` on
        // `ScreenCast.CreateSession` when the RD session is not yet
        // started, so `waydriver-capture-mutter::start_stream` defers
        // the Start call until after it has created the linked
        // ScreenCast session.
        // Read the RD session's `SessionId` property — it's the token
        // ScreenCast.CreateSession needs in `remote-desktop-session-id`
        // to link the two sessions. Without that link, mutter rejects
        // NotifyPointerMotionAbsolute with "No screen cast active".
        let rd_session_id_reply = mutter_conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                rd_session_path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.gnome.Mutter.RemoteDesktop.Session", "SessionId"),
            )
            .await
            .map_err(|e| Error::Process(format!("Get SessionId: {e}")))?;
        // `Get` returns a variant; deserialize as `OwnedValue` to detach
        // the string from the reply's body before the reply is dropped.
        let rd_session_id_body = rd_session_id_reply.body();
        let rd_session_id_variant: zbus::zvariant::OwnedValue = rd_session_id_body
            .deserialize()
            .map_err(|e| Error::Process(format!("parse SessionId variant: {e}")))?;
        let rd_session_id: String = rd_session_id_variant
            .try_into()
            .map_err(|e| Error::Process(format!("SessionId not a string: {e}")))?;

        let rd_session_path = rd_session_path.to_string();
        tracing::debug!(
            id = self.id,
            rd_session_path = %rd_session_path,
            rd_session_id = %rd_session_id,
            "RemoteDesktop session started"
        );

        self.state = Some(Arc::new(MutterState {
            conn: mutter_conn,
            rd_session_path,
            rd_session_id,
            rd_started: Arc::new(Mutex::new(false)),
            runtime_dir: self.runtime_dir.clone(),
            active_stream_path: Arc::new(Mutex::new(None)),
        }));

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!(id = self.id, "stopping mutter compositor");

        // Stop RemoteDesktop session if still reachable.
        if let Some(state) = &self.state {
            let _ = state
                .conn
                .call_method(
                    Some("org.gnome.Mutter.RemoteDesktop"),
                    state.rd_session_path.as_str(),
                    Some("org.gnome.Mutter.RemoteDesktop.Session"),
                    "Stop",
                    &(),
                )
                .await;
        }

        // Drop our strong ref to the shared state. If callers haven't dropped
        // theirs (the input/capture trait objects), their Arc still points at
        // the D-Bus connection we're about to tear down below — any method
        // call on them after this will fail with "connection closed".
        self.state = None;

        if let Some(mut mutter) = self.mutter.take() {
            let _ = mutter.kill().await;
            let _ = mutter.wait().await;
        }
        if let Some(mut wireplumber) = self.wireplumber.take() {
            let _ = wireplumber.kill().await;
            let _ = wireplumber.wait().await;
        }
        if let Some(mut pipewire) = self.pipewire.take() {
            let _ = pipewire.kill().await;
            let _ = pipewire.wait().await;
        }

        if let Some(pid) = self.mutter_dbus_pid.take() {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }

        let _ = tokio::fs::remove_dir_all(&self.runtime_dir).await;

        tracing::debug!(id = self.id, "mutter compositor stopped");
        Ok(())
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn wayland_display(&self) -> &str {
        &self.wayland_display
    }

    fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }
}

impl Drop for MutterCompositor {
    fn drop(&mut self) {
        // Best-effort cleanup when dropped without calling stop().
        // Can't use async here, so send SIGKILL synchronously.
        self.state = None;

        if let Some(ref mut child) = self.mutter {
            let _ = child.start_kill();
        }
        if let Some(ref mut child) = self.wireplumber {
            let _ = child.start_kill();
        }
        if let Some(ref mut child) = self.pipewire {
            let _ = child.start_kill();
        }
        if let Some(pid) = self.mutter_dbus_pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn parse_dbus_address(output: &str) -> Result<String> {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_ADDRESS='") {
            if let Some(addr) = rest.strip_suffix("';") {
                return Ok(addr.to_string());
            }
        }
    }
    Err(Error::Process(
        "could not parse DBUS_SESSION_BUS_ADDRESS from dbus-launch".to_string(),
    ))
}

fn parse_dbus_pid(output: &str) -> Result<u32> {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_PID=") {
            let pid_str = rest.trim_end_matches(';').trim();
            return pid_str
                .parse()
                .map_err(|e| Error::Process(format!("invalid dbus PID: {e}")));
        }
    }
    Err(Error::Process(
        "could not parse DBUS_SESSION_BUS_PID from dbus-launch".to_string(),
    ))
}

fn parse_resolution(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').ok_or_else(|| {
        Error::Process(format!("invalid resolution '{s}': expected WIDTHxHEIGHT"))
    })?;
    let parse = |part: &str| -> Result<u32> {
        part.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| {
            Error::Process(format!("invalid resolution '{s}': expected WIDTHxHEIGHT"))
        })
    };
    Ok((parse(w)?, parse(h)?))
}

async fn wait_for_wayland_socket(runtime_dir: &str, display: &str) -> Result<()> {
    let socket_path = PathBuf::from(runtime_dir).join(display);
    for _ in 0..50 {
        if socket_path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(Error::Timeout(format!(
        "wayland socket {} did not appear within 5s",
        socket_path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dbus_address_valid() {
        let output = "DBUS_SESSION_BUS_ADDRESS='unix:abstract=/tmp/dbus-XXX,guid=abc123';\nDBUS_SESSION_BUS_PID=12345;\n";
        let addr = parse_dbus_address(output).unwrap();
        assert_eq!(addr, "unix:abstract=/tmp/dbus-XXX,guid=abc123");
    }

    #[test]
    fn test_parse_dbus_address_missing() {
        let output = "DBUS_SESSION_BUS_PID=12345;\n";
        assert!(parse_dbus_address(output).is_err());
    }

    #[test]
    fn test_parse_dbus_pid_valid() {
        let output = "DBUS_SESSION_BUS_ADDRESS='unix:abstract=/tmp/dbus-XXX,guid=abc123';\nDBUS_SESSION_BUS_PID=12345;\n";
        let pid = parse_dbus_pid(output).unwrap();
        assert_eq!(pid, 12345);
    }

    #[test]
    fn test_parse_dbus_pid_missing() {
        let output = "DBUS_SESSION_BUS_ADDRESS='unix:abstract=/tmp/dbus-XXX,guid=abc123';\n";
        assert!(parse_dbus_pid(output).is_err());
    }

    #[test]
    fn test_parse_dbus_pid_invalid() {
        let output = "DBUS_SESSION_BUS_PID=notanumber;\n";
        assert!(parse_dbus_pid(output).is_err());
    }

    #[tokio::test]
    async fn test_wait_for_socket_found() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().to_str().unwrap().to_string();
        let display = "wayland-test-99";
        std::fs::File::create(dir.path().join(display)).unwrap();
        wait_for_wayland_socket(&runtime_dir, display)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_socket_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().to_str().unwrap().to_string();
        let display = "wayland-nonexistent-0";
        let err = wait_for_wayland_socket(&runtime_dir, display)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Timeout(_)),
            "expected Timeout, got: {err}"
        );
    }

    #[test]
    fn test_new_generates_unique_ids() {
        let a = MutterCompositor::new();
        let b = MutterCompositor::new();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn test_new_wayland_display_contains_id() {
        let c = MutterCompositor::new();
        assert!(
            c.wayland_display().contains(c.id()),
            "display '{}' should contain id '{}'",
            c.wayland_display(),
            c.id()
        );
    }

    #[test]
    fn test_new_runtime_dir_contains_id() {
        let c = MutterCompositor::new();
        let dir_str = c.runtime_dir().to_str().unwrap();
        assert!(
            dir_str.contains(c.id()),
            "runtime_dir '{}' should contain id '{}'",
            dir_str,
            c.id()
        );
    }

    #[test]
    fn test_new_wayland_display_prefix() {
        let c = MutterCompositor::new();
        assert!(c.wayland_display().starts_with("wayland-wd-"));
    }

    #[test]
    fn test_new_runtime_dir_contains_session_prefix() {
        let c = MutterCompositor::new();
        let dir_str = c.runtime_dir().to_str().unwrap();
        assert!(dir_str.contains("wd-session-"));
    }

    #[test]
    #[should_panic(expected = "before start")]
    fn test_state_panics_before_start() {
        let c = MutterCompositor::new();
        let _ = c.state();
    }

    #[test]
    fn test_parse_resolution_accepts_hd() {
        assert_eq!(parse_resolution("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_resolution("1024x768").unwrap(), (1024, 768));
    }

    #[test]
    fn test_parse_resolution_rejects_garbage() {
        for bad in [
            "",
            "1920",
            "1920x",
            "x1080",
            "0x0",
            "1920x0",
            "0x1080",
            "1920x1080x1",
            "abcxdef",
            "-1x1080",
            "1920 x 1080",
        ] {
            assert!(parse_resolution(bad).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn test_default_same_structure_as_new() {
        let c = MutterCompositor::default();
        assert!(c.wayland_display().starts_with("wayland-wd-"));
        assert!(c.runtime_dir().to_str().unwrap().contains("wd-session-"));
    }
}
