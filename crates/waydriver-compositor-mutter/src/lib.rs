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

mod error;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::process::{Child, Command};

use waydriver::{CompositorRuntime, Result};

use crate::error::MutterError;

/// Default virtual-monitor geometry passed to mutter when the caller doesn't
/// override it. Matches mutter's own implicit default.
const DEFAULT_RESOLUTION: &str = "1024x768";

/// Shared mutter-backend state consumed by `waydriver-input-mutter` and
/// `waydriver-capture-mutter`.
///
/// **Invariant:** while any `Arc<MutterState>` exists, the underlying D-Bus
/// connection and the mutter child process must remain alive. See the
/// module docs for details.
///
/// Fields are private — all access goes through the accessor methods
/// below. Sibling crates (`waydriver-input-mutter`,
/// `waydriver-capture-mutter`) that previously read fields directly
/// now call `state.conn()`, `state.rd_session_path()`, etc. The
/// shape of the underlying storage (e.g. how `active_stream_path` is
/// guarded) is therefore an implementation detail that can change
/// without breaking those callers — the contract lives entirely in
/// the method signatures.
pub struct MutterState {
    conn: zbus::Connection,
    rd_session_path: String,
    rd_session_id: String,
    rd_started: Arc<Mutex<bool>>,
    runtime_dir: PathBuf,
    active_stream_path: Arc<Mutex<Option<String>>>,
}

impl MutterState {
    /// Persistent connection to mutter's private D-Bus.
    ///
    /// Both sibling backends (`waydriver-input-mutter`,
    /// `waydriver-capture-mutter`) issue all their RemoteDesktop and
    /// ScreenCast method calls through this connection.
    pub fn conn(&self) -> &zbus::Connection {
        &self.conn
    }

    /// RemoteDesktop session object path. Used by
    /// `waydriver-input-mutter` as the `path` argument on every
    /// pointer / keyboard `Notify*` D-Bus call.
    pub fn rd_session_path(&self) -> &str {
        &self.rd_session_path
    }

    /// RemoteDesktop session id, read from the `SessionId` property on
    /// the RD session. `waydriver-capture-mutter` passes this as the
    /// `remote-desktop-session-id` option to
    /// `ScreenCast.CreateSession` so mutter links the two; the link is
    /// required for `NotifyPointerMotionAbsolute` to be accepted.
    pub fn rd_session_id(&self) -> &str {
        &self.rd_session_id
    }

    /// Per-session `XDG_RUNTIME_DIR`. `waydriver-capture-mutter` joins
    /// this with `pipewire-0` to locate the PipeWire socket.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Lock the "RD-started" flag.
    ///
    /// Acquires the underlying mutex and returns the guard so the
    /// caller can perform a check-and-set under one critical section
    /// (the capture backend defers `RD.Session.Start` until the first
    /// linked `ScreenCast.CreateSession` succeeds — that's a load,
    /// some D-Bus work, and a store; splitting the read and write
    /// would race). `Error::Process` if the mutex is poisoned.
    pub fn rd_started_lock(&self) -> Result<std::sync::MutexGuard<'_, bool>> {
        self.rd_started
            .lock()
            .map_err(|_| waydriver::Error::process("rd_started mutex poisoned"))
    }

    /// Lock the active ScreenCast Stream object path.
    ///
    /// Set by `waydriver-capture-mutter` in `start_stream`, cleared in
    /// `stop_stream`. `waydriver-input-mutter` reads it to route
    /// `NotifyPointerMotionAbsolute` at the correct monitor. `None`
    /// inside the guard means no stream is open — absolute pointer
    /// motion will error.
    pub fn active_stream_path_lock(&self) -> Result<std::sync::MutexGuard<'_, Option<String>>> {
        self.active_stream_path
            .lock()
            .map_err(|_| waydriver::Error::process("active_stream_path mutex poisoned"))
    }
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

    /// Returns the shared `Arc<MutterState>` for passing to sibling
    /// backends, or `None` when called outside the started window.
    ///
    /// `None` is returned when:
    /// - `start()` has not yet completed (or returned an error), or
    /// - `stop()` has been called and dropped the state.
    ///
    /// Callers that have just awaited `start()?` know the state is
    /// present — `expect()` or `?`-with-typed-error is appropriate
    /// there. Returning `Option` instead of panicking keeps the API
    /// honest about the lifecycle and lets callers detect "stopped"
    /// without first matching on a panic.
    pub fn state(&self) -> Option<Arc<MutterState>> {
        self.state.clone()
    }
}

impl Default for MutterCompositor {
    fn default() -> Self {
        Self::new()
    }
}

impl MutterCompositor {
    /// Typed-error implementation of `start`. The trait method calls
    /// this and converts the result via `From<MutterError>`.
    ///
    /// Steps (each fails with a specific `MutterError` variant):
    /// 1. validate resolution,
    /// 2. ensure the session runtime dir exists,
    /// 3. spawn a private `dbus-daemon` and parse its address + PID,
    /// 4. spawn `pipewire` + `wireplumber` on that bus,
    /// 5. spawn headless `mutter --wayland`,
    /// 6. wait for the Wayland socket,
    /// 7. open a zbus connection, retry-create the RemoteDesktop session,
    /// 8. read its `SessionId` property,
    /// 9. publish the `Arc<MutterState>` for sibling backends.
    async fn start_inner(
        &mut self,
        resolution: Option<&str>,
    ) -> std::result::Result<(), MutterError> {
        let resolution = resolution.unwrap_or(DEFAULT_RESOLUTION);
        // Validate before we start spawning subprocesses — mutter silently
        // ignores bad --virtual-monitor values and falls back to its own
        // default, which would surprise the caller.
        parse_resolution(resolution)?;

        tracing::info!(id = self.id, resolution, "starting mutter compositor");

        tokio::fs::create_dir_all(&self.runtime_dir).await?;
        // `runtime_dir` is built in `new()` from a UTF-8 String
        // (XDG_RUNTIME_DIR or `/run/user/<uid>`) joined with a UTF-8
        // ASCII session id, so the path is guaranteed valid UTF-8.
        // `expect` documents that invariant rather than re-deriving
        // it via the `to_str()` `Option`.
        let runtime_str = self
            .runtime_dir
            .to_str()
            .expect("invariant: runtime_dir built from UTF-8 inputs in new()")
            .to_string();

        // Step 1: Private D-Bus for mutter (so its ScreenCast API doesn't conflict with host).
        let dbus_output = Command::new("dbus-launch")
            .arg("--sh-syntax")
            .output()
            .await?;
        if !dbus_output.status.success() {
            return Err(MutterError::DbusLaunchFailed(
                String::from_utf8_lossy(&dbus_output.stderr).into_owned(),
            ));
        }
        let dbus_stdout = String::from_utf8_lossy(&dbus_output.stdout);
        self.mutter_dbus_address = parse_dbus_address(&dbus_stdout)?;
        self.mutter_dbus_pid = Some(parse_dbus_pid(&dbus_stdout)?);
        tracing::debug!(id = self.id, mutter_dbus_address = %self.mutter_dbus_address, "private D-Bus for mutter");

        // Step 2: PipeWire + WirePlumber (for screenshots via ScreenCast).
        //
        // `env_remove("PIPEWIRE_REMOTE")` is load-bearing: `waydriver`'s
        // `grab_png_sync` mutates the parent's process env to point
        // `pipewiresrc` at the live session's pipewire socket. After a
        // session stops, that socket is gone but the env var lingers in
        // the parent. Without scrubbing it here, a freshly spawned
        // `pipewire`/`wireplumber`/`mutter` for the next session would
        // inherit the stale value and try to connect to the previous
        // session's dead socket — wireplumber/mutter prefer
        // `PIPEWIRE_REMOTE` over `XDG_RUNTIME_DIR/pipewire-0`, so the
        // explicit `XDG_RUNTIME_DIR` override below isn't enough.
        // Symptom: `ScreenCast.Start` fails with "Couldn't connect
        // pipewire context" on every session after the first.
        let pipewire = Command::new("pipewire")
            .env_remove("PIPEWIRE_REMOTE")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| MutterError::Spawn {
                process: "pipewire",
                source,
            })?;
        self.pipewire = Some(pipewire);

        // Wait for pipewire's socket to appear before launching
        // wireplumber. Polling for the socket file is the same
        // readiness signal `wait_for_wayland_socket` uses for
        // mutter: it's the actual handshake clients use, so any
        // earlier signal would either be racier (process spawn) or
        // just as expensive to probe.
        wait_for_pipewire_socket(&runtime_str).await?;

        let wireplumber = Command::new("wireplumber")
            .env_remove("PIPEWIRE_REMOTE")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| MutterError::Spawn {
                process: "wireplumber",
                source,
            })?;
        self.wireplumber = Some(wireplumber);

        // No bus-readiness signal poll for wireplumber: it's a
        // session-policy daemon that doesn't register a stable D-Bus
        // name we can probe, and its initialisation runs in parallel
        // with mutter's own startup. The downstream
        // `ScreenCast.CreateSession` retry loop in
        // `waydriver-capture-mutter::start_stream` is what actually
        // gates on wireplumber having joined the graph — putting a
        // pessimistic sleep here as well would add startup latency
        // without changing correctness.
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
            .env_remove("PIPEWIRE_REMOTE")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| MutterError::Spawn {
                process: "mutter",
                source,
            })?;
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
            .map_err(|source: zbus::Error| MutterError::DbusAddressInvalid {
                addr: self.mutter_dbus_address.clone(),
                source,
            })?;
        let mutter_conn = zbus::connection::Builder::address(mutter_addr)
            .map_err(|source| MutterError::DbusConnect {
                stage: "build connection builder",
                source,
            })?
            .build()
            .await
            .map_err(|source| MutterError::DbusConnect {
                stage: "connect",
                source,
            })?;

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
                    return Err(MutterError::RemoteDesktopCreate(e));
                }
            }
        }
        // The retry loop above either `break`s with `rd_reply = Some(_)`
        // or returns `Err(...)` from the final attempt — `unwrap` here
        // is unreachable by construction.
        let rd_reply = rd_reply.expect("retry loop sets Some on break or returns Err");
        let rd_session_path: zbus::zvariant::OwnedObjectPath = rd_reply
            .body()
            .deserialize()
            .map_err(MutterError::RdSessionPathParse)?;
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
            .map_err(MutterError::SessionIdGet)?;
        // `Get` returns a variant; deserialize as `OwnedValue` to detach
        // the string from the reply's body before the reply is dropped.
        let rd_session_id_body = rd_session_id_reply.body();
        let rd_session_id_variant: zbus::zvariant::OwnedValue = rd_session_id_body
            .deserialize()
            .map_err(MutterError::SessionIdVariantParse)?;
        let rd_session_id: String = rd_session_id_variant
            .try_into()
            .map_err(MutterError::SessionIdNotString)?;

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
}

#[async_trait]
impl CompositorRuntime for MutterCompositor {
    async fn start(&mut self, resolution: Option<&str>) -> Result<()> {
        // Body uses the crate-local typed `MutterError`. The `?` at the
        // end of `self.start_inner(...).await?` runs the
        // `From<MutterError> for waydriver::Error` impl in `error.rs`,
        // which is the single boundary at which the typed enum becomes
        // the workspace's shared `waydriver::Error`.
        Ok(self.start_inner(resolution).await?)
    }

    async fn stop(&mut self) -> Result<()> {
        tracing::info!(id = self.id, "stopping mutter compositor");

        // Stop RemoteDesktop session if still reachable. We could
        // touch the private fields directly here (same crate), but
        // routing through the public accessors keeps the contract
        // visible and means a future change to the field layout
        // doesn't need to update this site.
        if let Some(state) = &self.state {
            let _ = state
                .conn()
                .call_method(
                    Some("org.gnome.Mutter.RemoteDesktop"),
                    state.rd_session_path(),
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

fn parse_dbus_address(output: &str) -> std::result::Result<String, MutterError> {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_ADDRESS='") {
            if let Some(addr) = rest.strip_suffix("';") {
                return Ok(addr.to_string());
            }
        }
    }
    Err(MutterError::DbusOutputMissingField {
        field: "DBUS_SESSION_BUS_ADDRESS",
    })
}

fn parse_dbus_pid(output: &str) -> std::result::Result<u32, MutterError> {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_PID=") {
            let pid_str = rest.trim_end_matches(';').trim();
            return pid_str.parse().map_err(MutterError::DbusPidParse);
        }
    }
    Err(MutterError::DbusOutputMissingField {
        field: "DBUS_SESSION_BUS_PID",
    })
}

fn parse_resolution(s: &str) -> std::result::Result<(u32, u32), MutterError> {
    let invalid = || MutterError::ResolutionInvalid {
        value: s.to_string(),
    };
    let (w, h) = s.split_once('x').ok_or_else(invalid)?;
    let parse = |part: &str| -> std::result::Result<u32, MutterError> {
        part.parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(invalid)
    };
    Ok((parse(w)?, parse(h)?))
}

async fn wait_for_wayland_socket(
    runtime_dir: &str,
    display: &str,
) -> std::result::Result<(), MutterError> {
    let socket_path = PathBuf::from(runtime_dir).join(display);
    for _ in 0..50 {
        if socket_path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(MutterError::WaylandSocketTimeout {
        socket: socket_path.display().to_string(),
    })
}

/// PipeWire creates `<runtime_dir>/pipewire-0` as soon as it's ready
/// to accept client connections. Polling for that file replaces the
/// previous unconditional `sleep(1s)` after spawning the pipewire
/// process — same readiness model as
/// [`wait_for_wayland_socket`].
async fn wait_for_pipewire_socket(runtime_dir: &str) -> std::result::Result<(), MutterError> {
    let socket_path = PathBuf::from(runtime_dir).join("pipewire-0");
    for _ in 0..50 {
        if socket_path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(MutterError::PipewireSocketTimeout {
        socket: socket_path.display().to_string(),
    })
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
    async fn test_wait_for_pipewire_socket_found() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().to_str().unwrap().to_string();
        std::fs::File::create(dir.path().join("pipewire-0")).unwrap();
        wait_for_pipewire_socket(&runtime_dir).await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_pipewire_socket_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().to_str().unwrap().to_string();
        let err = wait_for_pipewire_socket(&runtime_dir).await.unwrap_err();
        assert!(
            matches!(err, MutterError::PipewireSocketTimeout { .. }),
            "expected PipewireSocketTimeout, got: {err}"
        );
        // Public mapping: same Timeout bucket as the wayland one,
        // so workspace callers matching `Error::Timeout(_)` (e.g.
        // the e2e tests) keep working.
        let public: waydriver::Error = err.into();
        assert!(
            matches!(public, waydriver::Error::Timeout(_)),
            "expected waydriver::Error::Timeout, got: {public}"
        );
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
            matches!(err, MutterError::WaylandSocketTimeout { .. }),
            "expected WaylandSocketTimeout, got: {err}"
        );
        // And confirm the From<MutterError> -> waydriver::Error mapping
        // still produces the public Timeout variant — workspace callers
        // (notably the e2e tests) match on it.
        let public: waydriver::Error = err.into();
        assert!(
            matches!(public, waydriver::Error::Timeout(_)),
            "expected waydriver::Error::Timeout, got: {public}"
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
    fn test_state_returns_none_before_start() {
        // `state()` previously panicked when called outside the started
        // window. The current contract returns `None` so callers can
        // detect the lifecycle without trapping a panic.
        let c = MutterCompositor::new();
        assert!(c.state().is_none());
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
