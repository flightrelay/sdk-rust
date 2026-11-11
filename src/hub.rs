//! [`Hub`]: forwards every message to every other connection.
//!
//! Requires the `transport` feature. A hub joins bus segments: each message
//! that arrives on one connection goes out, as its text arrived, on every
//! other connection, never back to the one it came from. Messages of every
//! kind are forwarded, unknown and extension kinds included; text that is
//! not an FRP message is not. Hubs connected in a loop forward messages
//! forever: a loop is a configuration error, and nothing here detects it.
//!
//! The hub also hands every message it received to its caller, which hosts
//! actors of its own: [`Hub::poll`] returns them, and [`Hub::send`] publishes
//! the caller's own messages on every connection.
//!
//! The hub is synchronous and caller-driven: call [`poll`](Hub::poll)
//! regularly. Its connections are non-blocking; a peer that reads slowly is
//! queued, and dropped once its queue passes the connection's
//! [`write_buffer_limit`](crate::EndpointConfig::write_buffer_limit).
//!
//! ```no_run
//! use flightrelay::{BusActor, EndpointConfig, FrpEndpoint, FrpMessage, Hub, HubEvent, Transport};
//!
//! let mut endpoint = FrpEndpoint::open(EndpointConfig::new(Transport::listen("0.0.0.0:5880")))?;
//! let me = FrpMessage::new("a1b2c3d4e5f60718", BusActor::new("Range hub", 2.0));
//! let mut hub = Hub::new();
//! let mut conn = endpoint.establish()?;
//! conn.greet(std::slice::from_ref(&me), &[])?;
//! hub.add(conn)?;
//! loop {
//!     for event in hub.poll() {
//!         if let HubEvent::Message { message, .. } = event {
//!             println!("{} from {}", message.kind(), message.from);
//!         }
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(10));
//! }
//! # Ok::<(), flightrelay::FrpError>(())
//! ```

use std::fmt;
use std::net::SocketAddr;

use crate::endpoint::FrpConnection;
use crate::error::FrpError;
use crate::message::FrpMessage;

/// The most messages [`Hub::poll`] reads from one connection per call, so a
/// busy peer cannot starve the others.
const READS_PER_POLL: usize = 64;

/// Identifies one connection of a [`Hub`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(u64);

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer {}", self.0)
    }
}

/// Something that happened on a hub's connections.
// Events are handled one at a time, so the size gap between `Message` and
// the small variants costs nothing; boxing would only complicate matching.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum HubEvent {
    /// A message arrived on `peer` and was forwarded to every other
    /// connection.
    Message {
        /// The connection it arrived on.
        peer: PeerId,
        /// The message.
        message: FrpMessage,
    },
    /// Text that is not an FRP message arrived on `peer`; it was not
    /// forwarded. The connection stays.
    Invalid {
        /// The connection it arrived on.
        peer: PeerId,
        /// Why it is not an FRP message.
        error: FrpError,
    },
    /// A connection closed or failed and was dropped.
    Left {
        /// The connection.
        peer: PeerId,
        /// Its peer's address, when known.
        addr: Option<SocketAddr>,
        /// Why.
        reason: String,
    },
}

struct Peer {
    id: PeerId,
    conn: FrpConnection,
}

/// Forwards every message to every other connection. See the
/// [module docs](self).
#[derive(Default)]
pub struct Hub {
    peers: Vec<Peer>,
    next: u64,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub")
            .field(
                "peers",
                &self.peers.iter().map(|p| p.id).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// A hub with no connections.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a connection into the hub, making it non-blocking. Greet it
    /// ([`FrpConnection::greet`]) before or right after.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be made non-blocking.
    pub fn add(&mut self, conn: FrpConnection) -> Result<PeerId, FrpError> {
        conn.set_nonblocking(true)?;
        let id = PeerId(self.next);
        self.next += 1;
        self.peers.push(Peer { id, conn });
        Ok(id)
    }

    /// Take a connection out of the hub, still open.
    pub fn remove(&mut self, peer: PeerId) -> Option<FrpConnection> {
        let i = self.index_of(peer)?;
        Some(self.peers.remove(i).conn)
    }

    /// The connections in the hub.
    pub fn peers(&self) -> impl Iterator<Item = (PeerId, Option<SocketAddr>)> {
        self.peers.iter().map(|p| (p.id, p.conn.peer_addr()))
    }

    /// How many connections the hub has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the hub has no connections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Publish one of the caller's own messages on every connection.
    /// Returns the connections that failed and were dropped.
    ///
    /// # Errors
    ///
    /// Returns an error, and sends nothing, if the message cannot be
    /// serialized.
    pub fn send(&mut self, msg: &FrpMessage) -> Result<Vec<HubEvent>, FrpError> {
        let text = msg.to_json()?;
        Ok(self.fan_out(&text, None))
    }

    /// Send one of the caller's own messages on one connection only, e.g.
    /// a greeting. Returns the connection if it failed and was dropped, and
    /// `None` when it was sent or `peer` is not in the hub.
    ///
    /// # Errors
    ///
    /// Returns an error, and sends nothing, if the message cannot be
    /// serialized.
    pub fn send_to(
        &mut self,
        peer: PeerId,
        msg: &FrpMessage,
    ) -> Result<Option<HubEvent>, FrpError> {
        let text = msg.to_json()?;
        let Some(i) = self.index_of(peer) else {
            return Ok(None);
        };
        Ok(match self.peers[i].conn.send_text(&text) {
            Ok(()) => None,
            Err(e) => Some(self.drop_peer(i, &e)),
        })
    }

    /// Read what has arrived on every connection, forward each message to
    /// every other connection, and push queued messages out. Returns the
    /// messages that arrived, text that was not FRP, and the connections
    /// that were dropped, in that order per connection.
    pub fn poll(&mut self) -> Vec<HubEvent> {
        let mut events = Vec::new();
        // Forwarding can drop any other peer, so peers are visited by id,
        // from a snapshot, and looked up again after every forward.
        let ids: Vec<PeerId> = self.peers.iter().map(|p| p.id).collect();
        for id in ids {
            let mut failed = None;
            for _ in 0..READS_PER_POLL {
                let Some(i) = self.index_of(id) else { break };
                match self.peers[i].conn.try_recv_text() {
                    Ok(None) => break,
                    Ok(Some(text)) => match FrpMessage::parse(&text) {
                        Ok(message) => {
                            events.extend(self.fan_out(&text, Some(id)));
                            events.push(HubEvent::Message { peer: id, message });
                        }
                        Err(error) => events.push(HubEvent::Invalid { peer: id, error }),
                    },
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let Some(i) = self.index_of(id) else {
                continue;
            };
            if let Some(e) = failed.or_else(|| self.peers[i].conn.flush().err()) {
                events.push(self.drop_peer(i, &e));
            }
        }
        events
    }

    /// Close every connection, after pushing out what is queued for about
    /// `flush_for`. A peer that reads more slowly than that sees the
    /// connection reset instead of a close.
    pub fn close_all(&mut self, flush_for: std::time::Duration) {
        let until = std::time::Instant::now() + flush_for;
        while std::time::Instant::now() < until {
            for p in &mut self.peers {
                let _ = p.conn.flush();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for p in std::mem::take(&mut self.peers) {
            let _ = p.conn.close();
        }
    }

    /// Send `text` on every connection but `except`.
    fn fan_out(&mut self, text: &str, except: Option<PeerId>) -> Vec<HubEvent> {
        let mut failed = Vec::new();
        for (i, p) in self.peers.iter_mut().enumerate() {
            if Some(p.id) == except {
                continue;
            }
            if let Err(e) = p.conn.send_text(text) {
                failed.push((i, e));
            }
        }
        failed
            .into_iter()
            .rev()
            .map(|(i, e)| self.drop_peer(i, &e))
            .collect()
    }

    /// Where `peer` is in `peers` now.
    fn index_of(&self, peer: PeerId) -> Option<usize> {
        self.peers.iter().position(|p| p.id == peer)
    }

    fn drop_peer(&mut self, i: usize, error: &FrpError) -> HubEvent {
        let p = self.peers.remove(i);
        HubEvent::Left {
            peer: p.id,
            addr: p.conn.peer_addr(),
            reason: describe(error),
        }
    }
}

/// Why a connection was dropped, in words.
pub(crate) fn describe(e: &FrpError) -> String {
    match e {
        FrpError::WebSocket(inner) if matches!(**inner, tungstenite::Error::WriteBufferFull(_)) => {
            "write buffer full: the peer reads too slowly".into()
        }
        FrpError::Closed => "closed".into(),
        other => other.to_string(),
    }
}
