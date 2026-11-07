//! Duplicate dropping for endpoints that forward events.
//!
//! Requires the `device` feature. A bridge or hub passes every envelope it
//! receives through one [`ForwardFilter`] and forwards it only when
//! [`should_forward`](ForwardFilter::should_forward) returns `true`. That
//! keeps a repeat from traveling further than the first hop, so an event
//! that comes back around a loop dies out:
//!
//! - **Shot events** pass once per `(device, shot_id, kind)` within the
//!   window ([`DEFAULT_SHOT_TIMEOUT`] unless set with
//!   [`ForwardFilter::with_window`]). So a shot keeps at most one
//!   `flight_result`, however many routes it takes.
//! - **`device_telemetry`** passes only when it changes the device's
//!   last-forwarded state: a field or `telemetry` key whose value differs
//!   from the one last forwarded for that device. A device's first telemetry
//!   always passes.
//! - **Alerts** with a device context pass once per identical
//!   `(device, severity, message)` within the window.
//! - **Raw frames** pass once per `(device, seq)` within the window. They are
//!   not shot events: a raw frame's `key` plays no part in the filter.
//! - **[`FrpEvent::Unknown`]** never passes: an unknown kind, or a known kind
//!   whose fields don't match it. FRP peers ignore both.
//!
//! A forwarder passes `device`, `shot_id`, `shot_number` and `timestamp`
//! through unchanged; the filter keys on them as they arrive.
//!
//! A forwarder may add a `flight_result` from its own model to a shot whose
//! device sent none. It builds the event when the upstream `shot_finished`
//! arrives, with the shot's `device` and `key`, `basis: modeled` and a
//! `model` naming itself, and passes it through
//! [`should_forward`](ForwardFilter::should_forward) before forwarding that
//! `shot_finished`: `true` means no `flight_result` was forwarded for the
//! shot yet, so it sends its own.
//!
//! A forwarder may drop raw frames instead of forwarding them. One that
//! forwards them passes each unchanged, and only to a controller that asked
//! for them with `raw_frames` in its `start`; to receive them itself, it asks
//! upstream the same way.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::DEFAULT_SHOT_TIMEOUT;
use crate::message::{FrpEnvelope, FrpEvent, Severity, Source};

/// Drops duplicate events before an endpoint forwards them.
///
/// ```
/// use flightrelay::{ForwardFilter, FrpEnvelope, FrpEvent, ShotKey};
///
/// let mut filter = ForwardFilter::new();
/// let trigger = FrpEnvelope::new("EagleOne-X4K2", FrpEvent::shot_trigger(ShotKey::new("id", 1)));
/// assert!(filter.should_forward(&trigger));
/// assert!(!filter.should_forward(&trigger)); // the same event, back around a loop
/// ```
///
/// Shot, alert and raw frame entries expire after the window. Telemetry state is kept
/// per device id until [`forget_device`](Self::forget_device), because a
/// device re-sends `device_telemetry` only when something changes. Call
/// `forget_device` for each device of an upstream connection that closes;
/// otherwise the state grows by one entry for every device id the filter
/// has seen, e.g. a bridge that makes up a new id on each connection.
#[derive(Debug)]
pub struct ForwardFilter {
    window: Duration,
    shots: HashMap<(String, String, &'static str), Instant>,
    alerts: HashMap<(String, Severity, String), Instant>,
    raw_frames: HashMap<(String, u64), Instant>,
    telemetry: HashMap<String, TelemetryState>,
}

/// The last-forwarded `device_telemetry` state of one device: every field
/// and `telemetry` key at the value it was last forwarded with.
#[derive(Debug, Default)]
struct TelemetryState {
    manufacturer: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    source: Option<Source>,
    telemetry: HashMap<String, String>,
}

impl TelemetryState {
    /// Applies an incoming telemetry event and reports whether it changed
    /// anything.
    fn apply(
        &mut self,
        manufacturer: Option<&String>,
        model: Option<&String>,
        firmware: Option<&String>,
        source: Option<Source>,
        telemetry: Option<&HashMap<String, String>>,
    ) -> bool {
        let mut changed = latch(&mut self.manufacturer, manufacturer.cloned());
        changed |= latch(&mut self.model, model.cloned());
        changed |= latch(&mut self.firmware, firmware.cloned());
        changed |= latch(&mut self.source, source);
        for (key, value) in telemetry.into_iter().flatten() {
            if self.telemetry.get(key) != Some(value) {
                self.telemetry.insert(key.clone(), value.clone());
                changed = true;
            }
        }
        changed
    }

    fn to_event(&self) -> FrpEvent {
        FrpEvent::DeviceTelemetry {
            manufacturer: self.manufacturer.clone(),
            model: self.model.clone(),
            firmware: self.firmware.clone(),
            source: self.source,
            telemetry: (!self.telemetry.is_empty()).then(|| self.telemetry.clone()),
        }
    }
}

/// Stores `incoming` in `slot` when it is present and different, and reports
/// whether it did.
fn latch<T: PartialEq>(slot: &mut Option<T>, incoming: Option<T>) -> bool {
    match incoming {
        Some(value) if slot.as_ref() != Some(&value) => {
            *slot = Some(value);
            true
        }
        _ => false,
    }
}

impl Default for ForwardFilter {
    fn default() -> Self {
        Self {
            window: DEFAULT_SHOT_TIMEOUT,
            shots: HashMap::new(),
            alerts: HashMap::new(),
            raw_frames: HashMap::new(),
            telemetry: HashMap::new(),
        }
    }
}

impl ForwardFilter {
    /// A filter whose window is the [`DEFAULT_SHOT_TIMEOUT`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how long forwarded shot events, alerts and raw frames are
    /// remembered. Keep it at least as long as the accumulator timeout of the
    /// controllers downstream.
    #[must_use]
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// How long forwarded shot events, alerts and raw frames are remembered.
    #[must_use]
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Whether to forward `env`. Call it once for every envelope received;
    /// when it returns `true`, the filter records the event as forwarded.
    /// The record stays if sending it then fails, so a copy of the event that
    /// arrives by another route within the window is dropped too.
    pub fn should_forward(&mut self, env: &FrpEnvelope) -> bool {
        self.should_forward_at(env, Instant::now())
    }

    fn should_forward_at(&mut self, env: &FrpEnvelope, now: Instant) -> bool {
        let window = self.window;
        let fresh = |seen: &Instant| now.saturating_duration_since(*seen) <= window;
        self.shots.retain(|_, seen| fresh(seen));
        self.alerts.retain(|_, seen| fresh(seen));
        self.raw_frames.retain(|_, seen| fresh(seen));

        let shot = match &env.event {
            FrpEvent::ShotTrigger { key, .. } => Some((key, "shot_trigger")),
            FrpEvent::Launch { key, .. } => Some((key, "launch")),
            FrpEvent::FlightResult { key, .. } => Some((key, "flight_result")),
            FrpEvent::ClubPath { key, .. } => Some((key, "club_path")),
            FrpEvent::FaceImpact { key, .. } => Some((key, "face_impact")),
            FrpEvent::ShotFinished { key } => Some((key, "shot_finished")),
            _ => None,
        };
        if let Some((key, kind)) = shot {
            let id = (env.device.clone(), key.shot_id.clone(), kind);
            return first_in_window(&mut self.shots, id, now);
        }

        match &env.event {
            FrpEvent::DeviceTelemetry {
                manufacturer,
                model,
                firmware,
                source,
                telemetry,
            } => {
                let first = !self.telemetry.contains_key(&env.device);
                let state = self.telemetry.entry(env.device.clone()).or_default();
                let changed = state.apply(
                    manufacturer.as_ref(),
                    model.as_ref(),
                    firmware.as_ref(),
                    *source,
                    telemetry.as_ref(),
                );
                first || changed
            }
            FrpEvent::Alert { severity, message } => {
                let id = (env.device.clone(), *severity, message.clone());
                first_in_window(&mut self.alerts, id, now)
            }
            FrpEvent::RawFrame(frame) => {
                first_in_window(&mut self.raw_frames, (env.device.clone(), frame.seq), now)
            }
            _ => false,
        }
    }

    /// The `device_telemetry` state last forwarded for `device`, with every
    /// field and `telemetry` key at its latest value. A forwarder sends it to
    /// a controller that connects after the device's telemetry went by.
    #[must_use]
    pub fn telemetry(&self, device: &str) -> Option<FrpEvent> {
        self.telemetry.get(device).map(TelemetryState::to_event)
    }

    /// Forgets everything about `device`: the shot events, alerts and raw
    /// frames forwarded for it, and its telemetry state. For a forwarder that knows
    /// the device is gone, e.g. its upstream connection closed.
    pub fn forget_device(&mut self, device: &str) {
        self.shots.retain(|(d, _, _), _| d != device);
        self.alerts.retain(|(d, _, _), _| d != device);
        self.raw_frames.retain(|(d, _), _| d != device);
        self.telemetry.remove(device);
    }
}

/// Records `id` as seen at `now` and returns `true` if it was not already
/// seen within the window.
fn first_in_window<K: Eq + std::hash::Hash>(
    seen: &mut HashMap<K, Instant>,
    id: K,
    now: Instant,
) -> bool {
    if seen.contains_key(&id) {
        return false;
    }
    seen.insert(id, now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Basis, FlightResult, Launch, RawDir, RawFrame, ShotKey};

    const SECOND: Duration = Duration::from_secs(1);

    fn env(device: &str, event: FrpEvent) -> FrpEnvelope {
        FrpEnvelope::new(device, event)
    }

    fn telemetry(source: Option<Source>, pairs: &[(&str, &str)]) -> FrpEvent {
        FrpEvent::DeviceTelemetry {
            manufacturer: None,
            model: None,
            firmware: None,
            source,
            telemetry: (!pairs.is_empty()).then(|| {
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect()
            }),
        }
    }

    fn alert(severity: Severity, message: &str) -> FrpEvent {
        FrpEvent::Alert {
            severity,
            message: message.into(),
        }
    }

    fn shot_events(key: &ShotKey) -> Vec<FrpEvent> {
        vec![
            FrpEvent::shot_trigger_at(key.clone(), 1_794_063_845_123),
            FrpEvent::launch(key.clone(), Launch::default()),
            FrpEvent::flight_result(key.clone(), FlightResult::new(Basis::Tracked)),
            FrpEvent::club_path(key.clone(), crate::ClubData::default()),
            FrpEvent::face_impact(key.clone(), crate::FaceImpact::default()),
            FrpEvent::shot_finished(key.clone()),
        ]
    }

    #[test]
    fn default_window_is_the_shot_timeout() {
        assert_eq!(ForwardFilter::new().window(), DEFAULT_SHOT_TIMEOUT);
        assert_eq!(
            ForwardFilter::new().with_window(60 * SECOND).window(),
            60 * SECOND
        );
    }

    #[test]
    fn duplicate_shot_event_is_dropped() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let key = ShotKey::new("shot-1", 1);
        for event in shot_events(&key) {
            assert!(filter.should_forward_at(&env("Dev", event.clone()), t0));
            assert!(!filter.should_forward_at(&env("Dev", event), t0 + SECOND));
        }
    }

    #[test]
    fn duplicate_is_matched_on_kind_not_content() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let key = ShotKey::new("shot-1", 1);
        let ball = Launch {
            backspin_rpm: Some(3200),
            ..Default::default()
        };
        assert!(filter.should_forward_at(
            &env("Dev", FrpEvent::launch(key.clone(), Launch::default())),
            t0
        ));
        assert!(!filter.should_forward_at(&env("Dev", FrpEvent::launch(key, ball)), t0));
    }

    #[test]
    fn one_flight_result_per_shot() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let key = ShotKey::new("shot-1", 1);
        let tracked = FlightResult {
            carry_distance: Some(crate::Distance::Meters(180.5)),
            ..FlightResult::new(Basis::Tracked)
        };
        let added = FlightResult {
            model: Some("Range model (Example Hub)".into()),
            carry_distance: Some(crate::Distance::Meters(175.0)),
            ..FlightResult::new(Basis::Modeled)
        };

        // The device's flight result passes; another for the same shot,
        // whatever its content or basis, is dropped.
        assert!(filter.should_forward_at(
            &env("Dev", FrpEvent::flight_result(key.clone(), tracked)),
            t0
        ));
        assert!(!filter.should_forward_at(
            &env("Dev", FrpEvent::flight_result(key.clone(), added.clone())),
            t0 + SECOND
        ));

        // A forwarder adding its own for a shot whose device sent none: it
        // passes once, so a second forwarder's addition is dropped too.
        let other = ShotKey::new("shot-2", 2);
        assert!(filter.should_forward_at(&env("Dev", FrpEvent::shot_finished(other.clone())), t0));
        assert!(filter.should_forward_at(
            &env("Dev", FrpEvent::flight_result(other.clone(), added.clone())),
            t0
        ));
        assert!(!filter.should_forward_at(
            &env("Dev", FrpEvent::flight_result(other, added)),
            t0 + SECOND
        ));
    }

    #[test]
    fn flight_result_without_a_recognized_basis_is_dropped() {
        let mut filter = ForwardFilter::new();
        let invalid = r#"{"device":"Dev","event":{"kind":"flight_result","key":{"shot_id":"shot-1","shot_number":1},"flight":{"basis":"estimated","carry_distance":"175.0m"}}}"#;
        let crate::FrpMessage::Envelope(invalid) = crate::FrpMessage::parse(invalid).unwrap()
        else {
            panic!("expected an envelope");
        };
        assert!(!filter.should_forward(&invalid));

        // It doesn't count as the shot's flight result: one added later passes.
        let added = FlightResult {
            model: Some("Range model (Example Hub)".into()),
            ..FlightResult::new(Basis::Modeled)
        };
        assert!(filter.should_forward(&env(
            "Dev",
            FrpEvent::flight_result(ShotKey::new("shot-1", 1), added)
        )));
    }

    #[test]
    fn distinct_kinds_shots_and_devices_pass() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let first = ShotKey::new("shot-1", 1);
        let second = ShotKey::new("shot-2", 2);
        for event in shot_events(&first) {
            assert!(filter.should_forward_at(&env("Dev", event), t0));
        }
        for event in shot_events(&second) {
            assert!(filter.should_forward_at(&env("Dev", event), t0));
        }
        for event in shot_events(&first) {
            assert!(filter.should_forward_at(&env("Other", event), t0));
        }
    }

    #[test]
    fn shot_events_expire_after_the_window() {
        let mut filter = ForwardFilter::new().with_window(10 * SECOND);
        let t0 = Instant::now();
        let trigger = env("Dev", FrpEvent::shot_trigger(ShotKey::new("shot-1", 1)));
        assert!(filter.should_forward_at(&trigger, t0));
        assert!(!filter.should_forward_at(&trigger, t0 + 10 * SECOND));
        assert!(filter.should_forward_at(&trigger, t0 + 11 * SECOND));
    }

    #[test]
    fn telemetry_passes_only_on_change() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let battery = env("Dev", telemetry(None, &[("battery_pct", "85")]));
        let ready = env("Dev", telemetry(None, &[("ready", "true")]));

        // A device's first telemetry passes, even with no fields.
        assert!(filter.should_forward_at(&env("Empty", telemetry(None, &[])), t0));
        assert!(!filter.should_forward_at(&env("Empty", telemetry(None, &[])), t0));

        assert!(filter.should_forward_at(&battery, t0));
        assert!(!filter.should_forward_at(&battery, t0));
        assert!(filter.should_forward_at(&ready, t0));

        // Interleaved partial updates coming back around a loop stop here.
        assert!(!filter.should_forward_at(&battery, t0));
        assert!(!filter.should_forward_at(&ready, t0));

        // Telemetry state does not expire with the window.
        assert!(!filter.should_forward_at(&battery, t0 + 3600 * SECOND));

        let lower = env("Dev", telemetry(None, &[("battery_pct", "84")]));
        assert!(filter.should_forward_at(&lower, t0));
        let measured = env(
            "Dev",
            telemetry(Some(Source::Measured), &[("ready", "true")]),
        );
        assert!(filter.should_forward_at(&measured, t0));
        assert!(!filter.should_forward_at(&measured, t0));
        let model = env(
            "Dev",
            FrpEvent::DeviceTelemetry {
                manufacturer: None,
                model: Some("Eagle One".into()),
                firmware: None,
                source: None,
                telemetry: None,
            },
        );
        assert!(filter.should_forward_at(&model, t0));
        assert!(!filter.should_forward_at(&model, t0));

        // Another device has its own state.
        assert!(
            filter.should_forward_at(&env("Other", telemetry(None, &[("battery_pct", "84")])), t0)
        );
    }

    #[test]
    fn telemetry_state_merges_what_was_forwarded() {
        let mut filter = ForwardFilter::new();
        assert_eq!(filter.telemetry("Dev"), None);
        filter.should_forward(&env(
            "Dev",
            telemetry(Some(Source::Measured), &[("ready", "false")]),
        ));
        filter.should_forward(&env("Dev", telemetry(None, &[("battery_pct", "85")])));
        filter.should_forward(&env("Dev", telemetry(None, &[("ready", "true")])));
        assert_eq!(
            filter.telemetry("Dev"),
            Some(telemetry(
                Some(Source::Measured),
                &[("ready", "true"), ("battery_pct", "85")]
            ))
        );
    }

    #[test]
    fn identical_alerts_are_dropped_within_the_window() {
        let mut filter = ForwardFilter::new().with_window(10 * SECOND);
        let t0 = Instant::now();
        let weak = env("Dev", alert(Severity::Warn, "Signal weak"));
        assert!(filter.should_forward_at(&weak, t0));
        assert!(!filter.should_forward_at(&weak, t0 + 5 * SECOND));
        assert!(filter.should_forward_at(&env("Dev", alert(Severity::Error, "Signal weak")), t0));
        assert!(filter.should_forward_at(&env("Dev", alert(Severity::Warn, "Tilted")), t0));
        assert!(filter.should_forward_at(&env("Other", alert(Severity::Warn, "Signal weak")), t0));
        // A repeat after the window passes again.
        assert!(filter.should_forward_at(&weak, t0 + 11 * SECOND));
    }

    #[test]
    fn unknown_event_kinds_are_not_forwarded() {
        let mut filter = ForwardFilter::new();
        assert!(!filter.should_forward(&env("Dev", FrpEvent::Unknown)));
    }

    #[test]
    fn forget_device_clears_its_state_only() {
        let mut filter = ForwardFilter::new();
        let trigger = |device| env(device, FrpEvent::shot_trigger(ShotKey::new("shot-1", 1)));
        let weak = |device| env(device, alert(Severity::Warn, "Signal weak"));
        let ready = |device| env(device, telemetry(None, &[("ready", "true")]));
        let frame = |device| env(device, raw_frame(1, None));
        for device in ["Gone", "Kept"] {
            assert!(filter.should_forward(&trigger(device)));
            assert!(filter.should_forward(&weak(device)));
            assert!(filter.should_forward(&ready(device)));
            assert!(filter.should_forward(&frame(device)));
        }
        filter.forget_device("Gone");
        assert_eq!(filter.telemetry("Gone"), None);
        assert!(filter.should_forward(&trigger("Gone")));
        assert!(filter.should_forward(&weak("Gone")));
        assert!(filter.should_forward(&ready("Gone")));
        assert!(filter.should_forward(&frame("Gone")));
        assert!(!filter.should_forward(&trigger("Kept")));
        assert!(!filter.should_forward(&weak("Kept")));
        assert!(!filter.should_forward(&ready("Kept")));
        assert!(!filter.should_forward(&frame("Kept")));
    }

    fn raw_frame(seq: u64, key: Option<ShotKey>) -> FrpEvent {
        FrpEvent::raw_frame(RawFrame {
            key,
            ..RawFrame::new(RawDir::Rx, seq, vec![0x0a, 0x1b])
        })
    }

    #[test]
    fn raw_frames_pass_once_per_device_and_seq() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        assert!(filter.should_forward_at(&env("Dev", raw_frame(1, None)), t0));
        // The same frame back around a loop, or by a second route.
        assert!(!filter.should_forward_at(&env("Dev", raw_frame(1, None)), t0));
        // Matched on (device, seq), not on content.
        let other_content = FrpEvent::raw_frame(RawFrame::new(RawDir::Tx, 1, vec![0xff]));
        assert!(!filter.should_forward_at(&env("Dev", other_content), t0));
        // The next number, and the same number from another device, pass.
        assert!(filter.should_forward_at(&env("Dev", raw_frame(2, None)), t0));
        assert!(filter.should_forward_at(&env("Other", raw_frame(1, None)), t0));
    }

    #[test]
    fn raw_frames_expire_after_the_window() {
        let mut filter = ForwardFilter::new().with_window(10 * SECOND);
        let t0 = Instant::now();
        let frame = env("Dev", raw_frame(1, None));
        assert!(filter.should_forward_at(&frame, t0));
        assert!(!filter.should_forward_at(&frame, t0 + 10 * SECOND));
        assert!(filter.should_forward_at(&frame, t0 + 11 * SECOND));
    }

    #[test]
    fn raw_frames_are_not_tracked_per_shot() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let key = ShotKey::new("shot-1", 1);
        // Several frames for one shot all pass; each has its own number.
        for seq in 1..=3 {
            let frame = env("Dev", raw_frame(seq, Some(key.clone())));
            assert!(filter.should_forward_at(&frame, t0));
        }
        // They don't count as the shot's events: each shot event still passes once.
        for event in shot_events(&key) {
            assert!(filter.should_forward_at(&env("Dev", event.clone()), t0));
            assert!(!filter.should_forward_at(&env("Dev", event), t0));
        }
    }

    #[test]
    fn raw_frames_pass_unchanged() {
        let mut filter = ForwardFilter::new();
        let json = r#"{"device":"Dev","event":{"kind":"raw_frame","dir":"tx","seq":9,"timestamp":1794063845123,"label":"arm","data":"00ff","key":{"shot_id":"shot-1","shot_number":1}}}"#;
        let crate::FrpMessage::Envelope(frame) = crate::FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        assert!(filter.should_forward(&frame));
        assert_eq!(serde_json::to_string(&frame).unwrap(), json);
    }

    #[test]
    fn flight_result_with_assumed_passes_unchanged() {
        let mut filter = ForwardFilter::new();
        let t0 = Instant::now();
        let json = r#"{"device":"Dev","event":{"kind":"flight_result","key":{"shot_id":"shot-1","shot_number":1},"flight":{"basis":"modeled","model":"Range model (Example Hub)","carry_distance":"31.2m","assumed":{"backspin_rpm":4500,"sidespin_rpm":0}}}}"#;
        let crate::FrpMessage::Envelope(first) = crate::FrpMessage::parse(json).unwrap() else {
            panic!("expected an envelope");
        };
        assert!(filter.should_forward_at(&first, t0));
        assert_eq!(serde_json::to_string(&first).unwrap(), json);

        // A second flight result for the shot, assumed values or not, is
        // dropped.
        let key = ShotKey::new("shot-1", 1);
        let second = FlightResult {
            assumed: Some(crate::AssumedLaunch {
                backspin_rpm: Some(5200),
                ..Default::default()
            }),
            ..FlightResult::new(Basis::Modeled)
        };
        assert!(!filter.should_forward_at(
            &env("Dev", FrpEvent::flight_result(key, second)),
            t0 + SECOND
        ));
    }
}
