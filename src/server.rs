//! [`FrpServer`]: serve a [`DeviceDriver`] to FRP controllers.
//!
//! Requires the `server` feature. The server connects the driver, polls its
//! session and turns what the driver emits into FRP for every controller
//! that connects (listen mode) or for the controller it dials (bridge mode).
//! It reconnects the driver with the driver's advised delay, so a host's
//! `main` is a driver, a config and one call; [`run`](crate::run) is that
//! `main`.
//!
//! # Threads
//!
//! The caller's thread is the device thread: [`connect`](DeviceDriver::connect),
//! [`poll`](DeviceSession::poll), [`command`](DeviceSession::command),
//! [`close`](DeviceSession::close) and the retry sleeps all run on it, so a
//! session can be `!Send`. A network thread owns every controller connection
//! (handshake bookkeeping, replay, fan-out, commands), so controllers are
//! served while a connect blocks. An acceptor thread obtains connections:
//! it accepts in listen mode and dials, and redials, in bridge mode.
//!
//! # What the server puts on the wire
//!
//! - `device_telemetry` right after each handshake, from the device's merged
//!   state, once the driver has identified a device; before that there is no
//!   `device` id to send it under. After a loss the state is sent with
//!   `ready=false`.
//! - Until then, and again while the driver reconnects after a loss, what
//!   the driver emits is held back from the old device: an alert goes out as
//!   a protocol-level `alert` (without a `device`, and a `critical` lowered
//!   to `error`, because a `critical` would close the controller) and is not
//!   replayed, `device_telemetry` waits and is sent, merged into one event,
//!   right after the identity telemetry, and anything else is dropped.
//! - [`source`](crate::Source) on every `device_telemetry`, from
//!   [`DriverInfo::source`](crate::driver::DriverInfo::source), and an identity telemetry (manufacturer, model,
//!   firmware, source) at connect, before anything the driver emits.
//! - Every outgoing envelope passes one [`ForwardFilter`], so each lifecycle
//!   event of a shot goes out at most once, and telemetry only when it
//!   changes.
//! - A `shot_finished` for every shot a lost session left open, and for any
//!   shot still open after [`ServerConfig::shot_timeout`] (default
//!   [`DEFAULT_SHOT_TIMEOUT`]).
//! - On device loss, `ready=false` and a warn alert ("Connection lost: …").
//! - `raw_frame` events, numbered per device id across sessions, only to
//!   controllers that asked for them.
//! - A controller's `set_detection_mode` reaches the driver as a
//!   [`DeviceCommand`], each field latched on its own, last writer wins.
//!
//! A controller that stops reading is not disconnected while the kernel
//! buffers it: the server queues its messages, and drops it only once the
//! queue passes [`ServerConfig::write_buffer_limit`].
//!
//! The device thread and the network thread are joined by a bounded queue.
//! When it is full, a `raw_frame` is dropped (debug data; `seq` is numbered
//! on the network thread, after the queue, so the loss leaves no gap for a
//! controller to see) and anything else waits briefly.
//!
//! In bridge mode the server dials its controller again whenever the link
//! drops. A dial that fails is logged at [`Level::Info`] the first time and
//! whenever the error changes, and at [`Level::Debug`] while the same error
//! repeats. The target must be a `ws://` URL; `wss://` is refused when the
//! server starts.
//!
//! ```no_run
//! # use flightrelay::driver::{DeviceDriver, Stop};
//! # fn serve<D: DeviceDriver>(driver: D) -> Result<(), Box<dyn std::error::Error>> {
//! use flightrelay::{FrpServer, ServerConfig, Transport};
//!
//! let config = ServerConfig::new(Transport::listen("0.0.0.0:5880"));
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
    Backoff, ConnectCtx, ConnectHost, DeviceCommand, DeviceDriver, DeviceEvent, DeviceIdentity,
    DeviceSession, Events, HostFlags, Level, SessionEnd, Stop,
};
use crate::endpoint::DEFAULT_WRITE_BUFFER_LIMIT;
use crate::{
    DEFAULT_SHOT_TIMEOUT, EndpointConfig, ForwardFilter, FrpConnection, FrpEndpoint, FrpEnvelope,
    FrpError, FrpEvent, FrpMessage, FrpProtocolMessage, Role, Severity, Source, Transport,
};

/// Capacity of the queue from the device thread to the network thread.
const QUEUE: usize = 1024;

/// How long the device thread blocks on a full queue before it drops an
/// event that is not a raw frame.
const FULL_QUEUE_PATIENCE: Duration = Duration::from_secs(1);

/// How long the network thread waits for the device thread between passes
/// over the controller connections.
const NET_TICK: Duration = Duration::from_millis(10);

/// The most messages the network thread reads from one controller per pass.
const READS_PER_PASS: usize = 64;

/// The device thread's poll timeout.
const POLL: Duration = Duration::from_millis(20);

/// How often the device thread looks at its inputs while it sleeps.
const SLEEP_SLICE: Duration = Duration::from_millis(50);

/// How a server reaches its controllers, and its limits. Built with
/// [`new`](Self::new); the fields are then assigned.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServerConfig {
    /// [`Transport::Listen`] to serve controllers that connect, or
    /// [`Transport::Connect`] to dial one (a bridge).
    pub target: Transport,
    /// The most controllers served at once in listen mode. Default 8.
    pub max_controllers: usize,
    /// How long a handshake may take, so a silent client cannot stall the
    /// acceptor. Default 5 s.
    pub handshake_timeout: Duration,
    /// How many bytes the server queues for a controller that reads slowly
    /// before it drops that controller. Default 4 MiB.
    pub write_buffer_limit: usize,
    /// The lowest level of driver log lines the server lets through to
    /// [`ServerEvent::Device`]. Default [`Level::Info`].
    pub log_level: Level,
    /// How long a shot may stay open before the server finishes it for the
    /// controllers, and how long the duplicate filter remembers an event.
    /// Default [`DEFAULT_SHOT_TIMEOUT`].
    pub shot_timeout: Duration,
}

impl ServerConfig {
    /// A config for `target` with the defaults.
    #[must_use]
    pub fn new(target: Transport) -> Self {
        Self {
            target,
            max_controllers: 8,
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
    /// A controller finished its handshake.
    ControllerConnected {
        /// The controller's `name` from its `start`.
        peer: Option<&'a str>,
    },
    /// A controller left, or was dropped.
    ControllerLeft {
        /// The controller's `name` from its `start`.
        peer: Option<&'a str>,
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
    /// A controller's command reached the driver.
    Command(&'a DeviceCommand),
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
}

type Observer = Box<dyn FnMut(&ServerEvent<'_>)>;

/// Serves a driver to FRP controllers. See the [module docs](self).
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
    Event(FrpEvent),
    Identified(DeviceIdentity),
    SessionEnded { error: Option<String> },
    Shutdown,
}

/// Network and acceptor threads to the device thread, for the observer.
enum Note {
    ControllerConnected {
        peer: Option<String>,
    },
    ControllerLeft {
        peer: Option<String>,
        reason: String,
    },
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
    /// or the driver gave up. On a stop the session is closed first, and each
    /// controller gets a final `ready=false`.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Endpoint`] when the endpoint cannot open and
    /// [`ServerError::Driver`] when [`retry_delay`](DeviceDriver::retry_delay)
    /// returned `None`.
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
            EndpointConfig::new(Role::Device, config.target.clone())
                .with_name("frp-server")
                .with_handshake_timeout(config.handshake_timeout)
                .with_write_buffer_limit(config.write_buffer_limit),
        )?;
        if let Some(addr) = endpoint.local_addr()? {
            observe(&ServerEvent::Listening(addr));
        }

        let (to_net, net_rx) = sync_channel::<ToNet>(QUEUE);
        let (cmd_tx, cmd_rx) = channel::<DeviceCommand>();
        let (note_tx, note_rx) = channel::<Note>();
        let (conn_tx, conn_rx) = channel::<FrpConnection>();
        let (down_tx, down_rx) = channel::<()>();

        let net = Net {
            rx: net_rx,
            conn_rx,
            cmd_tx,
            note_tx: note_tx.clone(),
            down_tx: bridge.then_some(down_tx),
            flags: Arc::clone(&flags),
            source: info.source,
            max_controllers: config.max_controllers,
            conns: Vec::new(),
            filter: ForwardFilter::new().with_window(config.shot_timeout),
            shot_timeout: config.shot_timeout,
            device: None,
            connecting: false,
            pending: Pending::default(),
            seqs: HashMap::new(),
            open: HashMap::new(),
        };
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
            if let DeviceEvent::Frp(frp) = event {
                push(&to_net, ToNet::Event(frp));
            }
        };

        let mut latched = DeviceCommand::default();
        let mut backoff = Backoff::default();
        let mut attempts: u32 = 0;
        let mut inputs = Inputs {
            cmd_rx: &cmd_rx,
            note_rx: &note_rx,
            observe: &observe,
            log_level: config.log_level,
        };

        let outcome = loop {
            if stop.is_stopped() {
                break Ok(());
            }
            let connected = {
                let mut host = Host {
                    inputs: &mut inputs,
                    latched: &mut latched,
                };
                let mut ctx = ConnectCtx::new(stop, Events::new(&mut sink, &flags), &mut host);
                driver.connect(&mut ctx)
            };
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
                        let mut failed = None;
                        for cmd in inputs.drain(&mut latched) {
                            if let Err(e) = session.command(&cmd) {
                                failed = Some(e);
                                break;
                            }
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
                    push(
                        &to_net,
                        ToNet::SessionEnded {
                            error: error.as_ref().map(ToString::to_string),
                        },
                    );
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
            while Instant::now() < until && !stop.is_stopped() {
                let _ = inputs.drain(&mut latched);
                std::thread::sleep(
                    until
                        .saturating_duration_since(Instant::now())
                        .min(SLEEP_SLICE),
                );
            }
        };

        push(&to_net, ToNet::Shutdown);
        let _ = net_thread.join();
        inputs.pump_notes();
        inner.stop();
        // A handshake in progress can hold the acceptor for up to the
        // handshake timeout; it ends on its own, so don't wait for it.
        let waited = Instant::now();
        while !acceptor.is_finished() && waited.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(10));
        }
        outcome
    }
}

/// The device thread's view of what the other threads send it.
struct Inputs<'a> {
    cmd_rx: &'a Receiver<DeviceCommand>,
    note_rx: &'a Receiver<Note>,
    observe: &'a dyn Fn(&ServerEvent<'_>),
    log_level: Level,
}

impl Inputs<'_> {
    /// Hands the observer what the network threads noted.
    fn pump_notes(&self) {
        while let Ok(note) = self.note_rx.try_recv() {
            match note {
                Note::ControllerConnected { peer } => {
                    (self.observe)(&ServerEvent::ControllerConnected {
                        peer: peer.as_deref(),
                    });
                }
                Note::ControllerLeft { peer, reason } => {
                    (self.observe)(&ServerEvent::ControllerLeft {
                        peer: peer.as_deref(),
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

    /// Latches every command waiting and returns them, each with only the
    /// fields its controller sent.
    fn drain(&mut self, latched: &mut DeviceCommand) -> Vec<DeviceCommand> {
        self.pump_notes();
        let mut out = Vec::new();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            latched.merge(&cmd);
            (self.observe)(&ServerEvent::Command(&cmd));
            out.push(cmd);
        }
        out
    }
}

/// The [`ConnectHost`] of a server: the commands latched so far.
struct Host<'a, 'b> {
    inputs: &'a mut Inputs<'b>,
    latched: &'a mut DeviceCommand,
}

impl ConnectHost for Host<'_, '_> {
    fn tick(&mut self) {
        let _ = self.inputs.drain(self.latched);
    }

    fn latched(&self) -> &DeviceCommand {
        self.latched
    }
}

/// Queue a message for the network thread. A full queue drops a raw frame
/// (debug data; `seq` is numbered after the queue, so the loss leaves no gap)
/// and blocks briefly for anything else.
fn push(tx: &SyncSender<ToNet>, msg: ToNet) {
    let droppable = matches!(&msg, ToNet::Event(FrpEvent::RawFrame(_)));
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

/// Obtains controller connections for the network thread.
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
                        "cannot reach the controller",
                    )
                } else {
                    (Level::Debug, "handshake failed")
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

struct Controller {
    conn: FrpConnection,
    peer: Option<String>,
}

/// The `device_telemetry` a driver emitted while no device was identified,
/// merged: the last value of each key.
#[derive(Default)]
struct Pending {
    manufacturer: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    telemetry: HashMap<String, String>,
}

impl Pending {
    fn absorb(&mut self, event: FrpEvent) {
        if let FrpEvent::DeviceTelemetry {
            manufacturer,
            model,
            firmware,
            telemetry,
            ..
        } = event
        {
            self.manufacturer = manufacturer.or(self.manufacturer.take());
            self.model = model.or(self.model.take());
            self.firmware = firmware.or(self.firmware.take());
            self.telemetry.extend(telemetry.into_iter().flatten());
        }
    }

    /// The merged event, if anything was held.
    fn take(&mut self) -> Option<FrpEvent> {
        let held = std::mem::take(self);
        let empty = held.manufacturer.is_none()
            && held.model.is_none()
            && held.firmware.is_none()
            && held.telemetry.is_empty();
        (!empty).then(|| FrpEvent::DeviceTelemetry {
            manufacturer: held.manufacturer,
            model: held.model,
            firmware: held.firmware,
            source: None,
            telemetry: (!held.telemetry.is_empty()).then_some(held.telemetry),
        })
    }
}

struct OpenShot {
    key: crate::ShotKey,
    since: Instant,
}

/// The network thread: owns every controller connection.
struct Net {
    rx: Receiver<ToNet>,
    conn_rx: Receiver<FrpConnection>,
    cmd_tx: Sender<DeviceCommand>,
    note_tx: Sender<Note>,
    down_tx: Option<Sender<()>>,
    flags: Arc<HostFlags>,
    source: Source,
    max_controllers: usize,
    conns: Vec<Controller>,
    filter: ForwardFilter,
    shot_timeout: Duration,
    device: Option<String>,
    /// Whether the driver is between sessions, so its events do not belong to
    /// `device` (the device of the last session, kept for the replay).
    connecting: bool,
    /// Telemetry the driver emitted before it identified a device.
    pending: Pending,
    seqs: HashMap<String, u64>,
    open: HashMap<String, OpenShot>,
}

impl Net {
    fn run(mut self) {
        loop {
            self.accept_new();
            let mut shutdown = false;
            match self.rx.recv_timeout(NET_TICK) {
                Ok(msg) => {
                    shutdown |= self.handle(msg);
                    while !shutdown {
                        match self.rx.try_recv() {
                            Ok(msg) => shutdown |= self.handle(msg),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                shutdown = true;
                            }
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
            self.service();
        }
    }

    fn note(&self, note: Note) {
        let _ = self.note_tx.send(note);
    }

    fn update_flags(&self) {
        let wanted = self.conns.iter().any(|c| c.conn.raw_frames_requested());
        self.flags.set_wants_raw(wanted);
    }

    fn accept_new(&mut self) {
        while let Ok(conn) = self.conn_rx.try_recv() {
            if !self.adopt(conn) {
                // Every connection that does not stay is a link that is down,
                // or the bridge would never dial again.
                self.link_down_if_none();
            }
        }
    }

    /// Takes a new connection into service. Returns whether it stays.
    fn adopt(&mut self, conn: FrpConnection) -> bool {
        let peer = conn.peer_name().map(str::to_owned);
        if self.conns.len() >= self.max_controllers {
            self.note(Note::Log(
                Level::Warn,
                format!(
                    "controller {peer:?} refused: {} already connected",
                    self.conns.len()
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
        if conn.set_nonblocking(true).is_err() {
            return false;
        }
        let mut controller = Controller {
            conn,
            peer: peer.clone(),
        };
        // A late controller gets the device's merged state.
        if let Some(device) = &self.device
            && let Some(event) = self.filter.telemetry(device)
            && controller
                .conn
                .send_envelope(&FrpEnvelope::new(device.clone(), event))
                .is_err()
        {
            return false;
        }
        self.conns.push(controller);
        self.update_flags();
        self.note(Note::ControllerConnected { peer });
        true
    }

    fn link_down_if_none(&self) {
        if self.conns.is_empty()
            && let Some(tx) = &self.down_tx
        {
            let _ = tx.send(());
        }
    }

    /// Returns whether to shut down.
    fn handle(&mut self, msg: ToNet) -> bool {
        match msg {
            ToNet::Event(event) => self.on_event(event),
            ToNet::Identified(identity) => self.on_identified(&identity),
            ToNet::SessionEnded { error } => self.on_session_ended(error.as_deref()),
            ToNet::Shutdown => return true,
        }
        false
    }

    fn on_identified(&mut self, identity: &DeviceIdentity) {
        if let Some(old) = self.device.take()
            && old != identity.device
        {
            self.filter.forget_device(&old);
            self.seqs.remove(&old);
        }
        self.device = Some(identity.device.clone());
        self.connecting = false;
        self.broadcast_event(FrpEvent::DeviceTelemetry {
            manufacturer: identity.manufacturer.clone(),
            model: identity.model.clone(),
            firmware: identity.firmware.clone(),
            source: Some(self.source),
            telemetry: None,
        });
        if let Some(event) = self.pending.take() {
            self.broadcast_event(event);
        }
    }

    fn on_event(&mut self, event: FrpEvent) {
        if self.device.is_none() || self.connecting {
            match event {
                FrpEvent::Alert { severity, message } => {
                    // No device id yet: a protocol-level alert. Critical would
                    // close the controller, which a driver's hint must not.
                    let severity = severity.min(Severity::Error);
                    self.broadcast_protocol(&FrpProtocolMessage::Alert { severity, message });
                }
                FrpEvent::DeviceTelemetry { .. } => self.pending.absorb(event),
                _ => {}
            }
            return;
        }
        match &event {
            FrpEvent::ShotTrigger { key, .. } => {
                self.open.insert(
                    key.shot_id.clone(),
                    OpenShot {
                        key: key.clone(),
                        since: Instant::now(),
                    },
                );
            }
            FrpEvent::ShotFinished { key } => {
                self.open.remove(&key.shot_id);
            }
            _ => {}
        }
        self.broadcast_event(event);
    }

    fn on_session_ended(&mut self, error: Option<&str>) {
        self.connecting = true;
        self.finish_open_shots("the session ended");
        if self.device.is_some() {
            self.broadcast_event(not_ready());
            if let Some(error) = error {
                self.broadcast_event(FrpEvent::Alert {
                    severity: Severity::Warn,
                    message: format!("Connection lost: {error}"),
                });
            }
        }
    }

    fn expire_shots(&mut self) {
        let timeout = self.shot_timeout;
        if self.open.values().any(|s| s.since.elapsed() >= timeout) {
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
            format!(
                "finishing shot {} for the controllers: {why}",
                shot.key.shot_number
            ),
        ));
        self.broadcast_event(FrpEvent::shot_finished(shot.key.clone()));
    }

    fn broadcast_protocol(&mut self, msg: &FrpProtocolMessage) {
        let mut dropped = Vec::new();
        for (i, c) in self.conns.iter_mut().enumerate() {
            if let Err(e) = c.conn.send_protocol(msg) {
                dropped.push((i, e.to_string()));
            }
        }
        self.drop_conns(dropped);
    }

    /// Stamps, numbers, filters and fans out one device event.
    fn broadcast_event(&mut self, mut event: FrpEvent) {
        let Some(device) = self.device.clone() else {
            return;
        };
        match &mut event {
            FrpEvent::DeviceTelemetry { source, .. } => *source = Some(self.source),
            FrpEvent::RawFrame(frame) => {
                let seq = self.seqs.entry(device.clone()).or_insert(0);
                frame.seq = *seq;
                *seq += 1;
            }
            _ => {}
        }
        let raw = matches!(event, FrpEvent::RawFrame(_));
        let envelope = FrpEnvelope::new(device, event);
        if !self.filter.should_forward(&envelope) {
            return;
        }
        let mut dropped = Vec::new();
        for (i, c) in self.conns.iter_mut().enumerate() {
            if raw && !c.conn.raw_frames_requested() {
                continue;
            }
            if let Err(e) = c.conn.send_envelope(&envelope) {
                dropped.push((i, describe(&e)));
            }
        }
        self.drop_conns(dropped);
    }

    fn drop_conns(&mut self, dropped: Vec<(usize, String)>) {
        if dropped.is_empty() {
            return;
        }
        for (i, reason) in dropped.into_iter().rev() {
            let c = self.conns.remove(i);
            self.note(Note::ControllerLeft {
                peer: c.peer,
                reason,
            });
        }
        self.update_flags();
        self.link_down_if_none();
    }

    /// Reads commands and pushes queued messages out, for every controller.
    fn service(&mut self) {
        let mut dropped = Vec::new();
        for (i, c) in self.conns.iter_mut().enumerate() {
            if let Some(reason) = serve_one(c, &self.cmd_tx) {
                dropped.push((i, reason));
            }
        }
        self.drop_conns(dropped);
    }

    fn shut_down(&mut self) {
        self.accept_new();
        self.finish_open_shots("the server stopped");
        if self.device.is_some() {
            self.broadcast_event(not_ready());
        }
        // Flush for about 100 ms, then close without waiting for the answer:
        // a controller that reads more slowly than that sees the connection
        // reset instead of a close.
        for _ in 0..10 {
            for c in &mut self.conns {
                let _ = c.conn.flush();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for c in std::mem::take(&mut self.conns) {
            let _ = c.conn.close();
            self.note(Note::ControllerLeft {
                peer: c.peer,
                reason: "server stopped".into(),
            });
        }
        self.flags.set_wants_raw(false);
    }
}

fn not_ready() -> FrpEvent {
    FrpEvent::DeviceTelemetry {
        manufacturer: None,
        model: None,
        firmware: None,
        source: None,
        telemetry: Some(HashMap::from([("ready".to_owned(), "false".to_owned())])),
    }
}

fn describe(e: &FrpError) -> String {
    match e {
        FrpError::WebSocket(inner) if matches!(**inner, tungstenite::Error::WriteBufferFull(_)) => {
            "write buffer full: the controller reads too slowly".into()
        }
        other => other.to_string(),
    }
}

/// Reads one controller's messages and flushes its queue. Returns why to drop
/// it, if so.
fn serve_one(c: &mut Controller, cmd_tx: &Sender<DeviceCommand>) -> Option<String> {
    for _ in 0..READS_PER_PASS {
        match c.conn.try_recv() {
            Ok(None) => break,
            Ok(Some(FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                mode,
                handed,
                club,
            }))) => {
                let _ = cmd_tx.send(DeviceCommand { mode, handed, club });
            }
            Ok(Some(FrpMessage::Protocol(FrpProtocolMessage::Alert {
                severity: Severity::Critical,
                message,
            }))) => return Some(format!("critical alert: {message}")),
            Ok(Some(_)) | Err(FrpError::Json(_)) => {}
            Err(e) => return Some(describe(&e)),
        }
    }
    c.conn.flush().err().map(|e| describe(&e))
}

#[cfg(test)]
mod tests {
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

    use super::*;
    use crate::{RawDir, RawFrame};

    /// A network thread's state with all its channels in the hands of the
    /// test.
    struct Rig {
        net: Net,
        conn_tx: Sender<FrpConnection>,
        down_rx: Receiver<()>,
        _to_net: SyncSender<ToNet>,
        _notes: Receiver<Note>,
    }

    fn rig(bridge: bool) -> Rig {
        let (to_net, rx) = sync_channel(QUEUE);
        let (cmd_tx, _cmds) = channel();
        let (note_tx, notes) = channel();
        let (conn_tx, conn_rx) = channel();
        let (down_tx, down_rx) = channel();
        let net = Net {
            rx,
            conn_rx,
            cmd_tx,
            note_tx,
            down_tx: bridge.then_some(down_tx),
            flags: Arc::new(HostFlags::new()),
            source: Source::Virtual,
            max_controllers: 8,
            conns: Vec::new(),
            filter: ForwardFilter::new(),
            shot_timeout: DEFAULT_SHOT_TIMEOUT,
            device: None,
            connecting: false,
            pending: Pending::default(),
            seqs: HashMap::new(),
            open: HashMap::new(),
        };
        Rig {
            net,
            conn_tx,
            down_rx,
            _to_net: to_net,
            _notes: notes,
        }
    }

    /// A handshaken device-role connection and the controller's end of it.
    fn pair() -> (FrpConnection, WebSocket<MaybeTlsStream<TcpStream>>) {
        let mut endpoint = FrpEndpoint::open(EndpointConfig::new(
            Role::Device,
            Transport::listen("127.0.0.1:0"),
        ))
        .unwrap();
        let addr = endpoint.local_addr().unwrap().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).unwrap();
            ws.send(Message::text(
                r#"{"kind":"start","version":["0.2.0"],"name":"peer"}"#,
            ))
            .unwrap();
            ws.read().unwrap();
            ws
        });
        let conn = endpoint.establish().unwrap();
        (conn, peer.join().unwrap())
    }

    #[test]
    fn a_connection_whose_replay_fails_reports_the_link_down() {
        let mut rig = rig(true);
        rig.net.on_identified(&DeviceIdentity::new("Dev-1"));
        let (mut conn, peer) = pair();
        // The controller resets the connection: it closes with our message
        // unread, which makes the kernel send a reset.
        conn.send_protocol(&FrpProtocolMessage::Alert {
            severity: Severity::Warn,
            message: "unread".into(),
        })
        .unwrap();
        drop(peer);
        let give_up = Instant::now() + Duration::from_secs(5);
        while conn
            .send_protocol(&FrpProtocolMessage::Alert {
                severity: Severity::Warn,
                message: "probe".into(),
            })
            .is_ok()
        {
            assert!(Instant::now() < give_up, "the reset never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.conn_tx.send(conn).unwrap();
        rig.net.accept_new();
        assert!(rig.net.conns.is_empty());
        assert_eq!(
            rig.down_rx.try_recv(),
            Ok(()),
            "the acceptor is told, or a bridge never dials again"
        );
    }

    #[test]
    fn a_connection_that_stays_is_not_reported_down() {
        let mut rig = rig(true);
        let (conn, _peer) = pair();
        rig.conn_tx.send(conn).unwrap();
        rig.net.accept_new();
        assert_eq!(rig.net.conns.len(), 1);
        assert!(rig.down_rx.try_recv().is_err());
    }

    #[test]
    fn a_stale_link_down_does_not_redial_a_live_link() {
        // A controller that accepts and counts connections.
        let mut controller = FrpEndpoint::open(EndpointConfig::new(
            Role::Controller,
            Transport::listen("127.0.0.1:0"),
        ))
        .unwrap();
        let addr = controller.local_addr().unwrap().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Stop::new();
        let counting = {
            let (accepted, stop) = (Arc::clone(&accepted), stop.clone());
            std::thread::spawn(move || {
                let mut held = Vec::new();
                while let Ok(Some(conn)) = controller.establish_until(stop.flag()) {
                    held.push(conn);
                    accepted.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        let endpoint = FrpEndpoint::open(
            EndpointConfig::new(Role::Device, Transport::connect(format!("ws://{addr}/frp")))
                .with_handshake_timeout(Duration::from_secs(5)),
        )
        .unwrap();
        let (conn_tx, conn_rx) = channel();
        let (note_tx, _notes) = channel();
        let (down_tx, down_rx) = channel();
        // Two reports left over from an earlier link.
        down_tx.send(()).unwrap();
        down_tx.send(()).unwrap();
        let acceptor_stop = stop.clone();
        let acceptor = std::thread::spawn(move || {
            accept_loop(endpoint, true, &acceptor_stop, &conn_tx, &note_tx, &down_rx);
        });
        let _first = conn_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // The first redial would come after the one-second backoff.
        std::thread::sleep(Duration::from_millis(1600));
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "no second dial");
        drop(down_tx);
        stop.stop();
        acceptor.join().unwrap();
        counting.join().unwrap();
    }

    #[test]
    fn a_failed_dial_is_logged_at_info_once_per_distinct_error() {
        let mut last = None;
        assert_eq!(failure_level(&mut last, "refused"), Level::Info);
        assert_eq!(failure_level(&mut last, "refused"), Level::Debug);
        assert_eq!(failure_level(&mut last, "refused"), Level::Debug);
        assert_eq!(failure_level(&mut last, "timed out"), Level::Info);
        assert_eq!(failure_level(&mut last, "timed out"), Level::Debug);
        assert_eq!(failure_level(&mut last, "refused"), Level::Info);
    }

    fn telemetry<const N: usize>(pairs: [(&str, &str); N]) -> FrpEvent {
        FrpEvent::DeviceTelemetry {
            manufacturer: None,
            model: None,
            firmware: None,
            source: None,
            telemetry: Some(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
            ),
        }
    }

    #[test]
    fn telemetry_before_identification_is_merged_not_piled_up() {
        let mut pending = Pending::default();
        assert!(pending.take().is_none());
        for attempt in 0..1000 {
            pending.absorb(telemetry([("phase", "searching"), ("ready", "false")]));
            pending.absorb(telemetry([("attempt", &attempt.to_string())]));
        }
        pending.absorb(FrpEvent::DeviceTelemetry {
            manufacturer: Some("Birdie Labs".into()),
            model: None,
            firmware: None,
            source: None,
            telemetry: Some(HashMap::from([("phase".into(), "found".into())])),
        });
        pending.absorb(FrpEvent::DeviceTelemetry {
            manufacturer: None,
            model: Some("Eagle One".into()),
            firmware: None,
            source: None,
            telemetry: None,
        });
        let Some(FrpEvent::DeviceTelemetry {
            manufacturer,
            model,
            telemetry,
            ..
        }) = pending.take()
        else {
            panic!("one merged telemetry");
        };
        assert_eq!(manufacturer.as_deref(), Some("Birdie Labs"));
        assert_eq!(model.as_deref(), Some("Eagle One"));
        let telemetry = telemetry.unwrap();
        assert_eq!(telemetry.len(), 3, "{telemetry:?}");
        assert_eq!(telemetry["phase"], "found");
        assert_eq!(telemetry["attempt"], "999");
        assert!(pending.take().is_none(), "taking empties it");
    }

    #[test]
    fn a_device_that_is_replaced_forgets_its_raw_frame_numbering() {
        let mut rig = rig(false);
        let frame = || FrpEvent::raw_frame(RawFrame::new(RawDir::Rx, 0, vec![1]));
        rig.net.on_identified(&DeviceIdentity::new("Dev-1"));
        rig.net.on_event(frame());
        rig.net.on_event(frame());
        assert_eq!(rig.net.seqs["Dev-1"], 2);
        // The same device again keeps its numbering.
        rig.net.on_identified(&DeviceIdentity::new("Dev-1"));
        assert_eq!(rig.net.seqs["Dev-1"], 2);
        rig.net.on_identified(&DeviceIdentity::new("Dev-2"));
        assert!(!rig.net.seqs.contains_key("Dev-1"));
    }
}
