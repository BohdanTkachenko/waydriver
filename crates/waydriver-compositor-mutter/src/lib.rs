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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use tokio::process::{Child, Command};
use zbus::zvariant::OwnedValue;

use waydriver::gsettings::{self, GSettingEntry, GSettingsConfig};
use waydriver::{CompositorRuntime, Result};

use crate::error::MutterError;

/// Default virtual-monitor geometry passed to mutter when the caller doesn't
/// override it. Matches mutter's own implicit default.
const DEFAULT_RESOLUTION: &str = "1024x768";

/// Default logical-monitor scale: 1:1, i.e. `resolution` pixels are also the
/// logical (application) size. Any other value drives the HiDPI path in
/// [`apply_scale`].
const DEFAULT_SCALE: f64 = 1.0;

/// GVariant-text value seeded into `org.gnome.mutter experimental-features`
/// when GSettings isolation is on. `scale-monitor-framebuffer` switches the
/// native headless backend to logical layout mode, which is what makes
/// fractional scales (1.5, 1.75, …) appear in a mode's `supported-scales` and
/// be accepted by `ApplyMonitorsConfig`. Harmless at integer/1.0 scales.
const MUTTER_FRACTIONAL_SCALING: &str = "['scale-monitor-framebuffer']";

/// Accepted scale range. Below 0.5 the UI is unusably small; above 4.0 mutter
/// won't offer the scale for any virtual-monitor mode we'd create. Validated
/// up-front so a typo fails before we spawn any subprocess.
const MIN_SCALE: f64 = 0.5;
const MAX_SCALE: f64 = 4.0;

/// How far a requested scale may sit from the nearest mutter-supported scale
/// before we log a warning about snapping to it. Mutter only accepts scales it
/// lists in a mode's `supported-scales`, so an exact arbitrary value (e.g.
/// 1.66) may be nudged to the closest legal step.
const SCALE_SNAP_TOLERANCE: f64 = 0.01;

// ── DisplayConfig D-Bus shapes ───────────────────────────────────────────────
//
// Type aliases mirroring the `org.gnome.Mutter.DisplayConfig` wire types so
// `body().deserialize::<CurrentState>()` validates the reply against the exact
// signature mutter sends. The `a{sv}` property dicts are kept as
// `HashMap<String, OwnedValue>` (signature `a{sv}`) and ignored — we only need
// the connector, mode id, and supported-scales list.

/// `a{sv}` — a D-Bus property dict.
type DbusProps = HashMap<String, OwnedValue>;
/// `(siiddada{sv})` — one monitor mode: id, width, height, refresh, preferred
/// scale, supported scales, properties.
type MonitorMode = (String, i32, i32, f64, f64, Vec<f64>, DbusProps);
/// `(ssss)` — connector, vendor, product, serial.
type MonitorSpec = (String, String, String, String);
/// `((ssss)a(siiddada{sv})a{sv})` — one physical monitor: spec, modes, props.
type PhysicalMonitor = (MonitorSpec, Vec<MonitorMode>, DbusProps);
/// `(iiduba(ssss)a{sv})` — one logical monitor in the current layout.
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, DbusProps);
/// Return tuple of `GetCurrentState`: `(serial, monitors, logical, props)`.
type CurrentState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, DbusProps);

/// `(ssa{sv})` — one monitor assignment in an `ApplyMonitorsConfig` request:
/// connector, mode id, properties.
type MonitorAssignment = (String, String, DbusProps);
/// `(iiduba(ssa{sv}))` — one logical monitor to apply: x, y, scale, transform,
/// primary, assigned monitors.
type LogicalMonitorConfig = (i32, i32, f64, u32, bool, Vec<MonitorAssignment>);

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
    /// The private session bus, run as a *managed* `dbus-daemon` child (rather
    /// than `dbus-launch`, which daemonizes it out of our process tree). Kept
    /// alive for the compositor's lifetime; killed on `stop()`/`Drop` and —
    /// via [`set_pdeathsig`] — reaped by the kernel if the controlling process
    /// is hard-killed, taking the D-Bus-activated `at-spi-bus-launcher` down
    /// with it instead of orphaning a stale a11y bus.
    dbus_daemon: Option<Child>,
    mutter: Option<Child>,
    pipewire: Option<Child>,
    wireplumber: Option<Child>,
    state: Option<Arc<MutterState>>,
    gsettings: GSettingsConfig,
}

/// The host runtime root under which every session's `wd-session-<id>`
/// directory is created. Snapshotted once, lazily, on the first
/// `MutterCompositor::new()` call.
///
/// This is deliberately read **once** and cached, rather than re-read from
/// `XDG_RUNTIME_DIR` per session. `waydriver`'s screenshot and video pipelines
/// (`waydriver::capture`) mutate the parent process's `XDG_RUNTIME_DIR` to
/// point `pipewiresrc` at the *live* session's pipewire socket, and never
/// restore it. If `new()` re-read the live env each time, session N+1's
/// runtime dir would be created **inside** session N's dir
/// (`…/wd-session-A/wd-session-B/…`), nesting one level deeper per session.
/// After ~4 levels the `<dir>/pipewire-0` path exceeds the ~107-byte AF_UNIX
/// `sun_path` limit, pipewire can no longer bind its socket, and every
/// subsequent `start_session` fails with a "timeout: pipewire socket" error
/// until the server is restarted (which resets the process env). Snapshotting
/// the root keeps each session dir a flat sibling under the original
/// `XDG_RUNTIME_DIR`, independent of how many sessions preceded it.
///
/// The first `new()` runs before any session exists, so the env is still the
/// pristine value set by the launcher (e.g. the Docker entrypoint) — capturing
/// it then is safe.
static HOST_RUNTIME_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    PathBuf::from(root)
});

/// Eagerly capture [`HOST_RUNTIME_ROOT`] from the current `XDG_RUNTIME_DIR`
/// and return it.
///
/// The snapshot is otherwise taken lazily on the first [`MutterCompositor::new`].
/// Call this once at process startup — before any session is created and
/// before anything can mutate `XDG_RUNTIME_DIR` — to pin the root to the
/// pristine launcher value deterministically, rather than relying on `new()`
/// happening first. Idempotent: subsequent calls (and `new()`) return the same
/// captured value.
pub fn establish_runtime_root() -> &'static std::path::Path {
    HOST_RUNTIME_ROOT.as_path()
}

impl MutterCompositor {
    /// Construct but do not start. Generates the session id and computes
    /// where the Wayland socket and runtime dir will live. No I/O.
    pub fn new() -> Self {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let wayland_display = format!("wayland-wd-{}", id);

        let runtime_dir = HOST_RUNTIME_ROOT.join(format!("wd-session-{}", id));

        Self {
            id,
            wayland_display,
            runtime_dir,
            mutter_dbus_address: String::new(),
            dbus_daemon: None,
            mutter: None,
            pipewire: None,
            wireplumber: None,
            state: None,
            gsettings: GSettingsConfig::default(),
        }
    }

    /// Set the per-session GSettings isolation config (see
    /// [`waydriver::gsettings`]). Defaults to isolated with no seeded entries.
    /// When isolated, [`start`](Self::start) writes a private keyfile (seeded
    /// with `org.gnome.mutter experimental-features` plus `config.initial`)
    /// and points mutter at it, so fractional scales work and the host's dconf
    /// is neither read nor written. Pass `isolated: false` to run mutter
    /// against the host's GSettings instead.
    pub fn with_gsettings(mut self, config: GSettingsConfig) -> Self {
        self.gsettings = config;
        self
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
    /// 1. validate resolution + scale,
    /// 2. ensure the session runtime dir exists,
    /// 3. spawn a private `dbus-daemon` and parse its address + PID,
    /// 4. spawn `pipewire` + `wireplumber` on that bus,
    /// 5. spawn headless `mutter --wayland`,
    /// 6. wait for the Wayland socket,
    /// 7. open a zbus connection, retry-create the RemoteDesktop session,
    /// 8. read its `SessionId` property,
    /// 9. apply a non-default logical-monitor scale via DisplayConfig,
    /// 10. publish the `Arc<MutterState>` for sibling backends.
    async fn start_inner(
        &mut self,
        resolution: Option<&str>,
        scale: Option<f64>,
    ) -> std::result::Result<(), MutterError> {
        let resolution = resolution.unwrap_or(DEFAULT_RESOLUTION);
        // Validate before we start spawning subprocesses — mutter silently
        // ignores bad --virtual-monitor values and falls back to its own
        // default, which would surprise the caller.
        parse_resolution(resolution)?;
        let scale = scale.unwrap_or(DEFAULT_SCALE);
        // Fail fast on a nonsense scale too, for the same reason — the
        // DisplayConfig apply that consumes it doesn't run until mutter is up.
        validate_scale(scale)?;

        tracing::info!(
            id = self.id,
            resolution,
            scale,
            isolated = self.gsettings.isolated,
            "starting mutter compositor"
        );

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

        // GSettings isolation: when on, write the session's private keyfile
        // (read by both mutter and the app — see `waydriver::gsettings`) and
        // compute the env that points mutter at it. The keyfile is seeded with
        // the fractional-scaling experimental feature so a non-integer `scale`
        // is actually advertised by mutter, then any caller-supplied entries
        // are appended (last-wins, so callers can override). When off, mutter
        // reads the host's GSettings and `config_env` stays empty.
        let config_env: Vec<(&str, String)> = if self.gsettings.isolated {
            let mut entries = vec![GSettingEntry::new(
                "org.gnome.mutter",
                "experimental-features",
                MUTTER_FRACTIONAL_SCALING,
            )];
            entries.extend(self.gsettings.initial.iter().cloned());
            gsettings::write_keyfile(&self.runtime_dir, &entries)?;
            let config_dir = gsettings::config_dir(&self.runtime_dir)
                .to_str()
                .expect("invariant: config_dir is runtime_dir (UTF-8) + ASCII suffix")
                .to_string();
            vec![
                ("XDG_CONFIG_HOME", config_dir),
                ("GSETTINGS_BACKEND", gsettings::KEYFILE_BACKEND.to_string()),
            ]
        } else {
            Vec::new()
        };

        // Step 1: Private D-Bus for mutter (so its ScreenCast API doesn't
        // conflict with host). Run `dbus-daemon` directly as a managed,
        // PDEATHSIG-protected child instead of `dbus-launch`: dbus-launch
        // daemonizes the bus out of our process tree, so a hard-killed
        // controlling process would orphan it — and the `at-spi-bus-launcher`
        // it D-Bus-activates, whose stale socket then GUID-mismatches every
        // later run. As our own child, the kernel reaps it on hard-kill.
        // Pick the bus socket ourselves (under the per-session runtime dir,
        // alongside the wayland/pipewire sockets) and pass it via `--address`,
        // rather than parsing the daemon's stdout for `--print-address`: reading
        // stdout is fragile across distros/containers (an early stderr-only
        // failure reads back as an empty address), and a chosen path lets us use
        // the same socket-appears readiness poll as wayland/pipewire.
        let bus_socket = self.runtime_dir.join("bus");
        let address = format!("unix:path={}", bus_socket.display());
        let mut dbus_cmd = Command::new("dbus-daemon");
        dbus_cmd
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(format!("--address={address}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        set_pdeathsig(&mut dbus_cmd);
        let dbus_daemon = dbus_cmd.spawn().map_err(|source| MutterError::Spawn {
            process: "dbus-daemon",
            source,
        })?;
        self.dbus_daemon = Some(dbus_daemon);
        wait_for_dbus_socket(&bus_socket).await?;
        self.mutter_dbus_address = address;
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
        let mut pipewire_cmd = Command::new("pipewire");
        pipewire_cmd
            .env_remove("PIPEWIRE_REMOTE")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        set_pdeathsig(&mut pipewire_cmd);
        let pipewire = pipewire_cmd.spawn().map_err(|source| MutterError::Spawn {
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

        let mut wireplumber_cmd = Command::new("wireplumber");
        wireplumber_cmd
            .env_remove("PIPEWIRE_REMOTE")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.mutter_dbus_address)
            .env("XDG_RUNTIME_DIR", &runtime_str)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        set_pdeathsig(&mut wireplumber_cmd);
        let wireplumber = wireplumber_cmd
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
        let mut mutter_cmd = Command::new("mutter");
        mutter_cmd
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
            // Empty when isolation is off; otherwise points mutter at the
            // per-session keyfile GSettings store written above.
            .envs(config_env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        set_pdeathsig(&mut mutter_cmd);
        let mutter = mutter_cmd.spawn().map_err(|source| MutterError::Spawn {
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

        // Step: apply a non-default logical-monitor scale. `--virtual-monitor`
        // has no scale component, so HiDPI is configured here over mutter's
        // private bus once DisplayConfig is up. Skipped at 1.0 to leave the
        // default 1:1 path completely untouched.
        if (scale - DEFAULT_SCALE).abs() > f64::EPSILON {
            let applied = apply_scale(&mutter_conn, scale, &self.id).await?;
            tracing::info!(
                id = self.id,
                requested = scale,
                applied,
                "applied logical-monitor scale"
            );
        }

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
    async fn start(&mut self, resolution: Option<&str>, scale: Option<f64>) -> Result<()> {
        // Body uses the crate-local typed `MutterError`. The `?` at the
        // end of `self.start_inner(...).await?` runs the
        // `From<MutterError> for waydriver::Error` impl in `error.rs`,
        // which is the single boundary at which the typed enum becomes
        // the workspace's shared `waydriver::Error`.
        Ok(self.start_inner(resolution, scale).await?)
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

        // Kill the private bus last: its death drops the a11y bus connection,
        // so the D-Bus-activated `at-spi-bus-launcher` exits with it.
        if let Some(mut dbus) = self.dbus_daemon.take() {
            let _ = dbus.kill().await;
            let _ = dbus.wait().await;
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
        if let Some(ref mut child) = self.dbus_daemon {
            let _ = child.start_kill();
        }
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Linux parent-death protection for a spawned session daemon: ask the kernel
/// to `SIGKILL` the child the instant the spawning thread dies.
///
/// Drop/`stop()`-based teardown can't run when the *controlling* process is
/// itself hard-killed (`SIGKILL`, `panic = "abort"`, OOM, a CI/test-runner
/// timeout). Without this, such a death orphans the whole session quartet —
/// `dbus-daemon`, `pipewire`, `wireplumber`, `mutter` — and, worst of all, the
/// D-Bus-activated `at-spi-bus-launcher`, whose stale a11y-bus socket then
/// GUID-mismatches every later run. `PR_SET_PDEATHSIG` closes that hole at the
/// kernel level. The `getppid` check covers the race where the parent dies
/// between fork and exec (the child would otherwise miss the death signal).
fn set_pdeathsig(cmd: &mut Command) {
    // Capture our PID now (in the parent). The child compares it against its
    // own parent after fork: a mismatch means we already died and it was
    // reparented (to init, or a subreaper), so it should bail. We must NOT
    // hard-code "getppid() == 1 → orphaned": in a container the controlling
    // process often *is* PID 1, so every legitimately-parented child sees
    // getppid() == 1 and would be killed at exec.
    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and calls only
    // async-signal-safe libc functions (prctl, getppid, _exit).
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent as i32 {
                libc::_exit(0);
            }
            Ok(())
        });
    }
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

/// Reject a scale that isn't a finite, positive factor inside
/// [`MIN_SCALE`]..=[`MAX_SCALE`]. Run before any subprocess spawns so a bad
/// value fails fast.
fn validate_scale(scale: f64) -> std::result::Result<(), MutterError> {
    if scale.is_finite() && (MIN_SCALE..=MAX_SCALE).contains(&scale) {
        Ok(())
    } else {
        Err(MutterError::ScaleInvalid {
            value: scale,
            min: MIN_SCALE,
            max: MAX_SCALE,
        })
    }
}

/// Pick the entry of `supported` closest to `requested`. Mutter only accepts
/// scales it advertises for a mode, so an arbitrary request (e.g. 1.66) is
/// snapped to the nearest legal step. Falls back to `requested` when the list
/// is empty (mutter then validates — and likely rejects — it).
fn nearest_supported_scale(requested: f64, supported: &[f64]) -> f64 {
    supported
        .iter()
        .copied()
        .min_by(|a, b| {
            let da = (a - requested).abs();
            let db = (b - requested).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(requested)
}

/// Apply `requested` as the logical-monitor scale of the (single) virtual
/// monitor via `org.gnome.Mutter.DisplayConfig`, returning the scale mutter
/// actually accepted (snapped to a supported step).
///
/// Reads `GetCurrentState` fresh each call so the `serial` is current —
/// mutter rejects `ApplyMonitorsConfig` on a stale serial, and the serial
/// bumps when the virtual monitor first appears. Fractional scales (1.5,
/// 1.75, …) require the `scale-monitor-framebuffer` experimental feature; the
/// container entrypoint enables it. Without it only integer scales are
/// advertised, so [`nearest_supported_scale`] would snap a fractional request
/// to 1.0 or 2.0.
async fn apply_scale(
    conn: &zbus::Connection,
    requested: f64,
    id: &str,
) -> std::result::Result<f64, MutterError> {
    let state_reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await
        .map_err(|source| MutterError::DisplayConfigState {
            stage: "call",
            source,
        })?;
    let state_body = state_reply.body();
    let (serial, monitors, _logical, _props): CurrentState =
        state_body
            .deserialize()
            .map_err(|source| MutterError::DisplayConfigState {
                stage: "deserialize",
                source,
            })?;

    // Headless mutter started with a single `--virtual-monitor` exposes
    // exactly one monitor advertising exactly the mode we asked for, so the
    // first monitor / first mode is the one to scale.
    let (spec, modes, _mprops) = monitors
        .into_iter()
        .next()
        .ok_or(MutterError::DisplayConfigNoMonitor)?;
    let connector = spec.0;
    let (mode_id, _w, _h, _refresh, _preferred, supported, _modeprops) =
        modes
            .into_iter()
            .next()
            .ok_or(MutterError::DisplayConfigNoMonitor)?;

    let applied = nearest_supported_scale(requested, &supported);
    if (applied - requested).abs() > SCALE_SNAP_TOLERANCE {
        tracing::warn!(
            id,
            requested,
            applied,
            supported = ?supported,
            "requested scale not advertised by mutter; snapped to nearest supported"
        );
    }

    // (x, y, scale, transform, primary, [(connector, mode_id, {})]).
    let logical: LogicalMonitorConfig = (
        0,
        0,
        applied,
        0,
        true,
        vec![(connector, mode_id, DbusProps::new())],
    );
    // method 1 = temporary: applies for this session without writing
    // ~/.config/monitors.xml, which is all a throwaway headless run needs.
    conn.call_method(
        Some("org.gnome.Mutter.DisplayConfig"),
        "/org/gnome/Mutter/DisplayConfig",
        Some("org.gnome.Mutter.DisplayConfig"),
        "ApplyMonitorsConfig",
        &(serial, 1u32, vec![logical], DbusProps::new()),
    )
    .await
    .map_err(|source| MutterError::DisplayConfigApply {
        scale: applied,
        source,
    })?;

    Ok(applied)
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

/// Poll for the managed `dbus-daemon`'s listen socket to appear before any
/// client connects to it — the same readiness signal used for the wayland and
/// pipewire sockets. A timeout means the daemon failed to bind (bad config,
/// path too long for `AF_UNIX`, missing binary).
async fn wait_for_dbus_socket(socket: &Path) -> std::result::Result<(), MutterError> {
    for _ in 0..50 {
        if socket.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(MutterError::DbusLaunchFailed(format!(
        "dbus-daemon socket never appeared at {}",
        socket.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves [`set_pdeathsig`]'s mechanism: a child spawned with
    /// `PR_SET_PDEATHSIG = SIGKILL` is reaped by the kernel when its parent is
    /// hard-killed (bypassing all Drop/teardown) — the exact protection that
    /// stops a SIGKILL'd run from orphaning the session daemons + the
    /// `at-spi-bus-launcher`.
    ///
    /// Re-execs the test binary as a *helper*: the helper spawns `sleep 300`
    /// with the same `pre_exec` as `set_pdeathsig`, writes its PID to a shared
    /// pidfile, then blocks. The supervisor reads the PID, `SIGKILL`s the helper
    /// (so no Drop runs), and asserts the kernel reaps the `sleep`.
    #[test]
    #[ignore = "spawns and SIGKILLs a subprocess; run manually with --ignored"]
    fn pdeathsig_reaps_orphaned_child() {
        use std::os::unix::process::CommandExt;
        use std::process::Command as StdCommand;

        // The supervisor passes the pidfile path; its presence selects the role.
        if let Ok(pidfile) = std::env::var("WD_PDEATHSIG_PIDFILE") {
            // Helper role: spawn `sleep` protected by PR_SET_PDEATHSIG.
            let mut sleep = StdCommand::new("sleep");
            sleep.arg("300");
            let parent = std::process::id();
            // SAFETY: only async-signal-safe libc calls before exec.
            unsafe {
                sleep.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent as i32 {
                        libc::_exit(0);
                    }
                    Ok(())
                });
            }
            let mut child = sleep.spawn().expect("spawn sleep");
            // Write+rename so the supervisor never reads a half-written pid.
            let tmp = format!("{pidfile}.tmp");
            std::fs::write(&tmp, child.id().to_string()).unwrap();
            std::fs::rename(&tmp, &pidfile).unwrap();
            // Block on the sleep until the supervisor SIGKILLs us (also
            // satisfies clippy::zombie_processes — the child is waited on).
            let _ = child.wait();
            return;
        }

        // Supervisor role: re-exec self as the helper.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("sleep.pid");
        let exe = std::env::current_exe().unwrap();
        let mut helper = StdCommand::new(exe)
            .args([
                "--exact",
                "tests::pdeathsig_reaps_orphaned_child",
                "--ignored",
            ])
            .env("WD_PDEATHSIG_PIDFILE", &pidfile)
            .spawn()
            .expect("spawn helper");

        // Wait for the helper to publish the sleep PID.
        let mut sleep_pid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    sleep_pid = Some(pid);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let sleep_pid = sleep_pid.expect("helper never published the sleep PID");

        // The sleep is alive while the helper lives.
        assert_eq!(
            unsafe { libc::kill(sleep_pid, 0) },
            0,
            "sleep ({sleep_pid}) should be running before the helper is killed"
        );

        // Hard-kill the helper — no Drop, no teardown.
        unsafe {
            libc::kill(helper.id() as i32, libc::SIGKILL);
        }
        let _ = helper.wait();

        // The kernel must SIGKILL the orphaned sleep; init reaps the zombie.
        let mut reaped = false;
        for _ in 0..50 {
            if unsafe { libc::kill(sleep_pid, 0) } != 0 {
                reaped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !reaped {
            unsafe { libc::kill(sleep_pid, libc::SIGKILL) };
        }
        assert!(
            reaped,
            "PR_SET_PDEATHSIG did not reap the orphaned child after its parent was SIGKILLed"
        );
    }

    /// Live end-to-end check that a fractional scale is actually applied by a
    /// real mutter. Requires the runtime stack (mutter, pipewire, wireplumber,
    /// dbus-launch) on `PATH` plus the GSettings schemas in `XDG_DATA_DIRS`, so
    /// it's `#[ignore]`d by default; run with the dev shell's env via
    /// `cargo test -p waydriver-compositor-mutter -- --ignored`.
    ///
    /// This exercises the whole chain at once: the per-session keyfile must
    /// enable `scale-monitor-framebuffer` (otherwise 1.5 is not advertised and
    /// would snap to 1.0/2.0), and `apply_scale` must drive DisplayConfig
    /// correctly. We read the scale straight back from `GetCurrentState`.
    #[tokio::test]
    #[ignore = "requires a live mutter/pipewire/dbus runtime stack"]
    async fn applies_fractional_scale_against_real_mutter() {
        let mut compositor = MutterCompositor::new();
        compositor
            .start(Some("1920x1080"), Some(1.5))
            .await
            .expect("compositor should start");
        let state = compositor.state().expect("state present after start");

        let reply = state
            .conn()
            .call_method(
                Some("org.gnome.Mutter.DisplayConfig"),
                "/org/gnome/Mutter/DisplayConfig",
                Some("org.gnome.Mutter.DisplayConfig"),
                "GetCurrentState",
                &(),
            )
            .await
            .expect("GetCurrentState should succeed");
        let body = reply.body();
        let (_serial, _monitors, logical, _props): CurrentState = body
            .deserialize()
            .expect("GetCurrentState body should deserialize");
        let applied = logical.first().expect("at least one logical monitor").2;

        compositor.stop().await.expect("compositor should stop");

        assert!(
            (applied - 1.5).abs() < 0.01,
            "expected logical scale ~1.5 (fractional scaling enabled via keyfile), got {applied}"
        );
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

    /// Regression: session runtime dirs must be flat siblings under one root,
    /// never nested inside each other. `waydriver::capture` repoints the
    /// process-wide `XDG_RUNTIME_DIR` at the live session's dir after a
    /// screenshot/recording; if `new()` re-read that mutated value, each
    /// session would nest one level deeper and eventually overflow the
    /// AF_UNIX `sun_path` limit, wedging pipewire socket creation. See
    /// `HOST_RUNTIME_ROOT`.
    #[test]
    fn test_session_runtime_dirs_are_siblings_not_nested() {
        let a = MutterCompositor::new();
        let dir_a = a.runtime_dir().to_path_buf();

        // Simulate what a screenshot/recording does: point XDG_RUNTIME_DIR at
        // the live session's runtime dir and leave it there.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir_a);
        }

        let b = MutterCompositor::new();
        let dir_b = b.runtime_dir().to_path_buf();

        assert_eq!(
            dir_a.parent(),
            dir_b.parent(),
            "session dirs must share a parent (siblings), got a={dir_a:?} b={dir_b:?}"
        );
        assert!(
            !dir_b.starts_with(&dir_a),
            "session B nested inside session A: {dir_b:?}"
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
    fn test_validate_scale_accepts_common_factors() {
        for ok in [0.5, 1.0, 1.25, 1.5, 1.6666, 1.75, 2.0, 3.0, 4.0] {
            assert!(validate_scale(ok).is_ok(), "expected {ok} to validate");
        }
    }

    #[test]
    fn test_validate_scale_rejects_out_of_range_and_nonfinite() {
        for bad in [0.0, 0.49, 4.01, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                validate_scale(bad).is_err(),
                "expected {bad} to be rejected"
            );
        }
    }

    #[test]
    fn test_nearest_supported_scale_snaps_to_closest() {
        let supported = [1.0, 1.25, 1.5, 1.75, 2.0];
        // Exact hits pass straight through.
        assert_eq!(nearest_supported_scale(1.5, &supported), 1.5);
        assert_eq!(nearest_supported_scale(2.0, &supported), 2.0);
        // 1.66 (166%) isn't advertised → nearest is 1.75.
        assert_eq!(nearest_supported_scale(1.66, &supported), 1.75);
        // 1.6 is closer to 1.5.
        assert_eq!(nearest_supported_scale(1.6, &supported), 1.5);
    }

    #[test]
    fn test_nearest_supported_scale_empty_list_returns_request() {
        assert_eq!(nearest_supported_scale(1.5, &[]), 1.5);
    }

    #[test]
    fn test_default_same_structure_as_new() {
        let c = MutterCompositor::default();
        assert!(c.wayland_display().starts_with("wayland-wd-"));
        assert!(c.runtime_dir().to_str().unwrap().contains("wd-session-"));
    }
}
