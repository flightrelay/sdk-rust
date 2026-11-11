//! The device driver contract: how a launch monitor plugs into a host.
//!
//! Requires the `driver` feature, which adds no dependencies. A driver is
//! built in two steps:
//!
//! 1. [`DeviceDriver::new`] validates a config and builds the driver, with no
//!    I/O. The driver object holds everything that outlives one connection
//!    (a shot counter, a retry clock).
//! 2. [`DeviceDriver::connect`] opens one [`DeviceSession`]: transport,
//!    handshake, identity. The host then calls [`DeviceSession::poll`] until
//!    it fails or the host stops, and [`DeviceSession::close`] to release the
//!    device.
//!
//! The contract is synchronous and caller-driven, with no async runtime.
//! The host owns the reconnect loop; the driver only advises the delay
//! ([`DeviceDriver::retry_delay`]). A session may be `!Send`, and the driver
//! is `Send`; both are used on one thread.
//!
//! A driver is a launch monitor actor on the bus. It emits [`DeviceEvent`]s
//! into an [`Events`] sink: `shot.*` messages and alerts, its status (the
//! `blockers`, battery, attitude and telemetry of its `bus.actor`), and three
//! host-only kinds (phase, log, shot discarded). It sends each shot message
//! once, when it has it, and leaves out any value the device does not have.
//! The host stamps every message with the actor's id and publishes the
//! actor's `bus.actor`.
//!
//! The host holds the driver's current [`DeviceStatus`] and publishes exactly
//! that: the driver sends it whole ([`Events::status`]) or one part at a time
//! ([`Events::blockers`], [`Events::battery`], [`Events::attitude`],
//! [`Events::telemetry`]), and takes a value off the actor the same way
//! (`battery(None)`, [`Events::clear_telemetry`]). When a session ends the
//! host goes back to [`DeviceStatus::not_armed`].
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
//! use std::time::Duration;
//! use flightrelay::driver::{
//!     ConfigError, ConnectCtx, DeviceDriver, DeviceIdentity, DeviceSession, DriverError,
//!     DriverInfo, Events, NoAction, ShotIds, Stopped,
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
//! impl DeviceDriver for Beacon {
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
//! impl DeviceSession for BeaconSession {
//!     type Driver = Beacon;
//!
//!     fn poll(&mut self, timeout: Duration, out: &mut Events<'_>) -> Result<(), Error> {
//!         if !self.armed {
//!             self.armed = true;
//!             out.ready(true); // no blockers
//!         }
//!         std::thread::sleep(timeout);
//!         Ok(())
//!     }
//!     fn identity(&self) -> &DeviceIdentity {
//!         &self.identity
//!     }
//!     fn close(self, _: &mut Events<'_>) -> Result<(), Error> {
//!         Ok(())
//!     }
//! }
//! ```

mod events;
mod util;

use std::error::Error;
use std::fmt;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub use crate::config::{ConfigError, DriverConfig};
pub use events::{DeviceEvent, DeviceStatus, Events, HostFlags, Level, StatusChange};
pub use util::{Backoff, Live, STABLE_SESSION, ShotIds, Stop, connect_tcp};

use crate::{Club, GolferTurn, Handedness, Mode, RawFrame, Source};

/// The default [`DeviceDriver::STOP_WAIT`].
pub const DEFAULT_STOP_WAIT: Duration = Duration::from_secs(3);

/// How a launch monitor plugs into a host. See the [module docs](self).
pub trait DeviceDriver: Send + Sized + 'static {
    /// The driver's settings.
    type Config: DriverConfig;
    /// What a connect or a session can fail with. It converts from
    /// [`Stopped`], so `ctx.tick()?` works.
    type Error: Error + From<Stopped> + Send + Sync + 'static;
    /// Device-specific operations, e.g. "find mat". [`NoAction`] when there
    /// are none.
    type Action: fmt::Debug + Send + 'static;
    /// The connected device. Names this driver back.
    type Session: DeviceSession<Driver = Self>;

    /// Longest time from [`Stop::stop`] until [`connect`](Self::connect)
    /// returns or [`close`](DeviceSession::close) ends.
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
    /// [`ctx.tick()`](ConnectCtx::tick) between steps and at least every
    /// 250 ms inside waits; it returns `Err(Stopped)` after a stop, which `?`
    /// converts. Returns once the device is identified; configuring and
    /// arming may go on inside [`poll`](DeviceSession::poll). Events emitted
    /// while connecting (a phase, a hint alert, logs) reach the host at once
    /// through `ctx.out`.
    ///
    /// # Errors
    ///
    /// Returns the reason the device could not be reached.
    fn connect(&mut self, ctx: &mut ConnectCtx<'_>) -> Result<Self::Session, Self::Error>;

    /// How long to wait before the next [`connect`](Self::connect), or `None`
    /// to give up. The host owns `backoff` (one per driver instance). The
    /// default emits a warn alert with the error for every failed attempt,
    /// then asks `backoff`; a driver with its own alert policy overrides it.
    fn retry_delay(
        &mut self,
        ended: &SessionEnd<'_, Self::Error>,
        backoff: &mut Backoff,
        out: &mut Events<'_>,
    ) -> Option<Duration> {
        if let Some(error) = ended.error {
            out.alert(crate::Severity::Warn, error.to_string());
        }
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
pub trait DeviceSession: Sized {
    /// The driver this session belongs to.
    type Driver: DeviceDriver<Session = Self>;

    /// Push zero or more events into `out`, returning within about `timeout`
    /// (plus one device read). `Err` ends the session and the host asks
    /// [`retry_delay`](DeviceDriver::retry_delay). A wire or decode error that
    /// leaves the session usable is not an `Err`: the driver raises an alert
    /// and returns `Ok`.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn poll(
        &mut self,
        timeout: Duration,
        out: &mut Events<'_>,
    ) -> Result<(), DriverError<Self::Driver>>;

    /// The tracking hints of a new `golfer.turn`: the complete set, which
    /// replaces every earlier one. A field that is `None` is no hint for
    /// that field. Apply each field the device supports and ignore the
    /// rest; act only when a field differs from what the device has. The
    /// host calls this whenever the hints change. The default ignores them.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn hints(&mut self, _hints: &TurnHints) -> Result<(), DriverError<Self::Driver>> {
        Ok(())
    }

    /// A device-specific operation. The default ignores it.
    ///
    /// # Errors
    ///
    /// Returns what ended the session.
    fn action(
        &mut self,
        _action: &DriverAction<Self::Driver>,
    ) -> Result<(), DriverError<Self::Driver>> {
        Ok(())
    }

    /// Who the device is. Fixed for the session.
    #[must_use]
    fn identity(&self) -> &DeviceIdentity;

    /// Release the device (disarm, disconnect), bounded by
    /// [`STOP_WAIT`](DeviceDriver::STOP_WAIT). Dropping a session without
    /// `close` must still free the transport, only less politely.
    ///
    /// # Errors
    ///
    /// Returns why the device could not be released cleanly.
    fn close(self, out: &mut Events<'_>) -> Result<(), DriverError<Self::Driver>>;
}

/// A driver's error type.
pub type DriverError<D> = <D as DeviceDriver>::Error;
/// A driver's action type.
pub type DriverAction<D> = <D as DeviceDriver>::Action;

/// Returned by [`ConnectCtx::tick`] after a stop; every driver error converts
/// from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stopped")]
pub struct Stopped;

/// A driver with no [`DeviceDriver::Action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAction {}

/// How a session ended, as [`DeviceDriver::retry_delay`] sees it. A host
/// builds it with a struct literal.
#[derive(Debug)]
pub struct SessionEnd<'a, E> {
    /// What ended the session or failed the connect; `None` after a clean
    /// stop.
    pub error: Option<&'a E>,
    /// How long the session lasted; `None` when [`connect`](DeviceDriver::connect)
    /// failed.
    pub connected_for: Option<Duration>,
    /// Failed connects since the last session that connected, this one
    /// included; 0 right after a session that did connect.
    pub attempts: u32,
}

/// The host side of a connect: what [`ConnectCtx::tick`] and
/// [`ConnectCtx::hints`] ask of it.
pub trait ConnectHost {
    /// Drain the host's inputs (the bus) and refresh the hints.
    fn tick(&mut self);

    /// The hints of the last complete `golfer.turn`.
    fn hints(&self) -> &TurnHints;
}

/// What [`DeviceDriver::connect`] gets from its host.
pub struct ConnectCtx<'a> {
    /// The stop token. A driver passes `stop.flag()` to a transport layer
    /// that takes a cancel flag.
    pub stop: &'a Stop,
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

    /// Call between steps and between slices of a long wait, at most about
    /// 250 ms apart. The host drains its inputs and refreshes
    /// [`hints`](Self::hints).
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

    /// [`connect_tcp`] that keeps ticking while it waits. The outer error is
    /// a stop; the inner one is the connect failing.
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
    /// Stable kind for logs, config tables and export labels, e.g.
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
}

impl DriverInfo {
    /// Facts for a device of `kind` and `name`: not exclusive, no fixed
    /// mode, no raw frames.
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

    /// Set the raw-frame profile.
    #[must_use]
    pub fn with_raw(mut self, raw: RawProfile) -> Self {
        self.raw = Some(raw);
        self
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
    struct PlainSession(DeviceIdentity);

    impl DeviceDriver for Plain {
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
            Ok(PlainSession(DeviceIdentity::new("Plain-1")))
        }
    }

    impl DeviceSession for PlainSession {
        type Driver = Plain;
        fn poll(&mut self, _: Duration, _: &mut Events<'_>) -> Result<(), Oops> {
            Ok(())
        }
        fn identity(&self) -> &DeviceIdentity {
            &self.0
        }
        fn close(self, _: &mut Events<'_>) -> Result<(), Oops> {
            Ok(())
        }
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
        let failed = |attempts| SessionEnd {
            error: Some(&boom),
            connected_for: None,
            attempts,
        };
        assert_eq!(
            driver.retry_delay(&failed(1), &mut backoff, &mut out),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            driver.retry_delay(&failed(2), &mut backoff, &mut out),
            Some(Duration::from_secs(2))
        );
        // A session that lasted restarts the sequence; a clean end raises no alert.
        let clean = SessionEnd::<Oops> {
            error: None,
            connected_for: Some(STABLE_SESSION),
            attempts: 0,
        };
        assert_eq!(
            driver.retry_delay(&clean, &mut backoff, &mut out),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            alerts,
            [
                (crate::Severity::Warn, "boom".to_owned()),
                (crate::Severity::Warn, "boom".to_owned())
            ]
        );
        // `hints` and `action` default to ignoring what they are given.
        let mut session = PlainSession(DeviceIdentity::new("Plain-1"));
        assert!(session.hints(&TurnHints::default()).is_ok());
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
