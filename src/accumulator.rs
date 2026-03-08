//! Shot accumulator for controllers.
//!
//! Collects shot lifecycle events and produces a complete shot when
//! `shot_finished` arrives. Requires the `controller` feature.

use std::collections::HashMap;

use crate::message::{FrpEnvelope, FrpEvent, FrpMessage};
use crate::types::{BallFlight, ClubData, FaceImpact, ShotKey};

/// A completed shot with all available data.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedShot {
    /// Device that produced this shot.
    pub device: String,
    /// Shot correlation key.
    pub key: ShotKey,
    /// Ball flight data (if hardware provided it).
    pub ball: Option<BallFlight>,
    /// Club path data (if hardware provided it).
    pub club: Option<ClubData>,
    /// Face impact data (if hardware provided it).
    pub impact: Option<FaceImpact>,
}

/// Accumulates events for a single in-flight shot.
#[derive(Debug)]
struct ShotAccumulator {
    device: String,
    key: ShotKey,
    ball: Option<BallFlight>,
    club: Option<ClubData>,
    impact: Option<FaceImpact>,
}

impl ShotAccumulator {
    fn new(device: String, key: ShotKey) -> Self {
        Self {
            device,
            key,
            ball: None,
            club: None,
            impact: None,
        }
    }

    fn finish(self) -> CompletedShot {
        CompletedShot {
            device: self.device,
            key: self.key,
            ball: self.ball,
            club: self.club,
            impact: self.impact,
        }
    }
}

/// Manages multiple in-flight shots, keyed by `(device, shot_id)`.
///
/// Feed [`FrpMessage`] events via [`feed`](Self::feed) and receive
/// [`CompletedShot`] when a shot lifecycle finishes.
///
/// ```ignore
/// let mut agg = ShotAggregator::new();
/// // ... receive messages from FRP connection ...
/// if let Some(shot) = agg.feed(&msg) {
///     println!("Shot complete: {:?}", shot.ball);
/// }
/// ```
#[derive(Debug, Default)]
pub struct ShotAggregator {
    pending: HashMap<(String, String), ShotAccumulator>,
}

impl ShotAggregator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        match &env.event {
            FrpEvent::ShotTrigger { key } => {
                let acc = ShotAccumulator::new(env.device.clone(), key.clone());
                self.pending
                    .insert((env.device.clone(), key.shot_id.clone()), acc);
                None
            }
            FrpEvent::BallFlight { key, ball } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.ball = Some(ball.clone());
                }
                None
            }
            FrpEvent::ClubPath { key, club } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.club = Some(club.clone());
                }
                None
            }
            FrpEvent::FaceImpact { key, impact } => {
                if let Some(acc) = self
                    .pending
                    .get_mut(&(env.device.clone(), key.shot_id.clone()))
                {
                    acc.impact = Some(impact.clone());
                }
                None
            }
            FrpEvent::ShotFinished { key } => self
                .pending
                .remove(&(env.device.clone(), key.shot_id.clone()))
                .map(ShotAccumulator::finish),
            FrpEvent::DeviceTelemetry { .. } | FrpEvent::Alert { .. } | FrpEvent::Unknown => None,
        }
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
            event: FrpEvent::ShotTrigger { key: key.clone() },
        });
        assert!(agg.feed(&trigger).is_none());
        assert_eq!(agg.pending_count(), 1);

        // Ball flight
        let ball = BallFlight {
            launch_speed: Some(Velocity::MetersPerSecond(67.2)),
            launch_azimuth: Some(-1.3),
            launch_elevation: Some(14.2),
            carry_distance: Some(Distance::Meters(180.5)),
            total_distance: None,
            roll_distance: None,
            max_height: None,
            flight_time: None,
            backspin_rpm: Some(3200),
            sidespin_rpm: Some(-450),
        };
        let ball_msg = FrpMessage::Envelope(FrpEnvelope {
            device: device.into(),
            event: FrpEvent::BallFlight {
                key: key.clone(),
                ball: ball.clone(),
            },
        });
        assert!(agg.feed(&ball_msg).is_none());

        // Club path
        let club = ClubData {
            club_speed: Some(Velocity::MetersPerSecond(42.1)),
            club_speed_post: None,
            path: Some(-2.1),
            attack_angle: None,
            face_angle: None,
            dynamic_loft: None,
            smash_factor: None,
            swing_plane_horizontal: None,
            swing_plane_vertical: None,
            club_offset: None,
            club_height: None,
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
        assert_eq!(shot.ball, Some(ball));
        assert_eq!(shot.club, Some(club));
        assert_eq!(shot.impact, Some(impact));
        assert_eq!(agg.pending_count(), 0);
    }

    #[test]
    fn shot_without_club_or_impact() {
        let mut agg = ShotAggregator::new();
        let key = test_key();

        agg.feed(&FrpMessage::Envelope(FrpEnvelope {
            device: "Dev".into(),
            event: FrpEvent::ShotTrigger { key: key.clone() },
        }));

        let ball = BallFlight {
            launch_speed: Some(Velocity::MilesPerHour(100.0)),
            launch_azimuth: None,
            launch_elevation: Some(12.0),
            carry_distance: None,
            total_distance: None,
            roll_distance: None,
            max_height: None,
            flight_time: None,
            backspin_rpm: None,
            sidespin_rpm: None,
        };
        agg.feed(&FrpMessage::Envelope(FrpEnvelope {
            device: "Dev".into(),
            event: FrpEvent::BallFlight {
                key: key.clone(),
                ball,
            },
        }));

        let shot = agg
            .feed(&FrpMessage::Envelope(FrpEnvelope {
                device: "Dev".into(),
                event: FrpEvent::ShotFinished { key },
            }))
            .unwrap();
        assert!(shot.ball.is_some());
        assert!(shot.club.is_none());
        assert!(shot.impact.is_none());
    }

    #[test]
    fn protocol_messages_ignored() {
        let mut agg = ShotAggregator::new();
        let msg = FrpMessage::Protocol(crate::FrpProtocolMessage::Init {
            version: "0.1.0".into(),
        });
        assert!(agg.feed(&msg).is_none());
    }
}
