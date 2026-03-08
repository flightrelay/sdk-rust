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

/// Ball flight measurement data. All fields optional — send what the hardware supports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub total_distance: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_distance: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<Distance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backspin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidespin_rpm: Option<i32>,
}

/// Club head measurement data. All fields optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

/// Face impact location — where on the club face the ball was struck.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
