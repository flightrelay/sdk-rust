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
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::Message;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{WebSocket, WebSocketConfig};
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
    /// How long a handshake may take, from the connection opening to the
    /// last handshake message. `None` waits as long as the peer does.
    pub handshake_timeout: Option<Duration>,
    /// How many bytes a connection may queue for a peer that reads slowly
    /// before a send fails with a write-buffer-full error. Only a
    /// non-blocking connection queues. A single message larger than this
    /// cannot be sent.
    pub write_buffer_limit: usize,
}

/// The default [`EndpointConfig::write_buffer_limit`].
pub const DEFAULT_WRITE_BUFFER_LIMIT: usize = 4 * 1024 * 1024;

/// How often [`FrpEndpoint::establish_until`] looks for a connection and for
/// a stop.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// How long a connect may take when a stop token is given but no
/// `handshake_timeout`.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

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
            handshake_timeout: None,
            write_buffer_limit: DEFAULT_WRITE_BUFFER_LIMIT,
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

    /// Limit how long a handshake may take, so a peer that connects and says
    /// nothing cannot hold the endpoint.
    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = Some(timeout);
        self
    }

    /// Set the most bytes a non-blocking connection queues for a slow peer.
    #[must_use]
    pub fn with_write_buffer_limit(mut self, bytes: usize) -> Self {
        self.write_buffer_limit = bytes;
        self
    }

    fn ws_config(&self) -> WebSocketConfig {
        let base = WebSocketConfig::default();
        let write = base.write_buffer_size;
        base.max_write_buffer_size(self.write_buffer_limit.max(write + 1))
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
    /// handshake in the direction determined by [`Role`]. The handshake is
    /// bounded by [`EndpointConfig::handshake_timeout`] when one is set.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the FRP handshake fails.
    pub fn establish(&mut self) -> Result<FrpConnection, FrpError> {
        let never = AtomicBool::new(false);
        match self.establish_inner(&never, false)? {
            Some(conn) => Ok(conn),
            None => Err(FrpError::Closed),
        }
    }

    /// Like [`establish`](Self::establish), but returns `Ok(None)` as soon as
    /// `stop` is set, so a thread that serves or dials in a loop can end.
    ///
    /// A listening endpoint polls its listener every 100 ms; a dialing one
    /// connects on a thread of its own and gives up the wait on a stop (and
    /// does not follow HTTP redirects, which [`establish`](Self::establish)
    /// does when nothing bounds or cancels the dial). Once a peer has
    /// connected, only [`EndpointConfig::handshake_timeout`] bounds the
    /// handshake, so set one to keep a peer that connects and says nothing
    /// from holding the call.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the FRP handshake fails.
    pub fn establish_until(
        &mut self,
        stop: &AtomicBool,
    ) -> Result<Option<FrpConnection>, FrpError> {
        self.establish_inner(stop, true)
    }

    fn establish_inner(
        &mut self,
        stop: &AtomicBool,
        cancelable: bool,
    ) -> Result<Option<FrpConnection>, FrpError> {
        let ws = self.config.ws_config();
        match (&self.config.transport, &self.listener) {
            (Transport::Listen(_), Some(listener)) => {
                let stream = if cancelable {
                    match accept_until(listener, stop)? {
                        Some(stream) => stream,
                        None => return Ok(None),
                    }
                } else {
                    listener.accept().map_err(io_error)?.0
                };
                // The handshake clock starts when the peer connects.
                let deadline = self.config.handshake_timeout.map(|t| Instant::now() + t);
                stream.set_nonblocking(false).map_err(io_error)?;
                arm(&stream, deadline)?;
                let socket =
                    tungstenite::accept_with_config(MaybeTlsStream::Plain(stream), Some(ws))
                        .map_err(|e| handshake_error(e, deadline))?;
                self.finish(socket, deadline).map(Some)
            }
            (Transport::Connect(url), _)
                if !cancelable && self.config.handshake_timeout.is_none() =>
            {
                // Nothing to bound or cancel: the plain blocking connect, which
                // follows HTTP redirects.
                let socket = tungstenite::client::connect_with_config(url.as_str(), Some(ws), 3)?.0;
                self.finish(socket, None).map(Some)
            }
            (Transport::Connect(url), _) => {
                let deadline = self.config.handshake_timeout.map(|t| Instant::now() + t);
                let Some(stream) = dial(
                    url,
                    self.config.handshake_timeout,
                    cancelable.then_some(stop),
                )?
                else {
                    return Ok(None);
                };
                let request = url.as_str().into_client_request()?;
                arm(&stream, deadline)?;
                let socket = tungstenite::client::client_with_config(
                    request,
                    MaybeTlsStream::Plain(stream),
                    Some(ws),
                )
                .map_err(|e| handshake_error(e, deadline))?
                .0;
                self.finish(socket, deadline).map(Some)
            }
            (Transport::Listen(_), None) => Err(FrpError::Handshake("listener not bound".into())),
        }
    }

    fn finish(&self, socket: Socket, deadline: Option<Instant>) -> Result<FrpConnection, FrpError> {
        let conn = match self.config.role {
            Role::Controller => self.handshake_as_controller(socket, deadline),
            Role::Device => Self::handshake_as_device(socket, deadline),
        }?;
        // The handshake's timeouts do not apply to the connection's life.
        if let MaybeTlsStream::Plain(tcp) = conn.socket.get_ref() {
            tcp.set_read_timeout(None).map_err(io_error)?;
            tcp.set_write_timeout(None).map_err(io_error)?;
        }
        Ok(conn)
    }

    /// Send `start` proposing [`SPEC_VERSION`](crate::SPEC_VERSION), await
    /// `init` confirming it.
    fn handshake_as_controller(
        &self,
        mut socket: Socket,
        deadline: Option<Instant>,
    ) -> Result<FrpConnection, FrpError> {
        let start = FrpProtocolMessage::Start {
            version: vec![crate::SPEC_VERSION.to_owned()],
            name: Some(self.config.name.clone()),
            raw_frames: self.config.raw_frames,
        };
        socket.send(Message::text(serde_json::to_string(&start)?))?;

        loop {
            match read_by(&mut socket, deadline)? {
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
    fn handshake_as_device(
        mut socket: Socket,
        deadline: Option<Instant>,
    ) -> Result<FrpConnection, FrpError> {
        let (peer_versions, peer_name, raw_frames) = loop {
            match read_by(&mut socket, deadline)? {
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

fn io_error(e: std::io::Error) -> FrpError {
    FrpError::WebSocket(Box::new(tungstenite::Error::Io(e)))
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Accepts a connection, polling the listener until `stop` is set.
fn accept_until(listener: &TcpListener, stop: &AtomicBool) -> Result<Option<TcpStream>, FrpError> {
    listener.set_nonblocking(true).map_err(io_error)?;
    let result = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(None);
        }
        match listener.accept() {
            Ok((stream, _)) => break Ok(Some(stream)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) => break Err(io_error(e)),
        }
    };
    listener.set_nonblocking(false).map_err(io_error)?;
    result
}

/// Opens the TCP connection to a `ws://` URL for a bounded or cancelable
/// dial. `Ok(None)` when `stop` is set first.
fn dial(
    url: &str,
    timeout: Option<Duration>,
    stop: Option<&AtomicBool>,
) -> Result<Option<TcpStream>, FrpError> {
    let request = url.into_client_request()?;
    let uri = request.uri();
    if uri.scheme_str() != Some("ws") {
        return Err(FrpError::Handshake(format!(
            "cannot connect to {url:?}: only ws:// is supported"
        )));
    }
    let port = uri.port_u16().unwrap_or(80);
    let host = uri
        .host()
        .ok_or_else(|| FrpError::Handshake(format!("no host in {url:?}")))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let mut last = None;
    for addr in (host, port).to_socket_addrs().map_err(io_error)? {
        let attempt = crate::net::connect_cancelable(addr, timeout.unwrap_or(DIAL_TIMEOUT), || {
            stop.is_none_or(|s| !s.load(Ordering::SeqCst))
        });
        match attempt {
            Ok(Some(stream)) => return Ok(Some(stream)),
            Ok(None) => return Ok(None),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or_else(
        || FrpError::Handshake(format!("{host} resolved to no address")),
        io_error,
    ))
}

/// Limits the next socket wait to what is left of the handshake deadline.
fn arm(stream: &TcpStream, deadline: Option<Instant>) -> Result<(), FrpError> {
    let Some(deadline) = deadline else {
        return Ok(());
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(FrpError::Handshake("handshake timed out".into()));
    }
    stream.set_read_timeout(Some(left)).map_err(io_error)?;
    stream.set_write_timeout(Some(left)).map_err(io_error)
}

/// One handshake read, limited by the deadline.
fn read_by(socket: &mut Socket, deadline: Option<Instant>) -> Result<Message, FrpError> {
    if deadline.is_some()
        && let MaybeTlsStream::Plain(tcp) = socket.get_ref()
    {
        arm(tcp, deadline)?;
    }
    match socket.read() {
        Err(tungstenite::Error::Io(e)) if deadline.is_some() && is_timeout(&e) => {
            Err(FrpError::Handshake("handshake timed out".into()))
        }
        other => Ok(other?),
    }
}

fn handshake_error<R: tungstenite::handshake::HandshakeRole>(
    e: tungstenite::HandshakeError<R>,
    deadline: Option<Instant>,
) -> FrpError {
    match e {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Io(e))
            if deadline.is_some() && is_timeout(&e) =>
        {
            FrpError::Handshake("handshake timed out".into())
        }
        tungstenite::HandshakeError::Failure(e) => FrpError::WebSocket(Box::new(e)),
        tungstenite::HandshakeError::Interrupted(_) if deadline.is_some() => {
            FrpError::Handshake("handshake timed out".into())
        }
        tungstenite::HandshakeError::Interrupted(_) => {
            FrpError::Handshake("WebSocket handshake interrupted".into())
        }
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
    /// On a non-blocking connection a full socket is not an error: the
    /// message is queued, and it leaves with a later [`flush`](Self::flush)
    /// or send. Call [`flush`](Self::flush) regularly (e.g. on each poll of
    /// the connection): a queued message is otherwise delayed until the next
    /// send.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send(&mut self, msg: &FrpMessage) -> Result<(), FrpError> {
        queued(self.socket.send(Message::text(msg.to_json()?)))
    }

    /// Send an [`FrpEnvelope`] to the peer.
    ///
    /// On a non-blocking connection a full socket is not an error: the
    /// message is queued, and it leaves with a later [`flush`](Self::flush)
    /// or send. Call [`flush`](Self::flush) regularly (e.g. on each poll of
    /// the connection): a queued message is otherwise delayed until the next
    /// send.
    ///
    /// # Errors
    ///
    /// Returns an error if the envelope cannot be serialized or sent.
    pub fn send_envelope(&mut self, env: &FrpEnvelope) -> Result<(), FrpError> {
        queued(self.socket.send(Message::text(serde_json::to_string(env)?)))
    }

    /// Send a bare [`FrpProtocolMessage`] to the peer.
    ///
    /// On a non-blocking connection a full socket is not an error: the
    /// message is queued, and it leaves with a later [`flush`](Self::flush)
    /// or send. Call [`flush`](Self::flush) regularly (e.g. on each poll of
    /// the connection): a queued message is otherwise delayed until the next
    /// send.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or sent.
    pub fn send_protocol(&mut self, msg: &FrpProtocolMessage) -> Result<(), FrpError> {
        queued(self.socket.send(Message::text(serde_json::to_string(msg)?)))
    }

    /// Push queued messages out to the peer. Returns `Ok` when the socket
    /// cannot take more yet; call it again later. A non-blocking connection
    /// calls it regularly.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection failed or closed.
    pub fn flush(&mut self) -> Result<(), FrpError> {
        match self.socket.flush() {
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                Err(FrpError::Closed)
            }
            other => queued(other),
        }
    }

    /// Cleanly close the WebSocket connection.
    ///
    /// On a non-blocking connection the close frame is queued and the call
    /// returns without waiting for the peer's answer; [`flush`](Self::flush)
    /// first to get queued messages out.
    ///
    /// # Errors
    ///
    /// Returns an error if the close handshake fails.
    pub fn close(mut self) -> Result<(), FrpError> {
        queued(self.socket.close(None))?;
        loop {
            match self.socket.read() {
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                    return Ok(());
                }
                Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
                _ => {}
            }
        }
    }
}

/// Turns a send that found the socket full into success: tungstenite keeps
/// the message in its write buffer and [`FrpConnection::flush`] pushes it out
/// later. [`tungstenite::Error::WriteBufferFull`] still fails, once the
/// buffer passes [`EndpointConfig::write_buffer_limit`].
fn queued(result: Result<(), tungstenite::Error>) -> Result<(), FrpError> {
    match result {
        Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        other => Ok(other?),
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
