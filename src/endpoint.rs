//! Role- and direction-agnostic FRP transport.
//!
//! Requires the `transport` feature.
//!
//! FRP roles are entity names, not directions: a **device** emits shot data and
//! accepts commands, a **controller** receives shot data and sends commands.
//! Either role may open the WebSocket or accept it. [`FrpEndpoint`] separates
//! these two axes — [`Role`] decides the handshake direction, [`Transport`]
//! decides who opens the socket.
//!
//! # Device serving controllers
//!
//! ```no_run
//! use flightrelay::{EndpointConfig, FrpEndpoint, Role, Transport};
//!
//! let mut ep = FrpEndpoint::open(
//!     EndpointConfig::new(Role::Device, Transport::listen("0.0.0.0:5880")),
//! )?;
//! let mut conn = ep.establish()?;
//! println!("controller connected: {:?}", conn.peer_name());
//! # Ok::<(), flightrelay::FrpError>(())
//! ```
//!
//! # Device bridging out to a central controller
//!
//! ```no_run
//! use flightrelay::{EndpointConfig, FrpEndpoint, Role, Transport};
//!
//! let mut ep = FrpEndpoint::open(
//!     EndpointConfig::new(Role::Device, Transport::connect("ws://flighthook:5880/frp"))
//!         .with_name("ironsight"),
//! )?;
//! let mut conn = ep.establish()?;
//! # Ok::<(), flightrelay::FrpError>(())
//! ```

use std::fmt;
use std::net::{TcpListener, TcpStream};

use tungstenite::Message;
use tungstenite::protocol::WebSocket;
use tungstenite::stream::MaybeTlsStream;

use crate::error::FrpError;
use crate::message::{FrpEnvelope, FrpMessage, FrpProtocolMessage, Severity};

/// The FRP role this endpoint plays.
///
/// Roles are independent of transport direction — either role may open or
/// accept the WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Emits shot data and accepts commands. Answers `start` with `init`.
    Device,
    /// Receives shot data and sends commands. Initiates the `start` handshake.
    Controller,
}

impl Role {
    /// The role of the peer on the other end of the connection.
    #[must_use]
    pub fn peer(self) -> Self {
        match self {
            Self::Device => Self::Controller,
            Self::Controller => Self::Device,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device => f.write_str("device"),
            Self::Controller => f.write_str("controller"),
        }
    }
}

/// How this endpoint obtains a WebSocket connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// Bind a TCP listener and accept incoming connections.
    Listen(String),
    /// Dial a remote FRP endpoint.
    Connect(String),
}

impl Transport {
    /// Accept incoming connections on `addr` (e.g. `"0.0.0.0:5880"`).
    #[must_use]
    pub fn listen(addr: impl Into<String>) -> Self {
        Self::Listen(addr.into())
    }

    /// Dial `url` (e.g. `"ws://host:5880/frp"`).
    #[must_use]
    pub fn connect(url: impl Into<String>) -> Self {
        Self::Connect(url.into())
    }
}

/// Configuration for an [`FrpEndpoint`].
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// The FRP role this endpoint plays.
    pub role: Role,
    /// How this endpoint obtains a connection.
    pub transport: Transport,
    /// Human-readable identifier sent in `start`. Only used by [`Role::Controller`].
    pub name: String,
    /// FRP versions this endpoint supports, highest preference first.
    pub versions: Vec<String>,
}

impl EndpointConfig {
    /// A config for `role` over `transport`, named `"anonymous"` and supporting
    /// [`SPEC_VERSION`](crate::SPEC_VERSION).
    #[must_use]
    pub fn new(role: Role, transport: Transport) -> Self {
        Self {
            role,
            transport,
            name: "anonymous".to_owned(),
            versions: vec![crate::SPEC_VERSION.to_owned()],
        }
    }

    /// Set the human-readable identifier sent in `start`.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the supported FRP versions, highest preference first.
    #[must_use]
    pub fn with_versions(mut self, versions: &[&str]) -> Self {
        self.versions = versions.iter().map(|&s| s.to_owned()).collect();
        self
    }
}

/// An FRP participant: a role plus a way of obtaining connections.
///
/// An endpoint is long-lived and outlives the connections it produces. Call
/// [`establish`](Self::establish) again to reconnect — the caller owns any
/// retry or backoff policy.
pub struct FrpEndpoint {
    config: EndpointConfig,
    listener: Option<TcpListener>,
}

impl FrpEndpoint {
    /// Open an endpoint. Binds the TCP listener for [`Transport::Listen`];
    /// [`Transport::Connect`] defers all I/O to [`establish`](Self::establish).
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind.
    pub fn open(config: EndpointConfig) -> Result<Self, FrpError> {
        let listener = match &config.transport {
            Transport::Listen(addr) => Some(
                TcpListener::bind(addr)
                    .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e))))?,
            ),
            Transport::Connect(_) => None,
        };
        Ok(Self { config, listener })
    }

    /// The role this endpoint plays.
    #[must_use]
    pub fn role(&self) -> Role {
        self.config.role
    }

    /// The local address of the bound listener, for [`Transport::Listen`].
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be read.
    pub fn local_addr(&self) -> Result<Option<std::net::SocketAddr>, FrpError> {
        match &self.listener {
            Some(l) => l
                .local_addr()
                .map(Some)
                .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e)))),
            None => Ok(None),
        }
    }

    /// Obtain a handshake-complete [`FrpConnection`].
    ///
    /// Blocks accepting an inbound connection ([`Transport::Listen`]) or dials
    /// the configured URL ([`Transport::Connect`]), then performs the FRP
    /// handshake in the direction determined by [`Role`].
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the FRP handshake fails.
    pub fn establish(&mut self) -> Result<FrpConnection, FrpError> {
        let socket = match (&self.config.transport, &self.listener) {
            (Transport::Listen(_), Some(listener)) => {
                let (stream, _addr) = listener
                    .accept()
                    .map_err(|e| FrpError::WebSocket(Box::new(tungstenite::Error::Io(e))))?;
                tungstenite::accept(MaybeTlsStream::Plain(stream)).map_err(|e| match e {
                    tungstenite::HandshakeError::Failure(e) => FrpError::WebSocket(Box::new(e)),
                    tungstenite::HandshakeError::Interrupted(_) => {
                        FrpError::Handshake("WebSocket handshake interrupted".into())
                    }
                })?
            }
            (Transport::Connect(url), _) => tungstenite::connect(url)?.0,
            (Transport::Listen(_), None) => {
                return Err(FrpError::Handshake("listener not bound".into()));
            }
        };

        match self.config.role {
            Role::Controller => self.handshake_as_controller(socket),
            Role::Device => self.handshake_as_device(socket),
        }
    }

    /// Send `start`, await `init`.
    fn handshake_as_controller(&self, mut socket: Socket) -> Result<FrpConnection, FrpError> {
        let start = FrpProtocolMessage::Start {
            version: self.config.versions.clone(),
            name: Some(self.config.name.clone()),
        };
        socket.send(Message::text(serde_json::to_string(&start)?))?;

        loop {
            match socket.read()? {
                Message::Text(text) => match FrpMessage::parse(&text) {
                    Ok(FrpMessage::Protocol(FrpProtocolMessage::Init { version })) => {
                        return Ok(FrpConnection {
                            socket,
                            role: Role::Controller,
                            version,
                            peer_name: None,
                        });
                    }
                    Ok(FrpMessage::Protocol(FrpProtocolMessage::Alert {
                        severity: Severity::Critical,
                        message,
                    })) => return Err(FrpError::Handshake(message)),
                    _ => {}
                },
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        }
    }

    /// Await `start`, reply `init` with the highest mutually supported version.
    fn handshake_as_device(&self, mut socket: Socket) -> Result<FrpConnection, FrpError> {
        let (peer_versions, peer_name) = loop {
            match socket.read()? {
                Message::Text(text) => {
                    if let Ok(FrpMessage::Protocol(FrpProtocolMessage::Start { version, name })) =
                        FrpMessage::parse(&text)
                    {
                        break (version, name);
                    }
                }
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        };

        let Some(version) = select_version(&self.config.versions, &peer_versions) else {
            let alert = FrpProtocolMessage::Alert {
                severity: Severity::Critical,
                message: "No compatible FRP version".into(),
            };
            socket.send(Message::text(serde_json::to_string(&alert)?))?;
            socket.close(None)?;
            return Err(FrpError::Handshake("No compatible FRP version".into()));
        };

        let init = FrpProtocolMessage::Init {
            version: version.clone(),
        };
        socket.send(Message::text(serde_json::to_string(&init)?))?;

        Ok(FrpConnection {
            socket,
            role: Role::Device,
            version,
            peer_name,
        })
    }
}

impl fmt::Debug for FrpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrpEndpoint")
            .field("role", &self.config.role)
            .field("transport", &self.config.transport)
            .finish_non_exhaustive()
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// A handshake-complete FRP connection.
///
/// Both roles send and receive: devices emit envelopes and accept commands,
/// controllers send commands and receive envelopes.
pub struct FrpConnection {
    socket: Socket,
    role: Role,
    version: String,
    peer_name: Option<String>,
}

impl FrpConnection {
    /// The role of this side of the connection.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// The FRP version negotiated during the handshake.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The peer's `name` from `start`, when this side is the [`Role::Device`].
    #[must_use]
    pub fn peer_name(&self) -> Option<&str> {
        self.peer_name.as_deref()
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

    /// Send an [`FrpMessage`] to the peer.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send(&mut self, msg: &FrpMessage) -> Result<(), FrpError> {
        self.socket.send(Message::text(msg.to_json()?))?;
        Ok(())
    }

    /// Send an [`FrpEnvelope`] to the peer.
    ///
    /// # Errors
    ///
    /// Returns an error if the envelope cannot be serialized or sent.
    pub fn send_envelope(&mut self, env: &FrpEnvelope) -> Result<(), FrpError> {
        self.socket
            .send(Message::text(serde_json::to_string(env)?))?;
        Ok(())
    }

    /// Send a bare [`FrpProtocolMessage`] to the peer.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send_protocol(&mut self, msg: &FrpProtocolMessage) -> Result<(), FrpError> {
        self.socket
            .send(Message::text(serde_json::to_string(msg)?))?;
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

impl fmt::Debug for FrpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrpConnection")
            .field("role", &self.role)
            .field("version", &self.version)
            .field("peer_name", &self.peer_name)
            .finish_non_exhaustive()
    }
}

/// Select the highest version present in both lists.
///
/// `peer` is in preference order, highest first.
fn select_version(local: &[String], peer: &[String]) -> Option<String> {
    peer.iter().find(|v| local.contains(v)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_selection() {
        let local = vec!["0.1.0".to_owned(), "0.2.0".to_owned()];
        let peer = vec!["0.2.0".to_owned(), "0.1.0".to_owned()];
        assert_eq!(select_version(&local, &peer), Some("0.2.0".to_owned()));
    }

    #[test]
    fn version_selection_no_match() {
        let local = vec!["0.1.0".to_owned()];
        let peer = vec!["0.2.0".to_owned()];
        assert_eq!(select_version(&local, &peer), None);
    }

    #[test]
    fn roles_are_reciprocal() {
        assert_eq!(Role::Device.peer(), Role::Controller);
        assert_eq!(Role::Controller.peer(), Role::Device);
    }

    #[test]
    fn config_defaults_to_spec_version() {
        let cfg = EndpointConfig::new(Role::Device, Transport::listen("0.0.0.0:5880"));
        assert_eq!(cfg.versions, vec![crate::SPEC_VERSION.to_owned()]);
        assert_eq!(cfg.name, "anonymous");
    }
}
