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
    /// Whether to ask the device for `raw_frame` events in `start`. Only used
    /// by [`Role::Controller`].
    pub raw_frames: bool,
}

impl EndpointConfig {
    /// A config for `role` over `transport`, named `"anonymous"`.
    ///
    /// The handshake proposes and accepts only
    /// [`SPEC_VERSION`](crate::SPEC_VERSION).
    #[must_use]
    pub fn new(role: Role, transport: Transport) -> Self {
        Self {
            role,
            transport,
            name: "anonymous".to_owned(),
            raw_frames: false,
        }
    }

    /// Set the human-readable identifier sent in `start`.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set whether a controller asks the device for `raw_frame` events in
    /// `start`.
    #[must_use]
    pub fn with_raw_frames(mut self, raw_frames: bool) -> Self {
        self.raw_frames = raw_frames;
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
            Role::Device => Self::handshake_as_device(socket),
        }
    }

    /// Send `start` proposing [`SPEC_VERSION`](crate::SPEC_VERSION), await
    /// `init` confirming it.
    fn handshake_as_controller(&self, mut socket: Socket) -> Result<FrpConnection, FrpError> {
        let start = FrpProtocolMessage::Start {
            version: vec![crate::SPEC_VERSION.to_owned()],
            name: Some(self.config.name.clone()),
            raw_frames: self.config.raw_frames,
        };
        socket.send(Message::text(serde_json::to_string(&start)?))?;

        loop {
            match socket.read()? {
                Message::Text(text) => match FrpMessage::parse(&text) {
                    Ok(FrpMessage::Protocol(FrpProtocolMessage::Init { version })) => {
                        if version != crate::SPEC_VERSION {
                            let message = format!("Unsupported FRP version {version:?}");
                            let alert = FrpProtocolMessage::Alert {
                                severity: Severity::Critical,
                                message: message.clone(),
                            };
                            // Best effort: the peer may already be gone, and the
                            // caller needs to see the rejection either way.
                            let _ = socket.send(Message::text(serde_json::to_string(&alert)?));
                            let _ = socket.close(None);
                            return Err(FrpError::Handshake(message));
                        }
                        return Ok(FrpConnection {
                            socket,
                            role: Role::Controller,
                            version,
                            peer_name: None,
                            raw_frames: self.config.raw_frames,
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

    /// Await `start`, reply `init` with [`SPEC_VERSION`](crate::SPEC_VERSION)
    /// when the controller proposed it.
    fn handshake_as_device(mut socket: Socket) -> Result<FrpConnection, FrpError> {
        let (peer_versions, peer_name, raw_frames) = loop {
            match socket.read()? {
                Message::Text(text) => {
                    if let Ok(FrpMessage::Protocol(FrpProtocolMessage::Start {
                        version,
                        name,
                        raw_frames,
                    })) = FrpMessage::parse(&text)
                    {
                        break (version, name, raw_frames);
                    }
                }
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        };

        let Some(version) = select_version(&peer_versions) else {
            let alert = FrpProtocolMessage::Alert {
                severity: Severity::Critical,
                message: "No compatible FRP version".into(),
            };
            // Best effort: the peer may already be gone, and the caller needs
            // to see the rejection either way.
            let _ = socket.send(Message::text(serde_json::to_string(&alert)?));
            let _ = socket.close(None);
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
            raw_frames,
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
    raw_frames: bool,
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

    /// Whether the controller asked for `raw_frame` events with `raw_frames`
    /// in its `start`. On the [`Role::Device`] side, send raw frames only
    /// when this is `true`; on the [`Role::Controller`] side, it is what this
    /// endpoint asked for.
    #[must_use]
    pub fn raw_frames_requested(&self) -> bool {
        self.raw_frames
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
    /// An envelope whose event the spec has a controller ignore, such as a
    /// `flight_result` without a recognized `basis`, arrives with an
    /// [`FrpEvent::Unknown`](crate::FrpEvent::Unknown) event.
    ///
    /// # Errors
    ///
    /// Returns [`FrpError::Json`] for a message that isn't valid FRP JSON.
    /// That error is about the one message: the connection stays open, and
    /// the next call reads the next message. Other errors mean the
    /// connection failed or closed.
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
    /// As for [`recv`](Self::recv): [`FrpError::Json`] is about the one
    /// message and the connection stays open.
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
            .field("raw_frames", &self.raw_frames)
            .finish_non_exhaustive()
    }
}

/// [`SPEC_VERSION`](crate::SPEC_VERSION) if the controller proposed it. An
/// endpoint implements exactly one FRP version.
fn select_version(peer: &[String]) -> Option<String> {
    peer.iter()
        .any(|v| v == crate::SPEC_VERSION)
        .then(|| crate::SPEC_VERSION.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<String> {
        list.iter().map(|&v| v.to_owned()).collect()
    }

    #[test]
    fn version_selection_accepts_only_the_spec_version() {
        assert_eq!(crate::SPEC_VERSION, "0.2.0");
        for peer in [
            &["0.2.0"][..],
            &["0.3.0", "0.2.0", "0.1.0"],
            &["0.1.0", "0.2.0"],
        ] {
            assert_eq!(
                select_version(&versions(peer)),
                Some("0.2.0".to_owned()),
                "{peer:?}"
            );
        }
    }

    #[test]
    fn version_selection_no_match() {
        for peer in [
            &["0.1.0"][..],
            &["0.2.1"],
            &["0.3.0", "1.0.0"],
            &["0.2"],
            &[],
        ] {
            assert_eq!(select_version(&versions(peer)), None, "{peer:?}");
        }
    }

    #[test]
    fn roles_are_reciprocal() {
        assert_eq!(Role::Device.peer(), Role::Controller);
        assert_eq!(Role::Controller.peer(), Role::Device);
    }

    #[test]
    fn config_defaults_to_anonymous() {
        let cfg = EndpointConfig::new(Role::Device, Transport::listen("0.0.0.0:5880"));
        assert_eq!(cfg.name, "anonymous");
    }

    #[test]
    fn config_asks_for_no_raw_frames_by_default() {
        let cfg = EndpointConfig::new(Role::Controller, Transport::connect("ws://h:5880/frp"));
        assert!(!cfg.raw_frames);
        assert!(cfg.with_raw_frames(true).raw_frames);
    }
}
