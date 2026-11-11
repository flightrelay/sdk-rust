//! What a driver emits: [`DeviceEvent`] and the [`Events`] sink.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::blockers;
use crate::{
    BusAlert, ClubData, FaceImpact, FlightResult, FrpBody, Launch, RawDir, RawFrame, Severity,
    ShotClub, ShotFinished, ShotFlight, ShotImpact, ShotKey, ShotLaunch, ShotTrigger,
};

/// Log severity of a [`DeviceEvent::Log`] line, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Wire-level detail, e.g. the hex of a frame.
    Trace,
    /// Detail for debugging.
    Debug,
    /// Normal progress.
    Info,
    /// Something went wrong that the driver handled.
    Warn,
}

impl Level {
    fn to_u8(self) -> u8 {
        self as u8
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Trace,
            1 => Self::Debug,
            2 => Self::Info,
            _ => Self::Warn,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
        })
    }
}

/// Something a driver reports to its host.
// Events are passed on one at a time, so the size gap between `Frp` and the
// small variants costs nothing; boxing would only complicate matching.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// A message body of this launch monitor: a `shot.*` message or a
    /// `bus.alert`. The host publishes it with the actor's id as `from`;
    /// other kinds are not the driver's to send, and the host drops them.
    Frp(FrpBody),
    /// The device's whole status: blockers, battery, attitude, telemetry.
    /// It replaces the status the host holds, so a value it leaves out is
    /// off the actor's next `bus.actor`. The host republishes `bus.actor`
    /// when something changed.
    Status(DeviceStatus),
    /// A change to one part of the device's status; the rest stays as the
    /// host holds it. The host republishes `bus.actor` when something
    /// changed.
    StatusChange(StatusChange),
    /// Where a connect or a (re)configure is, for status displays:
    /// `"discovering"`, `"connecting"`, `"handshaking"`, `"configuring"`,
    /// `"arming"`, `"measuring"`. Host-only.
    Phase(&'static str),
    /// A log line for the host's logger. Host-only.
    Log(Level, String),
    /// The raw frames emitted since the last shot finished belong to no
    /// shot (a misread the driver threw away). The host clears its
    /// pre-roll. Host-only.
    ShotDiscarded,
}

/// A launch monitor's whole status, as its `bus.actor` carries it: the
/// blockers, battery, attitude and telemetry. A host holds one per actor and
/// publishes exactly what it holds; a value that is `None` (or a telemetry
/// key that is not there) is not on the bus.
///
/// Build one with [`new`](Self::new) or [`not_armed`](Self::not_armed) and
/// the `with_*` methods, and send it with [`Events::status`]; or change one
/// part at a time with the other [`Events`] helpers.
///
/// ```
/// use flightrelay::driver::DeviceStatus;
///
/// let status = DeviceStatus::new()
///     .with_blockers(["no_ball"])
///     .with_battery(80)
///     .with_attitude(0.4, -0.2)
///     .with_telemetry("radar_mode", "armed");
/// assert_eq!(status.blockers, ["no_ball"]);
/// assert_eq!(status.battery_pct, Some(80));
///
/// // No battery reading any more: the battery leaves the actor.
/// let status = status.with_battery(None);
/// assert_eq!(status.battery_pct, None);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct DeviceStatus {
    /// The device's reasons a shot shouldn't be taken now, all of them;
    /// empty when it sees none. See [`blockers`].
    pub blockers: Vec<String>,
    /// Battery charge, 0 to 100 percent; `None` when there is no reading.
    pub battery_pct: Option<u8>,
    /// Device attitude, degrees; `None` when there is no reading.
    pub tilt: Option<f64>,
    /// Device attitude, degrees; `None` when there is no reading.
    pub roll: Option<f64>,
    /// Free key/value pairs, for display only.
    pub telemetry: BTreeMap<String, String>,
}

impl DeviceStatus {
    /// Nothing in the way, and no battery, attitude or telemetry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocked by [`blockers::NOT_ARMED`], and no battery, attitude or
    /// telemetry: a launch monitor actor's status while no session is up.
    #[must_use]
    pub fn not_armed() -> Self {
        Self::new().with_blockers([blockers::NOT_ARMED])
    }

    /// With these blockers (empty: nothing in the way). Standard names are
    /// in [`blockers`].
    #[must_use]
    pub fn with_blockers<S: Into<String>>(mut self, list: impl IntoIterator<Item = S>) -> Self {
        self.blockers = list.into_iter().map(Into::into).collect();
        self
    }

    /// With this battery charge, 0 to 100 percent (more is read as 100), or
    /// with none.
    #[must_use]
    pub fn with_battery(mut self, pct: impl Into<Option<u8>>) -> Self {
        self.battery_pct = pct.into().map(|p| p.min(100));
        self
    }

    /// With this attitude, degrees; `None` for a value there is no reading
    /// of.
    #[must_use]
    pub fn with_attitude(
        mut self,
        tilt: impl Into<Option<f64>>,
        roll: impl Into<Option<f64>>,
    ) -> Self {
        self.tilt = tilt.into();
        self.roll = roll.into();
        self
    }

    /// With one more telemetry key, replacing its value if it was there.
    #[must_use]
    pub fn with_telemetry(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.telemetry.insert(key.into(), value.into());
        self
    }

    /// Applies a change: the part it names is replaced (or, for telemetry,
    /// each key it names is set or removed), and the rest stays.
    pub fn apply(&mut self, change: StatusChange) {
        match change {
            StatusChange::Blockers(list) => self.blockers = list,
            StatusChange::Battery(pct) => self.battery_pct = pct.map(|p| p.min(100)),
            StatusChange::Attitude { tilt, roll } => {
                self.tilt = tilt;
                self.roll = roll;
            }
            StatusChange::Telemetry(pairs) => {
                for (key, value) in pairs {
                    match value {
                        Some(value) => self.telemetry.insert(key, value),
                        None => self.telemetry.remove(&key),
                    };
                }
            }
        }
    }
}

/// A change to one part of a [`DeviceStatus`] ([`DeviceStatus::apply`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StatusChange {
    /// The blockers, all of them; empty when nothing is in the way.
    Blockers(Vec<String>),
    /// The battery charge, 0 to 100 percent, or `None` for no reading.
    Battery(Option<u8>),
    /// The attitude, degrees; `None` for a value there is no reading of.
    Attitude {
        /// Tilt, degrees.
        tilt: Option<f64>,
        /// Roll, degrees.
        roll: Option<f64>,
    },
    /// Telemetry keys: each `Some` value is set, each `None` key removed.
    /// Keys not named stay.
    Telemetry(BTreeMap<String, Option<String>>),
}

/// Flags and limits the host keeps live for a driver to read through
/// [`Events`].
#[derive(Debug)]
pub struct HostFlags {
    wants_raw: AtomicBool,
    shots_allowed: AtomicBool,
    min_level: AtomicU8,
}

impl Default for HostFlags {
    /// Raw frames off, shots allowed, every log level on.
    fn default() -> Self {
        Self {
            wants_raw: AtomicBool::new(false),
            shots_allowed: AtomicBool::new(true),
            min_level: AtomicU8::new(Level::Trace.to_u8()),
        }
    }
}

impl HostFlags {
    /// Default flags.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the actor's configuration turns raw frames on.
    pub fn set_wants_raw(&self, wanted: bool) {
        self.wants_raw.store(wanted, Ordering::SeqCst);
    }

    /// Whether a virtual driver may start a shot right now.
    pub fn set_shots_allowed(&self, allowed: bool) {
        self.shots_allowed.store(allowed, Ordering::SeqCst);
    }

    /// The lowest log level the host shows; lines below it are dropped at
    /// the source.
    pub fn set_min_level(&self, level: Level) {
        self.min_level.store(level.to_u8(), Ordering::SeqCst);
    }

    /// Whether the actor's configuration turns raw frames on.
    #[must_use]
    pub fn wants_raw(&self) -> bool {
        self.wants_raw.load(Ordering::SeqCst)
    }

    /// Whether a virtual driver may start a shot right now.
    #[must_use]
    pub fn shots_allowed(&self) -> bool {
        self.shots_allowed.load(Ordering::SeqCst)
    }

    /// The lowest log level the host shows.
    #[must_use]
    pub fn min_level(&self) -> Level {
        Level::from_u8(self.min_level.load(Ordering::SeqCst))
    }
}

/// The sink a driver pushes [`DeviceEvent`]s into.
///
/// Events reach the host as they happen, from `connect` too. Two flags the
/// host keeps live are readable here: [`wants_raw`](Self::wants_raw) and
/// [`shots_allowed`](Self::shots_allowed).
pub struct Events<'a> {
    sink: &'a mut dyn FnMut(DeviceEvent),
    flags: &'a HostFlags,
}

impl<'a> Events<'a> {
    /// A sink over a host closure and the host's flags.
    #[must_use]
    pub fn new(sink: &'a mut dyn FnMut(DeviceEvent), flags: &'a HostFlags) -> Self {
        Self { sink, flags }
    }

    /// A shorter-lived sink over the same closure and flags, to hand to a
    /// callee while this one stays usable afterwards.
    #[must_use]
    pub fn reborrow(&mut self) -> Events<'_> {
        Events {
            sink: &mut *self.sink,
            flags: self.flags,
        }
    }

    /// Push any event.
    pub fn emit(&mut self, event: DeviceEvent) {
        (self.sink)(event);
    }

    /// Push a message body.
    pub fn frp(&mut self, body: impl Into<FrpBody>) {
        self.emit(DeviceEvent::Frp(body.into()));
    }

    /// Push the device's whole status, replacing what the host holds: a
    /// value `status` leaves out comes off the actor.
    pub fn status(&mut self, status: DeviceStatus) {
        self.emit(DeviceEvent::Status(status));
    }

    /// Push a change to one part of the status.
    pub fn status_change(&mut self, change: StatusChange) {
        self.emit(DeviceEvent::StatusChange(change));
    }

    /// Whether raw frames are on: the host's configuration for the actor
    /// decides. Skip building frames while it is `false`.
    #[must_use]
    pub fn wants_raw(&self) -> bool {
        self.flags.wants_raw()
    }

    /// Whether a virtual driver may start a shot now. Check it before
    /// numbering a shot or raising a blocker for it, and ask again later when
    /// it is `false`.
    #[must_use]
    pub fn shots_allowed(&self) -> bool {
        self.flags.shots_allowed()
    }

    /// Whether lines at `level` are shown. Check before formatting an
    /// expensive line.
    #[must_use]
    pub fn enabled(&self, level: Level) -> bool {
        level >= self.flags.min_level()
    }

    /// `shot.trigger` without a timestamp.
    pub fn shot_trigger(&mut self, key: ShotKey) {
        self.frp(ShotTrigger::new(key));
    }

    /// `shot.trigger` with the strike time in milliseconds since the Unix
    /// epoch (UTC).
    pub fn shot_trigger_at(&mut self, key: ShotKey, timestamp_ms: u64) {
        self.frp(ShotTrigger::at(key, timestamp_ms));
    }

    /// `shot.launch`.
    pub fn launch(&mut self, key: ShotKey, launch: Launch) {
        self.frp(ShotLaunch { key, launch });
    }

    /// `shot.flight`: the launch monitor's own flight result, at most one per
    /// shot.
    pub fn flight(&mut self, key: ShotKey, flight: FlightResult) {
        self.frp(ShotFlight { key, flight });
    }

    /// `shot.club`.
    pub fn club(&mut self, key: ShotKey, club: ClubData) {
        self.frp(ShotClub { key, club });
    }

    /// `shot.impact`.
    pub fn impact(&mut self, key: ShotKey, impact: FaceImpact) {
        self.frp(ShotImpact { key, impact });
    }

    /// `shot.finished`.
    pub fn shot_finished(&mut self, key: ShotKey) {
        self.frp(ShotFinished { key });
    }

    /// Replace the device's blockers with `list` (empty: nothing in the
    /// way). Standard names are in [`blockers`].
    pub fn blockers<S: Into<String>>(&mut self, list: impl IntoIterator<Item = S>) {
        self.status_change(StatusChange::Blockers(
            list.into_iter().map(Into::into).collect(),
        ));
    }

    /// `ready(true)` clears the blockers; `ready(false)` sets them to
    /// [`blockers::NOT_ARMED`].
    pub fn ready(&mut self, ready: bool) {
        if ready {
            self.blockers(Vec::<String>::new());
        } else {
            self.blockers([blockers::NOT_ARMED]);
        }
    }

    /// The battery charge, 0 to 100 percent (more is read as 100);
    /// `battery(None)` takes the battery off the actor.
    pub fn battery(&mut self, pct: impl Into<Option<u8>>) {
        self.status_change(StatusChange::Battery(pct.into().map(|p| p.min(100))));
    }

    /// The device attitude, degrees; `None` takes a value off the actor
    /// (`attitude(None, None)` takes both).
    pub fn attitude(&mut self, tilt: impl Into<Option<f64>>, roll: impl Into<Option<f64>>) {
        self.status_change(StatusChange::Attitude {
            tilt: tilt.into(),
            roll: roll.into(),
        });
    }

    /// Set free telemetry keys, for display only. Send only what changed:
    /// keys not named stay as they are.
    pub fn telemetry<K: Into<String>, V: Into<String>>(
        &mut self,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) {
        self.status_change(StatusChange::Telemetry(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), Some(v.into())))
                .collect(),
        ));
    }

    /// Take telemetry keys off the actor.
    pub fn clear_telemetry<K: Into<String>>(&mut self, keys: impl IntoIterator<Item = K>) {
        self.status_change(StatusChange::Telemetry(
            keys.into_iter().map(|k| (k.into(), None)).collect(),
        ));
    }

    /// A `bus.alert`.
    pub fn alert(&mut self, severity: Severity, message: impl Into<String>) {
        self.frp(BusAlert::new(severity, message));
    }

    /// A [`DeviceEvent::Phase`].
    pub fn phase(&mut self, phase: &'static str) {
        self.emit(DeviceEvent::Phase(phase));
    }

    /// A log line, unless the host hides `level`.
    pub fn log(&mut self, level: Level, line: impl Into<String>) {
        if self.enabled(level) {
            self.emit(DeviceEvent::Log(level, line.into()));
        }
    }

    /// [`DeviceEvent::ShotDiscarded`].
    pub fn shot_discarded(&mut self) {
        self.emit(DeviceEvent::ShotDiscarded);
    }

    /// A `shot.raw` of one protocol message, stamped with the current time,
    /// with `seq` left at 0 for the host to number. Does nothing while raw
    /// frames are off ([`wants_raw`](Self::wants_raw)). The caller makes sure
    /// the message carries no device or user identity; those go to
    /// [`audit`](Self::audit).
    pub fn raw(&mut self, dir: RawDir, label: impl Into<String>, data: &[u8]) {
        if !self.wants_raw() {
            return;
        }
        let frame = RawFrame {
            timestamp: now_ms(),
            label: Some(label.into()),
            ..RawFrame::new(dir, 0, data.to_vec())
        };
        self.frp(FrpBody::ShotRaw(frame));
    }

    /// A message that must not leave the machine as a `shot.raw`, such as
    /// one that carries identity: a trace log line with the hex, only when
    /// the host shows trace lines.
    pub fn audit(&mut self, dir: RawDir, label: &str, data: &[u8]) {
        if !self.enabled(Level::Trace) {
            return;
        }
        let mut line = format!("{dir} {label} ");
        for byte in data {
            use std::fmt::Write;
            let _ = write!(line, "{byte:02x}");
        }
        self.emit(DeviceEvent::Log(Level::Trace, line));
    }
}

impl fmt::Debug for Events<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Events")
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

/// The current UTC time in milliseconds since the Unix epoch.
fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(flags: &HostFlags, f: impl FnOnce(&mut Events<'_>)) -> Vec<DeviceEvent> {
        let mut seen = Vec::new();
        let mut sink = |e| seen.push(e);
        f(&mut Events::new(&mut sink, flags));
        seen
    }

    #[test]
    fn builders_make_the_matching_messages() {
        let flags = HostFlags::new();
        let key = ShotKey::new("id", 1);
        let seen = collect(&flags, |out| {
            out.shot_trigger(key.clone());
            out.shot_finished(key.clone());
            out.ready(false);
            out.alert(Severity::Warn, "careful");
        });
        let [trigger, finished, ready, alert] = &seen[..] else {
            panic!("{seen:?}");
        };
        assert!(matches!(
            trigger,
            DeviceEvent::Frp(FrpBody::ShotTrigger(ShotTrigger {
                timestamp: None,
                ..
            }))
        ));
        assert!(matches!(
            finished,
            DeviceEvent::Frp(FrpBody::ShotFinished(_))
        ));
        assert!(matches!(
            ready,
            DeviceEvent::StatusChange(StatusChange::Blockers(list)) if list == &["not_armed"]
        ));
        assert!(matches!(
            alert,
            DeviceEvent::Frp(FrpBody::BusAlert(BusAlert {
                severity: Severity::Warn,
                ..
            }))
        ));
    }

    #[test]
    fn status_helpers_change_one_part_each_and_can_clear_it() {
        let flags = HostFlags::new();
        let seen = collect(&flags, |out| {
            out.ready(true);
            out.blockers(["no_ball"]);
            out.battery(140);
            out.battery(None);
            out.attitude(0.4, -0.2);
            out.attitude(None, None);
            out.telemetry([("radar_mode", "armed")]);
            out.clear_telemetry(["radar_mode"]);
        });
        let changes: Vec<&StatusChange> = seen
            .iter()
            .map(|e| match e {
                DeviceEvent::StatusChange(c) => c,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(changes[0], &StatusChange::Blockers(vec![]));
        assert_eq!(changes[1], &StatusChange::Blockers(vec!["no_ball".into()]));
        assert_eq!(changes[2], &StatusChange::Battery(Some(100)));
        assert_eq!(changes[3], &StatusChange::Battery(None));
        assert_eq!(
            changes[4],
            &StatusChange::Attitude {
                tilt: Some(0.4),
                roll: Some(-0.2)
            }
        );
        assert_eq!(
            changes[5],
            &StatusChange::Attitude {
                tilt: None,
                roll: None
            }
        );
        let StatusChange::Telemetry(set) = changes[6] else {
            panic!("{:?}", changes[6]);
        };
        assert_eq!(set["radar_mode"].as_deref(), Some("armed"));
        let StatusChange::Telemetry(cleared) = changes[7] else {
            panic!("{:?}", changes[7]);
        };
        assert_eq!(cleared["radar_mode"], None);
    }

    #[test]
    fn a_whole_status_goes_out_as_one_event() {
        let flags = HostFlags::new();
        let status = DeviceStatus::new()
            .with_battery(90)
            .with_telemetry("radar_mode", "armed");
        let seen = collect(&flags, |out| out.status(status.clone()));
        let [DeviceEvent::Status(sent)] = &seen[..] else {
            panic!("{seen:?}");
        };
        assert_eq!(sent, &status);
    }

    #[test]
    fn the_builders_set_and_clear_each_part() {
        let status = DeviceStatus::not_armed()
            .with_battery(250)
            .with_attitude(1.5, None)
            .with_telemetry("a", "1")
            .with_telemetry("a", "2");
        assert_eq!(status.blockers, ["not_armed"]);
        assert_eq!(status.battery_pct, Some(100));
        assert_eq!((status.tilt, status.roll), (Some(1.5), None));
        assert_eq!(status.telemetry["a"], "2");
        let cleared = status
            .with_blockers(Vec::<String>::new())
            .with_battery(None)
            .with_attitude(None, None);
        assert!(cleared.blockers.is_empty());
        assert_eq!(cleared.battery_pct, None);
        assert_eq!((cleared.tilt, cleared.roll), (None, None));
        assert_eq!(DeviceStatus::new(), DeviceStatus::default());
    }

    #[test]
    fn a_change_replaces_its_part_and_keeps_the_rest() {
        let mut status = DeviceStatus::new()
            .with_blockers(["no_ball"])
            .with_battery(80)
            .with_attitude(0.4, -0.2)
            .with_telemetry("radar_mode", "armed")
            .with_telemetry("tracking_mode", "full");
        status.apply(StatusChange::Battery(None));
        assert_eq!(status.battery_pct, None);
        assert_eq!(status.blockers, ["no_ball"], "other parts stay");
        status.apply(StatusChange::Attitude {
            tilt: None,
            roll: Some(1.0),
        });
        assert_eq!((status.tilt, status.roll), (None, Some(1.0)));
        status.apply(StatusChange::Telemetry(BTreeMap::from([
            ("radar_mode".to_owned(), None),
            ("signal".to_owned(), Some("good".to_owned())),
        ])));
        assert_eq!(
            status.telemetry,
            BTreeMap::from([
                ("signal".to_owned(), "good".to_owned()),
                ("tracking_mode".to_owned(), "full".to_owned()),
            ])
        );
        status.apply(StatusChange::Blockers(vec![]));
        assert!(status.blockers.is_empty());
        status.apply(StatusChange::Battery(Some(200)));
        assert_eq!(status.battery_pct, Some(100));
    }

    #[test]
    fn raw_frames_are_built_only_while_wanted() {
        let flags = HostFlags::new();
        assert!(collect(&flags, |out| out.raw(RawDir::Rx, "x", &[1])).is_empty());
        flags.set_wants_raw(true);
        let seen = collect(&flags, |out| {
            out.raw(RawDir::Tx, "tx result", &[0xde, 0xad]);
        });
        let [DeviceEvent::Frp(FrpBody::ShotRaw(frame))] = &seen[..] else {
            panic!("{seen:?}");
        };
        assert_eq!(frame.dir, RawDir::Tx);
        assert_eq!(frame.seq, 0);
        assert_eq!(frame.label.as_deref(), Some("tx result"));
        assert_eq!(frame.data, [0xde, 0xad]);
        assert!(frame.timestamp.is_some_and(|t| t > 1_600_000_000_000));
    }

    #[test]
    fn audit_lines_are_hex_trace_logs_only_when_trace_is_shown() {
        let flags = HostFlags::new();
        let seen = collect(&flags, |out| {
            out.audit(RawDir::Rx, "rx result", &[0x01, 0xab]);
        });
        let [DeviceEvent::Log(Level::Trace, line)] = &seen[..] else {
            panic!("{seen:?}");
        };
        assert_eq!(line, "rx rx result 01ab");
        flags.set_min_level(Level::Info);
        assert!(collect(&flags, |out| out.audit(RawDir::Rx, "x", &[1])).is_empty());
    }

    #[test]
    fn logs_below_the_host_level_are_dropped_at_the_source() {
        let flags = HostFlags::new();
        flags.set_min_level(Level::Info);
        let seen = collect(&flags, |out| {
            assert!(!out.enabled(Level::Debug));
            assert!(out.enabled(Level::Info));
            out.log(Level::Debug, "no");
            out.log(Level::Info, "yes");
            out.log(Level::Warn, "also");
        });
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn flags_are_live_and_levels_order() {
        let flags = HostFlags::new();
        let mut sink = |_| {};
        let out = Events::new(&mut sink, &flags);
        assert!(out.shots_allowed());
        flags.set_shots_allowed(false);
        assert!(!out.shots_allowed());
        assert!(
            Level::Trace < Level::Debug && Level::Debug < Level::Info && Level::Info < Level::Warn
        );
        for level in [Level::Trace, Level::Debug, Level::Info, Level::Warn] {
            flags.set_min_level(level);
            assert_eq!(flags.min_level(), level);
        }
    }
}
