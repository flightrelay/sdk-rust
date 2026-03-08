//! WebSocket server for accepting FRP controller connections.
//!
//! Requires the `server` feature. Provides a synchronous, caller-driven API.
//!
//! ```no_run
//! use flightrelay::FrpListener;
//!
//! // Bind to the default FRP port (clients connect to ws://host:5880/frp)
//! let listener = FrpListener::bind("0.0.0.0:5880", &["0.1.0"]).unwrap();
//! let mut conn = listener.accept().unwrap();
//! println!("Controller connected: version={}", conn.version());
//! ```

use std::fmt;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};

use tungstenite::protocol::WebSocket;
use tungstenite::Message;

use crate::error::FrpError;
use crate::message::{FrpMessage, FrpProtocolMessage, Severity};

/// Listens for incoming FRP controller connections.
pub struct FrpListener {
    listener: TcpListener,
    supported_versions: Vec<String>,
}

impl FrpListener {
    /// Bind to the given address and listen for FRP connections.
    ///
    /// `supported_versions` are the FRP versions this device supports.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind.
    pub fn bind(
        addr: impl ToSocketAddrs,
        supported_versions: &[&str],
    ) -> Result<Self, FrpError> {
        let listener = TcpListener::bind(addr)
            .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e))))?;
        Ok(Self {
            listener,
            supported_versions: supported_versions.iter().map(|&s| s.to_owned()).collect(),
        })
    }

    /// Accept a single incoming connection and perform the FRP handshake.
    ///
    /// Blocks until a controller connects. Receives `start`, sends `init` with
    /// the highest mutually supported version.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or handshake fails.
    pub fn accept(&self) -> Result<FrpConnection, FrpError> {
        let (stream, _addr) = self
            .listener
            .accept()
            .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e))))?;
        let mut socket = tungstenite::accept(stream).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(e) => FrpError::WebSocket(Box::new(e)),
            tungstenite::HandshakeError::Interrupted(_) => {
                FrpError::Handshake("WebSocket handshake interrupted".into())
            }
        })?;

        // Wait for start message
        let (client_versions, client_name) = loop {
            match socket.read()? {
                Message::Text(text) => {
                    if let Ok(FrpMessage::Protocol(FrpProtocolMessage::Start {
                        version,
                        name,
                    })) = FrpMessage::parse(&text)
                    {
                        break (version, name);
                    }
                }
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        };

        // Select highest mutually supported version
        let selected = select_version(&self.supported_versions, &client_versions);
        if let Some(version) = selected {
            let init = FrpProtocolMessage::Init {
                version: version.clone(),
            };
            let json = serde_json::to_string(&init)?;
            socket.send(Message::text(json))?;

            Ok(FrpConnection {
                socket,
                version,
                client_name,
            })
        } else {
            let alert = FrpProtocolMessage::Alert {
                severity: Severity::Critical,
                message: "No compatible FRP version".into(),
            };
            let json = serde_json::to_string(&alert)?;
            socket.send(Message::text(json))?;
            socket.close(None)?;
            Err(FrpError::Handshake("No compatible FRP version".into()))
        }
    }
}

impl fmt::Debug for FrpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrpListener")
            .field("supported_versions", &self.supported_versions)
            .finish_non_exhaustive()
    }
}

/// An established FRP connection from a controller.
pub struct FrpConnection {
    socket: WebSocket<TcpStream>,
    version: String,
    client_name: Option<String>,
}

impl FrpConnection {
    /// The FRP version negotiated during the handshake.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The client name from the `start` message, if provided.
    #[must_use]
    pub fn client_name(&self) -> Option<&str> {
        self.client_name.as_deref()
    }

    /// Block until the next [`FrpMessage`] arrives.
    ///
    /// # Errors
    ///
    /// Returns an error on connection failure or invalid JSON.
    pub fn recv(&mut self) -> Result<FrpMessage, FrpError> {
        loop {
            match self.socket.read()? {
                Message::Text(text) => return FrpMessage::parse(&text),
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        }
    }

    /// Poll for the next message without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error on connection failure or invalid JSON.
    pub fn try_recv(&mut self) -> Result<Option<FrpMessage>, FrpError> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(Some(FrpMessage::parse(&text)?)),
                Ok(Message::Close(_)) => return Err(FrpError::Closed),
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Send an FRP message to the controller.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send(&mut self, msg: &FrpMessage) -> Result<(), FrpError> {
        let json = msg.to_json()?;
        self.socket.send(Message::text(json))?;
        Ok(())
    }

    /// Send an FRP envelope to the controller.
    ///
    /// # Errors
    ///
    /// Returns an error if the envelope cannot be serialized or sent.
    pub fn send_envelope(&mut self, env: &crate::FrpEnvelope) -> Result<(), FrpError> {
        let json = serde_json::to_string(env)?;
        self.socket.send(Message::text(json))?;
        Ok(())
    }

    /// Set the underlying TCP stream to non-blocking mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP stream mode cannot be set.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), FrpError> {
        self.socket
            .get_ref()
            .set_nonblocking(nonblocking)
            .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e))))
    }

    /// Cleanly close the WebSocket connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the close handshake fails.
    pub fn close(mut self) -> Result<(), FrpError> {
        self.socket.close(None)?;
        loop {
            match self.socket.read() {
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                    return Ok(());
                }
                Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(e.into()),
                _ => {}
            }
        }
    }
}

impl fmt::Debug for FrpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrpConnection")
            .field("version", &self.version)
            .field("client_name", &self.client_name)
            .finish_non_exhaustive()
    }
}

/// Select the highest version present in both lists.
fn select_version(server: &[String], client: &[String]) -> Option<String> {
    // Simple: find the first client version that the server supports,
    // assuming client lists versions in preference order (highest first).
    for cv in client {
        if server.contains(cv) {
            return Some(cv.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_selection() {
        let server = vec!["0.1.0".to_owned(), "0.2.0".to_owned()];
        let client = vec!["0.2.0".to_owned(), "0.1.0".to_owned()];
        assert_eq!(select_version(&server, &client), Some("0.2.0".to_owned()));
    }

    #[test]
    fn version_selection_no_match() {
        let server = vec!["0.1.0".to_owned()];
        let client = vec!["0.2.0".to_owned()];
        assert_eq!(select_version(&server, &client), None);
    }
}
