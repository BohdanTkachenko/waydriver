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
use waydriver::{Error, InputBackend, PointerAxis, PointerButton, Result};
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

    /// Issue a method call on `org.gnome.Mutter.RemoteDesktop.Session`
    /// at the active session path, mapping any `zbus::Error` into a
    /// `waydriver::Error::Process` tagged with `op` so the chain
    /// reads "process: <op>: <zbus error>".
    ///
    /// Every input method (`NotifyKeyboardKeysym`,
    /// `NotifyPointerButton`, `NotifyPointerMotionRelative/Absolute`,
    /// `NotifyPointerAxisDiscrete`) is structurally identical apart
    /// from the method name and the argument tuple — without this
    /// helper each one repeated the same five-line `call_method`
    /// invocation with the same destination/path/interface triple
    /// hard-coded inline. Centralising means a future change to the
    /// D-Bus API only edits one place, and the mapping from
    /// "operation name" to error context is no longer scattered as
    /// free-form literals.
    async fn call_rd_session<Args>(
        &self,
        method: &'static str,
        args: &Args,
        op: &'static str,
    ) -> Result<zbus::Message>
    where
        Args: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.state
            .conn()
            .call_method(
                Some("org.gnome.Mutter.RemoteDesktop"),
                self.state.rd_session_path(),
                Some("org.gnome.Mutter.RemoteDesktop.Session"),
                method,
                args,
            )
            .await
            .map_err(|e| Error::process_with(op, e))
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
        self.call_rd_session(
            "NotifyKeyboardKeysym",
            &(keysym, true),
            "NotifyKeyboardKeysym press",
        )
        .await?;
        Ok(())
    }

    async fn key_up(&self, keysym: u32, _cancel: &CancellationToken) -> Result<()> {
        self.call_rd_session(
            "NotifyKeyboardKeysym",
            &(keysym, false),
            "NotifyKeyboardKeysym release",
        )
        .await?;
        Ok(())
    }

    async fn pointer_motion_relative(
        &self,
        dx: f64,
        dy: f64,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        self.call_rd_session(
            "NotifyPointerMotionRelative",
            &(dx, dy),
            "NotifyPointerMotionRelative",
        )
        .await?;
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
            .active_stream_path_lock()?
            .clone()
            .ok_or_else(|| {
                Error::process("no active ScreenCast stream; absolute pointer motion needs one")
            })?;
        self.call_rd_session(
            "NotifyPointerMotionAbsolute",
            &(stream.as_str(), x, y),
            "NotifyPointerMotionAbsolute",
        )
        .await?;
        Ok(())
    }

    async fn pointer_button_down(
        &self,
        button: PointerButton,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        // Mutter's `NotifyPointerButton` takes the evdev code as `i32`.
        // Named variants are <i32::MAX, but `PointerButton::Other(u32)`
        // accepts the full `u32` range, so a fallible `try_from` is the
        // only safe conversion at the boundary — `as i32` would silently
        // wrap on values past `i32::MAX`.
        let button = i32::try_from(button.evdev_code())
            .map_err(|e| Error::process_with("NotifyPointerButton press", e))?;
        self.call_rd_session(
            "NotifyPointerButton",
            &(button, true),
            "NotifyPointerButton press",
        )
        .await?;
        Ok(())
    }

    async fn pointer_button_up(
        &self,
        button: PointerButton,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let button = i32::try_from(button.evdev_code())
            .map_err(|e| Error::process_with("NotifyPointerButton release", e))?;
        self.call_rd_session(
            "NotifyPointerButton",
            &(button, false),
            "NotifyPointerButton release",
        )
        .await?;
        // Tail throttle — see press_keysym.
        cancellable_tail(Duration::from_millis(30), cancel).await;
        Ok(())
    }

    async fn pointer_axis_discrete(
        &self,
        axis: PointerAxis,
        steps: i32,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Mutter's `NotifyPointerAxisDiscrete` takes 0=vertical,
        // 1=horizontal as a `u32`. This translation is the entire
        // reason the trait surface is an enum: a future KWin/Sway
        // backend can route differently here without changing the
        // trait callers.
        let axis_code: u32 = match axis {
            PointerAxis::Vertical => 0,
            PointerAxis::Horizontal => 1,
        };
        self.call_rd_session(
            "NotifyPointerAxisDiscrete",
            &(axis_code, steps),
            "NotifyPointerAxisDiscrete",
        )
        .await?;
        // Give GTK a beat to process the wheel event before the next call.
        // Same rationale as the tail throttle in press_keysym —
        // back-to-back axis events from a scroll loop can otherwise
        // stack up faster than the compositor delivers them.
        cancellable_tail(Duration::from_millis(30), cancel).await;
        Ok(())
    }
}
