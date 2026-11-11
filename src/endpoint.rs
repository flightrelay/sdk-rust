//! The WebSocket binding of FRP.
//!
//! Requires the `transport` feature. One message per text frame; the default
//! URL is `ws://localhost:5880/frp`. There is no handshake beyond the
//! WebSocket upgrade: on connecting, each side sends `bus.actor` for each
//! actor it hosts, then the state those actors own, then `bus.sync`
//! ([`FrpConnection::greet`]).
//!
//! Either side may open the socket. [`Transport`] picks whether an endpoint
//! listens or dials; once connected, both sides are peers on the bus.
//!
//! ```no_run
//! use flightrelay::{BusActor, EndpointConfig, FrpEndpoint, FrpMessage, Transport};
//!
//! let mut ep = FrpEndpoint::open(EndpointConfig::new(Transport::connect("ws://hub:5880/frp")))?;
//! let mut conn = ep.establish()?;
//! let me = FrpMessage::new("e4a7136b9f02cd58", BusActor::new("Fairway Physics", 2.0));
//! conn.greet(&[me], &[])?; // `bus.actor`, no state of its own, then `bus.sync`
//! loop {
//!     let msg = conn.recv()?;
//!     println!("{} from {}", msg.kind(), msg.from);
//! }
//! # Ok::<(), flightrelay::FrpError>(())
//! ```

use std::fmt;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::Message;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{WebSocket, WebSocketConfig};
use tungstenite::stream::MaybeTlsStream;

use crate::error::FrpError;
use crate::kinds::BusSync;
use crate::message::FrpMessage;

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
    /// How this endpoint obtains a connection.
    pub transport: Transport,
    /// How long the WebSocket upgrade may take, from the connection opening
    /// to the upgrade's answer. `None` waits as long as the peer does.
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
    /// A config over `transport`, with no upgrade timeout and the
    /// [`DEFAULT_WRITE_BUFFER_LIMIT`].
    #[must_use]
    pub fn new(transport: Transport) -> Self {
        Self {
            transport,
            handshake_timeout: None,
            write_buffer_limit: DEFAULT_WRITE_BUFFER_LIMIT,
        }
    }

    /// Limit how long the WebSocket upgrade may take, so a peer that connects
    /// and says nothing cannot hold the endpoint.
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

/// A way of obtaining FRP connections: a listener, or a URL to dial.
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
            Transport::Listen(addr) => Some(TcpListener::bind(addr).map_err(io_error)?),
            Transport::Connect(_) => None,
        };
        Ok(Self { config, listener })
    }

    /// The local address of the bound listener, for [`Transport::Listen`].
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be read.
    pub fn local_addr(&self) -> Result<Option<std::net::SocketAddr>, FrpError> {
        match &self.listener {
            Some(l) => l.local_addr().map(Some).map_err(io_error),
            None => Ok(None),
        }
    }

    /// Obtain a connection: accept an inbound one ([`Transport::Listen`]) or
    /// dial the configured URL ([`Transport::Connect`]), and complete the
    /// WebSocket upgrade, bounded by [`EndpointConfig::handshake_timeout`]
    /// when one is set. Nothing is sent on the new connection; call
    /// [`FrpConnection::greet`] next.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the WebSocket upgrade fails.
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
    /// upgrade, so set one to keep a peer that connects and says nothing
    /// from holding the call.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection or the WebSocket upgrade fails.
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
                // The upgrade clock starts when the peer connects.
                let deadline = self.config.handshake_timeout.map(|t| Instant::now() + t);
                stream.set_nonblocking(false).map_err(io_error)?;
                arm(&stream, deadline)?;
                let socket =
                    tungstenite::accept_with_config(MaybeTlsStream::Plain(stream), Some(ws))
                        .map_err(|e| handshake_error(e, deadline))?;
                finish(socket).map(Some)
            }
            (Transport::Connect(url), _)
                if !cancelable && self.config.handshake_timeout.is_none() =>
            {
                // Nothing to bound or cancel: the plain blocking connect, which
                // follows HTTP redirects.
                let socket = tungstenite::client::connect_with_config(url.as_str(), Some(ws), 3)?.0;
                finish(socket).map(Some)
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
                finish(socket).map(Some)
            }
            (Transport::Listen(_), None) => Err(FrpError::Handshake("listener not bound".into())),
        }
    }
}

/// A connection out of the upgrade: the upgrade's timeouts do not apply to
/// the connection's life.
fn finish(socket: Socket) -> Result<FrpConnection, FrpError> {
    let peer = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => {
            tcp.set_read_timeout(None).map_err(io_error)?;
            tcp.set_write_timeout(None).map_err(io_error)?;
            tcp.peer_addr().ok()
        }
        _ => None,
    };
    Ok(FrpConnection { socket, peer })
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
            .field("transport", &self.config.transport)
            .finish_non_exhaustive()
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// One WebSocket connection carrying FRP messages, one per text frame.
pub struct FrpConnection {
    socket: Socket,
    peer: Option<SocketAddr>,
}

impl FrpConnection {
    /// The peer's address, when known.
    #[must_use]
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// What a side sends on connecting, in the order the spec gives: each
    /// message of `actors` (the `bus.actor` of each actor it hosts), then
    /// each message of `state` (the current state those actors own, e.g. a
    /// `golfer.turn`), then one `bus.sync` from the first actor, so the other
    /// side's actors republish their state. Every `bus.actor` goes before any
    /// state, so a receiver knows each state's actor when the state arrives.
    /// Sends nothing when `actors` is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if a message cannot be serialized or sent; what was
    /// sent before it stays sent.
    pub fn greet(&mut self, actors: &[FrpMessage], state: &[FrpMessage]) -> Result<(), FrpError> {
        let Some(first) = actors.first() else {
            return Ok(());
        };
        for msg in actors.iter().chain(state) {
            self.send(msg)?;
        }
        self.send(&FrpMessage::new(first.from.clone(), BusSync {}))
    }

    /// Set the underlying TCP stream to non-blocking mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP stream mode cannot be set.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), FrpError> {
        match self.socket.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.set_nonblocking(nonblocking).map_err(io_error),
            _ => Ok(()),
        }
    }

    /// Block until the next message arrives, and parse it.
    ///
    /// # Errors
    ///
    /// Returns [`FrpError::Json`] for a message that isn't an FRP message
    /// (see [`FrpMessage::parse`]). That error is about the one message: the
    /// connection stays open, and the next call reads the next message.
    /// Other errors mean the connection failed or closed.
    pub fn recv(&mut self) -> Result<FrpMessage, FrpError> {
        FrpMessage::parse(&self.recv_text()?)
    }

    /// Block until the next message arrives, and return its text as it came,
    /// e.g. to forward it unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection failed or closed.
    pub fn recv_text(&mut self) -> Result<String, FrpError> {
        loop {
            match self.socket.read()? {
                Message::Text(text) => return Ok(text.as_str().to_owned()),
                Message::Close(_) => return Err(FrpError::Closed),
                _ => {}
            }
        }
    }

    /// Poll for the next message without blocking, and parse it.
    ///
    /// Returns `Ok(None)` when no message is immediately available (requires
    /// [`set_nonblocking(true)`](Self::set_nonblocking)).
    ///
    /// # Errors
    ///
    /// As for [`recv`](Self::recv): [`FrpError::Json`] is about the one
    /// message and the connection stays open.
    pub fn try_recv(&mut self) -> Result<Option<FrpMessage>, FrpError> {
        self.try_recv_text()?
            .map(|text| FrpMessage::parse(&text))
            .transpose()
    }

    /// Poll for the next message's text without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection failed or closed.
    pub fn try_recv_text(&mut self) -> Result<Option<String>, FrpError> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(Some(text.as_str().to_owned())),
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

    /// Send a message.
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
        self.send_text(&msg.to_json()?)
    }

    /// Send message text as it is, e.g. one received on another connection.
    /// Queues like [`send`](Self::send).
    ///
    /// # Errors
    ///
    /// Returns an error if the text cannot be sent.
    pub fn send_text(&mut self, text: &str) -> Result<(), FrpError> {
        queued(self.socket.send(Message::text(text)))
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
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = EndpointConfig::new(Transport::listen("0.0.0.0:5880"));
        assert_eq!(cfg.handshake_timeout, None);
        assert_eq!(cfg.write_buffer_limit, DEFAULT_WRITE_BUFFER_LIMIT);
        let cfg = cfg.with_handshake_timeout(Duration::from_secs(1));
        assert_eq!(cfg.handshake_timeout, Some(Duration::from_secs(1)));
    }
}
