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

pub struct MutterCapture {
    state: Arc<MutterState>,
}

impl MutterCapture {
    pub fn new(state: Arc<MutterState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl CaptureBackend for MutterCapture {
    async fn start_stream(&self) -> Result<PipeWireStream> {
        let conn = &self.state.conn;

        // Step 1: Create ScreenCast session.
        let empty_opts: HashMap<&str, Value> = HashMap::new();
        let reply = conn
            .call_method(
                Some("org.gnome.Mutter.ScreenCast"),
                "/org/gnome/Mutter/ScreenCast",
                Some("org.gnome.Mutter.ScreenCast"),
                "CreateSession",
                &(empty_opts.clone(),),
            )
            .await
            .map_err(|e| Error::Screenshot(format!("CreateSession: {e}")))?;
        let session_path: OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Screenshot(format!("parse session path: {e}")))?;

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
            .map_err(|e| Error::Screenshot(format!("RecordMonitor: {e}")))?;
        let stream_path: OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Screenshot(format!("parse stream path: {e}")))?;

        // Step 3: Subscribe to PipeWireStreamAdded BEFORE starting.
        // This ordering is load-bearing — mutter emits the signal synchronously
        // during `Session.Start`, so a late subscribe misses it.
        let stream_proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(conn)
            .destination("org.gnome.Mutter.ScreenCast")
            .map_err(|e| Error::Screenshot(format!("proxy destination: {e}")))?
            .path(stream_path.as_str())
            .map_err(|e| Error::Screenshot(format!("proxy path: {e}")))?
            .interface("org.gnome.Mutter.ScreenCast.Stream")
            .map_err(|e| Error::Screenshot(format!("proxy interface: {e}")))?
            .build()
            .await
            .map_err(|e| Error::Screenshot(format!("build stream proxy: {e}")))?;

        let mut signal_stream = stream_proxy
            .receive_signal("PipeWireStreamAdded")
            .await
            .map_err(|e| Error::Screenshot(format!("receive_signal: {e}")))?;

        // Step 4: Start the ScreenCast session.
        conn.call_method(
            Some("org.gnome.Mutter.ScreenCast"),
            session_path.as_str(),
            Some("org.gnome.Mutter.ScreenCast.Session"),
            "Start",
            &(),
        )
        .await
        .map_err(|e| Error::Screenshot(format!("Start: {e}")))?;

        // Step 5: Wait for PipeWireStreamAdded signal to get the node id.
        let node_id: u32 = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let signal = signal_stream
                .next()
                .await
                .ok_or_else(|| Error::Screenshot("signal stream ended".to_string()))?;
            signal
                .body()
                .deserialize::<u32>()
                .map_err(|e| Error::Screenshot(format!("parse node_id: {e}")))
        })
        .await
        .map_err(|_| Error::Screenshot("timeout waiting for PipeWireStreamAdded".to_string()))??;

        tracing::debug!(node_id, "got PipeWire node id for screenshot");

        Ok(PipeWireStream {
            node_id,
            token: Box::new(session_path),
        })
    }

    async fn stop_stream(&self, stream: PipeWireStream) -> Result<()> {
        let session_path = stream.token.downcast::<OwnedObjectPath>().map_err(|_| {
            Error::Screenshot("stop_stream: token was not an OwnedObjectPath".into())
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
        Ok(())
    }

    fn pipewire_socket(&self) -> PathBuf {
        self.state.runtime_dir.join("pipewire-0")
    }
}
