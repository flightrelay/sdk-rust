//! [`FrpServer`]: run a [`DeviceDriver`] as a launch monitor actor on the bus.
//!
//! Requires the `server` feature. The server connects the driver, polls its
//! session and publishes what the driver emits as one launch monitor actor,
//! with the actor id the host gives it ([`ServerConfig::actor_id`]). It
//! listens for peers (listen mode, a hub of its own) or dials one (bridge
//! mode), and reconnects the driver with the driver's advised delay, so a
//! host's `main` is a driver, a config and one call; `run` (feature `main`)
//! is that `main`.
//!
//! # Threads
//!
//! The caller's thread is the device thread: [`connect`](DeviceDriver::connect),
//! [`poll`](DeviceSession::poll), [`hints`](DeviceSession::hints),
//! [`close`](DeviceSession::close) and the retry sleeps all run on it, so a
//! session can be `!Send`. A network thread owns every connection, as a
//! [`Hub`], so peers are served while a connect blocks. An acceptor thread
//! obtains connections: it accepts in listen mode and dials, and redials, in
//! bridge mode.
//!
//! # What the server does on the bus
//!
//! - On each new connection it sends the actor's `bus.actor`, then
//!   `bus.sync`. It republishes `bus.actor` when it changes, every
//!   [`ServerConfig::heartbeat`], and in answer to any `bus.sync`.
//! - The `bus.actor` has the role `launch_monitor`, the name
//!   [`ServerConfig::name`] (else the identified device's manufacturer and
//!   model, else [`DriverInfo::name`](crate::driver::DriverInfo::name)), and
//!   a `device` block: manufacturer, model and firmware from the session's
//!   identity, `source` from
//!   [`DriverInfo::source`](crate::driver::DriverInfo::source), and the
//!   battery and attitude the driver reports. Its `blockers`, battery,
//!   attitude and telemetry are exactly the driver's current
//!   [`DeviceStatus`]: a value the driver takes off (`battery(None)`, a
//!   whole status without it) is off the next `bus.actor`. Before the first
//!   session and whenever a session ends the status is
//!   [`DeviceStatus::not_armed`]. The device's own id and card label never
//!   go on the bus.
//! - What the driver emits goes out under the actor id: `shot.*` messages
//!   and `bus.alert`. A `shot.raw` is numbered per actor, across sessions,
//!   and goes out only while [`ServerConfig::raw_frames`] is on; drivers
//!   are asked not to build one otherwise, and one that arrives anyway is
//!   dropped.
//! - A `shot.finished` for every shot a lost session left open, and for any
//!   shot still open after [`ServerConfig::shot_timeout`] (default
//!   [`DEFAULT_SHOT_TIMEOUT`]).
//! - Every message a peer sends is forwarded to every other peer, never back
//!   to its source.
//! - Each `golfer.turn` replaces all the hints ([`TurnHints::from_turn`]):
//!   a `null` or unrecognized field is no hint for it, and nothing is
//!   carried over from an earlier turn. Whenever the resulting hints change,
//!   the session gets them through [`hints`](DeviceSession::hints), and the
//!   next connect through
//!   [`ConnectCtx::hints`](crate::driver::ConnectCtx::hints). The hints of
//!   the last complete turn stay when the session actor goes away.
//! - On a stop, the session is closed and the actor's removal
//!   (`bus.actor` with `"removed": true`) goes out.
//!
//! The launch monitor role owns no command, so the server answers none.
//!
//! A peer that stops reading is not disconnected while the kernel buffers
//! it: the server queues its messages, and drops it only once the queue
//! passes [`ServerConfig::write_buffer_limit`].
//!
//! The device thread and the network thread are joined by a bounded queue.
//! When it is full, a `shot.raw` is dropped (debug data; `seq` is numbered
//! on the network thread, after the queue, so the loss leaves no gap) and
//! anything else waits briefly.
//!
//! In bridge mode the server dials its peer again whenever the link drops.
//! A dial that fails is logged at [`Level::Info`] the first time and
//! whenever the error changes, and at [`Level::Debug`] while the same error
//! repeats. The target must be a `ws://` URL; `wss://` is refused when the
//! server starts.
//!
//! ```no_run
//! # use flightrelay::driver::{DeviceDriver, Stop};
//! # fn serve<D: DeviceDriver>(driver: D) -> Result<(), Box<dyn std::error::Error>> {
//! use flightrelay::{FrpServer, ServerConfig, Transport};
//!
//! // Generated once with `flightrelay::new_actor_id()` and kept by the host.
//! let actor_id = "7d2f9a41c03be856";
//! let config = ServerConfig::new(Transport::listen("0.0.0.0:5880"), actor_id);
//! let stop = Stop::new();
//! FrpServer::new(driver, config)
//!     .on_event(|event| eprintln!("{event:?}"))
//!     .run(&stop)?;
//! # Ok(()) }
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError, channel,
    sync_channel,
};
use std::time::{Duration, Instant};

use crate::driver::{
    Backoff, ConnectCtx, ConnectHost, DeviceDriver, DeviceEvent, DeviceIdentity, DeviceSession,
    DeviceStatus, DriverInfo, Events, HostFlags, Level, SessionEnd, StatusChange, Stop, TurnHints,
};
use crate::endpoint::DEFAULT_WRITE_BUFFER_LIMIT;
use crate::types::roles;
use crate::{
    BusActor, DEFAULT_HEARTBEAT, DEFAULT_SHOT_TIMEOUT, DeviceInfo, EndpointConfig, FrpBody,
    FrpConnection, FrpEndpoint, FrpError, FrpMessage, Hub, HubEvent, Removal, ShotFinished,
    ShotKey, StateKind, Transport,
};

/// Capacity of the queue from the device thread to the network thread.
const QUEUE: usize = 1024;

/// How long the device thread blocks on a full queue before it drops a
/// message that is not a raw frame.
const FULL_QUEUE_PATIENCE: Duration = Duration::from_secs(1);

/// How long the network thread waits for the device thread between passes
/// over the connections.
const NET_TICK: Duration = Duration::from_millis(10);

/// The device thread's poll timeout.
const POLL: Duration = Duration::from_millis(20);

/// How often the device thread looks at its inputs while it sleeps.
const SLEEP_SLICE: Duration = Duration::from_millis(50);

/// The shortest heartbeat a server uses; a shorter
/// [`ServerConfig::heartbeat`] is raised to this.
pub const MIN_HEARTBEAT: Duration = Duration::from_millis(100);

/// How a server reaches the bus, who its actor is, and its limits. Built
/// with [`new`](Self::new); the other fields are then assigned.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServerConfig {
    /// [`Transport::Listen`] to serve peers that connect, or
    /// [`Transport::Connect`] to dial one (a bridge).
    pub target: Transport,
    /// The actor's id: generated once ([`new_actor_id`](crate::new_actor_id))
    /// and kept across restarts by the host.
    pub actor_id: String,
    /// The actor's display name. Default: the identified device's
    /// manufacturer and model, else the driver's name.
    pub name: Option<String>,
    /// How often the actor republishes its `bus.actor`. Default
    /// [`DEFAULT_HEARTBEAT`]. A value below [`MIN_HEARTBEAT`] is raised to
    /// it, and the raised value is the one the `bus.actor` advertises.
    pub heartbeat: Duration,
    /// Whether the actor publishes `shot.raw`. Default off.
    pub raw_frames: bool,
    /// The most peers served at once in listen mode. Default 8.
    pub max_peers: usize,
    /// How long a WebSocket upgrade may take, so a silent client cannot
    /// stall the acceptor. Default 5 s.
    pub handshake_timeout: Duration,
    /// How many bytes the server queues for a peer that reads slowly before
    /// it drops that peer. Default 4 MiB.
    pub write_buffer_limit: usize,
    /// The lowest level of driver log lines the server lets through to
    /// [`ServerEvent::Device`]. Default [`Level::Info`].
    pub log_level: Level,
    /// How long a shot may stay open before the server finishes it. Default
    /// [`DEFAULT_SHOT_TIMEOUT`].
    pub shot_timeout: Duration,
}

impl ServerConfig {
    /// A config for `target` and the actor `actor_id`, with the defaults.
    #[must_use]
    pub fn new(target: Transport, actor_id: impl Into<String>) -> Self {
        Self {
            target,
            actor_id: actor_id.into(),
            name: None,
            heartbeat: DEFAULT_HEARTBEAT,
            raw_frames: false,
            max_peers: 8,
            handshake_timeout: Duration::from_secs(5),
            write_buffer_limit: DEFAULT_WRITE_BUFFER_LIMIT,
            log_level: Level::Info,
            shot_timeout: DEFAULT_SHOT_TIMEOUT,
        }
    }
}

/// Something that happened in a running server, for logging.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServerEvent<'a> {
    /// The server listens on this address.
    Listening(SocketAddr),
    /// A peer connected and was greeted.
    PeerConnected {
        /// The peer's address, when known.
        addr: Option<SocketAddr>,
    },
    /// A peer left, or was dropped.
    PeerLeft {
        /// The peer's address, when known.
        addr: Option<SocketAddr>,
        /// Why.
        reason: &'a str,
    },
    /// A session is up.
    DeviceConnected(&'a DeviceIdentity),
    /// A connect failed or a session ended.
    DeviceLost {
        /// What went wrong; `None` after a stop.
        error: Option<&'a dyn Error>,
        /// When the next attempt starts; `None` when the driver gave up.
        retry_in: Option<Duration>,
    },
    /// Everything the driver emitted, log lines included (at or above
    /// [`ServerConfig::log_level`]).
    Device(&'a DeviceEvent),
    /// A `golfer.turn` changed the hints; these are the new ones.
    Hints(&'a TurnHints),
}

/// Why [`FrpServer::run`] ended with an error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError<E: Error + 'static> {
    /// The endpoint could not open.
    #[error("endpoint: {0}")]
    Endpoint(#[from] FrpError),
    /// A helper thread could not start.
    #[error("thread: {0}")]
    Thread(std::io::Error),
    /// The driver gave up: it advised no retry.
    #[error("driver gave up: {0}")]
    Driver(#[source] E),
    /// The network thread ended before the server stopped it (it panicked):
    /// the actor is off the bus.
    #[error("the network thread ended")]
    Network,
}

type Observer = Box<dyn FnMut(&ServerEvent<'_>)>;

/// Runs a driver as a launch monitor actor on the bus. See the
/// [module docs](self).
pub struct FrpServer<D: DeviceDriver> {
    driver: D,
    config: ServerConfig,
    observer: Option<Observer>,
}

impl<D: DeviceDriver> fmt::Debug for FrpServer<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrpServer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Device thread to network thread.
enum ToNet {
    /// Boxed: a body is large, and the queue holds many.
    Message(Box<FrpBody>),
    Status(DeviceStatus),
    StatusChange(StatusChange),
    Identified(DeviceIdentity),
    SessionEnded,
    Shutdown,
}

/// Network and acceptor threads to the device thread, for the observer.
enum Note {
    PeerConnected(Option<SocketAddr>),
    PeerLeft(Option<SocketAddr>, String),
    Log(Level, String),
}

impl<D: DeviceDriver> FrpServer<D> {
    /// A server for `driver`.
    #[must_use]
    pub fn new(driver: D, config: ServerConfig) -> Self {
        Self {
            driver,
            config,
            observer: None,
        }
    }

    /// Observe what happens, e.g. for logging. The closure runs on the thread
    /// that calls [`run`](Self::run), so it needs no `Send`.
    #[must_use]
    pub fn on_event(mut self, f: impl FnMut(&ServerEvent<'_>) + 'static) -> Self {
        self.observer = Some(Box::new(f));
        self
    }

    /// Serve until `stop` (`Ok`) or a fatal error: the endpoint cannot open,
    /// the driver gave up, or the network thread ended. On a stop the session is closed first, then the
    /// actor's removal goes out.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Endpoint`] when the endpoint cannot open,
    /// [`ServerError::Driver`] when [`retry_delay`](DeviceDriver::retry_delay)
    /// returned `None`, and [`ServerError::Network`] when the network thread
    /// ended on its own; the session, if any, is closed first.
    #[allow(clippy::too_many_lines)]
    pub fn run(self, stop: &Stop) -> Result<(), ServerError<D::Error>> {
        let Self {
            mut driver,
            config,
            observer,
        } = self;
        let info = D::info(driver.config());
        let observer = RefCell::new(observer);
        let observe = |event: &ServerEvent<'_>| {
            if let Some(f) = observer.borrow_mut().as_mut() {
                f(event);
            }
        };

        let flags = Arc::new(HostFlags::new());
        flags.set_min_level(config.log_level);
        flags.set_wants_raw(config.raw_frames);
        let inner = Stop::new();

        let bridge = matches!(config.target, Transport::Connect(_));
        if let Transport::Connect(url) = &config.target
            && !url
                .get(..5)
                .is_some_and(|s| s.eq_ignore_ascii_case("ws://"))
        {
            return Err(ServerError::Endpoint(FrpError::Handshake(format!(
                "cannot connect to {url:?}: only ws:// is supported"
            ))));
        }
        let endpoint = FrpEndpoint::open(
            EndpointConfig::new(config.target.clone())
                .with_handshake_timeout(config.handshake_timeout)
                .with_write_buffer_limit(config.write_buffer_limit),
        )?;
        if let Some(addr) = endpoint.local_addr()? {
            observe(&ServerEvent::Listening(addr));
        }

        let (to_net, net_rx) = sync_channel::<ToNet>(QUEUE);
        let (hints_tx, hints_rx) = channel::<TurnHints>();
        let (note_tx, note_rx) = channel::<Note>();
        let (conn_tx, conn_rx) = channel::<FrpConnection>();
        let (down_tx, down_rx) = channel::<()>();

        let net = Net::new(
            &config,
            &info,
            Links {
                rx: net_rx,
                conn_rx,
                hints_tx,
                note_tx: note_tx.clone(),
                down_tx: bridge.then_some(down_tx),
            },
        );
        let net_thread = std::thread::Builder::new()
            .name("frp-net".into())
            .spawn(move || net.run())
            .map_err(ServerError::Thread)?;
        let acceptor_stop = inner.clone();
        let acceptor = std::thread::Builder::new()
            .name("frp-accept".into())
            .spawn(move || {
                accept_loop(
                    endpoint,
                    bridge,
                    &acceptor_stop,
                    &conn_tx,
                    &note_tx,
                    &down_rx,
                );
            })
            .map_err(ServerError::Thread);
        let acceptor = match acceptor {
            Ok(handle) => handle,
            Err(e) => {
                push(&to_net, ToNet::Shutdown);
                let _ = net_thread.join();
                return Err(e);
            }
        };

        let mut sink = |event: DeviceEvent| {
            observe(&ServerEvent::Device(&event));
            match event {
                DeviceEvent::Frp(body) => push(&to_net, ToNet::Message(Box::new(body))),
                DeviceEvent::Status(status) => push(&to_net, ToNet::Status(status)),
                DeviceEvent::StatusChange(change) => push(&to_net, ToNet::StatusChange(change)),
                _ => {}
            }
        };

        let mut hints = TurnHints::default();
        let mut backoff = Backoff::default();
        let mut attempts: u32 = 0;
        let mut inputs = Inputs {
            hints_rx: &hints_rx,
            note_rx: &note_rx,
            observe: &observe,
            log_level: config.log_level,
        };

        let outcome = loop {
            if stop.is_stopped() {
                break Ok(());
            }
            if net_thread.is_finished() {
                break Err(ServerError::Network);
            }
            let connected = {
                let mut host = Host {
                    inputs: &mut inputs,
                    hints: &mut hints,
                };
                let mut ctx = ConnectCtx::new(stop, Events::new(&mut sink, &flags), &mut host);
                driver.connect(&mut ctx)
            };
            let mut net_down = false;
            let (error, connected_for) = match connected {
                Ok(mut session) => {
                    attempts = 0;
                    let started = Instant::now();
                    let identity = session.identity().clone();
                    push(&to_net, ToNet::Identified(identity.clone()));
                    observe(&ServerEvent::DeviceConnected(&identity));
                    let error = loop {
                        if stop.is_stopped() {
                            break None;
                        }
                        if net_thread.is_finished() {
                            net_down = true;
                            break None;
                        }
                        let mut failed = None;
                        if inputs.drain(&mut hints)
                            && let Err(e) = session.hints(&hints)
                        {
                            failed = Some(e);
                        }
                        if failed.is_none() {
                            failed = session
                                .poll(POLL, &mut Events::new(&mut sink, &flags))
                                .err();
                        }
                        if failed.is_some() {
                            break failed;
                        }
                    };
                    if let Err(e) = session.close(&mut Events::new(&mut sink, &flags)) {
                        sink(DeviceEvent::Log(
                            Level::Warn,
                            format!("closing the device: {e}"),
                        ));
                    }
                    push(&to_net, ToNet::SessionEnded);
                    (error, Some(started.elapsed()))
                }
                Err(e) => {
                    attempts = attempts.saturating_add(1);
                    (Some(e), None)
                }
            };
            if stop.is_stopped() {
                break Ok(());
            }
            if net_down || net_thread.is_finished() {
                break Err(ServerError::Network);
            }
            let delay = driver.retry_delay(
                &SessionEnd {
                    error: error.as_ref(),
                    connected_for,
                    attempts,
                },
                &mut backoff,
                &mut Events::new(&mut sink, &flags),
            );
            observe(&ServerEvent::DeviceLost {
                error: error.as_ref().map(|e| e as &dyn Error),
                retry_in: delay,
            });
            let Some(delay) = delay else {
                break match error {
                    Some(e) => Err(ServerError::Driver(e)),
                    None => Ok(()),
                };
            };
            let until = Instant::now() + delay;
            while Instant::now() < until && !stop.is_stopped() && !net_thread.is_finished() {
                let _ = inputs.drain(&mut hints);
                std::thread::sleep(
                    until
                        .saturating_duration_since(Instant::now())
                        .min(SLEEP_SLICE),
                );
            }
        };

        push(&to_net, ToNet::Shutdown);
        let joined = net_thread.join();
        inputs.pump_notes();
        inner.stop();
        // An upgrade in progress can hold the acceptor for up to the
        // handshake timeout; it ends on its own, so don't wait for it.
        let waited = Instant::now();
        while !acceptor.is_finished() && waited.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(10));
        }
        match (outcome, joined) {
            (Ok(()), Err(_)) => Err(ServerError::Network),
            (outcome, _) => outcome,
        }
    }
}

/// The device thread's view of what the other threads send it.
struct Inputs<'a> {
    hints_rx: &'a Receiver<TurnHints>,
    note_rx: &'a Receiver<Note>,
    observe: &'a dyn Fn(&ServerEvent<'_>),
    log_level: Level,
}

impl Inputs<'_> {
    /// Hands the observer what the network threads noted.
    fn pump_notes(&self) {
        while let Ok(note) = self.note_rx.try_recv() {
            match note {
                Note::PeerConnected(addr) => {
                    (self.observe)(&ServerEvent::PeerConnected { addr });
                }
                Note::PeerLeft(addr, reason) => {
                    (self.observe)(&ServerEvent::PeerLeft {
                        addr,
                        reason: &reason,
                    });
                }
                Note::Log(level, line) => {
                    if level >= self.log_level {
                        (self.observe)(&ServerEvent::Device(&DeviceEvent::Log(level, line)));
                    }
                }
            }
        }
    }

    /// Takes the turns waiting; the last one's hints replace `current`.
    /// Returns whether the hints differ from before.
    fn drain(&mut self, current: &mut TurnHints) -> bool {
        self.pump_notes();
        let before = *current;
        while let Ok(hints) = self.hints_rx.try_recv() {
            *current = hints;
        }
        let changed = *current != before;
        if changed {
            (self.observe)(&ServerEvent::Hints(current));
        }
        changed
    }
}

/// The [`ConnectHost`] of a server: the hints of the last complete turn.
struct Host<'a, 'b> {
    inputs: &'a mut Inputs<'b>,
    hints: &'a mut TurnHints,
}

impl ConnectHost for Host<'_, '_> {
    fn tick(&mut self) {
        let _ = self.inputs.drain(self.hints);
    }

    fn hints(&self) -> &TurnHints {
        self.hints
    }
}

/// Queue a message for the network thread. A full queue drops a raw frame
/// (debug data; `seq` is numbered after the queue, so the loss leaves no gap)
/// and blocks briefly for anything else.
fn push(tx: &SyncSender<ToNet>, msg: ToNet) {
    let droppable = matches!(&msg, ToNet::Message(body) if matches!(**body, FrpBody::ShotRaw(_)));
    let mut msg = match tx.try_send(msg) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => return,
        Err(TrySendError::Full(_)) if droppable => return,
        Err(TrySendError::Full(msg)) => msg,
    };
    let give_up = Instant::now() + FULL_QUEUE_PATIENCE;
    while Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(1));
        match tx.try_send(msg) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(back)) => msg = back,
        }
    }
}

/// Obtains connections for the network thread.
fn accept_loop(
    mut endpoint: FrpEndpoint,
    bridge: bool,
    stop: &Stop,
    conn_tx: &Sender<FrpConnection>,
    note_tx: &Sender<Note>,
    down_rx: &Receiver<()>,
) {
    let mut backoff = Backoff::default();
    let mut last_failure = None;
    while !stop.is_stopped() {
        match endpoint.establish_until(stop.flag()) {
            Ok(Some(conn)) => {
                last_failure = None;
                // A link-down report from before this connection is stale.
                while down_rx.try_recv().is_ok() {}
                if conn_tx.send(conn).is_err() {
                    return;
                }
                if bridge {
                    let up_since = Instant::now();
                    // Wait for the network thread to report the link down.
                    loop {
                        match down_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(()) => break,
                            Err(RecvTimeoutError::Timeout) if !stop.is_stopped() => {}
                            Err(_) => return,
                        }
                    }
                    if !stop.sleep(backoff.delay(Some(up_since.elapsed()))) {
                        return;
                    }
                }
            }
            Ok(None) => return,
            Err(e) => {
                let (level, what) = if bridge {
                    (
                        failure_level(&mut last_failure, &e.to_string()),
                        "cannot reach the bus",
                    )
                } else {
                    (Level::Debug, "connection failed")
                };
                let _ = note_tx.send(Note::Log(level, format!("{what}: {e}")));
                let pause = if bridge {
                    backoff.delay(None)
                } else {
                    Duration::from_millis(100)
                };
                if !stop.sleep(pause) {
                    return;
                }
            }
        }
    }
}

/// The level to log a failed dial at: [`Level::Info`] when `text` differs from
/// the failure before it, [`Level::Debug`] while the same failure repeats.
fn failure_level(last: &mut Option<String>, text: &str) -> Level {
    if last.as_deref() == Some(text) {
        Level::Debug
    } else {
        *last = Some(text.to_owned());
        Level::Info
    }
}

/// The channels of the network thread.
struct Links {
    rx: Receiver<ToNet>,
    conn_rx: Receiver<FrpConnection>,
    hints_tx: Sender<TurnHints>,
    note_tx: Sender<Note>,
    down_tx: Option<Sender<()>>,
}

struct OpenShot {
    key: ShotKey,
    since: Instant,
}

/// The network thread: owns every connection and the actor's state.
struct Net {
    links: Links,
    hub: Hub,
    max_peers: usize,
    actor_id: String,
    actor: BusActor,
    /// The driver's current status, which the actor carries exactly.
    status: DeviceStatus,
    /// The name from the config, which wins over any other.
    name: Option<String>,
    /// The driver's name, until a device is identified.
    fallback_name: String,
    heartbeat: Duration,
    last_beat: Instant,
    raw_frames: bool,
    shot_timeout: Duration,
    seq: u64,
    open: HashMap<String, OpenShot>,
}

impl Net {
    fn new(config: &ServerConfig, info: &DriverInfo, links: Links) -> Self {
        let heartbeat = config.heartbeat.max(MIN_HEARTBEAT);
        let mut actor = BusActor::new(
            config.name.clone().unwrap_or_else(|| info.name.to_owned()),
            heartbeat.as_secs_f64(),
        )
        .with_role(roles::LAUNCH_MONITOR)
        .with_device(DeviceInfo {
            manufacturer: info.manufacturer.map(str::to_owned),
            source: Some(info.source),
            ..DeviceInfo::default()
        });
        let status = DeviceStatus::not_armed();
        actor.blockers.clone_from(&status.blockers);
        Self {
            links,
            hub: Hub::new(),
            max_peers: config.max_peers,
            actor_id: config.actor_id.clone(),
            actor,
            status,
            name: config.name.clone(),
            fallback_name: info.name.to_owned(),
            heartbeat,
            last_beat: Instant::now(),
            raw_frames: config.raw_frames,
            shot_timeout: config.shot_timeout,
            seq: 0,
            open: HashMap::new(),
        }
    }

    fn run(mut self) {
        loop {
            self.accept_new();
            let mut shutdown = false;
            match self.links.rx.recv_timeout(NET_TICK) {
                Ok(msg) => {
                    shutdown |= self.handle(msg);
                    while !shutdown {
                        match self.links.rx.try_recv() {
                            Ok(msg) => shutdown |= self.handle(msg),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => shutdown = true,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => shutdown = true,
            }
            if shutdown {
                self.shut_down();
                return;
            }
            self.expire_shots();
            if self.last_beat.elapsed() >= self.heartbeat {
                self.publish_actor();
            }
            let events = self.hub.poll();
            self.on_hub(events);
        }
    }

    fn note(&self, note: Note) {
        let _ = self.links.note_tx.send(note);
    }

    fn actor_message(&self) -> FrpMessage {
        FrpMessage::new(self.actor_id.clone(), self.actor.clone())
    }

    fn accept_new(&mut self) {
        while let Ok(conn) = self.links.conn_rx.try_recv() {
            if !self.adopt(conn) {
                // Every connection that does not stay is a link that is down,
                // or the bridge would never dial again.
                self.link_down_if_none();
            }
        }
    }

    /// Greets a new connection and takes it into the hub. Returns whether it
    /// stays.
    fn adopt(&mut self, mut conn: FrpConnection) -> bool {
        let addr = conn.peer_addr();
        if self.hub.len() >= self.max_peers {
            self.note(Note::Log(
                Level::Warn,
                format!(
                    "peer {addr:?} refused: {} already connected",
                    self.hub.len()
                ),
            ));
            // Non-blocking, so a peer that never answers the close cannot
            // hold this thread; a connection that cannot be made
            // non-blocking is dropped without one.
            if conn.set_nonblocking(true).is_ok() {
                let _ = conn.close();
            }
            return false;
        }
        if conn.set_nonblocking(true).is_err() || conn.greet(&[self.actor_message()], &[]).is_err()
        {
            return false;
        }
        if self.hub.add(conn).is_err() {
            return false;
        }
        self.note(Note::PeerConnected(addr));
        true
    }

    fn link_down_if_none(&self) {
        if self.hub.is_empty()
            && let Some(tx) = &self.links.down_tx
        {
            let _ = tx.send(());
        }
    }

    /// Returns whether to shut down.
    fn handle(&mut self, msg: ToNet) -> bool {
        match msg {
            ToNet::Message(body) => self.on_driver_message(*body),
            ToNet::Status(status) => {
                if self.apply_status(status) {
                    self.publish_actor();
                }
            }
            ToNet::StatusChange(change) => {
                if self.apply_change(change) {
                    self.publish_actor();
                }
            }
            ToNet::Identified(identity) => {
                if self.apply_identity(&identity) {
                    self.publish_actor();
                }
            }
            ToNet::SessionEnded => self.on_session_ended(),
            ToNet::Shutdown => return true,
        }
        false
    }

    fn on_hub(&mut self, events: Vec<HubEvent>) {
        let mut left = false;
        for event in events {
            match event {
                HubEvent::Message { message, .. } => match &message.body {
                    FrpBody::GolferTurn(turn) => {
                        let _ = self.links.hints_tx.send(TurnHints::from_turn(turn));
                    }
                    FrpBody::BusSync(_) => self.publish_actor(),
                    _ => {}
                },
                HubEvent::Invalid { .. } => {}
                HubEvent::Left { addr, reason, .. } => {
                    self.note(Note::PeerLeft(addr, reason));
                    left = true;
                }
            }
        }
        if left {
            self.link_down_if_none();
        }
    }

    fn publish(&mut self, msg: &FrpMessage) {
        match self.hub.send(msg) {
            Ok(events) => self.on_hub(events),
            Err(e) => self.note(Note::Log(
                Level::Warn,
                format!("cannot send a {} message: {e}", msg.kind()),
            )),
        }
    }

    fn publish_actor(&mut self) {
        self.last_beat = Instant::now();
        let msg = self.actor_message();
        self.publish(&msg);
    }

    fn on_driver_message(&mut self, mut body: FrpBody) {
        match &mut body {
            FrpBody::ShotTrigger(m) => {
                self.open.insert(
                    m.key.shot_id.clone(),
                    OpenShot {
                        key: m.key.clone(),
                        since: Instant::now(),
                    },
                );
            }
            FrpBody::ShotFinished(m) => {
                self.open.remove(&m.key.shot_id);
            }
            FrpBody::ShotRaw(_) if !self.raw_frames => return,
            FrpBody::ShotRaw(frame) => {
                frame.seq = self.seq;
                self.seq += 1;
            }
            FrpBody::ShotLaunch(_)
            | FrpBody::ShotClub(_)
            | FrpBody::ShotImpact(_)
            | FrpBody::ShotFlight(_)
            | FrpBody::BusAlert(_) => {}
            // Not a launch monitor's to send.
            _ => return,
        }
        let msg = FrpMessage::new(self.actor_id.clone(), body);
        self.publish(&msg);
    }

    /// Takes a driver's whole status. Returns whether the actor changed.
    fn apply_status(&mut self, status: DeviceStatus) -> bool {
        self.status = status;
        self.sync_status()
    }

    /// Applies a change to the driver's status. Returns whether the actor
    /// changed.
    fn apply_change(&mut self, change: StatusChange) -> bool {
        self.status.apply(change);
        self.sync_status()
    }

    /// Puts the driver's status on the actor, exactly: blockers, battery,
    /// attitude and telemetry. Returns whether the actor changed.
    fn sync_status(&mut self) -> bool {
        let before = self.actor.clone();
        let status = &self.status;
        self.actor.blockers.clone_from(&status.blockers);
        let device = self.actor.device.get_or_insert_with(DeviceInfo::default);
        device.battery_pct = status.battery_pct.map(|p| p.min(100));
        device.tilt = status.tilt;
        device.roll = status.roll;
        self.actor.telemetry = (!status.telemetry.is_empty()).then(|| status.telemetry.clone());
        self.actor != before
    }

    /// Takes a session's identity into the actor. Returns whether it changed.
    fn apply_identity(&mut self, identity: &DeviceIdentity) -> bool {
        let before = self.actor.clone();
        let device = self.actor.device.get_or_insert_with(DeviceInfo::default);
        if identity.manufacturer.is_some() {
            device.manufacturer.clone_from(&identity.manufacturer);
        }
        device.model.clone_from(&identity.model);
        device.firmware.clone_from(&identity.firmware);
        let made: Vec<&str> = [device.manufacturer.as_deref(), device.model.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        self.actor.name = self.name.clone().unwrap_or_else(|| {
            if made.is_empty() {
                self.fallback_name.clone()
            } else {
                made.join(" ")
            }
        });
        self.actor != before
    }

    /// Finishes the session's open shots and takes what the session
    /// reported off the actor: it is `not_armed`, with no battery, attitude
    /// or telemetry.
    fn on_session_ended(&mut self) {
        self.finish_open_shots("the session ended");
        if self.clear_session_status() {
            self.publish_actor();
        }
    }

    /// Goes back to [`DeviceStatus::not_armed`]: blockers `not_armed`, no
    /// battery, attitude or telemetry. Returns whether the actor changed.
    fn clear_session_status(&mut self) -> bool {
        self.apply_status(DeviceStatus::not_armed())
    }

    fn expire_shots(&mut self) {
        let timeout = self.shot_timeout;
        let stale: Vec<String> = self
            .open
            .iter()
            .filter(|(_, s)| s.since.elapsed() >= timeout)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(shot) = self.open.remove(&id) {
                self.finish(&shot, "it stayed open too long");
            }
        }
    }

    fn finish_open_shots(&mut self, why: &str) {
        let shots: Vec<OpenShot> = self.open.drain().map(|(_, s)| s).collect();
        for shot in &shots {
            self.finish(shot, why);
        }
    }

    fn finish(&mut self, shot: &OpenShot, why: &str) {
        self.note(Note::Log(
            Level::Warn,
            format!("finishing shot {}: {why}", shot.key.shot_number),
        ));
        let msg = FrpMessage::new(
            self.actor_id.clone(),
            ShotFinished {
                key: shot.key.clone(),
            },
        );
        self.publish(&msg);
    }

    fn shut_down(&mut self) {
        self.accept_new();
        self.finish_open_shots("the server stopped");
        let gone = FrpMessage::new(self.actor_id.clone(), Removal::of(StateKind::BusActor));
        self.publish(&gone);
        let addrs: Vec<Option<SocketAddr>> = self.hub.peers().map(|(_, addr)| addr).collect();
        self.hub.close_all(Duration::from_millis(100));
        for addr in addrs {
            self.note(Note::PeerLeft(addr, "server stopped".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn net(name: Option<&str>) -> Net {
        let mut config = ServerConfig::new(Transport::listen("127.0.0.1:0"), "7d2f9a41c03be856");
        config.name = name.map(str::to_owned);
        net_with(&config)
    }

    fn net_with(config: &ServerConfig) -> Net {
        let (_to_net, rx) = sync_channel(QUEUE);
        let (hints_tx, _) = channel();
        let (note_tx, _) = channel();
        let (_conn_tx, conn_rx) = channel();
        let info =
            DriverInfo::new("beacon", "Beacon", Source::Virtual).with_manufacturer("Birdie Labs");
        Net::new(
            config,
            &info,
            Links {
                rx,
                conn_rx,
                hints_tx,
                note_tx,
                down_tx: None,
            },
        )
    }

    #[test]
    fn a_failed_dial_is_logged_at_info_once_per_distinct_error() {
        let mut last = None;
        assert_eq!(failure_level(&mut last, "refused"), Level::Info);
        assert_eq!(failure_level(&mut last, "refused"), Level::Debug);
        assert_eq!(failure_level(&mut last, "timed out"), Level::Info);
        assert_eq!(failure_level(&mut last, "timed out"), Level::Debug);
        assert_eq!(failure_level(&mut last, "refused"), Level::Info);
    }

    #[test]
    fn the_actor_starts_not_armed_with_the_driver_facts() {
        let net = net(None);
        assert_eq!(net.actor.name, "Beacon");
        assert_eq!(net.actor.blockers, ["not_armed"]);
        assert!(net.actor.has_role("launch_monitor"));
        assert!((net.actor.heartbeat - 2.0).abs() < f64::EPSILON);
        let device = net.actor.device.unwrap();
        assert_eq!(device.source, Some(Source::Virtual));
        assert_eq!(device.manufacturer.as_deref(), Some("Birdie Labs"));
    }

    #[test]
    fn changes_apply_to_the_status_and_report_whether_the_actor_changed() {
        let mut net = net(None);
        assert!(net.apply_change(StatusChange::Blockers(vec![])));
        assert!(net.apply_change(StatusChange::Battery(Some(80))));
        assert!(!net.apply_change(StatusChange::Battery(Some(80))));
        let set =
            StatusChange::Telemetry([("radar_mode".to_owned(), Some("armed".to_owned()))].into());
        assert!(net.apply_change(set.clone()));
        assert!(!net.apply_change(set));
        assert!(net.actor.is_clear());
        assert_eq!(net.actor.device.as_ref().unwrap().battery_pct, Some(80));
        assert_eq!(net.actor.telemetry.as_ref().unwrap()["radar_mode"], "armed");
    }

    #[test]
    fn a_driver_can_take_each_value_off_the_actor() {
        let mut net = net(None);
        net.apply_status(
            DeviceStatus::new()
                .with_blockers(["no_ball"])
                .with_battery(80)
                .with_attitude(0.4, -0.2)
                .with_telemetry("radar_mode", "armed"),
        );
        assert!(net.apply_change(StatusChange::Battery(None)));
        assert!(net.apply_change(StatusChange::Attitude {
            tilt: None,
            roll: None
        }));
        assert!(net.apply_change(StatusChange::Telemetry(
            [("radar_mode".to_owned(), None)].into()
        )));
        assert!(net.apply_change(StatusChange::Blockers(vec![])));
        let device = net.actor.device.as_ref().unwrap();
        assert_eq!(
            (device.battery_pct, device.tilt, device.roll),
            (None, None, None)
        );
        assert_eq!(net.actor.telemetry, None, "no keys: no telemetry object");
        assert!(net.actor.is_clear());
        let json = net.actor_message().to_json().unwrap();
        for gone in ["battery_pct", "tilt", "roll", "telemetry", "radar_mode"] {
            assert!(!json.contains(gone), "{gone} is off the bus: {json}");
        }
    }

    #[test]
    fn a_whole_status_replaces_the_one_held() {
        let mut net = net(None);
        net.apply_status(
            DeviceStatus::new()
                .with_battery(80)
                .with_attitude(0.4, -0.2)
                .with_telemetry("radar_mode", "armed")
                .with_telemetry("signal", "good"),
        );
        assert!(net.apply_status(DeviceStatus::new().with_telemetry("signal", "weak")));
        let device = net.actor.device.as_ref().unwrap();
        assert_eq!(
            (device.battery_pct, device.tilt, device.roll),
            (None, None, None),
            "what the new status leaves out is gone"
        );
        assert_eq!(
            net.actor.telemetry,
            Some([("signal".to_owned(), "weak".to_owned())].into())
        );
        assert!(net.actor.is_clear());
        assert!(!net.apply_status(DeviceStatus::new().with_telemetry("signal", "weak")));
    }

    #[test]
    fn the_name_follows_the_identity_unless_configured() {
        let mut net = net(None);
        let identity = DeviceIdentity::new("SERIAL-123")
            .with_model("Eagle One")
            .with_firmware("2.4.1")
            .with_label("Eagle One (SERIAL-123)");
        assert!(net.apply_identity(&identity));
        assert_eq!(net.actor.name, "Birdie Labs Eagle One");
        let device = net.actor.device.as_ref().unwrap();
        assert_eq!(device.firmware.as_deref(), Some("2.4.1"));
        let json = net.actor_message().to_json().unwrap();
        assert!(
            !json.contains("SERIAL-123"),
            "the device id stays off the bus: {json}"
        );
        assert!(!net.apply_identity(&identity));

        let mut named = super::tests::net(Some("Bay 3"));
        named.apply_identity(&identity);
        assert_eq!(named.actor.name, "Bay 3");
    }

    #[test]
    fn a_short_heartbeat_is_raised_once_and_advertised() {
        let mut config = ServerConfig::new(Transport::listen("127.0.0.1:0"), "7d2f9a41c03be856");
        config.heartbeat = Duration::from_millis(1);
        let net = net_with(&config);
        assert_eq!(net.heartbeat, MIN_HEARTBEAT);
        assert!((net.actor.heartbeat - MIN_HEARTBEAT.as_secs_f64()).abs() < f64::EPSILON);
        config.heartbeat = Duration::from_millis(1500);
        let net = net_with(&config);
        assert_eq!(net.heartbeat, Duration::from_millis(1500));
        assert!((net.actor.heartbeat - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_raw_frame_is_dropped_while_raw_frames_is_off() {
        let raw = || FrpBody::ShotRaw(crate::RawFrame::new(crate::RawDir::Rx, 99, vec![1]));
        let mut off = net(None);
        off.on_driver_message(raw());
        assert_eq!(off.seq, 0, "not numbered, not sent");

        let mut config = ServerConfig::new(Transport::listen("127.0.0.1:0"), "7d2f9a41c03be856");
        config.raw_frames = true;
        let mut on = net_with(&config);
        on.on_driver_message(raw());
        on.on_driver_message(raw());
        assert_eq!(on.seq, 2);
    }

    #[test]
    fn a_session_end_clears_what_the_session_reported() {
        let mut net = net(None);
        let identity = DeviceIdentity::new("SERIAL-123")
            .with_model("Eagle One")
            .with_firmware("2.4.1");
        net.apply_identity(&identity);
        let status = DeviceStatus::new()
            .with_battery(80)
            .with_attitude(0.4, -0.2)
            .with_telemetry("radar_mode", "armed");
        assert!(net.apply_status(status));
        net.on_session_ended();
        assert_eq!(net.actor.blockers, ["not_armed"]);
        assert_eq!(net.actor.telemetry, None);
        let device = net.actor.device.as_ref().unwrap();
        assert_eq!(
            (device.battery_pct, device.tilt, device.roll),
            (None, None, None)
        );
        assert_eq!(device.model.as_deref(), Some("Eagle One"), "identity stays");
        assert!(!net.clear_session_status(), "a second end changes nothing");
        assert_eq!(net.status, DeviceStatus::not_armed());
    }
}
