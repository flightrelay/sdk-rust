//! WebSocket client for connecting to an FRP device.
//!
//! Requires the `client` feature. Provides a synchronous, caller-driven API
//! with both blocking and non-blocking receive modes.
//!
//! # Blocking
//!
//! ```no_run
//! use flightrelay::FrpClient;
//!
//! let mut client = FrpClient::connect("ws://192.168.1.50:5880", "my-app", &["0.1.0"]).unwrap();
//! loop {
//!     match client.recv() {
//!         Ok(msg) => println!("{msg:?}"),
//!         Err(e) => { eprintln!("{e}"); break; }
//!     }
//! }
//! ```
//!
//! # Non-blocking
//!
//! ```no_run
//! use flightrelay::FrpClient;
//!
//! let mut client = FrpClient::connect("ws://192.168.1.50:5880", "my-sim", &["0.1.0"]).unwrap();
//! client.set_nonblocking(true).unwrap();
//!
//! loop {
//!     while let Ok(Some(msg)) = client.try_recv() {
//!         println!("{msg:?}");
//!     }
//!     // ... render frame, etc.
//! }
//! ```

use std::fmt;
use std::net::TcpStream;

use tungstenite::protocol::WebSocket;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use crate::error::FrpError;
use crate::message::{FrpMessage, FrpProtocolMessage};

/// A synchronous WebSocket client connected to an FRP device.
///
/// After [`connect`](Self::connect), the handshake is complete and the client
/// is ready to receive events.
pub struct FrpClient {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    version: String,
}

impl FrpClient {
    /// Connect to an FRP device and complete the `start`/`init` handshake.
    ///
    /// Sends `start` with the given `versions` and `name`, waits for `init`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails, the handshake fails, or no
    /// compatible version is found.
    pub fn connect(url: &str, name: &str, versions: &[&str]) -> Result<Self, FrpError> {
        let (mut socket, _response) = tungstenite::connect(url)?;

        let start = FrpProtocolMessage::Start {
            version: versions.iter().map(|&s| s.to_owned()).collect(),
            name: Some(name.to_owned()),
        };
        let json = serde_json::to_string(&start)?;
        socket.send(Message::text(json))?;

        // Wait for init response
        let version = loop {
            match socket.read()? {
                Message::Text(text) => {
                    if let Ok(FrpMessage::Protocol(FrpProtocolMessage::Init { version })) =
                        FrpMessage::parse(&text)
                    {
                        break version;
                    }
                    // Check for critical alert
                    if let Ok(FrpMessage::Protocol(FrpProtocolMessage::Alert {
                        severity: crate::Severity::Critical,
                        message,
                    })) = FrpMessage::parse(&text)
                    {
                        return Err(FrpError::Handshake(message));
                    }
                }
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        };

        Ok(Self { socket, version })
    }

    /// The FRP version negotiated during the handshake.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Set the underlying TCP stream to non-blocking mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP stream mode cannot be set.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), FrpError> {
        match self.socket.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp
                .set_nonblocking(nonblocking)
                .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e)))),
            _ => Ok(()),
        }
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
    /// Returns `Ok(None)` when no message is immediately available (requires
    /// [`set_nonblocking(true)`](Self::set_nonblocking)).
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

    /// Send an FRP message to the device.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send(&mut self, msg: &FrpMessage) -> Result<(), FrpError> {
        let json = msg.to_json()?;
        self.socket.send(Message::text(json))?;
        Ok(())
    }

    /// Send a raw FRP protocol message to the device.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send_protocol(&mut self, msg: &FrpProtocolMessage) -> Result<(), FrpError> {
        let json = serde_json::to_string(msg)?;
        self.socket.send(Message::text(json))?;
        Ok(())
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

impl fmt::Debug for FrpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrpClient")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
