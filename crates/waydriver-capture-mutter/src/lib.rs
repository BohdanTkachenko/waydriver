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

use waydriver::{CaptureBackend, Error, PipeWireStream, Result};
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
}

#[async_trait]
impl CaptureBackend for MutterCapture {
    async fn start_stream(&self) -> Result<PipeWireStream> {
        let conn = &self.state.conn;

        // Step 1: Create ScreenCast session, linking it to the existing
        // RemoteDesktop session so absolute pointer motion works (mutter
        // routes NotifyPointerMotionAbsolute through the linked stream).
        let empty_opts: HashMap<&str, Value> = HashMap::new();
        let mut create_opts: HashMap<&str, Value> = HashMap::new();
        create_opts.insert(
            "remote-desktop-session-id",
            Value::from(self.state.rd_session_id.as_str()),
        );
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

        // Step 4: Start the ScreenCast session — via either the SC
        // interface (standalone) or the linked RD session. When the SC
        // session is linked to an RD session, mutter requires
        // `RemoteDesktop.Session.Start` to drive it; calling
        // `ScreenCast.Session.Start` directly yields
        // "Must be started from remote desktop session". Starting RD
        // also unlocks `NotifyPointerMotionAbsolute` on the input
        // backend. Only the first stream triggers RD.Start; subsequent
        // streams share the same RD session and skip.
        let should_start_rd = {
            let mut guard = self
                .state
                .rd_started
                .lock()
                .map_err(|_| Error::process("rd_started mutex poisoned"))?;
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
                self.state.rd_session_path.as_str(),
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

        tracing::debug!(node_id, "got PipeWire node id for screenshot");

        // Publish the stream object path so MutterInput can route
        // NotifyPointerMotionAbsolute at the correct monitor.
        *self
            .state
            .active_stream_path
            .lock()
            .map_err(|_| Error::process("active_stream_path mutex poisoned"))? =
            Some(stream_path.to_string());

        Ok(PipeWireStream {
            node_id,
            token: Box::new(session_path),
        })
    }

    async fn stop_stream(&self, stream: PipeWireStream) -> Result<()> {
        let session_path = stream.token.downcast::<OwnedObjectPath>().map_err(|_| {
            Error::screenshot("stop_stream: token was not an OwnedObjectPath")
        })?;
        let _ = self
            .state
            .conn
            .call_method(
                Some("org.gnome.Mutter.ScreenCast"),
                session_path.as_str(),
                Some("org.gnome.Mutter.ScreenCast.Session"),
                "Stop",
                &(),
            )
            .await;
        *self
            .state
            .active_stream_path
            .lock()
            .map_err(|_| Error::process("active_stream_path mutex poisoned"))? = None;
        Ok(())
    }

    fn pipewire_socket(&self) -> PathBuf {
        self.state.runtime_dir.join("pipewire-0")
    }
}
