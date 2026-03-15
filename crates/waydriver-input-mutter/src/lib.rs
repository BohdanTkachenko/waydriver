//! Mutter implementation of [`waydriver::InputBackend`].
//!
//! Wraps an `Arc<MutterState>` obtained from [`waydriver_compositor_mutter::MutterCompositor::state`]
//! and sends keyboard / pointer events via
//! `org.gnome.Mutter.RemoteDesktop.Session.{NotifyKeyboardKeysym, NotifyPointerMotionRelative}`.

use std::sync::Arc;

use async_trait::async_trait;

use waydriver::{Error, InputBackend, Result};
use waydriver_compositor_mutter::MutterState;

pub struct MutterInput {
    state: Arc<MutterState>,
}

impl MutterInput {
    pub fn new(state: Arc<MutterState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl InputBackend for MutterInput {
    async fn press_keysym(&self, keysym: u32) -> Result<()> {
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
            .map_err(|e| Error::Process(format!("NotifyKeyboardKeysym press: {e}")))?;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
            .map_err(|e| Error::Process(format!("NotifyKeyboardKeysym release: {e}")))?;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        Ok(())
    }

    async fn pointer_motion_relative(&self, dx: f64, dy: f64) -> Result<()> {
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
            .map_err(|e| Error::Process(format!("NotifyPointerMotionRelative: {e}")))?;
        Ok(())
    }

    async fn pointer_button(&self, button: u32) -> Result<()> {
        let button: i32 = button
            .try_into()
            .map_err(|_| Error::Process(format!("button code {button} exceeds i32::MAX")))?;
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
            .map_err(|e| Error::Process(format!("NotifyPointerButton press: {e}")))?;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
            .map_err(|e| Error::Process(format!("NotifyPointerButton release: {e}")))?;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        Ok(())
    }
}
