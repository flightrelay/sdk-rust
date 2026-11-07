//! FRP message types and parsing.
//!
//! FRP has two wire shapes:
//! - **Envelopes**: `{ "device": "...", "event": { "kind": "..." } }` — device events
//! - **Protocol messages**: `{ "kind": "..." }` — handshake, alerts, commands
//!
//! [`FrpMessage::parse`] inspects for the `"device"` key to dispatch.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::FrpError;
use crate::types::{ClubData, FaceImpact, FlightResult, Launch, RawFrame, ShotKey};

/// Top-level FRP message — either an envelope (device event) or a protocol message.
// Messages are parsed, handled and dropped one at a time, so the size gap
// between variants costs nothing; boxing `Envelope` would only complicate
// matching on it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum FrpMessage {
    /// Device event envelope: `{ "device": "...", "event": { "kind": "..." } }`
    Envelope(FrpEnvelope),
    /// Protocol-level message: `{ "kind": "..." }`
    Protocol(FrpProtocolMessage),
}

impl FrpMessage {
    /// Parse a JSON string into an `FrpMessage`.
    ///
    /// Inspects for the `"device"` key to distinguish envelopes from protocol messages.
    ///
    /// # Errors
    ///
    /// Returns `FrpError::Json` if the JSON is malformed or doesn't match either
    /// shape. An envelope whose event has a known `kind` but doesn't match it
    /// is not an error: its event is [`FrpEvent::Unknown`].
    pub fn parse(json: &str) -> Result<Self, FrpError> {
        let raw: serde_json::Value = serde_json::from_str(json)?;
        if raw.get("device").is_some() {
            Ok(Self::Envelope(serde_json::from_value(raw)?))
        } else if raw.get("kind").is_some() {
            Ok(Self::Protocol(serde_json::from_value(raw)?))
        } else {
            Ok(Self::Protocol(FrpProtocolMessage::Unknown))
        }
    }

    /// Serialize this message to a JSON string.
    ///
    /// # Errors
    ///
    /// Returns `FrpError::Json` if serialization fails.
    pub fn to_json(&self) -> Result<String, FrpError> {
        match self {
            Self::Envelope(env) => Ok(serde_json::to_string(env)?),
            Self::Protocol(proto) => Ok(serde_json::to_string(proto)?),
        }
    }
}

/// Device event envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrpEnvelope {
    /// Identifier of the originating launch monitor (e.g. `"EagleOne-X4K2"`).
    pub device: String,
    /// The typed event. An event of a known kind whose fields don't match
    /// that kind parses as [`FrpEvent::Unknown`].
    #[serde(deserialize_with = "deserialize_event")]
    pub event: FrpEvent,
}

impl FrpEnvelope {
    /// Wrap `event` in an envelope for the launch monitor `device`.
    #[must_use]
    pub fn new(device: impl Into<String>, event: FrpEvent) -> Self {
        Self {
            device: device.into(),
            event,
        }
    }
}

/// Device events, tagged by `"kind"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrpEvent {
    /// Ball strike detected.
    ///
    /// `timestamp` is when the device detected the strike, in milliseconds
    /// since the Unix epoch (UTC), from the device's clock. A value that is
    /// not a non-negative integer parses as `None`, so the trigger still
    /// applies.
    ShotTrigger {
        key: ShotKey,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_lenient"
        )]
        timestamp: Option<u64>,
    },
    /// Ball launch measurement data.
    Launch {
        key: ShotKey,
        launch: Launch,
    },
    /// Results of the ball's flight. At most one per shot. Inside an
    /// [`FrpEnvelope`], one without a recognized `basis` parses as
    /// [`Unknown`](Self::Unknown).
    FlightResult {
        key: ShotKey,
        flight: FlightResult,
    },
    ClubPath {
        key: ShotKey,
        club: ClubData,
    },
    FaceImpact {
        key: ShotKey,
        impact: FaceImpact,
    },
    ShotFinished {
        key: ShotKey,
    },
    /// Device identification and telemetry.
    ///
    /// `source` declares where this device's shots originate. A value that
    /// is not recognized parses as `None` (unknown), so the rest of the event
    /// still applies.
    DeviceTelemetry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manufacturer: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        firmware: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_lenient"
        )]
        source: Option<Source>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry: Option<HashMap<String, String>>,
    },
    Alert {
        severity: Severity,
        message: String,
    },
    /// One protocol message behind the device's events, for debugging. Sent
    /// only to a controller that asked for raw frames in `start`. Not a shot
    /// event, even with a key.
    RawFrame(RawFrame),
    /// An event to ignore: an unknown kind or, inside an [`FrpEnvelope`], a
    /// known kind whose fields don't match it, e.g. a `flight_result` without
    /// a recognized `basis`. A controller ignores it and a forwarder drops it.
    #[serde(other)]
    Unknown,
}

impl FrpEvent {
    /// Ball strike detected — the first event of a shot sequence.
    #[must_use]
    pub fn shot_trigger(key: ShotKey) -> Self {
        Self::ShotTrigger {
            key,
            timestamp: None,
        }
    }

    /// Ball strike detected at `timestamp`, in milliseconds since the Unix
    /// epoch (UTC) by the device's clock.
    #[must_use]
    pub fn shot_trigger_at(key: ShotKey, timestamp: u64) -> Self {
        Self::ShotTrigger {
            key,
            timestamp: Some(timestamp),
        }
    }

    /// Ball launch measurement data.
    #[must_use]
    pub fn launch(key: ShotKey, launch: Launch) -> Self {
        Self::Launch { key, launch }
    }

    /// Results of the ball's flight.
    #[must_use]
    pub fn flight_result(key: ShotKey, flight: FlightResult) -> Self {
        Self::FlightResult { key, flight }
    }

    /// Club head measurement data.
    #[must_use]
    pub fn club_path(key: ShotKey, club: ClubData) -> Self {
        Self::ClubPath { key, club }
    }

    /// Club face impact location.
    #[must_use]
    pub fn face_impact(key: ShotKey, impact: FaceImpact) -> Self {
        Self::FaceImpact { key, impact }
    }

    /// Shot sequence complete — the terminal event of a shot sequence.
    #[must_use]
    pub fn shot_finished(key: ShotKey) -> Self {
        Self::ShotFinished { key }
    }

    /// One protocol message behind the device's events, for debugging.
    #[must_use]
    pub fn raw_frame(frame: RawFrame) -> Self {
        Self::RawFrame(frame)
    }
}

/// Protocol-level messages (no device envelope).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrpProtocolMessage {
    /// Controller → Device: version negotiation.
    ///
    /// `raw_frames` asks the device for `raw_frame` events. It is omitted on
    /// the wire when `false`, and any value other than `true` reads as
    /// `false`.
    Start {
        version: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "std::ops::Not::not",
            deserialize_with = "deserialize_flag"
        )]
        raw_frames: bool,
    },
    /// Device → Controller: version confirmation.
    Init { version: String },
    /// Protocol-level alert (no device context).
    Alert { severity: Severity, message: String },
    /// Controller → Device: set detection mode, handedness and/or club.
    ///
    /// All fields are optional and latched independently — the most recent
    /// value for each field is the active value. Omitting a field does not
    /// reset it. A field whose value is not recognized parses as `None`, so
    /// the rest of the message still applies.
    SetDetectionMode {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_lenient"
        )]
        mode: Option<DetectionMode>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_lenient"
        )]
        handed: Option<Handedness>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_lenient"
        )]
        club: Option<Club>,
    },
    /// Unknown protocol message kind — per spec, must be silently ignored.
    #[serde(other)]
    Unknown,
}

/// Alert severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warn,
    Error,
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warn => write!(f, "warn"),
            Self::Error => write!(f, "error"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Where a device's shots originate, declared on `device_telemetry`.
///
/// `None` where a `Source` is optional means unknown. A controller that
/// requires measured shots treats unknown as not measured. Relaying a shot
/// never changes its source. New classes may be added in minor releases, so
/// matches need a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Source {
    /// A physical launch monitor measured the shots.
    Measured,
    /// No physical measurement: simulators, mocks, generated or test shots.
    Virtual,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Measured => write!(f, "measured"),
            Self::Virtual => write!(f, "virtual"),
        }
    }
}

/// Shot detection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionMode {
    Full,
    Putting,
    Chipping,
}

impl fmt::Display for DetectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::Putting => write!(f, "putting"),
            Self::Chipping => write!(f, "chipping"),
        }
    }
}

/// Player handedness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Handedness {
    /// Right-handed.
    #[serde(rename = "rh")]
    Right,
    /// Left-handed.
    #[serde(rename = "lh")]
    Left,
}

impl fmt::Display for Handedness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Right => write!(f, "rh"),
            Self::Left => write!(f, "lh"),
        }
    }
}

/// Club in play, sent as a tracking hint on `set_detection_mode`.
///
/// Serializes to the FRP club codes (`"DR"`, `"7I"`, `"PT"`, ...). `Display`
/// and `FromStr` use the same codes, case-sensitively.
///
/// Ordered driver through putter, as in [`Club::ALL`]. New codes may be added
/// in minor releases, so matches need a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Club {
    #[serde(rename = "DR")]
    Driver,
    #[serde(rename = "MD")]
    MiniDriver,
    #[serde(rename = "2W")]
    Wood2,
    #[serde(rename = "3W")]
    Wood3,
    #[serde(rename = "4W")]
    Wood4,
    #[serde(rename = "5W")]
    Wood5,
    #[serde(rename = "6W")]
    Wood6,
    #[serde(rename = "7W")]
    Wood7,
    #[serde(rename = "8W")]
    Wood8,
    #[serde(rename = "9W")]
    Wood9,
    #[serde(rename = "1H")]
    Hybrid1,
    #[serde(rename = "2H")]
    Hybrid2,
    #[serde(rename = "3H")]
    Hybrid3,
    #[serde(rename = "4H")]
    Hybrid4,
    #[serde(rename = "5H")]
    Hybrid5,
    #[serde(rename = "6H")]
    Hybrid6,
    #[serde(rename = "7H")]
    Hybrid7,
    #[serde(rename = "8H")]
    Hybrid8,
    #[serde(rename = "9H")]
    Hybrid9,
    #[serde(rename = "1I")]
    Iron1,
    #[serde(rename = "2I")]
    Iron2,
    #[serde(rename = "3I")]
    Iron3,
    #[serde(rename = "4I")]
    Iron4,
    #[serde(rename = "5I")]
    Iron5,
    #[serde(rename = "6I")]
    Iron6,
    #[serde(rename = "7I")]
    Iron7,
    #[serde(rename = "8I")]
    Iron8,
    #[serde(rename = "9I")]
    Iron9,
    #[serde(rename = "PW")]
    PitchingWedge,
    #[serde(rename = "GW")]
    GapWedge,
    #[serde(rename = "SW")]
    SandWedge,
    #[serde(rename = "LW")]
    LobWedge,
    #[serde(rename = "PT")]
    Putter,
}

impl Club {
    /// Every club, driver through putter.
    pub const ALL: &[Club] = &[
        Club::Driver,
        Club::MiniDriver,
        Club::Wood2,
        Club::Wood3,
        Club::Wood4,
        Club::Wood5,
        Club::Wood6,
        Club::Wood7,
        Club::Wood8,
        Club::Wood9,
        Club::Hybrid1,
        Club::Hybrid2,
        Club::Hybrid3,
        Club::Hybrid4,
        Club::Hybrid5,
        Club::Hybrid6,
        Club::Hybrid7,
        Club::Hybrid8,
        Club::Hybrid9,
        Club::Iron1,
        Club::Iron2,
        Club::Iron3,
        Club::Iron4,
        Club::Iron5,
        Club::Iron6,
        Club::Iron7,
        Club::Iron8,
        Club::Iron9,
        Club::PitchingWedge,
        Club::GapWedge,
        Club::SandWedge,
        Club::LobWedge,
        Club::Putter,
    ];

    fn code(self) -> &'static str {
        match self {
            Self::Driver => "DR",
            Self::MiniDriver => "MD",
            Self::Wood2 => "2W",
            Self::Wood3 => "3W",
            Self::Wood4 => "4W",
            Self::Wood5 => "5W",
            Self::Wood6 => "6W",
            Self::Wood7 => "7W",
            Self::Wood8 => "8W",
            Self::Wood9 => "9W",
            Self::Hybrid1 => "1H",
            Self::Hybrid2 => "2H",
            Self::Hybrid3 => "3H",
            Self::Hybrid4 => "4H",
            Self::Hybrid5 => "5H",
            Self::Hybrid6 => "6H",
            Self::Hybrid7 => "7H",
            Self::Hybrid8 => "8H",
            Self::Hybrid9 => "9H",
            Self::Iron1 => "1I",
            Self::Iron2 => "2I",
            Self::Iron3 => "3I",
            Self::Iron4 => "4I",
            Self::Iron5 => "5I",
            Self::Iron6 => "6I",
            Self::Iron7 => "7I",
            Self::Iron8 => "8I",
            Self::Iron9 => "9I",
            Self::PitchingWedge => "PW",
            Self::GapWedge => "GW",
            Self::SandWedge => "SW",
            Self::LobWedge => "LW",
            Self::Putter => "PT",
        }
    }
}

impl fmt::Display for Club {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Club {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|club| club.code() == s)
            .ok_or_else(|| format!("unknown club code: {s:?}"))
    }
}

/// Reads an envelope's event. An object whose `kind` is a string but whose
/// other fields don't match that kind (e.g. a `flight_result` without a
/// recognized `basis`) becomes [`FrpEvent::Unknown`], so a controller ignores
/// that one event and keeps reading. Anything else that isn't an event is an
/// error.
fn deserialize_event<'de, D>(deserializer: D) -> Result<FrpEvent, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let has_kind = value.get("kind").is_some_and(serde_json::Value::is_string);
    match serde_json::from_value(value) {
        Ok(event) => Ok(event),
        Err(_) if has_kind => Ok(FrpEvent::Unknown),
        Err(e) => Err(serde::de::Error::custom(e)),
    }
}

/// Reads an optional field whose unrecognized values are ignored (the
/// `set_detection_mode` fields, `device_telemetry.source`,
/// `shot_trigger.timestamp` and the optional `raw_frame` fields), mapping
/// `null` and any value that is not a known one to `None` so the rest of the
/// message still applies.
pub(crate) fn deserialize_lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Reads a flag that only `true` sets, such as `start.raw_frames`: absent,
/// `null`, `false` and any other value read as `false`.
fn deserialize_flag<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value == Some(serde_json::Value::Bool(true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Basis, RawDir};

    #[test]
    fn parse_start() {
        let json = r#"{"kind":"start","version":["0.2.0"],"name":"My Dashboard"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert!(matches!(
            msg,
            FrpMessage::Protocol(FrpProtocolMessage::Start { .. })
        ));
    }

    #[test]
    fn parse_init() {
        let json = r#"{"kind":"init","version":"0.2.0"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert!(matches!(
            msg,
            FrpMessage::Protocol(FrpProtocolMessage::Init { version }) if version == "0.2.0"
        ));
    }

    #[test]
    fn parse_shot_trigger_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "shot_trigger",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                }
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => {
                assert_eq!(env.device, "EagleOne-X4K2");
                match env.event {
                    FrpEvent::ShotTrigger { key, timestamp } => {
                        assert_eq!(key.shot_number, 42);
                        assert_eq!(timestamp, None);
                    }
                    _ => panic!("expected ShotTrigger"),
                }
            }
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_launch_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "launch",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "launch": {
                    "launch_speed": "67.2mps",
                    "launch_azimuth": -1.3,
                    "launch_elevation": 14.2,
                    "backspin_rpm": 3200,
                    "sidespin_rpm": -450,
                    "total_spin_rpm": 3231,
                    "spin_axis": -8.0
                }
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => match env.event {
                FrpEvent::Launch { launch, .. } => {
                    assert_eq!(
                        launch.launch_speed,
                        Some(crate::Velocity::MetersPerSecond(67.2))
                    );
                    assert_eq!(launch.launch_elevation, Some(14.2));
                    assert_eq!(launch.backspin_rpm, Some(3200));
                    assert_eq!(launch.sidespin_rpm, Some(-450));
                    assert_eq!(launch.total_spin_rpm, Some(3231));
                    assert_eq!(launch.spin_axis, Some(-8.0));
                }
                _ => panic!("expected Launch"),
            },
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_flight_result_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "flight_result",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "flight": {
                    "basis": "modeled",
                    "model": "Eagle One flight model 2.1 (Birdie Labs)",
                    "carry_distance": "180.5m",
                    "carry_offline": "-7.6m",
                    "total_distance": "195.0m",
                    "total_offline": "-8.4m",
                    "roll_distance": "14.5m",
                    "max_height": "28.3m",
                    "flight_time": 6.2,
                    "descent_angle": 38.4
                }
            }
        }"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        let FrpEvent::FlightResult { key, flight } = env.event else {
            panic!("expected FlightResult");
        };
        assert_eq!(key.shot_number, 42);
        assert_eq!(flight.basis, Basis::Modeled);
        assert_eq!(
            flight.model.as_deref(),
            Some("Eagle One flight model 2.1 (Birdie Labs)")
        );
        assert_eq!(flight.carry_distance, Some(crate::Distance::Meters(180.5)));
        assert_eq!(flight.carry_offline, Some(crate::Distance::Meters(-7.6)));
        assert_eq!(flight.total_distance, Some(crate::Distance::Meters(195.0)));
        assert_eq!(flight.total_offline, Some(crate::Distance::Meters(-8.4)));
        assert_eq!(flight.roll_distance, Some(crate::Distance::Meters(14.5)));
        assert_eq!(flight.max_height, Some(crate::Distance::Meters(28.3)));
        assert_eq!(flight.flight_time, Some(6.2));
        assert_eq!(flight.descent_angle, Some(38.4));
    }

    #[test]
    fn parse_club_path_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "club_path",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "club": {
                    "club_speed": "42.1mps",
                    "club_speed_post": "38.6mps",
                    "path": -2.1,
                    "attack_angle": -3.5,
                    "face_angle": 1.2,
                    "dynamic_loft": 18.4,
                    "smash_factor": 1.50,
                    "swing_plane_horizontal": 5.3,
                    "swing_plane_vertical": 58.1,
                    "club_offset": "0.47in",
                    "club_height": "0.12in"
                }
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => match env.event {
                FrpEvent::ClubPath { club, .. } => {
                    assert_eq!(
                        club.club_speed,
                        Some(crate::Velocity::MetersPerSecond(42.1))
                    );
                    assert_eq!(club.club_offset, Some(crate::Distance::Inches(0.47)));
                }
                _ => panic!("expected ClubPath"),
            },
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_face_impact_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "face_impact",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "impact": {
                    "lateral": "0.31in",
                    "vertical": "0.15in"
                }
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => match env.event {
                FrpEvent::FaceImpact { impact, .. } => {
                    assert_eq!(impact.lateral, Some(crate::Distance::Inches(0.31)));
                    assert_eq!(impact.vertical, Some(crate::Distance::Inches(0.15)));
                }
                _ => panic!("expected FaceImpact"),
            },
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_device_telemetry_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "device_telemetry",
                "manufacturer": "Birdie Labs",
                "model": "Eagle One",
                "firmware": "1.2.0",
                "telemetry": {
                    "ready": "true",
                    "battery_pct": "85",
                    "tilt": "0.5",
                    "roll": "-0.2"
                }
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => match &env.event {
                FrpEvent::DeviceTelemetry {
                    manufacturer,
                    telemetry,
                    ..
                } => {
                    assert_eq!(manufacturer.as_deref(), Some("Birdie Labs"));
                    let t = telemetry.as_ref().unwrap();
                    assert_eq!(t.get("ready").unwrap(), "true");
                    assert_eq!(t.get("battery_pct").unwrap(), "85");
                }
                _ => panic!("expected DeviceTelemetry"),
            },
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_protocol_alert() {
        let json = r#"{"kind":"alert","severity":"critical","message":"Unsupported FRP version"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Protocol(FrpProtocolMessage::Alert { severity, message }) => {
                assert_eq!(severity, Severity::Critical);
                assert_eq!(message, "Unsupported FRP version");
            }
            _ => panic!("expected protocol Alert"),
        }
    }

    #[test]
    fn parse_device_alert() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "alert",
                "severity": "warn",
                "message": "Signal weak"
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => match env.event {
                FrpEvent::Alert { severity, .. } => {
                    assert_eq!(severity, Severity::Warn);
                }
                _ => panic!("expected device Alert"),
            },
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_set_detection_mode() {
        let json = r#"{"kind":"set_detection_mode","mode":"chipping"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode { mode, handed, club }) => {
                assert_eq!(mode, Some(DetectionMode::Chipping));
                assert_eq!(handed, None);
                assert_eq!(club, None);
            }
            _ => panic!("expected SetDetectionMode"),
        }
    }

    #[test]
    fn parse_set_detection_mode_with_handed() {
        let json = r#"{"kind":"set_detection_mode","mode":"full","handed":"lh"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode { mode, handed, club }) => {
                assert_eq!(mode, Some(DetectionMode::Full));
                assert_eq!(handed, Some(Handedness::Left));
                assert_eq!(club, None);
            }
            _ => panic!("expected SetDetectionMode"),
        }
    }

    #[test]
    fn parse_set_detection_mode_handed_only() {
        let json = r#"{"kind":"set_detection_mode","handed":"rh"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode { mode, handed, club }) => {
                assert_eq!(mode, None);
                assert_eq!(handed, Some(Handedness::Right));
                assert_eq!(club, None);
            }
            _ => panic!("expected SetDetectionMode"),
        }
    }

    #[test]
    fn set_detection_mode_roundtrip_mode_only() {
        let proto = FrpProtocolMessage::SetDetectionMode {
            mode: Some(DetectionMode::Putting),
            handed: None,
            club: None,
        };
        let msg = FrpMessage::Protocol(proto);
        let json = msg.to_json().unwrap();
        assert!(!json.contains("handed"));
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn set_detection_mode_roundtrip_both() {
        let proto = FrpProtocolMessage::SetDetectionMode {
            mode: Some(DetectionMode::Full),
            handed: Some(Handedness::Right),
            club: None,
        };
        let msg = FrpMessage::Protocol(proto);
        let json = msg.to_json().unwrap();
        assert!(json.contains(r#""handed":"rh""#));
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn set_detection_mode_roundtrip_handed_only() {
        let proto = FrpProtocolMessage::SetDetectionMode {
            mode: None,
            handed: Some(Handedness::Left),
            club: None,
        };
        let msg = FrpMessage::Protocol(proto);
        let json = msg.to_json().unwrap();
        assert!(!json.contains(r#""mode""#));
        assert!(json.contains(r#""handed":"lh""#));
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn parse_set_detection_mode_with_club() {
        let json = r#"{"kind":"set_detection_mode","mode":"chipping","handed":"rh","club":"SW"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert_eq!(
            msg,
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                mode: Some(DetectionMode::Chipping),
                handed: Some(Handedness::Right),
                club: Some(Club::SandWedge),
            })
        );

        let json = r#"{"kind":"set_detection_mode","club":"7I"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert_eq!(
            msg,
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                mode: None,
                handed: None,
                club: Some(Club::Iron7),
            })
        );
    }

    #[test]
    fn set_detection_mode_roundtrip_with_club() {
        let msg = FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
            mode: Some(DetectionMode::Chipping),
            handed: Some(Handedness::Right),
            club: Some(Club::SandWedge),
        });
        let json = msg.to_json().unwrap();
        assert_eq!(
            json,
            r#"{"kind":"set_detection_mode","mode":"chipping","handed":"rh","club":"SW"}"#
        );
        assert_eq!(FrpMessage::parse(&json).unwrap(), msg);

        let msg = FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
            mode: None,
            handed: None,
            club: Some(Club::Putter),
        });
        let json = msg.to_json().unwrap();
        assert_eq!(json, r#"{"kind":"set_detection_mode","club":"PT"}"#);
        assert_eq!(FrpMessage::parse(&json).unwrap(), msg);
    }

    #[test]
    fn set_detection_mode_without_club_serializes_unchanged() {
        let msg = FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
            mode: Some(DetectionMode::Putting),
            handed: None,
            club: None,
        });
        assert_eq!(
            msg.to_json().unwrap(),
            r#"{"kind":"set_detection_mode","mode":"putting"}"#
        );
        let msg = FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
            mode: Some(DetectionMode::Full),
            handed: Some(Handedness::Left),
            club: None,
        });
        assert_eq!(
            msg.to_json().unwrap(),
            r#"{"kind":"set_detection_mode","mode":"full","handed":"lh"}"#
        );
    }

    #[test]
    fn unknown_club_is_ignored_and_rest_applies() {
        for club in [
            r#""7i""#,
            r#""XX""#,
            r#""""#,
            "7",
            "null",
            r#"{"code":"7I"}"#,
            r#"["7I"]"#,
        ] {
            let json = format!(
                r#"{{"kind":"set_detection_mode","mode":"putting","handed":"lh","club":{club}}}"#
            );
            let msg = FrpMessage::parse(&json).unwrap();
            assert_eq!(
                msg,
                FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                    mode: Some(DetectionMode::Putting),
                    handed: Some(Handedness::Left),
                    club: None,
                }),
                "{club}"
            );
        }
    }

    #[test]
    fn unknown_mode_is_ignored_and_rest_applies() {
        for mode in [
            r#""range""#,
            r#""Putting""#,
            r#""""#,
            "1",
            "null",
            r#"{"mode":"full"}"#,
        ] {
            let json = format!(
                r#"{{"kind":"set_detection_mode","mode":{mode},"handed":"lh","club":"PT"}}"#
            );
            let msg = FrpMessage::parse(&json).unwrap();
            assert_eq!(
                msg,
                FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                    mode: None,
                    handed: Some(Handedness::Left),
                    club: Some(Club::Putter),
                }),
                "{mode}"
            );
        }
    }

    #[test]
    fn unknown_handed_is_ignored_and_rest_applies() {
        for handed in [r#""ambi""#, r#""RH""#, r#""""#, "true", "null", r#"["rh"]"#] {
            let json = format!(
                r#"{{"kind":"set_detection_mode","mode":"chipping","handed":{handed},"club":"SW"}}"#
            );
            let msg = FrpMessage::parse(&json).unwrap();
            assert_eq!(
                msg,
                FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                    mode: Some(DetectionMode::Chipping),
                    handed: None,
                    club: Some(Club::SandWedge),
                }),
                "{handed}"
            );
        }
    }

    #[test]
    fn all_fields_unknown_still_parses() {
        let json = r#"{"kind":"set_detection_mode","mode":"range","handed":"ambi","club":"XX"}"#;
        assert_eq!(
            FrpMessage::parse(json).unwrap(),
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode {
                mode: None,
                handed: None,
                club: None,
            })
        );
    }

    #[test]
    fn clubs_order_by_bag() {
        assert!(Club::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn every_club_code_round_trips() {
        assert_eq!(Club::ALL.len(), 33);
        for (i, &club) in Club::ALL.iter().enumerate() {
            let code = club.to_string();
            assert_eq!(code.parse::<Club>(), Ok(club), "{code}");
            let json = serde_json::to_string(&club).unwrap();
            assert_eq!(json, format!("\"{code}\""));
            assert_eq!(serde_json::from_str::<Club>(&json).unwrap(), club);
            assert!(
                Club::ALL[..i].iter().all(|other| other.to_string() != code),
                "duplicate code {code}"
            );
        }
    }

    #[test]
    fn club_codes_are_case_sensitive() {
        assert_eq!("PW".parse::<Club>(), Ok(Club::PitchingWedge));
        assert!("pw".parse::<Club>().is_err());
        assert!("I7".parse::<Club>().is_err());
        assert!(" 7I".parse::<Club>().is_err());
        assert!(serde_json::from_str::<Club>(r#""7i""#).is_err());
    }

    #[test]
    fn envelope_roundtrip() {
        let env = FrpEnvelope {
            device: "EagleOne-X4K2".into(),
            event: FrpEvent::ShotFinished {
                key: ShotKey {
                    shot_id: "abc-123".into(),
                    shot_number: 1,
                },
            },
        };
        let msg = FrpMessage::Envelope(env);
        let json = msg.to_json().unwrap();
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn protocol_roundtrip() {
        let proto = FrpProtocolMessage::Start {
            raw_frames: false,
            version: vec!["0.2.0".into()],
            name: Some("Test".into()),
        };
        let msg = FrpMessage::Protocol(proto);
        let json = msg.to_json().unwrap();
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
    }

    fn shot_fields_ball() -> Launch {
        Launch {
            launch_speed: Some(crate::Velocity::MetersPerSecond(67.2)),
            backspin_rpm: Some(3200),
            sidespin_rpm: Some(-450),
            total_spin_rpm: Some(3231),
            spin_axis: Some(-8.0),
            ..Default::default()
        }
    }

    fn shot_fields_flight() -> FlightResult {
        FlightResult {
            carry_distance: Some(crate::Distance::Meters(180.5)),
            carry_offline: Some(crate::Distance::Meters(-7.6)),
            total_offline: Some(crate::Distance::Yards(-9.2)),
            descent_angle: Some(38.4),
            ..FlightResult::new(Basis::Tracked)
        }
    }

    fn shot_fields_club() -> ClubData {
        ClubData {
            club_speed: Some(crate::Velocity::MetersPerSecond(42.1)),
            spin_loft: Some(22.1),
            low_point_distance: Some(crate::Distance::Inches(-0.8)),
            ..Default::default()
        }
    }

    #[test]
    fn parse_shot_fields() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "launch",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "launch": { "total_spin_rpm": 3231, "spin_axis": -8.0 }
            }
        }"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        let FrpEvent::Launch { launch, .. } = env.event else {
            panic!("expected Launch");
        };
        assert_eq!(launch.total_spin_rpm, Some(3231));
        assert_eq!(launch.spin_axis, Some(-8.0));
        assert_eq!(launch.backspin_rpm, None);

        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "club_path",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "club": { "spin_loft": 22.1, "low_point_distance": "1.6in" }
            }
        }"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        let FrpEvent::ClubPath { club, .. } = env.event else {
            panic!("expected ClubPath");
        };
        assert_eq!(club.spin_loft, Some(22.1));
        assert_eq!(club.low_point_distance, Some(crate::Distance::Inches(1.6)));
    }

    #[test]
    fn shot_fields_roundtrip() {
        let key = ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 42);
        for event in [
            FrpEvent::launch(key.clone(), shot_fields_ball()),
            FrpEvent::flight_result(key.clone(), shot_fields_flight()),
            FrpEvent::club_path(key.clone(), shot_fields_club()),
        ] {
            let msg = FrpMessage::Envelope(FrpEnvelope::new("EagleOne-X4K2", event));
            let json = msg.to_json().unwrap();
            assert_eq!(FrpMessage::parse(&json).unwrap(), msg);
        }

        let json = serde_json::to_value(shot_fields_ball()).unwrap();
        assert_eq!(json["total_spin_rpm"], 3231);
        assert_eq!(json["spin_axis"], -8.0);
        let json = serde_json::to_value(shot_fields_flight()).unwrap();
        assert_eq!(json["basis"], "tracked");
        assert_eq!(json["carry_offline"], "-7.6m");
        assert_eq!(json["total_offline"], "-9.2yd");
        assert_eq!(json["descent_angle"], 38.4);
        let json = serde_json::to_value(shot_fields_club()).unwrap();
        assert_eq!(json["spin_loft"], 22.1);
        assert_eq!(json["low_point_distance"], "-0.8in");
    }

    #[test]
    fn shot_fields_omitted_when_absent() {
        let ball = Launch {
            launch_speed: Some(crate::Velocity::MetersPerSecond(67.2)),
            backspin_rpm: Some(3200),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&ball).unwrap(),
            r#"{"launch_speed":"67.2mps","backspin_rpm":3200}"#
        );
        let club = ClubData {
            path: Some(-2.1),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&club).unwrap(), r#"{"path":-2.1}"#);
        assert_eq!(serde_json::to_string(&Launch::default()).unwrap(), "{}");
        assert_eq!(serde_json::to_string(&ClubData::default()).unwrap(), "{}");
        assert_eq!(serde_json::to_string(&FaceImpact::default()).unwrap(), "{}");
    }

    #[test]
    fn json_without_optional_fields_parses() {
        let ball: Launch = serde_json::from_str(
            r#"{"launch_speed":"67.2mps","launch_azimuth":-1.3,"launch_elevation":14.2,
                "backspin_rpm":3200,"sidespin_rpm":-450}"#,
        )
        .unwrap();
        assert_eq!(ball.launch_elevation, Some(14.2));
        assert_eq!(ball.total_spin_rpm, None);
        assert_eq!(ball.spin_axis, None);

        let flight: FlightResult =
            serde_json::from_str(r#"{"basis":"tracked","carry_distance":"180.5m"}"#).unwrap();
        assert_eq!(flight.carry_distance, Some(crate::Distance::Meters(180.5)));
        assert_eq!(flight.model, None);
        assert_eq!(flight.carry_offline, None);
        assert_eq!(flight.descent_angle, None);

        let club: ClubData = serde_json::from_str(
            r#"{"club_speed":"42.1mps","path":-2.1,"club_offset":"0.47in","club_height":"0.12in"}"#,
        )
        .unwrap();
        assert_eq!(club.path, Some(-2.1));
        assert_eq!(club.spin_loft, None);
        assert_eq!(club.low_point_distance, None);
    }

    #[test]
    fn unknown_event_kind_parses_as_unknown() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": { "kind": "future_event", "foo": 42 }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => assert_eq!(env.event, FrpEvent::Unknown),
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn unknown_protocol_kind_parses_as_unknown() {
        let json = r#"{"kind":"future_command","data":"something"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert_eq!(msg, FrpMessage::Protocol(FrpProtocolMessage::Unknown));
    }
    /// Parses `json` and serializes it again.
    fn reserialize(json: &str) -> String {
        FrpMessage::parse(json).unwrap().to_json().unwrap()
    }

    /// Every message kind, as FRP 0.2.0 writes it: each parses and
    /// serializes back to the same JSON.
    #[test]
    fn every_message_round_trips() {
        for json in [
            r#"{"device":"EagleOne-X4K2","event":{"kind":"shot_trigger","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"shot_trigger","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"timestamp":1794063845123}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"launch","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"launch":{"launch_speed":"67.2mps","launch_azimuth":-1.3,"launch_elevation":14.2,"backspin_rpm":3200,"sidespin_rpm":-450,"total_spin_rpm":3231,"spin_axis":-8.0}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"flight_result","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"flight":{"basis":"modeled","model":"Eagle One flight model 2.1 (Birdie Labs)","carry_distance":"180.5m","carry_offline":"-7.6m","total_distance":"195m","total_offline":"-8.4m","roll_distance":"14.5m","max_height":"28.3m","flight_time":6.2,"descent_angle":38.4}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"flight_result","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"flight":{"basis":"tracked","carry_distance":"197.4yd"}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"launch","key":{"shot_id":"6f1c2a9e-3b7d-4e0a-9c58-1d2e3f4a5b6c","shot_number":43},"launch":{"launch_speed":"22.4mps","launch_azimuth":0.8,"launch_elevation":31.5}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"flight_result","key":{"shot_id":"6f1c2a9e-3b7d-4e0a-9c58-1d2e3f4a5b6c","shot_number":43},"flight":{"basis":"modeled","model":"Eagle One flight model 2.1 (Birdie Labs)","carry_distance":"31.2m","carry_offline":"0.4m","total_distance":"36m","max_height":"5.1m","assumed":{"backspin_rpm":4500,"sidespin_rpm":0}}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"club_path","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"club":{"club_speed":"42.1mps","club_speed_post":"38.6mps","path":-2.1,"attack_angle":-3.5,"face_angle":1.2,"dynamic_loft":18.4,"smash_factor":1.5,"swing_plane_horizontal":5.3,"swing_plane_vertical":58.1,"club_offset":"0.47in","club_height":"0.12in","spin_loft":22.1,"low_point_distance":"1.6in"}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"face_impact","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"impact":{"lateral":"0.31in","vertical":"0.15in"}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"shot_finished","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"device_telemetry","manufacturer":"Birdie Labs","model":"Eagle One","firmware":"1.2.0","source":"measured","telemetry":{"ready":"true"}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"device_telemetry"}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"alert","severity":"warn","message":"Signal weak"}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"raw_frame","dir":"rx","seq":1042,"timestamp":1794063845123,"label":"result","data":"0a1b2c","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42}}}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"raw_frame","dir":"tx","seq":0,"data":""}}"#,
            r#"{"kind":"start","version":["0.2.0"],"name":"My Dashboard"}"#,
            r#"{"kind":"start","version":["0.2.0"]}"#,
            r#"{"kind":"start","version":["0.2.0"],"name":"Debugger","raw_frames":true}"#,
            r#"{"kind":"init","version":"0.2.0"}"#,
            r#"{"kind":"alert","severity":"critical","message":"Unsupported FRP version"}"#,
            r#"{"kind":"set_detection_mode","mode":"chipping","handed":"rh","club":"SW"}"#,
        ] {
            assert_eq!(reserialize(json), json);
        }
    }

    /// A `launch` carrying carry and the rest of the flight: the flight keys
    /// are unknown fields, so they are ignored and dropped on output.
    #[test]
    fn flight_keys_in_launch_are_ignored() {
        let json = r#"{"device":"EagleOne-X4K2","event":{"kind":"launch","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"launch":{"launch_speed":"67.2mps","launch_azimuth":-1.3,"launch_elevation":14.2,"carry_distance":"180.5m","carry_offline":"-7.6m","total_distance":"195.0m","total_offline":"-8.4m","roll_distance":"14.5m","max_height":"28.3m","flight_time":6.2,"descent_angle":38.4,"backspin_rpm":3200,"sidespin_rpm":-450}}}"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        assert_eq!(
            env.event,
            FrpEvent::launch(
                ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 42),
                Launch {
                    launch_speed: Some(crate::Velocity::MetersPerSecond(67.2)),
                    launch_azimuth: Some(-1.3),
                    launch_elevation: Some(14.2),
                    backspin_rpm: Some(3200),
                    sidespin_rpm: Some(-450),
                    ..Default::default()
                }
            )
        );
        assert_eq!(
            reserialize(json),
            r#"{"device":"EagleOne-X4K2","event":{"kind":"launch","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"launch":{"launch_speed":"67.2mps","launch_azimuth":-1.3,"launch_elevation":14.2,"backspin_rpm":3200,"sidespin_rpm":-450}}}"#
        );
    }

    /// The spec has a controller ignore a `flight_result` without a recognized
    /// `basis`, so the envelope parses with an `Unknown` event instead of
    /// failing the whole message.
    #[test]
    fn flight_result_without_a_recognized_basis_is_ignored() {
        for flight in [
            r#"{"carry_distance":"180.5m"}"#,
            r#"{"basis":null,"carry_distance":"180.5m"}"#,
            r#"{"basis":"estimated","carry_distance":"180.5m"}"#,
            r#"{"basis":"Tracked","carry_distance":"180.5m"}"#,
            r#"{"basis":true,"carry_distance":"180.5m"}"#,
        ] {
            let json = format!(
                r#"{{"device":"D","event":{{"kind":"flight_result","key":{{"shot_id":"s","shot_number":1}},"flight":{flight}}}}}"#
            );
            assert_eq!(
                FrpMessage::parse(&json).unwrap(),
                FrpMessage::Envelope(FrpEnvelope::new("D", FrpEvent::Unknown)),
                "{flight}"
            );
        }
        // An event without its `flight` object is ignored the same way.
        let json = r#"{"device":"D","event":{"kind":"flight_result","key":{"shot_id":"s","shot_number":1}}}"#;
        assert_eq!(
            FrpMessage::parse(json).unwrap(),
            FrpMessage::Envelope(FrpEnvelope::new("D", FrpEvent::Unknown))
        );
    }

    #[test]
    fn ball_flight_is_an_unknown_kind() {
        // FRP 0.1.0's event: an unknown kind that the connection reads past.
        let json = r#"{"device":"EagleOne-X4K2","event":{"kind":"ball_flight","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"ball":{"launch_speed":"67.2mps","carry_distance":"180.5m"}}}"#;
        assert_eq!(
            FrpMessage::parse(json).unwrap(),
            FrpMessage::Envelope(FrpEnvelope::new("EagleOne-X4K2", FrpEvent::Unknown))
        );
        // A `launch` event with a `ball` object instead of `launch` lacks its
        // `launch` object, so it is ignored too.
        let json = r#"{"device":"D","event":{"kind":"launch","key":{"shot_id":"s","shot_number":1},"ball":{"launch_speed":"67.2mps"}}}"#;
        assert_eq!(
            FrpMessage::parse(json).unwrap(),
            FrpMessage::Envelope(FrpEnvelope::new("D", FrpEvent::Unknown))
        );
    }

    #[test]
    fn parse_raw_frame_envelope() {
        let json = r#"{"device":"EagleOne-X4K2","event":{"kind":"raw_frame","dir":"rx","seq":1042,"timestamp":1794063845123,"label":"result","data":"0a1b2c","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42}}}"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        assert_eq!(
            env.event,
            FrpEvent::raw_frame(RawFrame {
                timestamp: Some(1_794_063_845_123),
                label: Some("result".into()),
                key: Some(ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 42)),
                ..RawFrame::new(RawDir::Rx, 1042, vec![0x0a, 0x1b, 0x2c])
            })
        );
    }

    #[test]
    fn raw_frame_reads_either_case_and_writes_lowercase() {
        let json =
            r#"{"device":"D","event":{"kind":"raw_frame","dir":"tx","seq":7,"data":"0A1b2C"}}"#;
        let msg = FrpMessage::parse(json).unwrap();
        let FrpMessage::Envelope(env) = &msg else {
            panic!("expected Envelope");
        };
        assert_eq!(
            env.event,
            FrpEvent::raw_frame(RawFrame::new(RawDir::Tx, 7, vec![0x0a, 0x1b, 0x2c]))
        );
        assert_eq!(
            msg.to_json().unwrap(),
            r#"{"device":"D","event":{"kind":"raw_frame","dir":"tx","seq":7,"data":"0a1b2c"}}"#
        );
        let all: Vec<u8> = (0..=255).collect();
        let frame = RawFrame::new(RawDir::Rx, 1, all.clone());
        let back: RawFrame = serde_json::from_str(&serde_json::to_string(&frame).unwrap()).unwrap();
        assert_eq!(back.data, all);
    }

    #[test]
    fn malformed_raw_frame_parses_as_unknown() {
        for event in [
            // data: odd length, not hex, a prefix, a separator, not a string, missing
            r#"{"kind":"raw_frame","dir":"rx","seq":1,"data":"0a1"}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":1,"data":"zz"}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":1,"data":"0x0a"}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":1,"data":"0a 1b"}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":1,"data":[10]}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":1}"#,
            // dir: unknown, wrong case, missing
            r#"{"kind":"raw_frame","dir":"in","seq":1,"data":"0a"}"#,
            r#"{"kind":"raw_frame","dir":"RX","seq":1,"data":"0a"}"#,
            r#"{"kind":"raw_frame","seq":1,"data":"0a"}"#,
            // seq: negative, fractional, a string, missing
            r#"{"kind":"raw_frame","dir":"rx","seq":-1,"data":"0a"}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":1.5,"data":"0a"}"#,
            r#"{"kind":"raw_frame","dir":"rx","seq":"1","data":"0a"}"#,
            r#"{"kind":"raw_frame","dir":"rx","data":"0a"}"#,
        ] {
            let json = format!(r#"{{"device":"D","event":{event}}}"#);
            assert_eq!(
                FrpMessage::parse(&json).unwrap(),
                FrpMessage::Envelope(FrpEnvelope::new("D", FrpEvent::Unknown)),
                "{event}"
            );
        }
    }

    #[test]
    fn raw_frame_ignores_bad_optional_fields() {
        let json = r#"{"device":"D","event":{"kind":"raw_frame","dir":"rx","seq":3,"timestamp":-5,"label":7,"data":"ff","key":"shot","extra":true}}"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        assert_eq!(
            env.event,
            FrpEvent::raw_frame(RawFrame::new(RawDir::Rx, 3, vec![0xff]))
        );
    }

    #[test]
    fn start_asks_for_raw_frames_only_with_true() {
        let start = |raw_frames| FrpProtocolMessage::Start {
            version: vec!["0.2.0".into()],
            name: None,
            raw_frames,
        };
        assert_eq!(
            serde_json::to_string(&start(false)).unwrap(),
            r#"{"kind":"start","version":["0.2.0"]}"#
        );
        assert_eq!(
            serde_json::to_string(&start(true)).unwrap(),
            r#"{"kind":"start","version":["0.2.0"],"raw_frames":true}"#
        );
        for (flag, wanted) in [
            ("", false),
            (r#","raw_frames":true"#, true),
            (r#","raw_frames":false"#, false),
            (r#","raw_frames":null"#, false),
            (r#","raw_frames":"true""#, false),
            (r#","raw_frames":1"#, false),
        ] {
            let json = format!(r#"{{"kind":"start","version":["0.2.0"]{flag}}}"#);
            assert_eq!(
                FrpMessage::parse(&json).unwrap(),
                FrpMessage::Protocol(start(wanted)),
                "{json}"
            );
        }
    }

    #[test]
    fn malformed_known_event_parses_as_unknown() {
        for event in [
            r#"{"kind":"launch","key":{"shot_id":"s","shot_number":1},"launch":{"launch_speed":"fast"}}"#,
            r#"{"kind":"launch","key":{"shot_id":"s","shot_number":1}}"#,
            r#"{"kind":"club_path","key":{"shot_id":"s"},"club":{}}"#,
            r#"{"kind":"face_impact","key":{"shot_id":"s","shot_number":1},"impact":{"lateral":3}}"#,
            r#"{"kind":"shot_trigger"}"#,
            r#"{"kind":"shot_finished","key":"s"}"#,
            r#"{"kind":"device_telemetry","telemetry":{"ready":true}}"#,
            r#"{"kind":"alert","severity":"info","message":"m"}"#,
        ] {
            let json = format!(r#"{{"device":"D","event":{event}}}"#);
            assert_eq!(
                FrpMessage::parse(&json).unwrap(),
                FrpMessage::Envelope(FrpEnvelope::new("D", FrpEvent::Unknown)),
                "{event}"
            );
        }
    }

    #[test]
    fn envelope_without_an_event_does_not_parse() {
        for json in [
            r#"{"device":"D"}"#,
            r#"{"device":"D","event":null}"#,
            r#"{"device":"D","event":"shot_finished"}"#,
            r#"{"device":"D","event":{"key":{"shot_id":"s","shot_number":1}}}"#,
            r#"{"device":"D","event":{"kind":7}}"#,
            r#"{"device":7,"event":{"kind":"shot_finished","key":{"shot_id":"s","shot_number":1}}}"#,
        ] {
            assert!(
                matches!(FrpMessage::parse(json), Err(FrpError::Json(_))),
                "{json}"
            );
        }
    }

    /// A receiver ignores `assumed` on a `tracked` result, but the event is
    /// still a flight result, never `Unknown`, and passes through unchanged.
    #[test]
    fn tracked_flight_result_with_assumed_still_parses() {
        let json = r#"{"device":"D","event":{"kind":"flight_result","key":{"shot_id":"s","shot_number":1},"flight":{"basis":"tracked","carry_distance":"180.5m","assumed":{"backspin_rpm":4500}}}}"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected an envelope");
        };
        let FrpEvent::FlightResult { flight, .. } = env.event else {
            panic!("expected FlightResult, got {:?}", env.event);
        };
        assert_eq!(flight.basis, Basis::Tracked);
        assert_eq!(flight.carry_distance, Some(crate::Distance::Meters(180.5)));
        assert_eq!(reserialize(json), json);

        // A malformed `assumed` is dropped; the result still applies.
        let json = r#"{"device":"D","event":{"kind":"flight_result","key":{"shot_id":"s","shot_number":1},"flight":{"basis":"tracked","carry_distance":"180.5m","assumed":"none"}}}"#;
        assert_eq!(
            reserialize(json),
            r#"{"device":"D","event":{"kind":"flight_result","key":{"shot_id":"s","shot_number":1},"flight":{"basis":"tracked","carry_distance":"180.5m"}}}"#
        );
    }

    #[test]
    fn flight_result_ignores_unknown_fields() {
        let json = r#"{"device":"D","event":{"kind":"flight_result","key":{"shot_id":"s","shot_number":1},"flight":{"basis":"tracked","carry_distance":"180.5m","spin_decay":0.04}}}"#;
        assert_eq!(
            reserialize(json),
            r#"{"device":"D","event":{"kind":"flight_result","key":{"shot_id":"s","shot_number":1},"flight":{"basis":"tracked","carry_distance":"180.5m"}}}"#
        );
    }

    #[test]
    fn device_telemetry_source_round_trips() {
        for (source, name) in [(Source::Measured, "measured"), (Source::Virtual, "virtual")] {
            let json = format!(
                r#"{{"device":"EagleOne-X4K2","event":{{"kind":"device_telemetry","manufacturer":"Birdie Labs","model":"Eagle One","firmware":"1.2.0","source":"{name}","telemetry":{{"ready":"true"}}}}}}"#
            );
            let FrpMessage::Envelope(env) = FrpMessage::parse(&json).unwrap() else {
                panic!("expected Envelope");
            };
            let FrpEvent::DeviceTelemetry { source: parsed, .. } = &env.event else {
                panic!("expected DeviceTelemetry");
            };
            assert_eq!(*parsed, Some(source));
            assert_eq!(source.to_string(), name);
            assert_eq!(reserialize(&json), json);
        }
    }

    #[test]
    fn unknown_source_is_ignored_and_rest_applies() {
        for value in [
            r#""Measured""#,
            r#""physical""#,
            r#""""#,
            "1",
            "null",
            r#"{"x":1}"#,
            r#"["measured"]"#,
        ] {
            let json = format!(
                r#"{{"device":"D","event":{{"kind":"device_telemetry","model":"Eagle One","source":{value},"telemetry":{{"ready":"true"}}}}}}"#
            );
            let FrpMessage::Envelope(env) = FrpMessage::parse(&json).unwrap() else {
                panic!("expected Envelope for {value}");
            };
            let FrpEvent::DeviceTelemetry {
                model,
                source,
                telemetry,
                ..
            } = &env.event
            else {
                panic!("expected DeviceTelemetry for {value}");
            };
            assert_eq!(*source, None, "{value}");
            assert_eq!(model.as_deref(), Some("Eagle One"));
            assert_eq!(telemetry.as_ref().unwrap()["ready"], "true");
        }
    }

    #[test]
    fn shot_trigger_timestamp_round_trips() {
        let json = r#"{"device":"EagleOne-X4K2","event":{"kind":"shot_trigger","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"timestamp":1794063845123}}"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        assert_eq!(
            env.event,
            FrpEvent::shot_trigger_at(
                ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 42),
                1_794_063_845_123
            )
        );
        assert_eq!(reserialize(json), json);
    }

    #[test]
    fn shot_trigger_without_timestamp_omits_it() {
        let event = FrpEvent::shot_trigger(ShotKey::new("s", 1));
        assert_eq!(
            serde_json::to_string(&FrpEnvelope::new("D", event)).unwrap(),
            r#"{"device":"D","event":{"kind":"shot_trigger","key":{"shot_id":"s","shot_number":1}}}"#
        );
    }

    #[test]
    fn malformed_timestamp_is_ignored_and_trigger_applies() {
        for value in [
            "-1",
            "1794063845123.5",
            r#""1794063845123""#,
            r#""2026-11-07T12:00:00.123Z""#,
            "null",
            "true",
            r#"{"ms":1}"#,
        ] {
            let json = format!(
                r#"{{"device":"D","event":{{"kind":"shot_trigger","key":{{"shot_id":"s","shot_number":7}},"timestamp":{value}}}}}"#
            );
            let FrpMessage::Envelope(env) = FrpMessage::parse(&json).unwrap() else {
                panic!("expected Envelope for {value}");
            };
            assert_eq!(
                env.event,
                FrpEvent::shot_trigger(ShotKey::new("s", 7)),
                "{value}"
            );
        }
    }
}
