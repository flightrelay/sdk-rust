//! FRP domain types: shot key, ball launch, flight result, club data, face
//! impact, raw frame, device info.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

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

/// Ball launch measurement data. All fields optional — send what the hardware supports.
///
/// Every value is measured: what the device measured, or a simple translation
/// of it, such as the other spin pair from [`complete_spin`](Self::complete_spin).
/// Results of the ball's flight, such as carry, travel in [`FlightResult`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_speed: Option<Velocity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_azimuth: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_elevation: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backspin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidespin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_spin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spin_axis: Option<f64>,
}

impl Launch {
    /// Fill in a missing spin pair by translating the other, complete pair.
    ///
    /// `backspin_rpm` + `sidespin_rpm` and `total_spin_rpm` + `spin_axis`
    /// describe the same spin: `backspin = total × cos(axis)`,
    /// `sidespin = total × sin(axis)`. When both values of one pair are present
    /// and neither value of the other is, the other pair is filled in and RPM
    /// values are rounded. A total spin of 0 has no axis: zero components
    /// leave `spin_axis` unset, and a total of 0 without an axis completes to
    /// zero components. A partial pair on either side leaves the spin unchanged.
    ///
    /// The translated pair is measured, like the pair it comes from.
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

    /// Fill each value this launch lacks from the `assumed` launch of
    /// `flight`, when its [`basis`](FlightResult::basis) is
    /// [`Modeled`](Basis::Modeled). A value this launch has is kept, and a
    /// [`Tracked`](Basis::Tracked) result changes nothing.
    ///
    /// The filled values are not measured: they are what a flight model
    /// assumed. Use the result to fly the ball, not as a measurement.
    pub fn fill_assumed(&mut self, flight: &FlightResult) {
        let Some(assumed) = flight
            .assumed
            .as_ref()
            .filter(|_| flight.basis == Basis::Modeled)
        else {
            return;
        };
        self.launch_speed = self.launch_speed.or(assumed.launch_speed);
        self.launch_azimuth = self.launch_azimuth.or(assumed.launch_azimuth);
        self.launch_elevation = self.launch_elevation.or(assumed.launch_elevation);
        self.backspin_rpm = self.backspin_rpm.or(assumed.backspin_rpm);
        self.sidespin_rpm = self.sidespin_rpm.or(assumed.sidespin_rpm);
        self.total_spin_rpm = self.total_spin_rpm.or(assumed.total_spin_rpm);
        self.spin_axis = self.spin_axis.or(assumed.spin_axis);
    }
}

/// Launch values a flight model assumed because the shot's [`Launch`]
/// lacks them, e.g. the spin of a chip whose speed and angles were measured
/// but whose spin was not. Sent in [`FlightResult::assumed`] of a
/// [`Modeled`](Basis::Modeled) result only.
///
/// Not measured, so never sent in a [`Launch`]. Same fields, units and
/// signs as [`Launch`]. A sender includes only fields the shot's
/// `launch` does not carry; a receiver that finds a field in both uses
/// the `launch` value. It may hold a partial spin pair.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssumedLaunch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_speed: Option<Velocity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_azimuth: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_elevation: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backspin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidespin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_spin_rpm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spin_axis: Option<f64>,
}

impl AssumedLaunch {
    /// Whether no value is set. An empty `assumed` is omitted on the wire.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Results of the ball's flight: where it landed, where it came to rest, and
/// how it flew. Sent in `flight_result`, at most one per shot.
///
/// [`basis`](Self::basis) is required on the wire: JSON without it, or with a
/// value other than `"tracked"` or `"modeled"`, does not parse as a
/// `FlightResult`. Inside an [`FrpEnvelope`](crate::FrpEnvelope) such an
/// event parses as [`FrpEvent::Unknown`](crate::FrpEvent::Unknown), so a
/// controller ignores it.
///
/// [`assumed`](Self::assumed) carries the launch values a flight model
/// assumed, on a [`Modeled`](Basis::Modeled) result only. Build one with
/// [`FlightResult::new`] and struct update syntax:
///
/// ```
/// use flightrelay::{Basis, Distance, FlightResult};
///
/// let flight = FlightResult {
///     model: Some("Eagle One flight model 2.1 (Birdie Labs)".into()),
///     carry_distance: Some(Distance::Meters(180.5)),
///     ..FlightResult::new(Basis::Modeled)
/// };
/// assert_eq!(
///     serde_json::to_string(&flight).unwrap(),
///     r#"{"basis":"modeled","model":"Eagle One flight model 2.1 (Birdie Labs)","carry_distance":"180.5m"}"#
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlightResult {
    /// How the values were obtained.
    pub basis: Basis,
    /// Free-text name of the flight model and its producer, for display and
    /// logging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
    /// Launch values the flight model assumed because the shot's
    /// `launch` lacks them. Not measured. A receiver ignores it on a
    /// [`Tracked`](Basis::Tracked) result, and uses the `launch` value of
    /// a field that is in both; [`Launch::fill_assumed`] does both.
    ///
    /// Omitted on the wire when `None` or empty. When parsing, an empty or
    /// malformed `assumed` reads as `None` and the rest of the result still
    /// applies.
    #[serde(
        default,
        skip_serializing_if = "assumed_is_empty",
        deserialize_with = "deserialize_assumed"
    )]
    pub assumed: Option<AssumedLaunch>,
}

impl FlightResult {
    /// A flight result on `basis` with no model name and no values.
    #[must_use]
    pub fn new(basis: Basis) -> Self {
        Self {
            basis,
            model: None,
            carry_distance: None,
            carry_offline: None,
            total_distance: None,
            total_offline: None,
            roll_distance: None,
            max_height: None,
            flight_time: None,
            descent_angle: None,
            assumed: None,
        }
    }
}

#[allow(clippy::ref_option)] // serde's skip_serializing_if passes `&Option<_>`
fn assumed_is_empty(assumed: &Option<AssumedLaunch>) -> bool {
    assumed.as_ref().is_none_or(AssumedLaunch::is_empty)
}

/// Reads `assumed`, mapping `null`, an empty object and a value that isn't a
/// valid launch object to `None`, so a bad `assumed` never costs the rest of
/// the flight result.
fn deserialize_assumed<'de, D>(deserializer: D) -> Result<Option<AssumedLaunch>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|v| serde_json::from_value::<AssumedLaunch>(v).ok())
        .filter(|assumed| !assumed.is_empty()))
}

/// How the values of a [`FlightResult`] were obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// The device followed the ball's flight, and the values come from that
    /// tracking.
    Tracked,
    /// A flight model produced the values, e.g. from the launch values. A
    /// result that a model completed or extended is modeled too.
    Modeled,
}

impl fmt::Display for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tracked => write!(f, "tracked"),
            Self::Modeled => write!(f, "modeled"),
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

/// One protocol message behind a device's FRP events, sent as `raw_frame`
/// for debugging.
///
/// A device sends raw frames only to a controller that asked for them with
/// `raw_frames` in its `start`, and never a frame that carries device or
/// user identity (serial numbers, hardware addresses, network names,
/// account data). Raw frames are not shot events: they never create,
/// complete or change a shot, even with a [`key`](Self::key).
///
/// [`data`](Self::data) is opaque to FRP. On the wire it is lowercase
/// hexadecimal; when parsing, either case is accepted, and a `data` that is
/// not an even number of hexadecimal digits does not parse. Inside an
/// [`FrpEnvelope`](crate::FrpEnvelope) such an event parses as
/// [`FrpEvent::Unknown`](crate::FrpEvent::Unknown).
///
/// ```
/// use flightrelay::{RawDir, RawFrame};
///
/// let frame = RawFrame {
///     label: Some("result".into()),
///     ..RawFrame::new(RawDir::Rx, 1042, vec![0x0a, 0x1b, 0x2c])
/// };
/// assert_eq!(
///     serde_json::to_string(&frame).unwrap(),
///     r#"{"dir":"rx","seq":1042,"label":"result","data":"0a1b2c"}"#
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFrame {
    /// Direction, seen from the device implementation.
    pub dir: RawDir,
    /// Per-device counter: increases by one for every raw frame the device
    /// sends for its `device` id, so a controller sees gaps and a forwarder
    /// recognizes a repeat.
    pub seq: u64,
    /// When the device implementation received or sent the message, in
    /// milliseconds since the Unix epoch (UTC) by the device's clock. A value
    /// that is not a non-negative integer parses as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::message::deserialize_lenient"
    )]
    pub timestamp: Option<u64>,
    /// Short text the device implementation chooses, e.g. the message's name.
    /// A value that is not a string parses as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::message::deserialize_lenient"
    )]
    pub label: Option<String>,
    /// The message's bytes: exactly one protocol message, as the device
    /// implementation framed it.
    #[serde(with = "hex")]
    pub data: Vec<u8>,
    /// The shot the message belongs to, when the device knows it. A value
    /// that is not a shot key parses as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::message::deserialize_lenient"
    )]
    pub key: Option<ShotKey>,
}

impl RawFrame {
    /// A raw frame of `data` in direction `dir`, number `seq`, with no
    /// timestamp, label or shot key.
    #[must_use]
    pub fn new(dir: RawDir, seq: u64, data: impl Into<Vec<u8>>) -> Self {
        Self {
            dir,
            seq,
            timestamp: None,
            label: None,
            data: data.into(),
            key: None,
        }
    }
}

/// Direction of a [`RawFrame`], seen from the device implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawDir {
    /// A message the device implementation received, e.g. from the launch
    /// monitor.
    Rx,
    /// A message the device implementation sent, e.g. to the launch monitor.
    Tx,
}

impl fmt::Display for RawDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rx => write!(f, "rx"),
            Self::Tx => write!(f, "tx"),
        }
    }
}

/// Hexadecimal serde for [`RawFrame::data`]: writes lowercase, reads either
/// case, and rejects an odd number of digits or a character that isn't a
/// hexadecimal digit.
mod hex {
    use serde::{Deserialize, Deserializer, Serializer};

    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    pub(super) fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(data.len() * 2);
        for byte in data {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        serializer.serialize_str(&text)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        decode(&text).ok_or_else(|| {
            serde::de::Error::custom("data is not an even number of hexadecimal digits")
        })
    }

    fn decode(text: &str) -> Option<Vec<u8>> {
        let digits = text.as_bytes();
        if !digits.len().is_multiple_of(2) {
            return None;
        }
        digits
            .chunks_exact(2)
            .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
            .collect()
    }

    fn nibble(digit: u8) -> Option<u8> {
        match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            b'A'..=b'F' => Some(digit - b'A' + 10),
            _ => None,
        }
    }
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
        let mut ball = Launch {
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
        let mut ball = Launch {
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
        let mut ball = Launch {
            total_spin_rpm: Some(2650),
            spin_axis: Some(12.5),
            ..Default::default()
        };
        ball.complete_spin();
        let mut back = Launch {
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
        let mut ball = Launch {
            backspin_rpm: Some(0),
            sidespin_rpm: Some(0),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.total_spin_rpm, Some(0));
        assert_eq!(ball.spin_axis, None);

        let mut ball = Launch {
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
        let mut ball = Launch {
            backspin_rpm: Some(3200),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(
            ball,
            Launch {
                backspin_rpm: Some(3200),
                ..Default::default()
            }
        );

        // A nonzero total without an axis: no components are derived.
        let mut ball = Launch {
            total_spin_rpm: Some(3000),
            ..Default::default()
        };
        ball.complete_spin();
        assert_eq!(ball.backspin_rpm, None);
        assert_eq!(ball.sidespin_rpm, None);
    }

    #[test]
    fn complete_spin_keeps_both_pairs() {
        let original = Launch {
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

    #[test]
    fn flight_result_serializes_basis_first_and_omits_absent_values() {
        let flight = FlightResult {
            carry_distance: Some(Distance::Meters(180.5)),
            descent_angle: Some(38.4),
            ..FlightResult::new(Basis::Tracked)
        };
        assert_eq!(
            serde_json::to_string(&flight).unwrap(),
            r#"{"basis":"tracked","carry_distance":"180.5m","descent_angle":38.4}"#
        );
        assert_eq!(
            serde_json::to_string(&FlightResult::new(Basis::Modeled)).unwrap(),
            r#"{"basis":"modeled"}"#
        );
    }

    #[test]
    fn basis_is_required_and_closed() {
        for (basis, name) in [(Basis::Tracked, "tracked"), (Basis::Modeled, "modeled")] {
            assert_eq!(basis.to_string(), name);
            let json = format!(r#"{{"basis":"{name}"}}"#);
            assert_eq!(
                serde_json::from_str::<FlightResult>(&json).unwrap(),
                FlightResult::new(basis)
            );
        }
        for json in [
            r#"{"carry_distance":"180.5m"}"#,
            r#"{"basis":null,"carry_distance":"180.5m"}"#,
            r#"{"basis":"Modeled","carry_distance":"180.5m"}"#,
            r#"{"basis":"estimated","carry_distance":"180.5m"}"#,
            r#"{"basis":1,"carry_distance":"180.5m"}"#,
        ] {
            assert!(
                serde_json::from_str::<FlightResult>(json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn launch_ignores_flight_result_keys() {
        let ball: Launch = serde_json::from_str(
            r#"{"launch_speed":"67.2mps","carry_distance":"180.5m","total_distance":"195.0m",
                "max_height":"28.3m","flight_time":6.2,"descent_angle":38.4,"backspin_rpm":3200}"#,
        )
        .unwrap();
        assert_eq!(
            ball,
            Launch {
                launch_speed: Some(Velocity::MetersPerSecond(67.2)),
                backspin_rpm: Some(3200),
                ..Default::default()
            }
        );
        assert_eq!(
            serde_json::to_string(&ball).unwrap(),
            r#"{"launch_speed":"67.2mps","backspin_rpm":3200}"#
        );
    }

    #[test]
    fn raw_dir_names_match_the_wire() {
        for (dir, name) in [(RawDir::Rx, "rx"), (RawDir::Tx, "tx")] {
            assert_eq!(dir.to_string(), name);
            assert_eq!(serde_json::to_string(&dir).unwrap(), format!(r#""{name}""#));
        }
    }

    #[test]
    fn raw_frame_omits_absent_optional_fields() {
        let frame = RawFrame::new(RawDir::Tx, 0, Vec::new());
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"dir":"tx","seq":0,"data":""}"#
        );
    }

    fn chip_assumed() -> AssumedLaunch {
        AssumedLaunch {
            backspin_rpm: Some(4500),
            sidespin_rpm: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn assumed_round_trips_after_the_flight_values() {
        let flight = FlightResult {
            model: Some("Eagle One flight model 2.1 (Birdie Labs)".into()),
            carry_distance: Some(Distance::Meters(31.2)),
            assumed: Some(chip_assumed()),
            ..FlightResult::new(Basis::Modeled)
        };
        let json = r#"{"basis":"modeled","model":"Eagle One flight model 2.1 (Birdie Labs)","carry_distance":"31.2m","assumed":{"backspin_rpm":4500,"sidespin_rpm":0}}"#;
        assert_eq!(serde_json::to_string(&flight).unwrap(), json);
        assert_eq!(serde_json::from_str::<FlightResult>(json).unwrap(), flight);
    }

    #[test]
    fn empty_assumed_is_omitted_and_reads_as_none() {
        let flight = FlightResult {
            carry_distance: Some(Distance::Meters(31.2)),
            assumed: Some(AssumedLaunch::default()),
            ..FlightResult::new(Basis::Modeled)
        };
        assert!(AssumedLaunch::default().is_empty());
        assert!(!chip_assumed().is_empty());
        assert_eq!(
            serde_json::to_string(&flight).unwrap(),
            r#"{"basis":"modeled","carry_distance":"31.2m"}"#
        );
        for assumed in ["{}", "null"] {
            let json =
                format!(r#"{{"basis":"modeled","carry_distance":"31.2m","assumed":{assumed}}}"#);
            assert_eq!(
                serde_json::from_str::<FlightResult>(&json).unwrap().assumed,
                None,
                "{assumed}"
            );
        }
    }

    #[test]
    fn malformed_assumed_is_dropped_and_the_rest_applies() {
        for assumed in [
            "5",
            r#""4500rpm""#,
            "[4500]",
            r#"{"backspin_rpm":"lots"}"#,
            r#"{"launch_speed":22.4}"#,
        ] {
            for basis in ["modeled", "tracked"] {
                let json = format!(
                    r#"{{"basis":"{basis}","carry_distance":"31.2m","assumed":{assumed}}}"#
                );
                let flight: FlightResult = serde_json::from_str(&json).unwrap();
                assert_eq!(flight.assumed, None, "{json}");
                assert_eq!(
                    flight.carry_distance,
                    Some(Distance::Meters(31.2)),
                    "{json}"
                );
            }
        }
    }

    #[test]
    fn assumed_ignores_keys_outside_the_launch() {
        let flight: FlightResult = serde_json::from_str(
            r#"{"basis":"modeled","assumed":{"backspin_rpm":4500,"carry_distance":"31.2m"}}"#,
        )
        .unwrap();
        assert_eq!(
            flight.assumed,
            Some(AssumedLaunch {
                backspin_rpm: Some(4500),
                ..Default::default()
            })
        );
    }

    fn chip_ball() -> Launch {
        Launch {
            launch_speed: Some(Velocity::MetersPerSecond(22.4)),
            launch_azimuth: Some(0.8),
            launch_elevation: Some(31.5),
            ..Default::default()
        }
    }

    #[test]
    fn fill_assumed_fills_missing_values_from_a_modeled_result() {
        let mut launch = chip_ball();
        launch.fill_assumed(&FlightResult {
            assumed: Some(chip_assumed()),
            ..FlightResult::new(Basis::Modeled)
        });
        assert_eq!(
            launch,
            Launch {
                backspin_rpm: Some(4500),
                sidespin_rpm: Some(0),
                ..chip_ball()
            }
        );
    }

    #[test]
    fn fill_assumed_keeps_measured_values() {
        // A field in both: the launch value wins.
        let mut launch = Launch {
            backspin_rpm: Some(3900),
            ..chip_ball()
        };
        launch.fill_assumed(&FlightResult {
            assumed: Some(AssumedLaunch {
                launch_speed: Some(Velocity::MetersPerSecond(30.0)),
                ..chip_assumed()
            }),
            ..FlightResult::new(Basis::Modeled)
        });
        assert_eq!(launch.launch_speed, Some(Velocity::MetersPerSecond(22.4)));
        assert_eq!(launch.backspin_rpm, Some(3900));
        assert_eq!(launch.sidespin_rpm, Some(0));
    }

    #[test]
    fn fill_assumed_ignores_a_tracked_result() {
        let mut launch = chip_ball();
        launch.fill_assumed(&FlightResult {
            assumed: Some(chip_assumed()),
            ..FlightResult::new(Basis::Tracked)
        });
        assert_eq!(launch, chip_ball());
    }

    #[test]
    fn fill_assumed_completes_a_partial_pair() {
        // Measured total spin without an axis; the model assumed the axis.
        let mut launch = Launch {
            total_spin_rpm: Some(3000),
            ..chip_ball()
        };
        let partial = AssumedLaunch {
            spin_axis: Some(0.0),
            ..Default::default()
        };
        launch.fill_assumed(&FlightResult {
            assumed: Some(partial),
            ..FlightResult::new(Basis::Modeled)
        });
        assert_eq!(launch.total_spin_rpm, Some(3000));
        assert_eq!(launch.spin_axis, Some(0.0));
        assert_eq!(launch.backspin_rpm, None);

        // An assumed partial pair on its own stays partial.
        let mut launch = chip_ball();
        launch.fill_assumed(&FlightResult {
            assumed: Some(AssumedLaunch {
                backspin_rpm: Some(4500),
                ..Default::default()
            }),
            ..FlightResult::new(Basis::Modeled)
        });
        assert_eq!(launch.backspin_rpm, Some(4500));
        assert_eq!(launch.sidespin_rpm, None);
        assert_eq!(launch.total_spin_rpm, None);
    }
}
