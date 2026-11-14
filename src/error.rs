//! FRP error types.

/// Errors from FRP message parsing and transport.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    WebSocket(WebSocketError),
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

/// A failure of the WebSocket layer under an FRP connection: an I/O error,
/// a protocol violation by the peer, or a full write buffer. Its text says
/// which.
#[cfg(feature = "transport")]
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct WebSocketError(Box<tungstenite::Error>);

#[cfg(feature = "transport")]
impl WebSocketError {
    /// Whether the connection's write buffer limit was reached: the peer
    /// reads too slowly.
    pub(crate) fn is_write_buffer_full(&self) -> bool {
        matches!(*self.0, tungstenite::Error::WriteBufferFull(_))
    }
}

/// Wraps a WebSocket failure as an [`FrpError`].
#[cfg(feature = "transport")]
pub(crate) fn websocket(e: tungstenite::Error) -> FrpError {
    FrpError::WebSocket(WebSocketError(Box::new(e)))
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

    #[cfg(feature = "transport")]
    #[test]
    fn a_full_write_buffer_is_told_apart() {
        let full = super::websocket(tungstenite::Error::WriteBufferFull(
            tungstenite::Message::text("x"),
        ));
        assert!(matches!(&full, FrpError::WebSocket(e) if e.is_write_buffer_full()));
        let closed = super::websocket(tungstenite::Error::ConnectionClosed);
        assert!(matches!(&closed, FrpError::WebSocket(e) if !e.is_write_buffer_full()));
    }
}
