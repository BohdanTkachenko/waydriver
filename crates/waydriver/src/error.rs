use thiserror::Error;

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

    #[error("AT-SPI: {0}")]
    Atspi(String),

    #[error("D-Bus: {0}")]
    Zbus(#[from] zbus::Error),

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("process: {0}")]
    Process(String),

    #[error("screenshot: {0}")]
    Screenshot(String),
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
            Error::Atspi("registry unavailable".to_string()).to_string(),
            "AT-SPI: registry unavailable"
        );
        assert_eq!(
            Error::Timeout("socket did not appear".to_string()).to_string(),
            "timeout: socket did not appear"
        );
        assert_eq!(
            Error::Process("dbus-launch failed".to_string()).to_string(),
            "process: dbus-launch failed"
        );
        assert_eq!(
            Error::Screenshot("capture failed".to_string()).to_string(),
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
}
