//! Crate-local typed error for mutter-compositor failure modes.
//!
//! Every fallible step inside this crate's startup, shutdown, and helper
//! routines uses [`MutterError`] internally. The [`From<MutterError>`]
//! impl for [`waydriver::Error`] is the single boundary at which we
//! string-format for the public API — so each call site stays typed
//! while the surface remains the workspace's shared error type.
//!
//! Why bother:
//!
//! - **No string drift between sites.** Previous code wrote literals
//!   like `"invalid mutter dbus address"` and `"connect to mutter dbus"`
//!   inline at each `map_err`; renaming a step risked silent
//!   inconsistency between the message and any test or log that
//!   referred to it.
//! - **The underlying error stays typed.** Variants carry a
//!   `#[source] zbus::Error` (or similar) where applicable, so callers
//!   walking [`std::error::Error::source`] can downcast to the real
//!   cause instead of parsing a `Display` string.
//! - **Failure modes are documented in one place.** This enum is the
//!   exhaustive list of things that can go wrong starting/stopping
//!   mutter; reviewers and future authors don't need to grep for
//!   `Error::process` calls to find them.

use thiserror::Error;

/// All failure modes for `MutterCompositor`.
///
/// `From<MutterError> for waydriver::Error` is implemented at the bottom
/// of this module — it's the only boundary where this enum becomes a
/// stringly-typed value.
#[derive(Debug, Error)]
pub(crate) enum MutterError {
    #[error("dbus-launch failed: {0}")]
    DbusLaunchFailed(String),

    /// `parse_dbus_address` / `parse_dbus_pid` couldn't find their key
    /// in the `dbus-launch --sh-syntax` output. `field` names the key
    /// (e.g. `"DBUS_SESSION_BUS_ADDRESS"`).
    #[error("could not parse {field} from dbus-launch output")]
    DbusOutputMissingField { field: &'static str },

    #[error("invalid dbus PID in dbus-launch output")]
    DbusPidParse(#[source] std::num::ParseIntError),

    #[error("invalid mutter D-Bus address {addr:?}")]
    DbusAddressInvalid {
        addr: String,
        #[source]
        source: zbus::Error,
    },

    /// Connection-construction failure on the mutter private bus.
    /// `stage` distinguishes the two adjacent steps that can fail
    /// with the same `zbus::Error` kind:
    /// - `"build connection builder"` — `Builder::address(...)`
    ///   couldn't accept the address (malformed / unsupported
    ///   transport).
    /// - `"connect"` — `Builder::build().await` couldn't reach the
    ///   bus (handshake / auth / socket).
    /// Carrying the stage avoids needing two near-identical
    /// variants while keeping the failure point identifiable in
    /// logs and `source` walks.
    #[error("mutter D-Bus: {stage}")]
    DbusConnect {
        stage: &'static str,
        #[source]
        source: zbus::Error,
    },

    /// RemoteDesktop.CreateSession finally failed after the retry loop
    /// gave up; the source is the *last* zbus error, not all of them.
    #[error("RemoteDesktop.CreateSession")]
    RemoteDesktopCreate(#[source] zbus::Error),

    #[error("parse RemoteDesktop session path")]
    RdSessionPathParse(#[source] zbus::Error),

    #[error("Get SessionId property")]
    SessionIdGet(#[source] zbus::Error),

    #[error("parse SessionId variant")]
    SessionIdVariantParse(#[source] zbus::Error),

    #[error("SessionId is not a string")]
    SessionIdNotString(#[source] zbus::zvariant::Error),

    #[error("invalid resolution {value:?}: expected WIDTHxHEIGHT")]
    ResolutionInvalid { value: String },

    #[error("spawning {process}")]
    Spawn {
        process: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// Used for create_dir_all / fs failures inside `start`. `From<io>`
    /// keeps the `?`-on-`std::io::Result` ergonomic.
    #[error("io")]
    Io(#[from] std::io::Error),

    /// Wayland socket didn't appear within the polling window. The
    /// `From<MutterError> for waydriver::Error` impl maps this to the
    /// shared `Error::Timeout` variant so existing
    /// `matches!(_, Error::Timeout(_))` callers / tests continue to
    /// match.
    #[error("wayland socket {socket} did not appear within 5s")]
    WaylandSocketTimeout { socket: String },

    /// PipeWire's per-session socket didn't appear within the polling
    /// window. Surfaced as `Error::Timeout` for the same reason
    /// `WaylandSocketTimeout` is — startup-stage timeouts share a
    /// public bucket.
    #[error("pipewire socket {socket} did not appear within 5s")]
    PipewireSocketTimeout { socket: String },
}

impl From<MutterError> for waydriver::Error {
    fn from(e: MutterError) -> Self {
        match e {
            // Preserve the public Timeout variant so callers can match
            // on it (the e2e tests in this workspace already do).
            MutterError::WaylandSocketTimeout { ref socket } => waydriver::Error::Timeout(format!(
                "wayland socket {socket} did not appear within 5s"
            )),
            MutterError::PipewireSocketTimeout { ref socket } => waydriver::Error::Timeout(
                format!("pipewire socket {socket} did not appear within 5s"),
            ),
            // Plain I/O surfaces as the shared Io variant — matching
            // the behaviour of the previous `Error::from(io::Error)`
            // that the parsers used implicitly.
            MutterError::Io(io) => waydriver::Error::Io(io),
            // Everything else: render through `process_with`. The
            // typed error becomes the boxed `source`, so anything
            // walking `std::error::Error::source()` can still downcast
            // to `MutterError` and pattern-match.
            other => waydriver::Error::process_with("mutter compositor", other),
        }
    }
}
