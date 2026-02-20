use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum AppError {
    #[error("Signal API error: {0}")]
    Signal(String),
    #[error("Claude error: {0}")]
    Claude(String),
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

impl From<String> for AppError {
    fn from(s: String) -> Self {
        AppError::Other(s)
    }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        AppError::Other(s.to_string())
    }
}

impl From<std::string::FromUtf8Error> for AppError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        AppError::Other(e.to_string())
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for AppError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        AppError::Other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_string() {
        let err: AppError = "something broke".to_string().into();
        assert!(matches!(err, AppError::Other(_)));
        assert_eq!(err.to_string(), "something broke");
    }

    #[test]
    fn test_from_str() {
        let err: AppError = "bad input".into();
        assert!(matches!(err, AppError::Other(_)));
        assert_eq!(err.to_string(), "bad input");
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err: AppError = io_err.into();
        assert!(matches!(err, AppError::Io(_)));
        assert!(err.to_string().contains("file gone"));
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json{").unwrap_err();
        let err: AppError = json_err.into();
        assert!(matches!(err, AppError::Json(_)));
        assert!(err.to_string().contains("JSON error"));
    }

    #[test]
    fn test_from_utf8_error() {
        let bytes = vec![0, 159, 146, 150]; // invalid UTF-8
        let utf8_err = String::from_utf8(bytes).unwrap_err();
        let err: AppError = utf8_err.into();
        assert!(matches!(err, AppError::Other(_)));
    }

    #[test]
    fn test_display_signal_variant() {
        let err = AppError::Signal("ws closed".to_string());
        assert_eq!(err.to_string(), "Signal API error: ws closed");
    }

    #[test]
    fn test_display_claude_variant() {
        let err = AppError::Claude("rate limited".to_string());
        assert_eq!(err.to_string(), "Claude error: rate limited");
    }
}
