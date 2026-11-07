//! Shot accumulator for controllers.
//!
//! Collects shot lifecycle events and produces a complete shot when
//! `shot_finished` arrives. Requires the `controller` feature.
//!
//! A shot keeps the first event of each kind (`launch`, `flight_result`,
//! `club_path`, `face_impact`) and ignores a repeat, as a forwarder does. It
//! carries the `source` its device declared in `device_telemetry`. Raw
//! frames are not shot events: they never create, complete or change a shot,
//! even with a key.
//!
//! A shot that has not finished within the accumulator timeout
//! ([`DEFAULT_SHOT_TIMEOUT`] unless set with
//! [`ShotAggregator::with_timeout`]) is discarded. Events for a shot that was
//! finished or discarded are ignored for one more timeout, so a late event or
//! the same shot arriving by a second route never starts a new partial shot.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::DEFAULT_SHOT_TIMEOUT;
use crate::message::{FrpEnvelope, FrpEvent, FrpMessage, Source};
use crate::types::{ClubData, FaceImpact, FlightResult, Launch, ShotKey};

/// A completed shot with all available data.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedShot {
    /// Device that produced this shot.
    pub device: String,
    /// Shot correlation key.
    pub key: ShotKey,
    /// When the device detected the strike, in milliseconds since the Unix
    /// epoch (UTC) by the device's clock, if its `shot_trigger` said.
    pub timestamp: Option<u64>,
    /// Ball launch data (if hardware provided it). Measured.
    pub launch: Option<Launch>,
    /// Flight result (if the device, or a forwarder on its behalf, sent one).
    /// A measured-only controller ignores it.
    pub flight: Option<FlightResult>,
    /// Club path data (if hardware provided it).
    pub club: Option<ClubData>,
    /// Face impact data (if hardware provided it).
    pub impact: Option<FaceImpact>,
    /// Where the shot originated: the most recent `source` the device
    /// declared in `device_telemetry` when the shot finished. `None` is
    /// unknown.
    pub source: Option<Source>,
}

impl CompletedShot {
    /// Launch values for a controller that flies the ball itself: each
    /// [`launch`](Self::launch) value, with a missing one filled from the
    /// flight result's `assumed` launch when its basis is
    /// [`Modeled`](crate::Basis::Modeled) (see
    /// [`Launch::fill_assumed`]). `None` when the shot has neither.
    ///
    /// The filled values are not measured: they are what a flight model
    /// assumed. A measured-only controller reads [`launch`](Self::launch)
    /// instead.
    #[must_use]
    pub fn launch_for_flight(&self) -> Option<Launch> {
        let mut launch = self.launch.clone().unwrap_or_default();
        if let Some(flight) = &self.flight {
            launch.fill_assumed(flight);
        }
        (self.launch.is_some() || launch != Launch::default()).then_some(launch)
    }
}

/// Accumulates events for a single in-flight shot.
#[derive(Debug)]
struct ShotAccumulator {
    device: String,
    key: ShotKey,
    timestamp: Option<u64>,
    started: Instant,
    launch: Option<Launch>,
    flight: Option<FlightResult>,
    club: Option<ClubData>,
    impact: Option<FaceImpact>,
}

impl ShotAccumulator {
    fn new(device: String, key: ShotKey, timestamp: Option<u64>, started: Instant) -> Self {
        Self {
            device,
            key,
            timestamp,
            started,
            launch: None,
            flight: None,
            club: None,
            impact: None,
        }
    }

    fn finish(self, source: Option<Source>) -> CompletedShot {
        CompletedShot {
            device: self.device,
            key: self.key,
            timestamp: self.timestamp,
            launch: self.launch,
            flight: self.flight,
            club: self.club,
            impact: self.impact,
            source,
        }
    }
}

/// Manages multiple in-flight shots, keyed by `(device, shot_id)`.
///
/// Feed [`FrpMessage`] events via [`feed`](Self::feed) and receive
/// [`CompletedShot`] when a shot lifecycle finishes. It also remembers the
/// most recent `source` each device declared in `device_telemetry`.
///
/// A shot without `shot_finished` within the timeout of its `shot_trigger`
/// is discarded, never emitted. Every `feed` discards expired shots first;
/// [`discard_expired`](Self::discard_expired) does it without an event.
/// After a shot finishes or is discarded, its events are ignored for one
/// more timeout. A second `shot_trigger` for a shot in flight is ignored too,
/// and so is a second event of any other kind: a shot keeps the first
/// `launch`, `flight_result`, `club_path` and `face_impact` it receives.
///
/// Declared sources are kept per device id until
/// [`forget_device`](Self::forget_device), because a device re-sends
/// `device_telemetry` only when something changes. Call `forget_device` for
/// each device of a connection that closes; otherwise the sources grow by one
/// entry for every device id the aggregator has seen, e.g. a bridge that
/// makes up a new id on each connection.
///
/// ```ignore
/// let mut agg = ShotAggregator::new();
/// // ... receive messages from FRP connection ...
/// if let Some(shot) = agg.feed(&msg) {
///     println!("Shot complete: {:?}", shot.launch);
/// }
/// ```
#[derive(Debug)]
pub struct ShotAggregator {
    timeout: Duration,
    pending: HashMap<(String, String), ShotAccumulator>,
    /// Shots finished or discarded, with the time they closed.
    closed: HashMap<(String, String), Instant>,
    sources: HashMap<String, Source>,
}

impl Default for ShotAggregator {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SHOT_TIMEOUT,
            pending: HashMap::new(),
            closed: HashMap::new(),
            sources: HashMap::new(),
        }
    }
}

impl ShotAggregator {
    /// An aggregator with the [`DEFAULT_SHOT_TIMEOUT`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the accumulator timeout: how long a shot may take from
    /// `shot_trigger` to `shot_finished` before it is discarded.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The accumulator timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Process an FRP message. Returns a [`CompletedShot`] when `shot_finished`
    /// finalizes an accumulated shot.
    pub fn feed(&mut self, msg: &FrpMessage) -> Option<CompletedShot> {
        let env = match msg {
            FrpMessage::Envelope(env) => env,
            FrpMessage::Protocol(_) => return None,
        };
        self.feed_envelope(env)
    }

    /// Process an FRP envelope directly.
    pub fn feed_envelope(&mut self, env: &FrpEnvelope) -> Option<CompletedShot> {
        self.feed_envelope_at(env, Instant::now())
    }

    fn feed_envelope_at(&mut self, env: &FrpEnvelope, now: Instant) -> Option<CompletedShot> {
        self.discard_expired_at(now);
        match &env.event {
            FrpEvent::ShotTrigger { key, timestamp } => {
                let id = (env.device.clone(), key.shot_id.clone());
                if !self.closed.contains_key(&id) && !self.pending.contains_key(&id) {
                    let acc =
                        ShotAccumulator::new(env.device.clone(), key.clone(), *timestamp, now);
                    self.pending.insert(id, acc);
                }
                None
            }
            FrpEvent::Launch { key, launch } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.launch.get_or_insert_with(|| launch.clone());
                }
                None
            }
            FrpEvent::FlightResult { key, flight } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.flight.get_or_insert_with(|| flight.clone());
                }
                None
            }
            FrpEvent::ClubPath { key, club } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.club.get_or_insert_with(|| club.clone());
                }
                None
            }
            FrpEvent::FaceImpact { key, impact } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.impact.get_or_insert_with(|| impact.clone());
                }
                None
            }
            FrpEvent::ShotFinished { key } => {
                let id = (env.device.clone(), key.shot_id.clone());
                let acc = self.pending.remove(&id)?;
                self.closed.insert(id, now);
                Some(acc.finish(self.source(&env.device)))
            }
            FrpEvent::DeviceTelemetry { source, .. } => {
                if let Some(source) = source {
                    self.sources.insert(env.device.clone(), *source);
                }
                None
            }
            // A raw frame is not a shot event, even with a key.
            FrpEvent::Alert { .. } | FrpEvent::RawFrame(_) | FrpEvent::Unknown => None,
        }
    }

    /// Discards every shot that has not finished within the timeout of its
    /// `shot_trigger`, and returns how many it discarded. Their later events
    /// are ignored.
    pub fn discard_expired(&mut self) -> usize {
        self.discard_expired_at(Instant::now())
    }

    fn discard_expired_at(&mut self, now: Instant) -> usize {
        let timeout = self.timeout;
        self.closed
            .retain(|_, closed| now.saturating_duration_since(*closed) <= timeout);
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, acc)| now.saturating_duration_since(acc.started) > timeout)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.pending.remove(id);
            self.closed.insert(id.clone(), now);
        }
        expired.len()
    }

    /// Forgets everything about `device`: its shots in flight, the shots it
    /// recently finished, and its declared `source`. For a controller that
    /// knows the device is gone, e.g. its connection closed.
    pub fn forget_device(&mut self, device: &str) {
        self.pending.retain(|(d, _), _| d != device);
        self.closed.retain(|(d, _), _| d != device);
        self.sources.remove(device);
    }

    /// The most recent `source` `device` declared, or `None` while it is unknown.
    #[must_use]
    pub fn source(&self, device: &str) -> Option<Source> {
        self.sources.get(device).copied()
    }

    /// Number of shots currently being accumulated.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Basis, RawDir, RawFrame};
    use crate::units::{Distance, Velocity};

    fn test_key() -> ShotKey {
        ShotKey {
            shot_id: "test-uuid".into(),
            shot_number: 1,
        }
    }

    #[test]
    fn full_shot_lifecycle() {
        let mut agg = ShotAggregator::new();
        let device = "TestDevice-001";
        let key = test_key();

        // Trigger
        let trigger = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::shot_trigger(key.clone()),
        });
        assert!(agg.feed(&trigger).is_none());
        assert_eq!(agg.pending_count(), 1);

        // Ball flight
        let ball = Launch {
            launch_speed: Some(Velocity::MetersPerSecond(67.2)),
            launch_azimuth: Some(-1.3),
            launch_elevation: Some(14.2),
            backspin_rpm: Some(3200),
            sidespin_rpm: Some(-450),
            total_spin_rpm: Some(3231),
            spin_axis: Some(-8.0),
        };
        let ball_msg = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::Launch {
                key: key.clone(),
                launch: ball.clone(),
            },
        });
        assert!(agg.feed(&ball_msg).is_none());

        // Flight result
        let flight = FlightResult {
            carry_distance: Some(Distance::Meters(180.5)),
            carry_offline: Some(Distance::Meters(-7.6)),
            descent_angle: Some(38.4),
            ..FlightResult::new(Basis::Tracked)
        };
        let flight_msg = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::flight_result(key.clone(), flight.clone()),
        });
        assert!(agg.feed(&flight_msg).is_none());

        // Club path
        let club = ClubData {
            club_speed: Some(Velocity::MetersPerSecond(42.1)),
            path: Some(-2.1),
            spin_loft: Some(22.1),
            low_point_distance: Some(Distance::Inches(1.6)),
            ..Default::default()
        };
        let club_msg = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::ClubPath {
                key: key.clone(),
                club: club.clone(),
            },
        });
        assert!(agg.feed(&club_msg).is_none());

        // Face impact
        let impact = FaceImpact {
            lateral: Some(Distance::Inches(0.31)),
            vertical: Some(Distance::Inches(0.15)),
        };
        let impact_msg = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::FaceImpact {
                key: key.clone(),
                impact: impact.clone(),
            },
        });
        assert!(agg.feed(&impact_msg).is_none());

        // Finish
        let finish = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::ShotFinished { key },
        });
        let shot = agg.feed(&finish).expect("should produce CompletedShot");
        assert_eq!(shot.device, device);
        assert_eq!(shot.launch, Some(ball));
        assert_eq!(shot.flight, Some(flight));
        assert_eq!(shot.club, Some(club));
        assert_eq!(shot.impact, Some(impact));
        assert_eq!(shot.source, None);
        assert_eq!(agg.pending_count(), 0);
    }

    fn envelope(device: &str, event: FrpEvent) -> FrpMessage {
        FrpMessage::Envelope(FrpEnvelope::new(device, event))
    }

    fn telemetry(source: Option<Source>) -> FrpEvent {
        FrpEvent::DeviceTelemetry {
            manufacturer: None,
            model: None,
            firmware: None,
            source,
            telemetry: None,
        }
    }

    #[test]
    fn shot_keeps_its_first_flight_result() {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        let tracked = FlightResult {
            carry_distance: Some(Distance::Meters(180.5)),
            ..FlightResult::new(Basis::Tracked)
        };
        let modeled = FlightResult {
            model: Some("Range model (Example Hub)".into()),
            carry_distance: Some(Distance::Meters(175.0)),
            ..FlightResult::new(Basis::Modeled)
        };
        agg.feed(&envelope("Dev", FrpEvent::shot_trigger(key.clone())));
        agg.feed(&envelope(
            "Dev",
            FrpEvent::flight_result(key.clone(), tracked.clone()),
        ));
        agg.feed(&envelope(
            "Dev",
            FrpEvent::flight_result(key.clone(), modeled),
        ));
        let shot = agg
            .feed(&envelope("Dev", FrpEvent::shot_finished(key)))
            .unwrap();
        assert_eq!(shot.flight, Some(tracked));
    }

    #[test]
    fn flight_result_without_a_recognized_basis_is_ignored() {
        let mut agg = ShotAggregator::new();
        // `kind` plus the event's fields after `key`.
        let parse = |kind: &str, fields: &str| {
            FrpMessage::parse(&format!(
                r#"{{"device":"Dev","event":{{"kind":"{kind}","key":{{"shot_id":"s","shot_number":1}}{fields}}}}}"#
            ))
            .unwrap()
        };
        agg.feed(&parse("shot_trigger", ""));
        agg.feed(&parse(
            "flight_result",
            r#","flight":{"basis":"estimated","carry_distance":"175.0m"}"#,
        ));
        agg.feed(&parse(
            "flight_result",
            r#","flight":{"basis":"tracked","carry_distance":"180.5m"}"#,
        ));
        let shot = agg.feed(&parse("shot_finished", "")).unwrap();
        assert_eq!(
            shot.flight,
            Some(FlightResult {
                carry_distance: Some(Distance::Meters(180.5)),
                ..FlightResult::new(Basis::Tracked)
            })
        );
    }

    #[test]
    fn shot_keeps_its_first_event_of_each_kind() {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        let first_ball = Launch {
            launch_speed: Some(Velocity::MetersPerSecond(40.0)),
            ..Default::default()
        };
        let second_ball = Launch {
            launch_speed: Some(Velocity::MetersPerSecond(67.2)),
            launch_elevation: Some(14.2),
            total_spin_rpm: Some(3231),
            ..Default::default()
        };
        let first_club = ClubData {
            club_speed: Some(Velocity::MetersPerSecond(44.7)),
            ..Default::default()
        };
        let second_club = ClubData {
            club_speed: Some(Velocity::MetersPerSecond(45.1)),
            path: Some(2.0),
            ..Default::default()
        };
        let first_impact = FaceImpact {
            lateral: Some(Distance::Millimeters(3.0)),
            vertical: None,
        };
        let second_impact = FaceImpact {
            lateral: Some(Distance::Millimeters(-5.0)),
            vertical: Some(Distance::Millimeters(2.0)),
        };

        agg.feed(&envelope("Dev", FrpEvent::shot_trigger(key.clone())));
        for (ball, club, impact) in [
            (first_ball.clone(), first_club.clone(), first_impact.clone()),
            (second_ball, second_club, second_impact),
        ] {
            agg.feed(&envelope("Dev", FrpEvent::launch(key.clone(), ball)));
            agg.feed(&envelope("Dev", FrpEvent::club_path(key.clone(), club)));
            agg.feed(&envelope("Dev", FrpEvent::face_impact(key.clone(), impact)));
        }
        let shot = agg
            .feed(&envelope("Dev", FrpEvent::shot_finished(key)))
            .unwrap();
        assert_eq!(shot.launch, Some(first_ball));
        assert_eq!(shot.club, Some(first_club));
        assert_eq!(shot.impact, Some(first_impact));
    }

    #[test]
    fn flight_result_may_arrive_in_any_order_or_not_at_all() {
        let mut agg = ShotAggregator::new();
        let flight = FlightResult {
            max_height: Some(Distance::Meters(28.3)),
            ..FlightResult::new(Basis::Modeled)
        };

        let key = ShotKey::new("first", 1);
        agg.feed(&envelope("Dev", FrpEvent::shot_trigger(key.clone())));
        agg.feed(&envelope(
            "Dev",
            FrpEvent::flight_result(key.clone(), flight.clone()),
        ));
        agg.feed(&envelope("Dev", FrpEvent::launch(key.clone(), ball())));
        let shot = agg
            .feed(&envelope("Dev", FrpEvent::shot_finished(key)))
            .unwrap();
        assert_eq!(shot.launch, Some(ball()));
        assert_eq!(shot.flight, Some(flight));

        let key = ShotKey::new("second", 2);
        agg.feed(&envelope("Dev", FrpEvent::shot_trigger(key.clone())));
        agg.feed(&envelope("Dev", FrpEvent::launch(key.clone(), ball())));
        let shot = agg
            .feed(&envelope("Dev", FrpEvent::shot_finished(key)))
            .unwrap();
        assert_eq!(shot.flight, None);
    }

    #[test]
    fn shots_carry_the_source_their_device_declared() {
        let mut agg = ShotAggregator::new();
        agg.feed(&envelope("Monitor", telemetry(Some(Source::Measured))));
        agg.feed(&envelope("Mock", telemetry(Some(Source::Virtual))));
        assert_eq!(agg.source("Monitor"), Some(Source::Measured));
        assert_eq!(agg.source("Mock"), Some(Source::Virtual));
        assert_eq!(agg.source("Other"), None);

        // Telemetry without `source` keeps the class already declared.
        agg.feed(&envelope("Monitor", telemetry(None)));
        assert_eq!(agg.source("Monitor"), Some(Source::Measured));

        for (device, source) in [
            ("Monitor", Some(Source::Measured)),
            ("Mock", Some(Source::Virtual)),
            ("Other", None),
        ] {
            let key = ShotKey::new(format!("{device}-shot"), 1);
            agg.feed(&envelope(device, FrpEvent::shot_trigger(key.clone())));
            let shot = agg
                .feed(&envelope(device, FrpEvent::shot_finished(key)))
                .unwrap();
            assert_eq!(shot.device, device);
            assert_eq!(shot.source, source);
        }
    }

    #[test]
    fn source_declared_mid_shot_applies_at_finish() {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        agg.feed(&envelope("Dev", FrpEvent::shot_trigger(key.clone())));
        agg.feed(&envelope("Dev", telemetry(Some(Source::Virtual))));
        let shot = agg
            .feed(&envelope("Dev", FrpEvent::shot_finished(key)))
            .unwrap();
        assert_eq!(shot.source, Some(Source::Virtual));
    }

    #[test]
    fn shot_without_club_or_impact() {
        let mut agg = ShotAggregator::new();
        let key = test_key();

        agg.feed(&FrpMessage::Envelope(FrpEnvelope {
            device: "Dev".into(),
            event: FrpEvent::shot_trigger(key.clone()),
        }));

        let ball = Launch {
            launch_speed: Some(Velocity::MilesPerHour(100.0)),
            launch_elevation: Some(12.0),
            ..Default::default()
        };
        agg.feed(&FrpMessage::Envelope(FrpEnvelope {
            device: "Dev".into(),
            event: FrpEvent::Launch {
                key: key.clone(),
                launch: ball,
            },
        }));

        let shot = agg
            .feed(&FrpMessage::Envelope(FrpEnvelope {
                device: "Dev".into(),
                event: FrpEvent::ShotFinished { key },
            }))
            .unwrap();
        assert!(shot.launch.is_some());
        assert!(shot.flight.is_none());
        assert!(shot.club.is_none());
        assert!(shot.impact.is_none());
    }

    fn raw_frame(seq: u64, key: Option<ShotKey>) -> FrpEvent {
        FrpEvent::raw_frame(RawFrame {
            key,
            ..RawFrame::new(RawDir::Rx, seq, vec![0x0a, 0x1b])
        })
    }

    #[test]
    fn raw_frames_never_touch_a_shot() {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        let t0 = Instant::now();
        let env = |event| FrpEnvelope::new("Dev", event);

        // A raw frame for a shot nobody triggered starts nothing.
        assert!(
            agg.feed_envelope_at(&env(raw_frame(1, Some(key.clone()))), t0)
                .is_none()
        );
        assert_eq!(agg.pending_count(), 0);

        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(key.clone())), t0);
        // Raw frames before and between the shot's events change nothing: the
        // first launch still wins, and the shot still finishes as composed.
        for (n, event) in [
            raw_frame(2, Some(key.clone())),
            FrpEvent::launch(key.clone(), ball()),
            raw_frame(3, Some(key.clone())),
            raw_frame(4, None),
        ]
        .into_iter()
        .enumerate()
        {
            let at = t0 + SECOND * u32::try_from(n).unwrap();
            assert!(agg.feed_envelope_at(&env(event), at).is_none());
            assert_eq!(agg.pending_count(), 1);
        }
        let shot = agg
            .feed_envelope_at(&env(FrpEvent::shot_finished(key.clone())), t0 + 5 * SECOND)
            .unwrap();
        assert_eq!(shot.launch, Some(ball()));
        assert_eq!(shot.flight, None);

        // A raw frame for the closed shot is not a late event: it neither
        // reopens the shot nor emits anything.
        assert!(
            agg.feed_envelope_at(&env(raw_frame(5, Some(key.clone()))), t0 + 6 * SECOND)
                .is_none()
        );
        assert_eq!(agg.pending_count(), 0);
    }

    #[test]
    fn raw_frames_do_not_extend_the_timeout() {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        let t0 = Instant::now();
        let env = |event| FrpEnvelope::new("Dev", event);

        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(key.clone())), t0);
        for n in 1..=31 {
            agg.feed_envelope_at(
                &env(raw_frame(n, Some(key.clone()))),
                t0 + SECOND * u32::try_from(n).unwrap(),
            );
        }
        assert_eq!(agg.pending_count(), 0);
        assert!(
            agg.feed_envelope_at(&env(FrpEvent::shot_finished(key)), t0 + 32 * SECOND)
                .is_none()
        );
    }

    #[test]
    fn protocol_messages_ignored() {
        let mut agg = ShotAggregator::new();
        let msg = FrpMessage::Protocol(crate::FrpProtocolMessage::Init {
            version: "0.2.0".into(),
        });
        assert!(agg.feed(&msg).is_none());
    }

    const SECOND: Duration = Duration::from_secs(1);

    fn ball() -> Launch {
        Launch {
            launch_speed: Some(Velocity::MetersPerSecond(67.2)),
            ..Default::default()
        }
    }

    #[test]
    fn default_timeout_is_thirty_seconds() {
        assert_eq!(DEFAULT_SHOT_TIMEOUT, Duration::from_secs(30));
        assert_eq!(ShotAggregator::new().timeout(), DEFAULT_SHOT_TIMEOUT);
        let agg = ShotAggregator::new().with_timeout(5 * SECOND);
        assert_eq!(agg.timeout(), 5 * SECOND);
    }

    #[test]
    fn shot_finishing_within_the_timeout_completes() {
        let mut agg = ShotAggregator::new();
        let t0 = Instant::now();
        let key = test_key();
        let at = |agg: &mut ShotAggregator, event, secs| {
            let env = FrpEnvelope::new("Dev", event);
            agg.feed_envelope_at(&env, t0 + secs * SECOND)
        };
        assert!(
            at(
                &mut agg,
                FrpEvent::shot_trigger_at(key.clone(), 1_794_063_845_123),
                0
            )
            .is_none()
        );
        assert!(at(&mut agg, FrpEvent::launch(key.clone(), ball()), 8).is_none());
        let shot = at(&mut agg, FrpEvent::shot_finished(key), 30).unwrap();
        assert_eq!(shot.launch, Some(ball()));
        assert_eq!(shot.timestamp, Some(1_794_063_845_123));
        assert_eq!(agg.pending_count(), 0);
    }

    #[test]
    fn expired_shot_is_discarded_and_late_events_are_ignored() {
        let mut agg = ShotAggregator::new();
        let t0 = Instant::now();
        let key = test_key();
        let env = |event| FrpEnvelope::new("Dev", event);

        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(key.clone())), t0);
        agg.feed_envelope_at(&env(FrpEvent::launch(key.clone(), ball())), t0 + SECOND);
        assert_eq!(agg.discard_expired_at(t0 + 30 * SECOND), 0);
        assert_eq!(agg.pending_count(), 1);
        assert_eq!(agg.discard_expired_at(t0 + 31 * SECOND), 1);
        assert_eq!(agg.pending_count(), 0);

        // Late events, a repeated trigger included, start no new partial shot.
        let late = t0 + 32 * SECOND;
        for event in [
            FrpEvent::shot_trigger(key.clone()),
            FrpEvent::launch(key.clone(), ball()),
            FrpEvent::flight_result(key.clone(), FlightResult::new(Basis::Modeled)),
            FrpEvent::club_path(key.clone(), ClubData::default()),
            FrpEvent::face_impact(key.clone(), FaceImpact::default()),
        ] {
            assert!(agg.feed_envelope_at(&env(event), late).is_none());
            assert_eq!(agg.pending_count(), 0);
        }
        assert!(
            agg.feed_envelope_at(&env(FrpEvent::shot_finished(key.clone())), late)
                .is_none()
        );

        // Feeding any event discards expired shots first.
        let other = ShotKey::new("other", 2);
        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(other.clone())), late);
        agg.feed_envelope_at(
            &FrpEnvelope::new("Dev2", telemetry(None)),
            late + 31 * SECOND,
        );
        assert_eq!(agg.pending_count(), 0);
        assert!(
            agg.feed_envelope_at(&env(FrpEvent::shot_finished(other)), late + 31 * SECOND)
                .is_none()
        );
    }

    #[test]
    fn finished_shot_is_not_started_again() {
        let mut agg = ShotAggregator::new();
        let t0 = Instant::now();
        let key = test_key();
        let env = |event| FrpEnvelope::new("Dev", event);
        let route = [
            FrpEvent::shot_trigger(key.clone()),
            FrpEvent::launch(key.clone(), ball()),
            FrpEvent::shot_finished(key.clone()),
        ];

        let first: Vec<_> = route
            .iter()
            .filter_map(|e| agg.feed_envelope_at(&env(e.clone()), t0))
            .collect();
        assert_eq!(first.len(), 1);

        // The same shot by a second route, within the timeout, is ignored.
        let second: Vec<_> = route
            .iter()
            .filter_map(|e| agg.feed_envelope_at(&env(e.clone()), t0 + 10 * SECOND))
            .collect();
        assert!(second.is_empty());
        assert_eq!(agg.pending_count(), 0);

        // A shot id is remembered for one timeout after it closed.
        let much_later = t0 + 31 * SECOND;
        let third: Vec<_> = route
            .iter()
            .filter_map(|e| agg.feed_envelope_at(&env(e.clone()), much_later))
            .collect();
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn repeated_trigger_keeps_the_shot_in_flight() {
        let mut agg = ShotAggregator::new();
        let t0 = Instant::now();
        let key = test_key();
        let env = |event| FrpEnvelope::new("Dev", event);
        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(key.clone())), t0);
        agg.feed_envelope_at(&env(FrpEvent::launch(key.clone(), ball())), t0);
        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(key.clone())), t0 + SECOND);
        let shot = agg
            .feed_envelope_at(&env(FrpEvent::shot_finished(key)), t0 + 2 * SECOND)
            .unwrap();
        assert_eq!(shot.launch, Some(ball()));
    }

    #[test]
    fn custom_timeout_applies() {
        let mut agg = ShotAggregator::new().with_timeout(5 * SECOND);
        let t0 = Instant::now();
        let key = test_key();
        let env = |event| FrpEnvelope::new("Dev", event);
        agg.feed_envelope_at(&env(FrpEvent::shot_trigger(key.clone())), t0);
        assert!(
            agg.feed_envelope_at(&env(FrpEvent::shot_finished(key)), t0 + 6 * SECOND)
                .is_none()
        );
    }

    #[test]
    fn forget_device_clears_its_state_only() {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        for device in ["Gone", "Kept"] {
            agg.feed(&envelope(device, telemetry(Some(Source::Measured))));
            agg.feed(&envelope(device, FrpEvent::shot_trigger(key.clone())));
        }
        agg.forget_device("Gone");
        assert_eq!(agg.source("Gone"), None);
        assert_eq!(agg.source("Kept"), Some(Source::Measured));
        assert_eq!(agg.pending_count(), 1);
        assert!(
            agg.feed(&envelope("Gone", FrpEvent::shot_finished(key.clone())))
                .is_none()
        );
        let shot = agg
            .feed(&envelope("Kept", FrpEvent::shot_finished(key)))
            .unwrap();
        assert_eq!(shot.source, Some(Source::Measured));
    }

    fn chip_ball() -> Launch {
        Launch {
            launch_speed: Some(Velocity::MetersPerSecond(22.4)),
            launch_azimuth: Some(0.8),
            launch_elevation: Some(31.5),
            ..Default::default()
        }
    }

    fn chip_flight(basis: Basis) -> FlightResult {
        FlightResult {
            model: Some("Eagle One flight model 2.1 (Birdie Labs)".into()),
            carry_distance: Some(Distance::Meters(31.2)),
            assumed: Some(crate::AssumedLaunch {
                backspin_rpm: Some(4500),
                sidespin_rpm: Some(0),
                ..Default::default()
            }),
            ..FlightResult::new(basis)
        }
    }

    fn finish_shot(events: Vec<FrpEvent>) -> CompletedShot {
        let mut agg = ShotAggregator::new();
        let key = test_key();
        agg.feed(&envelope("Dev", FrpEvent::shot_trigger(key.clone())));
        for event in events {
            agg.feed(&envelope("Dev", event));
        }
        agg.feed(&envelope("Dev", FrpEvent::shot_finished(key)))
            .unwrap()
    }

    #[test]
    fn launch_for_flight_fills_missing_values_from_a_modeled_result() {
        let key = test_key();
        let shot = finish_shot(vec![
            FrpEvent::launch(key.clone(), chip_ball()),
            FrpEvent::flight_result(key, chip_flight(Basis::Modeled)),
        ]);
        assert_eq!(
            shot.launch_for_flight(),
            Some(Launch {
                backspin_rpm: Some(4500),
                sidespin_rpm: Some(0),
                ..chip_ball()
            })
        );
        // The measured launch and the flight result are kept as received.
        assert_eq!(shot.launch, Some(chip_ball()));
        assert_eq!(shot.flight, Some(chip_flight(Basis::Modeled)));
    }

    #[test]
    fn launch_for_flight_ignores_assumed_on_a_tracked_result() {
        let key = test_key();
        let shot = finish_shot(vec![
            FrpEvent::launch(key.clone(), chip_ball()),
            FrpEvent::flight_result(key, chip_flight(Basis::Tracked)),
        ]);
        assert_eq!(shot.flight, Some(chip_flight(Basis::Tracked)));
        assert_eq!(shot.launch_for_flight(), Some(chip_ball()));
    }

    #[test]
    fn launch_for_flight_prefers_measured_values() {
        let key = test_key();
        let measured = Launch {
            backspin_rpm: Some(3900),
            ..chip_ball()
        };
        let shot = finish_shot(vec![
            FrpEvent::launch(key.clone(), measured.clone()),
            FrpEvent::flight_result(key, chip_flight(Basis::Modeled)),
        ]);
        assert_eq!(
            shot.launch_for_flight(),
            Some(Launch {
                sidespin_rpm: Some(0),
                ..measured
            })
        );
    }

    #[test]
    fn launch_for_flight_without_launch_or_assumed_values() {
        let key = test_key();
        assert_eq!(finish_shot(vec![]).launch_for_flight(), None);
        let shot = finish_shot(vec![FrpEvent::flight_result(
            key.clone(),
            chip_flight(Basis::Tracked),
        )]);
        assert_eq!(shot.launch_for_flight(), None);
        let shot = finish_shot(vec![FrpEvent::launch(key.clone(), Launch::default())]);
        assert_eq!(shot.launch_for_flight(), Some(Launch::default()));
        let shot = finish_shot(vec![FrpEvent::flight_result(
            key,
            chip_flight(Basis::Modeled),
        )]);
        assert_eq!(
            shot.launch_for_flight(),
            Some(Launch {
                backspin_rpm: Some(4500),
                sidespin_rpm: Some(0),
                ..Default::default()
            })
        );
    }

    #[test]
    fn first_flight_result_wins_with_its_assumed_values() {
        let key = test_key();
        let first = FlightResult {
            assumed: None,
            ..chip_flight(Basis::Modeled)
        };
        let shot = finish_shot(vec![
            FrpEvent::launch(key.clone(), chip_ball()),
            FrpEvent::flight_result(key.clone(), first.clone()),
            FrpEvent::flight_result(key, chip_flight(Basis::Modeled)),
        ]);
        assert_eq!(shot.flight, Some(first));
        assert_eq!(shot.launch_for_flight(), Some(chip_ball()));
    }
}
