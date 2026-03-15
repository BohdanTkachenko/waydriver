use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("element not found: {0}")]
    ElementNotFound(String),

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

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        assert_eq!(
            Error::ElementNotFound("button".to_string()).to_string(),
            "element not found: button"
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
