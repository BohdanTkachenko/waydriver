//! Mutter implementation of [`waydriver::InputBackend`].
//!
//! Wraps an `Arc<MutterState>` obtained from [`waydriver_compositor_mutter::MutterCompositor::state`]
//! and sends keyboard / pointer events via
//! `org.gnome.Mutter.RemoteDesktop.Session.{NotifyKeyboardKeysym, NotifyPointerMotionRelative}`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use waydriver::backend::cancellable_tail;
use waydriver::{Error, InputBackend, Result};
use waydriver_compositor_mutter::MutterState;

/// Mutter RemoteDesktop input backend.
pub struct MutterInput {
    state: Arc<MutterState>,
}

impl MutterInput {
    /// Create a new input backend from shared compositor state.
    pub fn new(state: Arc<MutterState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl InputBackend for MutterInput {
    async fn press_keysym(&self, keysym: u32, cancel: &CancellationToken) -> Result<()> {
        self.key_down(keysym, cancel).await?;
        // Mutter's RemoteDesktop needs a short gap between press and
        // release or the app sees a 0ms keystroke that some handlers
        // drop. This gap is *atomic* — we don't race it against the
        // token, because cancelling here would leave the key stuck
        // down in mutter's internal state with no scoped unwind.
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.key_up(keysym, cancel).await?;
        // Tail throttle so back-to-back calls from a test loop don't
        // stack up faster than GTK can process them. The event already
        // committed, so a cancelled session can cut this short.
        cancellable_tail(Duration::from_millis(30), cancel).await;
        Ok(())
    }

    async fn key_down(&self, keysym: u32, _cancel: &CancellationToken) -> Result<()> {
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyKeyboardKeysym",
                &(keysym, true),
            )
            .await
            .map_err(|e| Error::process_with("NotifyKeyboardKeysym press", e))?;
        Ok(())
    }

    async fn key_up(&self, keysym: u32, _cancel: &CancellationToken) -> Result<()> {
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyKeyboardKeysym",
                &(keysym, false),
            )
            .await
            .map_err(|e| Error::process_with("NotifyKeyboardKeysym release", e))?;
        Ok(())
    }

    async fn pointer_motion_relative(
        &self,
        dx: f64,
        dy: f64,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyPointerMotionRelative",
                &(dx, dy),
            )
            .await
            .map_err(|e| Error::process_with("NotifyPointerMotionRelative", e))?;
        Ok(())
    }

    async fn pointer_motion_absolute(
        &self,
        x: f64,
        y: f64,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        let stream = self
            .state
            .active_stream_path
            .lock()
            .map_err(|_| Error::process("active_stream_path mutex poisoned"))?
            .clone()
            .ok_or_else(|| {
                Error::process("no active ScreenCast stream; absolute pointer motion needs one")
            })?;
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyPointerMotionAbsolute",
                &(stream.as_str(), x, y),
            )
            .await
            .map_err(|e| Error::process_with("NotifyPointerMotionAbsolute", e))?;
        Ok(())
    }

    async fn pointer_button_down(&self, button: u32, _cancel: &CancellationToken) -> Result<()> {
        let button: i32 = button
            .try_into()
            .map_err(|_| Error::process(format!("button code {button} exceeds i32::MAX")))?;
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyPointerButton",
                &(button, true),
            )
            .await
            .map_err(|e| Error::process_with("NotifyPointerButton press", e))?;
        Ok(())
    }

    async fn pointer_button_up(&self, button: u32, cancel: &CancellationToken) -> Result<()> {
        let button: i32 = button
            .try_into()
            .map_err(|_| Error::process(format!("button code {button} exceeds i32::MAX")))?;
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyPointerButton",
                &(button, false),
            )
            .await
            .map_err(|e| Error::process_with("NotifyPointerButton release", e))?;
        // Tail throttle — see press_keysym.
        cancellable_tail(Duration::from_millis(30), cancel).await;
        Ok(())
    }

    async fn pointer_axis_discrete(
        &self,
        axis: u32,
        steps: i32,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.state
            .conn
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path.as_str(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                "NotifyPointerAxisDiscrete",
                &(axis, steps),
            )
            .await
            .map_err(|e| Error::process_with("NotifyPointerAxisDiscrete", e))?;
        // Give GTK a beat to process the wheel event before the next call.
        // Same rationale as the tail throttle in press_keysym —
        // back-to-back axis events from a scroll loop can otherwise
        // stack up faster than the compositor delivers them.
        cancellable_tail(Duration::from_millis(30), cancel).await;
        Ok(())
    }
}
