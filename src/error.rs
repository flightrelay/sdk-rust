//! FRP error types.

/// Errors from FRP message parsing and transport.
#[derive(Debug, thiserror::Error)]
pub enum FrpError {
    /// Text that is not an FRP message: not JSON, or not an object with a
    /// string `kind` and `from`. About that one message only.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// A connection could not be set up: a URL that can't be dialed, or a
    /// WebSocket upgrade that failed or timed out.
    #[error("handshake failed: {0}")]
    Handshake(String),

    /// The connection closed.
    #[error("connection closed")]
    Closed,

    /// The WebSocket failed.
    #[cfg(feature = "transport")]
    #[error("websocket: {0}")]
    WebSocket(Box<tungstenite::Error>),
}

impl FrpError {
    /// Whether the connection is unusable after this error. `false` only
    /// for [`Json`](Self::Json), which is about one message: a receive loop
    /// can log it and keep reading.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        !matches!(self, Self::Json(_))
    }
}

#[cfg(feature = "transport")]
impl From<tungstenite::Error> for FrpError {
    fn from(e: tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::FrpError;

    #[test]
    fn only_a_bad_message_is_not_fatal() {
        let json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(!FrpError::Json(json).is_fatal());
        assert!(FrpError::Closed.is_fatal());
        assert!(FrpError::Handshake("refused".into()).is_fatal());
    }
}
