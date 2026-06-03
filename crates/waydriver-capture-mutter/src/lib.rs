//! Mutter implementation of [`waydriver::CaptureBackend`].
//!
//! Creates a ScreenCast session on mutter's private D-Bus, records the
//! virtual monitor, waits for the `PipeWireStreamAdded` signal to learn the
//! PipeWire node id, and hands that off to the shared `waydriver::capture::grab_png`
//! helper via the trait's default `take_screenshot` impl.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, Value};

use waydriver::{CaptureBackend, Error, PipeWireStream, Result, StreamToken};
use waydriver_compositor_mutter::MutterState;

/// Mutter ScreenCast + PipeWire capture backend.
pub struct MutterCapture {
    state: Arc<MutterState>,
}

impl MutterCapture {
    /// Create a new capture backend from shared compositor state.
    pub fn new(state: Arc<MutterState>) -> Self {
        Self { state }
    }

    /// Open a ScreenCast monitor-record stream and resolve its PipeWire node id.
    ///
    /// `link_rd` selects between the two stream flavors waydriver needs:
    ///
    /// - `true` — the interactive/keepalive stream. The ScreenCast session is
    ///   linked to the RemoteDesktop session so mutter accepts
    ///   `NotifyPointerMotionAbsolute`, the session is driven via
    ///   `RemoteDesktop.Session.Start` (mutter rejects
    ///   `ScreenCast.Session.Start` on a linked session), and the resulting
    ///   stream path is published as the active stream for pointer routing.
    /// - `false` — a standalone stream for the video recorder. It is *not*
    ///   linked to RemoteDesktop (the recorder needs only pixels), is started
    ///   via `ScreenCast.Session.Start` directly, and does not touch the
    ///   active-stream path. Keeping the recorder on its own node is what
    ///   prevents it from starving the screenshot consumer on the shared
    ///   keepalive node — see [`CaptureBackend::start_recording_stream`].
    async fn create_stream(&self, link_rd: bool) -> Result<PipeWireStream> {
        let conn = self.state.conn();

        // Step 1: Create the ScreenCast session. For the interactive stream we
        // link it to the existing RemoteDesktop session so absolute pointer
        // motion works (mutter routes NotifyPointerMotionAbsolute through the
        // linked stream). The recorder's standalone stream skips the link.
        let empty_opts: HashMap<&str, Value> = HashMap::new();
        let mut create_opts: HashMap<&str, Value> = HashMap::new();
        if link_rd {
            create_opts.insert(
                "remote-desktop-session-id",
                Value::from(self.state.rd_session_id()),
            );
        }
        let reply = conn
            .call_method(
                Some("org.gnome.Mutter.ScreenCast"),
                "/org/gnome/Mutter/ScreenCast",
                Some("org.gnome.Mutter.ScreenCast"),
                "CreateSession",
                &(create_opts,),
            )
            .await
            .map_err(|e| Error::screenshot_with("CreateSession", e))?;
        let session_path: OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| Error::screenshot_with("parse session path", e))?;

        // Step 2: RecordMonitor on the session.
        let reply = conn
            .call_method(
                Some("org.gnome.Mutter.ScreenCast"),
                session_path.as_str(),
                Some("org.gnome.Mutter.ScreenCast.Session"),
                "RecordMonitor",
                &("", empty_opts),
            )
            .await
            .map_err(|e| Error::screenshot_with("RecordMonitor", e))?;
        let stream_path: OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| Error::screenshot_with("parse stream path", e))?;

        // Step 3: Subscribe to PipeWireStreamAdded BEFORE starting.
        // This ordering is load-bearing — mutter emits the signal synchronously
        // during `Session.Start`, so a late subscribe misses it.
        let stream_proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(conn)
            .destination("org.gnome.Mutter.ScreenCast")
            .map_err(|e| Error::screenshot_with("proxy destination", e))?
            .path(stream_path.as_str())
            .map_err(|e| Error::screenshot_with("proxy path", e))?
            .interface("org.gnome.Mutter.ScreenCast.Stream")
            .map_err(|e| Error::screenshot_with("proxy interface", e))?
            .build()
            .await
            .map_err(|e| Error::screenshot_with("build stream proxy", e))?;

        let mut signal_stream = stream_proxy
            .receive_signal("PipeWireStreamAdded")
            .await
            .map_err(|e| Error::screenshot_with("receive_signal", e))?;

        // Step 4: Start the ScreenCast session. A linked (RD) session must be
        // driven via `RemoteDesktop.Session.Start` — calling
        // `ScreenCast.Session.Start` on it yields "Must be started from remote
        // desktop session". Starting RD also unlocks
        // `NotifyPointerMotionAbsolute` on the input backend. Only the first
        // linked stream triggers RD.Start; subsequent linked streams share the
        // same RD session and skip. A standalone (non-linked) stream — the
        // recorder's — is started directly via `ScreenCast.Session.Start` and
        // never touches the RD-started flag.
        let should_start_rd = link_rd && {
            let mut guard = self.state.rd_started_lock()?;
            if *guard {
                false
            } else {
                *guard = true;
                true
            }
        };
        if should_start_rd {
            conn.call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "Start",
                &(),
            )
            .await
            .map_err(|e| Error::screenshot_with("RemoteDesktop Start", e))?;
        } else {
            conn.call_method(
                Some("org.gnome.Mutter.ScreenCast"),
                session_path.as_str(),
                Some("org.gnome.Mutter.ScreenCast.Session"),
                "Start",
                &(),
            )
            .await
            .map_err(|e| Error::screenshot_with("Start", e))?;
        }

        // Step 5: Wait for PipeWireStreamAdded signal to get the node id.
        let node_id: u32 = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let signal = signal_stream
                .next()
                .await
                .ok_or_else(|| Error::screenshot("signal stream ended"))?;
            signal
                .body()
                .deserialize::<u32>()
                .map_err(|e| Error::screenshot_with("parse node_id", e))
        })
        .await
        .map_err(|_| Error::screenshot("timeout waiting for PipeWireStreamAdded"))??;

        tracing::debug!(node_id, link_rd, "got PipeWire node id");

        // Publish the stream object path so MutterInput can route
        // NotifyPointerMotionAbsolute at the correct monitor. Only the
        // interactive (RD-linked) stream owns pointer routing; the recorder's
        // standalone stream must not overwrite it.
        if link_rd {
            *self.state.active_stream_path_lock()? = Some(stream_path.to_string());
        }

        Ok(PipeWireStream {
            node_id,
            // The session_path is an `OwnedObjectPath`; `stop_stream`
            // below downcasts back to it. `StreamToken::downcast`
            // returns a typed error if a future change ever feeds a
            // different value here, instead of a `()` from
            // `Box::downcast`.
            token: StreamToken::new(session_path),
        })
    }

    /// Stop a ScreenCast session by object path. Shared by `stop_stream` and
    /// `stop_recording_stream`; best-effort (a failed `Session.Stop` on a
    /// teardown path is logged by the caller, not surfaced).
    async fn stop_session(&self, stream: PipeWireStream) -> Result<()> {
        let session_path = stream.token.downcast::<OwnedObjectPath>()?;
        let _ = self
            .state
            .conn()
            .call_method(
                Some("org.gnome.Mutter.ScreenCast"),
                session_path.as_str(),
                Some("org.gnome.Mutter.ScreenCast.Session"),
                "Stop",
                &(),
            )
            .await;
        Ok(())
    }
}

#[async_trait]
impl CaptureBackend for MutterCapture {
    async fn start_stream(&self) -> Result<PipeWireStream> {
        self.create_stream(true).await
    }

    async fn stop_stream(&self, stream: PipeWireStream) -> Result<()> {
        self.stop_session(stream).await?;
        // The interactive stream owns pointer routing; clear it on teardown.
        *self.state.active_stream_path_lock()? = None;
        Ok(())
    }

    async fn start_recording_stream(&self) -> Result<PipeWireStream> {
        self.create_stream(false).await
    }

    async fn stop_recording_stream(&self, stream: PipeWireStream) -> Result<()> {
        // Standalone stream: never published itself as the active stream, so
        // it must not clear the interactive stream's pointer-routing path.
        self.stop_session(stream).await
    }

    fn pipewire_socket(&self) -> PathBuf {
        self.state.runtime_dir().join("pipewire-0")
    }
}
