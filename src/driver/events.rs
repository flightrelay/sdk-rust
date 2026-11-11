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
    /// A change to the status in the actor's `bus.actor`: blockers, battery,
    /// attitude, telemetry. The host merges it and republishes `bus.actor`
    /// when something changed.
    Status(DeviceStatus),
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

/// A change to a launch monitor's status, as a driver reports it. Each field
/// that is set replaces the host's value; `telemetry` keys merge into the
/// host's. Build one with [`Default`] and struct update syntax, or use the
/// [`Events`] helpers.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct DeviceStatus {
    /// The device's reasons a shot shouldn't be taken now, all of them;
    /// empty when it sees none. See [`blockers`].
    pub blockers: Option<Vec<String>>,
    /// Battery charge, 0 to 100 percent.
    pub battery_pct: Option<u8>,
    /// Device attitude, degrees.
    pub tilt: Option<f64>,
    /// Device attitude, degrees.
    pub roll: Option<f64>,
    /// Free key/value pairs, for display only.
    pub telemetry: BTreeMap<String, String>,
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

    /// Push a status change.
    pub fn status(&mut self, status: DeviceStatus) {
        self.emit(DeviceEvent::Status(status));
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
        self.status(DeviceStatus {
            blockers: Some(list.into_iter().map(Into::into).collect()),
            ..DeviceStatus::default()
        });
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

    /// The battery charge, 0 to 100 percent (more is read as 100).
    pub fn battery(&mut self, pct: u8) {
        self.status(DeviceStatus {
            battery_pct: Some(pct.min(100)),
            ..DeviceStatus::default()
        });
    }

    /// The device attitude, degrees.
    pub fn attitude(&mut self, tilt: f64, roll: f64) {
        self.status(DeviceStatus {
            tilt: Some(tilt),
            roll: Some(roll),
            ..DeviceStatus::default()
        });
    }

    /// Free telemetry keys, for display only. Send only what changed: the
    /// host merges.
    pub fn telemetry<K: Into<String>, V: Into<String>>(
        &mut self,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) {
        self.status(DeviceStatus {
            telemetry: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            ..DeviceStatus::default()
        });
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
        let DeviceEvent::Status(status) = ready else {
            panic!("{ready:?}");
        };
        assert_eq!(
            status.blockers.as_deref(),
            Some(&["not_armed".to_owned()][..])
        );
        assert!(matches!(
            alert,
            DeviceEvent::Frp(FrpBody::BusAlert(BusAlert {
                severity: Severity::Warn,
                ..
            }))
        ));
    }

    #[test]
    fn status_helpers_set_one_field_each() {
        let flags = HostFlags::new();
        let seen = collect(&flags, |out| {
            out.ready(true);
            out.blockers(["no_ball"]);
            out.battery(140);
            out.attitude(0.4, -0.2);
            out.telemetry([("radar_mode", "armed")]);
        });
        let statuses: Vec<&DeviceStatus> = seen
            .iter()
            .map(|e| match e {
                DeviceEvent::Status(s) => s,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(statuses[0].blockers, Some(vec![]));
        assert_eq!(statuses[1].blockers, Some(vec!["no_ball".to_owned()]));
        assert_eq!(statuses[2].battery_pct, Some(100));
        assert_eq!(
            (statuses[3].tilt, statuses[3].roll),
            (Some(0.4), Some(-0.2))
        );
        assert_eq!(statuses[4].telemetry["radar_mode"], "armed");
        assert_eq!(statuses[4].blockers, None, "untouched fields stay unset");
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
