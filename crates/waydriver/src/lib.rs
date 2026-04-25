//! Headless GUI testing for Wayland applications.
//!
//! WayDriver launches apps in isolated compositor sessions, interacts with them
//! via AT-SPI accessibility APIs, and captures screenshots via PipeWire.
//!
//! The library is backend-agnostic: three traits ([`CompositorRuntime`],
//! [`InputBackend`], [`CaptureBackend`]) define the interface, and concrete
//! implementations live in separate crates (e.g. `waydriver-compositor-mutter`).

/// AT-SPI accessibility tree inspection and interaction.
pub mod atspi;
/// Backend trait definitions for compositors, input, and capture.
pub mod backend;
/// GStreamer-based PipeWire frame capture helpers.
pub mod capture;
/// Error types used throughout the crate.
pub mod error;
/// X11 keysym utilities for keyboard input.
pub mod keysym;
/// XPath-based lazy locators over the AT-SPI tree.
pub mod locator;
/// Test session lifecycle management.
pub mod session;

pub use atspi::Rect;
pub use backend::{
    CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream, PointerAxis, PointerButton,
    StreamToken,
};
pub use error::{Error, Result};
pub use locator::{FillMode, Locator, SelectBy};
pub use session::{Session, SessionConfig};
