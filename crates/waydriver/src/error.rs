use thiserror::Error;

/// Boxed underlying error preserved on infrastructure-failure variants
/// (`Atspi`, `Process`, `Screenshot`) so callers can walk
/// [`std::error::Error::source`] and downcast to concrete types
/// (e.g. `zbus::Error`, `gstreamer::glib::Error`) when needed.
type BoxSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors that can occur during a waydriver session.
#[derive(Debug, Error)]
pub enum Error {
    #[error("element not found for selector: {xpath}")]
    ElementNotFound { xpath: String },

    #[error("selector matched {count} elements (expected exactly one): {xpath}")]
    AmbiguousSelector { xpath: String, count: usize },

    #[error("invalid selector '{xpath}': {reason}")]
    InvalidSelector { xpath: String, reason: String },

    #[error("element went stale during action (selector: {xpath}, bus: {bus}, path: {path})")]
    ElementStale {
        xpath: String,
        bus: String,
        path: String,
    },

    /// AT-SPI introspection / action failure.
    ///
    /// `message` carries human-readable context (`"<operation>: <details>"`);
    /// `source` carries the typed underlying error (commonly `zbus::Error`)
    /// when one exists, so callers can downcast via
    /// [`std::error::Error::source`]. Construct via [`Error::atspi`] /
    /// [`Error::atspi_with`].
    #[error("AT-SPI: {message}")]
    Atspi {
        message: String,
        #[source]
        source: Option<BoxSource>,
    },

    #[error("D-Bus: {0}")]
    Zbus(#[from] zbus::Error),

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout: {0}")]
    Timeout(String),

    /// Subprocess / IPC / mutex-poisoning failure. See [`Error::Atspi`] for
    /// the field semantics; construct via [`Error::process`] /
    /// [`Error::process_with`].
    #[error("process: {message}")]
    Process {
        message: String,
        #[source]
        source: Option<BoxSource>,
    },

    /// Screenshot / video-capture failure. See [`Error::Atspi`] for the
    /// field semantics; construct via [`Error::screenshot`] /
    /// [`Error::screenshot_with`].
    #[error("screenshot: {message}")]
    Screenshot {
        message: String,
        #[source]
        source: Option<BoxSource>,
    },
}

impl Error {
    /// AT-SPI failure with a free-form message and no underlying source.
    pub fn atspi(message: impl Into<String>) -> Self {
        Error::Atspi {
            message: message.into(),
            source: None,
        }
    }

    /// AT-SPI failure caused by an underlying error. The `Display` of
    /// `source` is appended to `operation` so the rendered message stays
    /// `"AT-SPI: <operation>: <source>"`, and the typed source is preserved
    /// for [`std::error::Error::source`].
    pub fn atspi_with<E>(operation: impl std::fmt::Display, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error::Atspi {
            message: format!("{operation}: {source}"),
            source: Some(Box::new(source)),
        }
    }

    /// Process / IPC failure with a free-form message and no source.
    pub fn process(message: impl Into<String>) -> Self {
        Error::Process {
            message: message.into(),
            source: None,
        }
    }

    /// Process / IPC failure caused by an underlying error. See
    /// [`Error::atspi_with`] for the message-formatting rules.
    pub fn process_with<E>(operation: impl std::fmt::Display, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error::Process {
            message: format!("{operation}: {source}"),
            source: Some(Box::new(source)),
        }
    }

    /// Screenshot / capture failure with a free-form message and no source.
    pub fn screenshot(message: impl Into<String>) -> Self {
        Error::Screenshot {
            message: message.into(),
            source: None,
        }
    }

    /// Screenshot / capture failure caused by an underlying error. See
    /// [`Error::atspi_with`] for the message-formatting rules.
    pub fn screenshot_with<E>(operation: impl std::fmt::Display, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error::Screenshot {
            message: format!("{operation}: {source}"),
            source: Some(Box::new(source)),
        }
    }
}

/// Convenience alias for `std::result::Result<T, waydriver::Error>`.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        assert_eq!(
            Error::ElementNotFound {
                xpath: "//PushButton[@name='OK']".into()
            }
            .to_string(),
            "element not found for selector: //PushButton[@name='OK']"
        );
        assert_eq!(
            Error::AmbiguousSelector {
                xpath: "//PushButton".into(),
                count: 12,
            }
            .to_string(),
            "selector matched 12 elements (expected exactly one): //PushButton"
        );
        assert_eq!(
            Error::InvalidSelector {
                xpath: "//[".into(),
                reason: "unexpected token".into(),
            }
            .to_string(),
            "invalid selector '//[': unexpected token"
        );
        assert_eq!(
            Error::atspi("registry unavailable").to_string(),
            "AT-SPI: registry unavailable"
        );
        assert_eq!(
            Error::Timeout("socket did not appear".to_string()).to_string(),
            "timeout: socket did not appear"
        );
        assert_eq!(
            Error::process("dbus-launch failed").to_string(),
            "process: dbus-launch failed"
        );
        assert_eq!(
            Error::screenshot("capture failed").to_string(),
            "screenshot: capture failed"
        );
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = Error::from(io_err);
        assert!(err.to_string().contains("IO:"));
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn with_constructor_appends_source_to_message() {
        let io_err = std::io::Error::other("boom");
        let err = Error::screenshot_with("CreateSession", io_err);
        assert_eq!(err.to_string(), "screenshot: CreateSession: boom");
    }

    #[test]
    fn with_constructor_preserves_typed_source() {
        use std::error::Error as _;
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let err = Error::process_with("spawn", io_err);
        let src = err.source().expect("source should be present");
        let downcast = src
            .downcast_ref::<std::io::Error>()
            .expect("source should downcast to io::Error");
        assert_eq!(downcast.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn sourceless_constructor_has_no_source() {
        use std::error::Error as _;
        let err = Error::atspi("registry gone");
        assert!(err.source().is_none());
    }
}
