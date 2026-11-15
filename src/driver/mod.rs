//! The driver contract: how a launch monitor, or a program, plugs into a host.
//!
//! A driver is anything that speaks FRP on one side and a launch monitor or
//! a program (a simulator, a game) on the other; the "device" in these types
//! is whatever sits at that other end.
//!
//! Requires the `driver` feature, which adds no dependencies. A driver is
//! built in two steps:
//!
//! 1. [`FrpDriver::new`] validates a config and builds the driver, with no
//!    I/O. The driver object holds everything that outlives one connection
//!    (a shot counter, a retry clock).
//! 2. [`FrpDriver::connect`] opens one [`FrpSession`]: transport,
//!    handshake, identity. The host then steps the session until it fails or
//!    the host stops, and always calls [`FrpSession::close`] to release the
//!    device, after a failed step too.
//!
//! Time is an input. A session implements [`FrpSession::step`], which
//! takes `now`, handles what has arrived and what is due, and returns
//! without blocking; every timer compares against that `now`. A host calls
//! `step` in a loop, waiting up to [`IDLE`] after a step that found nothing
//! to do, and a test drives a session the same way in virtual time
//! (`flightrelay::testing::TestHost`, feature `testing`). A connect reads the
//! time with [`ConnectCtx::now`] and lets it pass with [`ConnectCtx::wait`],
//! so its deadlines are the host's too.
//!
//! The contract is synchronous and caller-driven, with no async runtime.
//! The host owns the reconnect loop; the driver only advises the delay
//! ([`FrpDriver::retry_delay`]). A session may be `!Send`, and the driver
//! is `Send`; both are used on one thread.
//!
//! A driver is an actor on the bus, a launch monitor unless its roles say
//! otherwise. It emits [`DeviceEvent`]s into an [`Events`] sink: message
//! bodies of the kinds its roles send (`shot.*` for a launch monitor,
//! `game.*` and `golfer.profile` for a game, and `golfer.select`,
//! `bus.reply` and `bus.alert` for any; [`DriverInfo::sends`] is the rule),
//! which the host drops otherwise;
//! its status (the `blockers`, battery, telemetry and flagged values of its
//! `bus.actor`); and three host-only kinds (phase, log, shot discarded). It
//! sends each shot message once, when it has it, and leaves out any value
//! the device does not have. The host stamps every message with the actor's
//! id and publishes the actor's `bus.actor`.
//!
//! The host holds the driver's current [`DeviceStatus`] and publishes it:
//! the driver sends it whole ([`Events::status`]) or one part at a time
//! ([`Events::blockers`], [`Events::battery`], [`Events::telemetry`],
//! [`Events::flag`]). Its telemetry keys are fixed: the driver declares
//! each one with the value it has while not reported
//! ([`DriverInfo::telemetry`]), and every `bus.actor` carries every one of
//! them, in every state. Every connect starts from
//! [`DeviceStatus::not_armed`], and when a session ends the host goes back
//! to it. While no session is up (a connect runs, or one failed) the actor
//! carries the telemetry and flagged values the driver reports, a
//! connect's at once, but stays blocked by `not_armed` with no battery
//! ([`DeviceStatus::off_session`]); blockers and battery count once the
//! session is up.
//!
//! The driver reads `golfer.turn` as [`TurnHints`]: each turn replaces all
//! the hints (handedness, club, mode), and a field that is `null` or not
//! recognized is no hint for that field. The host keeps the last complete
//! turn's hints when the session actor goes away; the driver applies each
//! field its device supports and ignores the rest, and decides for itself
//! what the hardware does when a field is not given.
//!
//! A host that runs a driver on the bus is `FrpServer` (feature `server`),
//! and `run` (feature `main`) is a complete `main` for it.
//!
//! # A minimal driver
//!
//! ```
//! use std::time::Instant;
//! use flightrelay::driver::{
//!     ConfigError, ConnectCtx, FrpDriver, DeviceIdentity, FrpSession,
//!     DriverInfo, Events, Activity, NoAction, ShotIds, Stopped,
//! };
//! use flightrelay::{Launch, Source, Velocity};
//! use flightrelay::config::DriverConfig;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
//! #[serde(default)]
//! struct Config {}
//! impl DriverConfig for Config {}
//!
//! #[derive(Debug, thiserror::Error)]
//! enum Error {
//!     #[error("stopped")]
//!     Stopped,
//! }
//! impl From<Stopped> for Error {
//!     fn from(_: Stopped) -> Self {
//!         Self::Stopped
//!     }
//! }
//!
//! struct Beacon {
//!     config: Config,
//!     ids: ShotIds,
//! }
//!
//! struct BeaconSession {
//!     identity: DeviceIdentity,
//!     ids: ShotIds,
//!     armed: bool,
//! }
//!
//! impl FrpDriver for Beacon {
//!     type Config = Config;
//!     type Error = Error;
//!     type Action = NoAction;
//!     type Session = BeaconSession;
//!
//!     fn info(_: &Config) -> DriverInfo {
//!         DriverInfo::new("beacon", "Beacon", Source::Virtual)
//!     }
//!     fn new(config: Config) -> Result<Self, ConfigError> {
//!         config.validate()?;
//!         Ok(Self { config, ids: ShotIds::new() })
//!     }
//!     fn config(&self) -> &Config {
//!         &self.config
//!     }
//!     fn apply_config(&mut self, new: &Config) {
//!         self.config = new.clone();
//!     }
//!     fn connect(&mut self, ctx: &mut ConnectCtx<'_>) -> Result<BeaconSession, Error> {
//!         ctx.tick()?; // between steps; `Err(Stopped)` after a stop
//!         Ok(BeaconSession {
//!             identity: DeviceIdentity::new("Beacon-1"),
//!             ids: self.ids.clone(),
//!             armed: false,
//!         })
//!     }
//! }
//!
//! impl FrpSession for BeaconSession {
//!     type Error = Error;
//!     type Action = NoAction;
//!
//!     fn step(&mut self, _now: Instant, out: &mut Events<'_>) -> Result<Activity, Error> {
//!         if !self.armed {
//!             self.armed = true;
//!             out.ready(true); // no blockers
//!         }
//!         Ok(Activity::Idle) // nothing more waiting
//!     }
//!     fn identity(&self) -> &DeviceIdentity {
//!         &self.identity
//!     }
//!     fn close(self, _: &mut Events<'_>) {}
//! }
//! ```

mod events;
mod util;

use std::error::Error;
use std::fmt;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

pub use crate::config::{ConfigError, DriverConfig};
pub use events::{DeviceEvent, DeviceStatus, Events, HostFlags, Level, StatusChange};
pub use util::{Backoff, Live, ShotIds, Stop};

use crate::types::{blockers, roles};
use crate::{
    BusActor, Club, DeviceInfo, FrpBody, FrpMessage, GolferTurn, Handedness, Mode, RawFrame, Source,
};

/// How long a host waits before the next [`FrpSession::step`] after one
/// that returned [`Activity::Idle`]. Also the longest slice of
/// [`ConnectCtx::wait`].
pub const IDLE: Duration = Duration::from_millis(5);

/// What a [`FrpSession::step`] did. A host treats a variant it does not
/// know as [`Activity::Idle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Activity {
    /// This pass did work (handled input, sent something). More may be
    /// waiting, so a host steps again at once.
    Active,
    /// This pass found nothing to do. A host waits up to [`IDLE`] before
    /// the next step.
    Idle,
}

/// How often [`ConnectCtx::run_blocking`] ticks while its helper works.
const HELPER_TICK: Duration = Duration::from_millis(50);

/// The default [`FrpDriver::STOP_WAIT`].
const DEFAULT_STOP_WAIT: Duration = Duration::from_secs(3);

/// How a launch monitor plugs into a host. See the [module docs](self).
pub trait FrpDriver: Send + Sized + 'static {
    /// The driver's settings.
    type Config: DriverConfig;
    /// What a connect or a session can fail with. It converts from
    /// [`Stopped`], so `ctx.tick()?` works.
    type Error: Error + From<Stopped> + Send + Sync + 'static;
    /// Device-specific operations, e.g. "find mat". [`NoAction`] when there
    /// are none.
    type Action: fmt::Debug + Send + 'static;
    /// The connected device. Its error and action types are the driver's.
    type Session: FrpSession<Error = Self::Error, Action = Self::Action>;

    /// Longest time from [`Stop::stop`] until [`connect`](Self::connect)
    /// returns or [`close`](FrpSession::close) ends. Default: 3 s.
    const STOP_WAIT: Duration = DEFAULT_STOP_WAIT;

    /// Static facts, known before any I/O, so a host can classify the device
    /// up front.
    #[must_use]
    fn info(config: &Self::Config) -> DriverInfo;

    /// Validate and build. No I/O. The driver keeps the config and everything
    /// that outlives one connection.
    ///
    /// # Errors
    ///
    /// Returns the [`ConfigError`] of a config that is not usable.
    fn new(config: Self::Config) -> Result<Self, ConfigError>;

    /// The config the driver currently runs with.
    #[must_use]
    fn config(&self) -> &Self::Config;

    /// Open one session: transport, handshake, identity. Blocks, calling
    /// [`ctx.tick()`](ConnectCtx::tick) or [`ctx.wait(d)`](ConnectCtx::wait)
    /// on every pass of a loop and at least every 250 ms; both return
    /// `Err(Stopped)` after a stop, which `?` converts. Deadlines compare
    /// against [`ctx.now()`](ConnectCtx::now). Returns once the device is
    /// identified; configuring and arming may go on inside
    /// [`step`](FrpSession::step). Events emitted
    /// while connecting (a phase, a hint alert, logs) reach the host at once
    /// through `ctx.out`. The connect starts from
    /// [`DeviceStatus::not_armed`]; telemetry and flags it reports go on the
    /// actor at once, its blockers and battery once the session is up
    /// ([`DeviceStatus::off_session`]). When the hints change while it connects, the host
    /// hands the session the new ones ([`hints`](FrpSession::hints)) before
    /// its first step, so a connect may read `ctx.hints()` at any point.
    ///
    /// # Errors
    ///
    /// Returns the reason the device could not be reached.
    fn connect(&mut self, ctx: &mut ConnectCtx<'_>) -> Result<Self::Session, Self::Error>;

    /// How long to wait before the next [`connect`](Self::connect), or `None`
    /// to give up. The host asks after a failed connect or a session that
    /// ended with an error, never after a stop. The host owns `backoff` (one
    /// per driver instance). The default emits a warn alert with the error
    /// for every failed attempt, then asks `backoff`; a driver with its own
    /// alert policy overrides it.
    fn retry_delay(
        &mut self,
        ended: &SessionEnd<'_, Self::Error>,
        backoff: &mut Backoff,
        out: &mut Events<'_>,
    ) -> Option<Duration> {
        out.alert(crate::Severity::Warn, ended.error.to_string());
        Some(backoff.delay(ended.connected_for))
    }

    /// Take a config change: store `new`, so [`config`](Self::config) returns
    /// it and the next [`connect`](Self::connect) uses it. Fields
    /// [`restart_needed`](DriverConfig::restart_needed) says are applied live
    /// also reach the running session, through state the driver shares with
    /// its sessions (a [`Live`] cell). A driver that holds a config has no
    /// default to fall back on, so every driver writes this, usually as one
    /// line.
    fn apply_config(&mut self, new: &Self::Config);
}

/// One connection to a device.
///
/// A session names its own error and action types, which are its driver's
/// ([`FrpDriver::Session`] binds them), so a session generic over its
/// transport (`impl<S> FrpSession for MySession<S>`) implements this trait
/// directly.
pub trait FrpSession: Sized {
    /// What a step can fail with: the driver's [`FrpDriver::Error`].
    type Error;
    /// Device-specific operations: the driver's [`FrpDriver::Action`].
    type Action;

    /// Handle what has arrived and what is due at `now`, push events into
    /// `out`, and return without blocking. Every timer of the session
    /// compares against `now`; a session reads no clock of its own (a device
    /// whose samples carry their own clock, like an input device, may keep
    /// that one). One step is one pass: [`Activity::Active`] when more may be
    /// waiting, [`Activity::Idle`] when nothing is. `Err` ends the session and
    /// the host asks [`retry_delay`](FrpDriver::retry_delay). A wire or
    /// decode error that leaves the session usable is not an `Err`: the
    /// driver raises an alert and returns `Ok`.
    ///
    /// This is the one method a session implements, and the entry point for
    /// a host or a test that runs the session itself.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn step(&mut self, now: Instant, out: &mut Events<'_>) -> Result<Activity, Self::Error>;

    /// The tracking hints of a new `golfer.turn`: the complete set, which
    /// replaces every earlier one. A field that is `None` is no hint for
    /// that field. Apply each field the device supports and ignore the
    /// rest; act only when a field differs from what the device has. The
    /// host calls this whenever the hints change. It records what was asked
    /// and reads no clock; anything timed happens in the next
    /// [`step`](Self::step). What it has to say at once (a log line, an
    /// alert) goes into `out`. The default ignores them.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn hints(&mut self, _hints: &TurnHints, _out: &mut Events<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    /// A bus message the host routes to this session: a finished shot, an
    /// actor's `bus.actor`, a `golfer.turn`. It records the message and reads
    /// no clock; anything timed happens in the next [`step`](Self::step).
    /// Events go into `out`. Which messages a host routes is the host's
    /// choice. The default ignores it.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn bus(&mut self, _msg: &FrpMessage, _out: &mut Events<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    /// A device-specific operation. Like [`hints`](Self::hints), it reads no
    /// clock; anything timed happens in the next [`step`](Self::step), and
    /// events go into `out`. The default ignores it.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn action(&mut self, _action: &Self::Action, _out: &mut Events<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Who the device is. Fixed for the session.
    #[must_use]
    fn identity(&self) -> &DeviceIdentity;

    /// Release the device (disarm, disconnect), bounded by
    /// [`STOP_WAIT`](FrpDriver::STOP_WAIT). The host always calls `close`
    /// when a session ends: on a stop, and after a [`step`](Self::step),
    /// [`hints`](Self::hints), [`bus`](Self::bus) or
    /// [`action`](Self::action) returned `Err`. So a session releases the
    /// device here and nowhere else. Dropping a session without `close`
    /// (a panic, a host that broke the rule) must still free the transport,
    /// only less politely. What goes wrong while releasing goes into `out`
    /// (a log line); the session is over either way.
    fn close(self, out: &mut Events<'_>);
}

/// Returned by [`ConnectCtx::tick`] after a stop; every driver error converts
/// from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stopped")]
pub struct Stopped;

/// A driver with no [`FrpDriver::Action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAction {}

/// How a session ended, as [`FrpDriver::retry_delay`] sees it. A host
/// builds it with [`new`](Self::new).
#[derive(Debug)]
#[non_exhaustive]
pub struct SessionEnd<'a, E> {
    /// What ended the session or failed the connect.
    pub error: &'a E,
    /// When the host asks: the `now` to compare the driver's own timers
    /// against.
    pub now: Instant,
    /// How long the session lasted; `None` when [`connect`](FrpDriver::connect)
    /// failed.
    pub connected_for: Option<Duration>,
    /// Failed connects since the last session that connected, this one
    /// included; 0 right after a session that did connect.
    pub attempts: u32,
}

impl<'a, E> SessionEnd<'a, E> {
    /// How a session ended: its `error`, the host's `now`, how long it was
    /// `connected_for`, and the failed connect `attempts` (see the fields).
    #[must_use]
    pub const fn new(
        error: &'a E,
        now: Instant,
        connected_for: Option<Duration>,
        attempts: u32,
    ) -> Self {
        Self {
            error,
            now,
            connected_for,
            attempts,
        }
    }
}

/// The host side of a connect: what [`ConnectCtx`] asks of it.
pub trait ConnectHost {
    /// Drain the host's inputs (the bus) and refresh the hints.
    fn tick(&mut self);

    /// The hints of the last complete `golfer.turn`.
    fn hints(&self) -> &TurnHints;

    /// The time now. The default reads the clock.
    fn now(&self) -> Instant {
        Instant::now()
    }

    /// Let `d` pass (at most [`IDLE`]). The default sleeps it.
    fn wait(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// What [`FrpDriver::connect`] gets from its host.
pub struct ConnectCtx<'a> {
    /// The host's stop token, watched by [`tick`](Self::tick) and
    /// [`run_blocking`](Self::run_blocking).
    pub(crate) stop: &'a Stop,
    /// The sink for events while connecting.
    pub out: Events<'a>,
    host: &'a mut dyn ConnectHost,
}

impl<'a> ConnectCtx<'a> {
    /// A context over the host's stop token, event sink and inputs.
    #[must_use]
    pub fn new(stop: &'a Stop, out: Events<'a>, host: &'a mut dyn ConnectHost) -> Self {
        Self { stop, out, host }
    }

    /// The hints of the last complete `golfer.turn`; empty before any turn.
    /// Live: a `golfer.turn` that arrives during a long connect shows up
    /// after the next [`tick`](Self::tick).
    #[must_use]
    pub fn hints(&self) -> &TurnHints {
        self.host.hints()
    }

    /// Call on every pass of a connect loop that does not
    /// [`wait`](Self::wait), and at least about every 250 ms. The host
    /// drains its inputs and refreshes [`hints`](Self::hints). Time passes
    /// for a connect only in `wait` and, for a test host, in `tick`, so a
    /// loop that calls neither never reaches a deadline in a test.
    ///
    /// # Errors
    ///
    /// Returns [`Stopped`] after a stop.
    pub fn tick(&mut self) -> Result<(), Stopped> {
        if self.stop.is_stopped() {
            return Err(Stopped);
        }
        self.host.tick();
        Ok(())
    }

    /// The host's time: what a connect's deadlines compare against.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.host.now()
    }

    /// Let `d` pass in slices of at most [`IDLE`], ticking after each.
    ///
    /// # Errors
    ///
    /// Returns [`Stopped`] after a stop.
    pub fn wait(&mut self, d: Duration) -> Result<(), Stopped> {
        let mut left = d;
        loop {
            self.tick()?;
            if left.is_zero() {
                return Ok(());
            }
            let slice = left.min(IDLE);
            self.host.wait(slice);
            left -= slice;
        }
    }

    /// Connect to `addr` within `timeout`, ticking while it waits. The
    /// connect runs on a thread of its own; one that completes after a stop
    /// is closed at once. The outer error is a stop; the inner one is the
    /// connect failing.
    ///
    /// # Errors
    ///
    /// Returns [`Stopped`] after a stop.
    pub fn connect_tcp(
        &mut self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> Result<std::io::Result<TcpStream>, Stopped> {
        let mut stopped = false;
        let result = crate::net::connect_cancelable(addr, timeout, || {
            if self.tick().is_err() {
                stopped = true;
            }
            !stopped
        });
        match result {
            Ok(None) => Err(Stopped),
            Ok(Some(stream)) => Ok(Ok(stream)),
            Err(e) => Ok(Err(e)),
        }
    }

    /// Run blocking work (a BLE scan and connect, a discovery) on a helper
    /// thread, ticking about every 50 ms while it runs, so the host stays
    /// live and sees a stop. `work` gets the stop flag (shared, so it can
    /// hand a clone on to a transport that keeps one), to give up early, and
    /// a log function whose lines reach [`out`](Self::out) while this waits.
    ///
    /// After a stop this waits up to `stop_wait` (a driver passes its
    /// [`STOP_WAIT`](FrpDriver::STOP_WAIT)) for `work` to return. A result
    /// that comes after a stop, then or later, is handed to `release` (on
    /// the helper thread when it comes later), so a device found after
    /// all is let go.
    ///
    /// The outer error is a stop; the inner one is the helper thread
    /// failing to start or panicking.
    ///
    /// # Errors
    ///
    /// Returns [`Stopped`] after a stop.
    pub fn run_blocking<T: Send + 'static>(
        &mut self,
        stop_wait: Duration,
        work: impl FnOnce(&Arc<AtomicBool>, &mut dyn FnMut(Level, String)) -> T + Send + 'static,
        release: fn(T),
    ) -> Result<std::io::Result<T>, Stopped> {
        enum Msg<T> {
            Log(Level, String),
            Done(T),
        }
        self.tick()?;
        // A rendezvous channel: a result is either taken here or, once this
        // side has gone, refused back to the helper, which releases it.
        let (tx, rx) = sync_channel::<Msg<T>>(0);
        let stop = self.stop.clone();
        let spawned = std::thread::Builder::new()
            .name("frp-connect-helper".into())
            .spawn(move || {
                let logs = tx.clone();
                let mut log = |level, line| {
                    let _ = logs.send(Msg::Log(level, line));
                };
                let result = work(stop.shared(), &mut log);
                if let Err(std::sync::mpsc::SendError(Msg::Done(late))) = tx.send(Msg::Done(result))
                {
                    release(late);
                }
            });
        if let Err(e) = spawned {
            return Ok(Err(e));
        }
        let mut ticked = Instant::now();
        loop {
            match rx.recv_timeout(HELPER_TICK) {
                Ok(Msg::Log(level, line)) => self.out.log(level, line),
                Ok(Msg::Done(result)) if self.stop.is_stopped() => {
                    release(result);
                    return Err(Stopped);
                }
                Ok(Msg::Done(result)) => return Ok(Ok(result)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Ok(Err(std::io::Error::other("the helper thread panicked")));
                }
            }
            if ticked.elapsed() >= HELPER_TICK {
                ticked = Instant::now();
                if self.tick().is_err() {
                    break;
                }
            }
        }
        let give_up = Instant::now() + stop_wait;
        loop {
            match rx.recv_timeout(give_up.saturating_duration_since(Instant::now())) {
                Ok(Msg::Log(level, line)) => self.out.log(level, line),
                Ok(Msg::Done(result)) => {
                    release(result);
                    return Err(Stopped);
                }
                Err(_) => return Err(Stopped),
            }
        }
    }
}

impl fmt::Debug for ConnectCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectCtx")
            .field("stopped", &self.stop.is_stopped())
            .field("hints", &self.host.hints())
            .finish_non_exhaustive()
    }
}

/// The tracking hints a launch monitor takes from `golfer.turn`: the
/// golfer's handedness, the club and the mode. A turn is complete, so each
/// one replaces the hints before it; `None` means "no hint for that field"
/// (a `null` club, or a value that was not recognized), the same as never
/// having been told.
///
/// `club` never implies `mode`, and `mode` wins if they conflict. A device
/// that can't select a club uses its nearest or none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TurnHints {
    /// The golfer's handedness.
    pub handedness: Option<Handedness>,
    /// The club in play.
    pub club: Option<Club>,
    /// The mode.
    pub mode: Option<Mode>,
}

impl TurnHints {
    /// The hints a `golfer.turn` carries. A `null` or unrecognized value in
    /// any field is `None` for that field; nothing is carried over from an
    /// earlier turn.
    #[must_use]
    pub fn from_turn(turn: &GolferTurn) -> Self {
        Self {
            handedness: turn.handedness,
            club: turn.club,
            mode: turn.mode,
        }
    }

    /// These hints with `mode` too.
    #[must_use]
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// These hints with `handedness` too.
    #[must_use]
    pub fn with_handedness(mut self, handedness: Handedness) -> Self {
        self.handedness = Some(handedness);
        self
    }

    /// These hints with `club` too.
    #[must_use]
    pub fn with_club(mut self, club: Club) -> Self {
        self.club = Some(club);
        self
    }

    /// Whether no field is given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Static facts about a driver, known before any I/O.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DriverInfo {
    /// Stable short name for logs and a fallback program name, e.g.
    /// `"beacon"`.
    pub kind: &'static str,
    /// Display name, e.g. `"Eagle One launch monitor"`. The name of the
    /// actor's `bus.actor` until a device is identified.
    pub name: &'static str,
    /// The device's manufacturer, when known up front.
    pub manufacturer: Option<&'static str>,
    /// Whether the device measures or simulates.
    pub source: Source,
    /// The device takes one connection at a time (a BLE central, a single
    /// client), so a new session must wait until the old one has closed.
    pub exclusive: bool,
    /// The device has one mode (a putting mat): a host that simulates the
    /// ball flight uses this mode whatever the turn's mode is.
    pub fixed_mode: Option<Mode>,
    /// How a host captures this device's raw frames; `None` for a device
    /// with no wire protocol.
    pub raw: Option<RawProfile>,
    /// The FRP roles of the driver's actor, from [`roles`].
    /// A launch monitor is `&[roles::LAUNCH_MONITOR]`.
    pub roles: &'static [&'static str],
    /// The golfer id namespaces the driver's actor issues, as in
    /// [`BusActor::namespaces`](crate::BusActor::namespaces). Empty for a
    /// device that issues no golfer ids.
    pub namespaces: &'static [&'static str],
    /// Every telemetry key the driver reports, each with the value its
    /// actor carries while the driver has not reported one (`"none"`,
    /// `"unknown"`, `"idle"`). FRP has an actor send every telemetry key it
    /// uses in every `bus.actor`, from its first one on, with a value even
    /// when it has nothing to report (spec, `bus.actor`, "`telemetry`");
    /// [`bus_actor`](Self::bus_actor) publishes exactly these keys. Empty
    /// for a driver with no telemetry.
    pub telemetry: &'static [(&'static str, &'static str)],
}

impl DriverInfo {
    /// Facts for a device of `kind` and `name`: a launch monitor, not
    /// exclusive, no fixed mode, no raw frames.
    #[must_use]
    pub fn new(kind: &'static str, name: &'static str, source: Source) -> Self {
        Self {
            kind,
            name,
            manufacturer: None,
            source,
            exclusive: false,
            fixed_mode: None,
            raw: None,
            roles: &[roles::LAUNCH_MONITOR],
            namespaces: &[],
            telemetry: &[],
        }
    }

    /// Set the manufacturer.
    #[must_use]
    pub fn with_manufacturer(mut self, manufacturer: &'static str) -> Self {
        self.manufacturer = Some(manufacturer);
        self
    }

    /// Mark the device as taking one connection at a time.
    #[must_use]
    pub fn exclusive(mut self) -> Self {
        self.exclusive = true;
        self
    }

    /// Set the device's only mode.
    #[must_use]
    pub fn with_fixed_mode(mut self, mode: Mode) -> Self {
        self.fixed_mode = Some(mode);
        self
    }

    /// Set the actor's FRP roles.
    #[must_use]
    pub fn with_roles(mut self, list: &'static [&'static str]) -> Self {
        self.roles = list;
        self
    }

    /// Set the golfer id namespaces the actor issues.
    #[must_use]
    pub fn with_namespaces(mut self, list: &'static [&'static str]) -> Self {
        self.namespaces = list;
        self
    }

    /// Declare the telemetry keys, each with the value its actor carries
    /// while the driver has not reported one. Every `bus.actor` carries
    /// every key, as FRP asks (spec, `bus.actor`, "`telemetry`").
    #[must_use]
    pub fn with_telemetry(mut self, keys: &'static [(&'static str, &'static str)]) -> Self {
        self.telemetry = keys;
        self
    }

    /// Set the raw-frame profile.
    #[must_use]
    pub fn with_raw(mut self, raw: RawProfile) -> Self {
        self.raw = Some(raw);
        self
    }

    /// The actor's default name: with an identified device, its
    /// manufacturer (the identity's, else this driver's) and model joined
    /// by a space, else [`name`](Self::name).
    #[must_use]
    pub fn actor_name(&self, identity: Option<&DeviceIdentity>) -> String {
        let Some(identity) = identity else {
            return self.name.to_owned();
        };
        let made: Vec<&str> = [
            identity.manufacturer.as_deref().or(self.manufacturer),
            identity.model.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if made.is_empty() {
            self.name.to_owned()
        } else {
            made.join(" ")
        }
    }

    /// Whether a message `body` the driver emits goes on the bus: a kind
    /// its roles send. Any driver sends `bus.alert`, `golfer.select` and
    /// `bus.reply`; a [game](roles::GAME), or a driver that issues golfer
    /// ids ([`namespaces`](Self::namespaces)), sends `game.*` and
    /// `golfer.profile` (and their removals); a
    /// [launch monitor](roles::LAUNCH_MONITOR) sends `shot.*`. Everything
    /// else is the host's (`bus.actor`, `bus.sync`) or another role's
    /// (`golfer.turn`, `practice.*`), and a host drops it.
    ///
    /// `shot.raw` is a `shot.*` kind: only a launch monitor's goes on the
    /// bus. A host that keeps raw frames itself takes them before asking.
    #[must_use]
    pub fn sends(&self, body: &FrpBody) -> bool {
        let kind = body.kind();
        matches!(kind, "bus.alert" | "golfer.select" | "bus.reply")
            || ((self.roles.contains(&roles::GAME) || !self.namespaces.is_empty())
                && (kind.starts_with("game.") || kind == "golfer.profile"))
            || (self.roles.contains(&roles::LAUNCH_MONITOR) && kind.starts_with("shot."))
    }

    /// The driver's `bus.actor`, as a host publishes it: named `name`,
    /// republished every `heartbeat`, with this driver's
    /// [`roles`](Self::roles) and [`namespaces`](Self::namespaces); a
    /// `device` block with the manufacturer (the identity's, else this
    /// driver's), the identity's model and firmware, this driver's
    /// [`source`](Self::source), and the status's battery; the status's
    /// blockers and flagged values; and every declared
    /// [`telemetry`](Self::telemetry) key, with the status's value or, when
    /// it has none, the declared one. `not_armed` is a launch monitor's
    /// blocker, so an actor without that role leaves it out. The device's
    /// own id and label never go on it.
    ///
    /// A telemetry key the status has that is not declared is not
    /// published (a debug build panics on it), nor is a flagged value that
    /// is neither a declared key nor `battery_pct`.
    #[must_use]
    pub fn bus_actor(
        &self,
        name: impl Into<String>,
        heartbeat: Duration,
        identity: Option<&DeviceIdentity>,
        status: &DeviceStatus,
    ) -> BusActor {
        let mut actor = BusActor::new(name, heartbeat.as_secs_f64());
        for role in self.roles {
            actor = actor.with_role(*role);
        }
        for namespace in self.namespaces {
            actor = actor.with_namespace(*namespace);
        }
        let mut actor = actor.with_device(DeviceInfo {
            manufacturer: identity
                .and_then(|i| i.manufacturer.clone())
                .or_else(|| self.manufacturer.map(str::to_owned)),
            model: identity.and_then(|i| i.model.clone()),
            firmware: identity.and_then(|i| i.firmware.clone()),
            source: Some(self.source),
            battery_pct: status.battery_pct.map(|p| p.min(100)),
        });
        actor.blockers.clone_from(&status.blockers);
        if !self.roles.contains(&roles::LAUNCH_MONITOR) {
            actor.blockers.retain(|b| b != blockers::NOT_ARMED);
        }
        let declared = |key: &str| self.telemetry.iter().any(|(d, _)| *d == key);
        debug_assert!(
            status.telemetry.keys().all(|k| declared(k)),
            "a telemetry key is not in DriverInfo::telemetry: {:?}",
            status.telemetry.keys().collect::<Vec<_>>()
        );
        debug_assert!(
            status
                .flagged
                .iter()
                .all(|k| k == "battery_pct" || declared(k)),
            "a flagged value is not battery_pct or in DriverInfo::telemetry: {:?}",
            status.flagged
        );
        let rows = self.telemetry.iter().map(|&(key, unset)| {
            let value = status.telemetry.get(key).map_or(unset, String::as_str);
            (key.to_owned(), value.to_owned())
        });
        actor.telemetry = (!self.telemetry.is_empty()).then(|| rows.collect());
        actor.flagged = status
            .flagged
            .iter()
            .filter(|k| *k == "battery_pct" || declared(k))
            .cloned()
            .collect();
        actor
    }
}

/// How a host captures one driver's raw frames, as code rather than wire
/// data. Built with [`new`](Self::new).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RawProfile {
    /// Names the transport in a capture, e.g. `"example-tcp"`.
    pub transport: &'static str,
    /// How many frames before a shot's trigger a host keeps.
    pub pre_roll_frames: usize,
    /// The most frames a host keeps for one shot.
    pub max_frames: usize,
    /// The most bytes of bulk frames a host keeps for one shot.
    pub max_bulk_bytes: usize,
    /// Whether a frame is bulk data, charged to `max_bulk_bytes`.
    pub is_bulk: fn(&RawFrame) -> bool,
    /// The frame data is UTF-8 text (a JSON protocol), not binary.
    pub text: bool,
}

impl RawProfile {
    /// A profile with every field given.
    #[must_use]
    pub fn new(
        transport: &'static str,
        pre_roll_frames: usize,
        max_frames: usize,
        max_bulk_bytes: usize,
        is_bulk: fn(&RawFrame) -> bool,
        text: bool,
    ) -> Self {
        Self {
            transport,
            pre_roll_frames,
            max_frames,
            max_bulk_bytes,
            is_bulk,
            text,
        }
    }
}

/// Who a connected device is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeviceIdentity {
    /// The device's own id, e.g. a serial, SSID or BLE address, for the
    /// host's logs and displays. It identifies the machine, so a host never
    /// puts it on the bus; the actor id does that job.
    pub device: String,
    /// The manufacturer.
    pub manufacturer: Option<String>,
    /// The model.
    pub model: Option<String>,
    /// The firmware version.
    pub firmware: Option<String>,
    /// Card text for the host's own display, e.g. `"Eagle One (F5:D1:…)"`.
    /// Defaults to manufacturer plus model; see [`label`](Self::label). It
    /// may hold the device's id, so a host never puts it on the bus.
    pub label: Option<String>,
}

impl DeviceIdentity {
    /// An identity with only the `device` id.
    #[must_use]
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            manufacturer: None,
            model: None,
            firmware: None,
            label: None,
        }
    }

    /// Set the manufacturer.
    #[must_use]
    pub fn with_manufacturer(mut self, manufacturer: impl Into<String>) -> Self {
        self.manufacturer = Some(manufacturer.into());
        self
    }

    /// Set the model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the firmware version.
    #[must_use]
    pub fn with_firmware(mut self, firmware: impl Into<String>) -> Self {
        self.firmware = Some(firmware.into());
        self
    }

    /// Set the card text.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The card text: `label`, else manufacturer and model joined by a
    /// space, else the `device` id.
    #[must_use]
    pub fn label(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        let parts: Vec<&str> = [self.manufacturer.as_deref(), self.model.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if parts.is_empty() {
            self.device.clone()
        } else {
            parts.join(" ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_with_a_null_club_after_one_with_a_club_has_no_club() {
        let first = GolferTurn::new(None, Some(Club::Iron7), Handedness::Right, Mode::Full);
        let second = GolferTurn::new(None, None, Handedness::Right, Mode::Chipping);
        assert_eq!(TurnHints::from_turn(&first).club, Some(Club::Iron7));
        let hints = TurnHints::from_turn(&second);
        assert_eq!(hints.club, None);
        assert_eq!(
            hints,
            TurnHints::default()
                .with_handedness(Handedness::Right)
                .with_mode(Mode::Chipping)
        );
    }

    #[test]
    fn an_unknown_handedness_or_mode_is_no_hint() {
        let turn: GolferTurn = serde_json::from_str(
            r#"{"golfer":null,"club":"7I","handedness":"ambidextrous","mode":"scrambling"}"#,
        )
        .unwrap();
        assert_eq!(
            TurnHints::from_turn(&turn),
            TurnHints::default().with_club(Club::Iron7)
        );
    }

    #[test]
    fn a_full_turn_replaces_every_field() {
        let turn = GolferTurn::new(None, Some(Club::Putter), Handedness::Left, Mode::Putting);
        let hints = TurnHints::from_turn(&turn);
        assert_eq!(
            hints,
            TurnHints::default()
                .with_handedness(Handedness::Left)
                .with_club(Club::Putter)
                .with_mode(Mode::Putting)
        );
        assert!(!hints.is_empty());
        assert!(TurnHints::default().is_empty());
    }

    #[test]
    fn identity_label_falls_back() {
        let id = DeviceIdentity::new("EagleOne-X4K2");
        assert_eq!(id.label(), "EagleOne-X4K2");
        let id = id.with_manufacturer("Birdie Labs");
        assert_eq!(id.label(), "Birdie Labs");
        let id = id.with_model("Eagle One");
        assert_eq!(id.label(), "Birdie Labs Eagle One");
        assert_eq!(
            id.with_label("Eagle One (X4K2)").label(),
            "Eagle One (X4K2)"
        );
    }

    #[test]
    fn info_builders() {
        let info = DriverInfo::new("beacon", "Beacon", Source::Measured)
            .with_manufacturer("Birdie Labs")
            .exclusive()
            .with_fixed_mode(Mode::Putting);
        assert!(info.exclusive);
        assert_eq!(info.fixed_mode, Some(Mode::Putting));
        assert_eq!(info.manufacturer, Some("Birdie Labs"));
        assert!(info.raw.is_none());
    }

    #[test]
    fn the_bus_actor_carries_the_facts_identity_and_status() {
        let info = DriverInfo::new("beacon", "Beacon", Source::Measured)
            .with_manufacturer("Birdie Labs")
            .with_namespaces(&["range"])
            .with_telemetry(&[("mode", "unknown"), ("signal", "none")]);
        let status = DeviceStatus::not_armed()
            .with_battery(80)
            .with_telemetry("mode", "full")
            .with_flag("mode", true);
        let actor = info.bus_actor("Bay 3", Duration::from_secs(2), None, &status);
        assert_eq!(actor.name, "Bay 3");
        assert!(actor.has_role(roles::LAUNCH_MONITOR));
        assert_eq!(actor.namespaces, ["range"]);
        assert_eq!(actor.blockers, ["not_armed"]);
        let telemetry = actor.telemetry.as_ref().unwrap();
        assert_eq!(telemetry["mode"], "full");
        assert_eq!(
            telemetry["signal"], "none",
            "a key not reported: its declared value"
        );
        assert_eq!(actor.flagged, ["mode"]);
        let device = actor.device.unwrap();
        assert_eq!(device.manufacturer.as_deref(), Some("Birdie Labs"));
        assert_eq!(device.source, Some(Source::Measured));
        assert_eq!(device.battery_pct, Some(80));
        assert_eq!(info.actor_name(None), "Beacon");

        let identity = DeviceIdentity::new("SERIAL-1")
            .with_model("Eagle One")
            .with_firmware("2.4.1")
            .with_label("Eagle One (SERIAL-1)");
        assert_eq!(info.actor_name(Some(&identity)), "Birdie Labs Eagle One");
        let actor = info.bus_actor("x", Duration::from_secs(2), Some(&identity), &status);
        let device = actor.device.as_ref().unwrap();
        assert_eq!(device.model.as_deref(), Some("Eagle One"));
        assert_eq!(device.firmware.as_deref(), Some("2.4.1"));
        let json = serde_json::to_string(&actor).unwrap();
        assert!(!json.contains("SERIAL-1"), "{json}");
        let theirs = identity.clone().with_manufacturer("Eagle Works");
        assert_eq!(info.actor_name(Some(&theirs)), "Eagle Works Eagle One");
    }

    /// Any driver sends alerts, selects and replies; a game, or a driver
    /// that issues golfer ids, `game.*` and `golfer.profile` with their
    /// removals; a launch monitor `shot.*`. The host's own kinds and other
    /// roles' never pass.
    #[test]
    fn a_drivers_roles_decide_the_kinds_it_sends() {
        use crate::{
            BusAlert, BusReply, BusSync, GameRound, GolferId, GolferProfile, GolferSelect,
            PracticeBlock, Removal, RoundStatus, Severity, ShotFinished, ShotKey, ShotTrigger,
            StateKind,
        };
        let output = DriverInfo::new("o", "O", Source::Virtual).with_roles(&[]);
        let game = DriverInfo::new("g", "G", Source::Virtual).with_roles(&[roles::GAME]);
        let issuer = DriverInfo::new("i", "I", Source::Virtual)
            .with_roles(&[])
            .with_namespaces(&["soft"]);
        let monitor = DriverInfo::new("m", "M", Source::Measured);
        let key = ShotKey::new("shot-1", 1);
        let golfer = GolferId::new("soft", "1").expect("an id");
        let bodies: Vec<(FrpBody, [bool; 4])> = vec![
            (BusAlert::new(Severity::Warn, "w").into(), [true; 4]),
            (GolferSelect::new("r").into(), [true; 4]),
            (
                BusReply::ok("fake.0", "golfer.select", "r").into(),
                [true; 4],
            ),
            (
                GameRound::new("round-1", RoundStatus::Playing, Vec::new(), None, None).into(),
                [false, true, true, false],
            ),
            (
                Removal::of(StateKind::GameRound).into(),
                [false, true, true, false],
            ),
            (
                GolferProfile::new(golfer, "Ann").into(),
                [false, true, true, false],
            ),
            (
                ShotTrigger::new(key.clone()).into(),
                [false, false, false, true],
            ),
            (ShotFinished { key }.into(), [false, false, false, true]),
            (BusActor::new("x", 2.0).into(), [false; 4]),
            (Removal::of(StateKind::BusActor).into(), [false; 4]),
            (
                GolferTurn::new(None, None, Handedness::Right, Mode::Full).into(),
                [false; 4],
            ),
            (
                PracticeBlock::new("b", None, None, 1, 1, 0).into(),
                [false; 4],
            ),
            (BusSync::new().into(), [false; 4]),
            (
                crate::RawFrame::new(crate::RawDir::Rx, 99, vec![1]).into(),
                [false, false, false, true],
            ),
        ];
        for (body, want) in bodies {
            let got = [&output, &game, &issuer, &monitor].map(|info| info.sends(&body));
            assert_eq!(got, want, "{}", body.kind());
        }
    }

    #[test]
    fn not_armed_is_a_launch_monitors_blocker_only() {
        let game = DriverInfo::new("game", "Game", Source::Virtual).with_roles(&[roles::GAME]);
        let status = DeviceStatus::not_armed().with_blockers(["not_armed", "paused"]);
        let actor = game.bus_actor("Game", Duration::from_secs(2), None, &status);
        assert_eq!(actor.blockers, ["paused"]);
    }

    #[test]
    fn blocking_work_reports_its_result_and_logs() {
        let mut host = crate::testing::TestHost::new();
        let got = host.connect_with(|ctx| {
            ctx.run_blocking(
                Duration::from_secs(1),
                |_, log| {
                    log(Level::Info, "scanning".to_owned());
                    7
                },
                |_| {},
            )
        });
        assert_eq!(got.map(Result::ok), Ok(Some(7)));
        assert!(matches!(
            &host.take()[..],
            [DeviceEvent::Log(Level::Info, line)] if line == "scanning"
        ));
    }

    #[test]
    fn blocking_work_found_after_a_stop_is_released() {
        static RELEASED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut host = crate::testing::TestHost::new();
        let stop = host.stop().clone();
        let got = host.connect_with(|ctx| {
            ctx.run_blocking(
                Duration::from_secs(60),
                move |flag, _| {
                    // The device turns up just as the host stops.
                    stop.stop();
                    assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
                    "device"
                },
                |_| {
                    RELEASED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
            )
        });
        assert_eq!(got.map(Result::ok), Err(Stopped));
        assert_eq!(RELEASED.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Nothing {}
    impl DriverConfig for Nothing {}

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Oops(&'static str);
    impl From<Stopped> for Oops {
        fn from(_: Stopped) -> Self {
            Self("stopped")
        }
    }

    struct Plain(Nothing);
    /// Generic over its transport, as a real session is over its stream.
    struct PlainSession<T = ()>(DeviceIdentity, T);

    impl FrpDriver for Plain {
        type Config = Nothing;
        type Error = Oops;
        type Action = NoAction;
        type Session = PlainSession;
        fn info(_: &Nothing) -> DriverInfo {
            DriverInfo::new("plain", "Plain", Source::Virtual)
        }
        fn new(config: Nothing) -> Result<Self, ConfigError> {
            Ok(Self(config))
        }
        fn config(&self) -> &Nothing {
            &self.0
        }
        fn apply_config(&mut self, new: &Nothing) {
            self.0 = new.clone();
        }
        fn connect(&mut self, _: &mut ConnectCtx<'_>) -> Result<PlainSession, Oops> {
            Ok(PlainSession(DeviceIdentity::new("Plain-1"), ()))
        }
    }

    impl<T> FrpSession for PlainSession<T> {
        type Error = Oops;
        type Action = NoAction;
        fn step(&mut self, _: Instant, _: &mut Events<'_>) -> Result<Activity, Oops> {
            Ok(Activity::Idle)
        }
        fn identity(&self) -> &DeviceIdentity {
            &self.0
        }
        fn close(self, _: &mut Events<'_>) {}
    }

    #[test]
    fn the_default_retry_delay_alerts_on_an_error_and_follows_the_backoff() {
        let flags = HostFlags::new();
        let mut alerts = Vec::new();
        let mut sink = |e: DeviceEvent| {
            if let DeviceEvent::Frp(crate::FrpBody::BusAlert(alert)) = e {
                alerts.push((alert.severity, alert.message));
            }
        };
        let mut driver = Plain::new(Nothing {}).unwrap();
        let mut backoff = Backoff::default();
        let mut out = Events::new(&mut sink, &flags);
        let boom = Oops("boom");
        let now = Instant::now();
        let failed = |attempts| SessionEnd::new(&boom, now, None, attempts);
        assert_eq!(
            driver.retry_delay(&failed(1), &mut backoff, &mut out),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            driver.retry_delay(&failed(2), &mut backoff, &mut out),
            Some(Duration::from_secs(2))
        );
        // A session that lasted restarts the sequence.
        let lasted = SessionEnd::new(&boom, now, Some(util::STABLE_SESSION), 0);
        assert_eq!(
            driver.retry_delay(&lasted, &mut backoff, &mut out),
            Some(Duration::from_secs(1))
        );
        assert_eq!(alerts, vec![(crate::Severity::Warn, "boom".to_owned()); 3]);
        // `hints` and `action` default to ignoring what they are given.
        let mut session = PlainSession(DeviceIdentity::new("Plain-1"), ());
        let mut sink = |_| {};
        let mut out = Events::new(&mut sink, &flags);
        assert!(session.hints(&TurnHints::default(), &mut out).is_ok());
        driver.apply_config(&Nothing {});
    }

    struct Host {
        ticks: u32,
        hints: TurnHints,
    }

    impl ConnectHost for Host {
        fn tick(&mut self) {
            self.ticks += 1;
            self.hints.club = Some(Club::Driver);
        }
        fn hints(&self) -> &TurnHints {
            &self.hints
        }
    }

    #[test]
    fn tick_refreshes_the_hints_and_ends_with_a_stop() {
        let stop = Stop::new();
        let flags = HostFlags::new();
        let mut sink = |_| {};
        let mut host = Host {
            ticks: 0,
            hints: TurnHints::default(),
        };
        let mut ctx = ConnectCtx::new(&stop, Events::new(&mut sink, &flags), &mut host);
        assert_eq!(ctx.hints().club, None);
        ctx.tick().expect("running");
        assert_eq!(ctx.hints().club, Some(Club::Driver));
        stop.stop();
        assert_eq!(ctx.tick(), Err(Stopped));
        assert_eq!(host.ticks, 1, "a stopped tick does not reach the host");
    }

    #[test]
    fn a_ticking_connect_reaches_the_host_and_stops() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stop = Stop::new();
        let flags = HostFlags::new();
        let mut sink = |_| {};
        let mut host = Host {
            ticks: 0,
            hints: TurnHints::default(),
        };
        let mut ctx = ConnectCtx::new(&stop, Events::new(&mut sink, &flags), &mut host);
        let stream = ctx
            .connect_tcp(addr, Duration::from_secs(2))
            .expect("not stopped")
            .expect("connected");
        assert_eq!(stream.peer_addr().expect("peer"), addr);
        stop.stop();
        assert_eq!(
            ctx.connect_tcp(addr, Duration::from_secs(2))
                .map(|r| r.is_ok()),
            Err(Stopped)
        );
        assert!(host.ticks >= 1);
    }
}
