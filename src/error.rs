//! FRP error types.

/// Errors from FRP message parsing and transport.
#[derive(Debug, thiserror::Error)]
pub enum FrpError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("handshake failed: {0}")]
    Handshake(String),

    #[error("connection closed")]
    Closed,

    #[cfg(any(feature = "client", feature = "server"))]
    #[error("websocket: {0}")]
    WebSocket(Box<tungstenite::Error>),
}

#[cfg(any(feature = "client", feature = "server"))]
impl From<tungstenite::Error> for FrpError {
    fn from(e: tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(e))
    }
}
