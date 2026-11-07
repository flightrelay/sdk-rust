//! What a driver emits: [`DeviceEvent`] and the [`Events`] sink.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ClubData, FaceImpact, FlightResult, FrpEvent, Launch, RawDir, RawFrame, Severity, ShotKey,
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
    /// An FRP event of this device: shot lifecycle, `device_telemetry`, a
    /// device `alert` or a `raw_frame`. The host wraps it in an envelope
    /// with the session's [`identity().device`](crate::driver::DeviceIdentity::device).
    Frp(FrpEvent),
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

/// Flags and limits the host keeps live for a driver to read through
/// [`Events`].
#[derive(Debug)]
pub struct HostFlags {
    wants_raw: AtomicBool,
    shots_allowed: AtomicBool,
    min_level: AtomicU8,
}

impl Default for HostFlags {
    /// No one wants raw frames, shots are allowed, every log level is on.
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

    /// Whether any consumer wants `raw_frame` events right now.
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

    /// Whether any consumer wants `raw_frame` events right now.
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

    /// Push an FRP event.
    pub fn frp(&mut self, event: FrpEvent) {
        self.emit(DeviceEvent::Frp(event));
    }

    /// Whether any consumer wants `raw_frame` events right now. Live: a
    /// controller can ask for them, or stop, at any time. Skip building
    /// frames while it is `false`.
    #[must_use]
    pub fn wants_raw(&self) -> bool {
        self.flags.wants_raw()
    }

    /// Whether a virtual driver may start a shot now. Check it before
    /// numbering a shot or sending `ready=false`, and ask again later when
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

    /// `shot_trigger` without a timestamp.
    pub fn shot_trigger(&mut self, key: ShotKey) {
        self.frp(FrpEvent::shot_trigger(key));
    }

    /// `shot_trigger` with the strike time in milliseconds since the Unix
    /// epoch (UTC).
    pub fn shot_trigger_at(&mut self, key: ShotKey, timestamp_ms: u64) {
        self.frp(FrpEvent::shot_trigger_at(key, timestamp_ms));
    }

    /// `launch`.
    pub fn launch(&mut self, key: ShotKey, launch: Launch) {
        self.frp(FrpEvent::launch(key, launch));
    }

    /// `flight_result`.
    pub fn flight_result(&mut self, key: ShotKey, flight: FlightResult) {
        self.frp(FrpEvent::flight_result(key, flight));
    }

    /// `club_path`.
    pub fn club_path(&mut self, key: ShotKey, club: ClubData) {
        self.frp(FrpEvent::club_path(key, club));
    }

    /// `face_impact`.
    pub fn face_impact(&mut self, key: ShotKey, impact: FaceImpact) {
        self.frp(FrpEvent::face_impact(key, impact));
    }

    /// `shot_finished`.
    pub fn shot_finished(&mut self, key: ShotKey) {
        self.frp(FrpEvent::shot_finished(key));
    }

    /// `device_telemetry` with these keys. Send only what changed: the host
    /// merges, and stamps `source`.
    pub fn telemetry<K: Into<String>, V: Into<String>>(
        &mut self,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) {
        let telemetry = pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self.frp(FrpEvent::DeviceTelemetry {
            manufacturer: None,
            model: None,
            firmware: None,
            source: None,
            telemetry: Some(telemetry),
        });
    }

    /// The standard `ready` telemetry key.
    pub fn ready(&mut self, ready: bool) {
        self.telemetry([("ready", ready.to_string())]);
    }

    /// A device `alert`.
    pub fn alert(&mut self, severity: Severity, message: impl Into<String>) {
        self.frp(FrpEvent::Alert {
            severity,
            message: message.into(),
        });
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

    /// A `raw_frame` of one protocol message, stamped with the current time,
    /// with `seq` left at 0 for the host to number. Does nothing while
    /// nobody [`wants_raw`](Self::wants_raw). The caller makes sure the
    /// message carries no device or user identity; those go to
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
        self.frp(FrpEvent::raw_frame(frame));
    }

    /// A message that must not leave the machine as a `raw_frame`, such as
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
    fn builders_make_the_matching_frp_events() {
        let flags = HostFlags::new();
        let key = ShotKey::new("id", 1);
        let seen = collect(&flags, |out| {
            out.shot_trigger(key.clone());
            out.shot_finished(key.clone());
            out.ready(true);
            out.alert(Severity::Warn, "careful");
        });
        let [trigger, finished, ready, alert] = &seen[..] else {
            panic!("{seen:?}");
        };
        assert!(matches!(
            trigger,
            DeviceEvent::Frp(FrpEvent::ShotTrigger {
                timestamp: None,
                ..
            })
        ));
        assert!(matches!(
            finished,
            DeviceEvent::Frp(FrpEvent::ShotFinished { .. })
        ));
        let DeviceEvent::Frp(FrpEvent::DeviceTelemetry {
            telemetry: Some(t),
            source: None,
            ..
        }) = ready
        else {
            panic!("{ready:?}");
        };
        assert_eq!(t.get("ready").map(String::as_str), Some("true"));
        assert!(matches!(
            alert,
            DeviceEvent::Frp(FrpEvent::Alert {
                severity: Severity::Warn,
                ..
            })
        ));
    }

    #[test]
    fn raw_frames_are_built_only_while_wanted() {
        let flags = HostFlags::new();
        assert!(collect(&flags, |out| out.raw(RawDir::Rx, "x", &[1])).is_empty());
        flags.set_wants_raw(true);
        let seen = collect(&flags, |out| {
            out.raw(RawDir::Tx, "tx result", &[0xde, 0xad]);
        });
        let [DeviceEvent::Frp(FrpEvent::RawFrame(frame))] = &seen[..] else {
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
