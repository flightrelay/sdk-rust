//! FRP domain types: shot key, ball flight, club data, face impact, device info.

use serde::{Deserialize, Serialize};

use crate::units::{Distance, Velocity};

/// Correlates shot lifecycle events. Generated once at trigger time, carried on
/// every event in the shot sequence.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShotKey {
    /// Unique shot ID (UUID v4 string).
    pub shot_id: String,
    /// Monotonic counter from the device, for human display.
    pub shot_number: u32,
}

impl ShotKey {
    /// A shot key from a UUID v4 `shot_id` and a device-supplied `shot_number`.
    #[must_use]
    pub fn new(shot_id: impl Into<String>, shot_number: u32) -> Self {
        Self {
            shot_id: shot_id.into(),
            shot_number,
        }
    }
}

/// Ball flight measurement data. All fields optional — send what the hardware supports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BallFlight {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_speed: Option<Velocity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_azimuth: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_elevation: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carry_distance: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carry_offline: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_distance: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_offline: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_distance: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descent_angle: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backspin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidespin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_spin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spin_axis: Option<f64>,
}

impl BallFlight {
    /// Fill in a missing spin pair from the other, complete pair.
    ///
    /// `backspin_rpm` + `sidespin_rpm` and `total_spin_rpm` + `spin_axis`
    /// describe the same spin: `backspin = total × cos(axis)`,
    /// `sidespin = total × sin(axis)`. When both values of one pair are present
    /// and neither value of the other is, the other pair is computed and RPM
    /// values are rounded. A total spin of 0 has no axis: zero components
    /// leave `spin_axis` unset, and a total of 0 without an axis completes to
    /// zero components. A partial pair on either side leaves the spin unchanged.
    #[allow(clippy::cast_possible_truncation)]
    pub fn complete_spin(&mut self) {
        let components_absent = self.backspin_rpm.is_none() && self.sidespin_rpm.is_none();
        let total_absent = self.total_spin_rpm.is_none() && self.spin_axis.is_none();
        match (
            self.backspin_rpm,
            self.sidespin_rpm,
            self.total_spin_rpm,
            self.spin_axis,
        ) {
            (Some(back), Some(side), _, _) if total_absent => {
                let (back, side) = (f64::from(back), f64::from(side));
                let total = back.hypot(side).round() as i32;
                self.total_spin_rpm = Some(total);
                if total != 0 {
                    self.spin_axis = Some(side.atan2(back).to_degrees());
                }
            }
            (_, _, Some(total), axis) if components_absent => {
                let axis = match axis {
                    Some(axis) => axis.to_radians(),
                    None if total == 0 => 0.0,
                    None => return,
                };
                let total = f64::from(total);
                self.backspin_rpm = Some((total * axis.cos()).round() as i32);
                self.sidespin_rpm = Some((total * axis.sin()).round() as i32);
            }
            _ => {}
        }
    }
}

/// Club head measurement data. All fields optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClubData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club_speed: Option<Velocity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club_speed_post: Option<Velocity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attack_angle: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub face_angle: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_loft: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smash_factor: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swing_plane_horizontal: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swing_plane_vertical: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club_offset: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club_height: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spin_loft: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_point_distance: Option<Distance>,
}

/// Face impact location — where on the club face the ball was struck.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FaceImpact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lateral: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<Distance>,
}

/// Standard telemetry key constants.
pub mod telemetry {
    pub const READY: &str = "ready";
    pub const BATTERY_PCT: &str = "battery_pct";
    pub const TILT: &str = "tilt";
    pub const ROLL: &str = "roll";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_spin_from_components() {
        let mut ball = BallFlight {
            backspin_rpm: Some(3200),
            sidespin_rpm: Some(-450),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.total_spin_rpm, Some(3231));
        assert!((ball.spin_axis.unwrap() - -8.0047).abs() < 1e-3);
        assert_eq!(ball.backspin_rpm, Some(3200));
        assert_eq!(ball.sidespin_rpm, Some(-450));
    }

    #[test]
    fn complete_spin_from_total_and_axis() {
        let mut ball = BallFlight {
            total_spin_rpm: Some(3231),
            spin_axis: Some(-8.0),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.backspin_rpm, Some(3200));
        assert_eq!(ball.sidespin_rpm, Some(-450));
        assert_eq!(ball.total_spin_rpm, Some(3231));
        assert_eq!(ball.spin_axis, Some(-8.0));
    }

    #[test]
    fn complete_spin_pairs_agree_both_ways() {
        let mut ball = BallFlight {
            total_spin_rpm: Some(2650),
            spin_axis: Some(12.5),
            ..Default::default()
        };
        ball.complete_spin();
        let mut back = BallFlight {
            backspin_rpm: ball.backspin_rpm,
            sidespin_rpm: ball.sidespin_rpm,
            ..Default::default()
        };
        back.complete_spin();
        assert_eq!(back.total_spin_rpm, Some(2650));
        assert!((back.spin_axis.unwrap() - 12.5).abs() < 0.05);
    }

    #[test]
    fn complete_spin_zero_total_has_no_axis() {
        let mut ball = BallFlight {
            backspin_rpm: Some(0),
            sidespin_rpm: Some(0),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.total_spin_rpm, Some(0));
        assert_eq!(ball.spin_axis, None);

        let mut ball = BallFlight {
            total_spin_rpm: Some(0),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.backspin_rpm, Some(0));
        assert_eq!(ball.sidespin_rpm, Some(0));
    }

    #[test]
    fn complete_spin_ignores_partial_pairs() {
        // Backspin without sidespin: no total or axis is derived.
        let mut ball = BallFlight {
            backspin_rpm: Some(3200),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(
            ball,
            BallFlight {
                backspin_rpm: Some(3200),
                ..Default::default()
            }
        );

        // A nonzero total without an axis: no components are derived.
        let mut ball = BallFlight {
            total_spin_rpm: Some(3000),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.backspin_rpm, None);
        assert_eq!(ball.sidespin_rpm, None);
    }

    #[test]
    fn complete_spin_keeps_both_pairs() {
        let original = BallFlight {
            backspin_rpm: Some(3200),
            sidespin_rpm: Some(-450),
            total_spin_rpm: Some(3231),
            spin_axis: Some(-8.0),
            ..Default::default()
        };
        let mut ball = original.clone();
        ball.complete_spin();
        assert_eq!(ball, original);
    }
}
