//! FRP message types and parsing.
//!
//! FRP has two wire shapes:
//! - **Envelopes**: `{ "device": "...", "event": { "kind": "..." } }` — device events
//! - **Protocol messages**: `{ "kind": "..." }` — handshake, alerts, commands
//!
//! [`FrpMessage::parse`] inspects for the `"device"` key to dispatch.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::FrpError;
use crate::types::{BallFlight, ClubData, FaceImpact, ShotKey};

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
    /// Returns `FrpError::Json` if the JSON is malformed or doesn't match either shape.
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
    /// The typed event.
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
    ShotTrigger {
        key: ShotKey,
    },
    BallFlight {
        key: ShotKey,
        ball: BallFlight,
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
    DeviceTelemetry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manufacturer: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        firmware: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        telemetry: Option<HashMap<String, String>>,
    },
    Alert {
        severity: Severity,
        message: String,
    },
    /// Unknown event kind — per spec, must be silently ignored.
    #[serde(other)]
    Unknown,
}

impl FrpEvent {
    /// Ball strike detected — the first event of a shot sequence.
    #[must_use]
    pub fn shot_trigger(key: ShotKey) -> Self {
        Self::ShotTrigger { key }
    }

    /// Ball flight measurement data.
    #[must_use]
    pub fn ball_flight(key: ShotKey, ball: BallFlight) -> Self {
        Self::BallFlight { key, ball }
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
}

/// Protocol-level messages (no device envelope).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrpProtocolMessage {
    /// Controller → Device: version negotiation.
    Start {
        version: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Device → Controller: version confirmation.
    Init { version: String },
    /// Protocol-level alert (no device context).
    Alert { severity: Severity, message: String },
    /// Controller → Device: set detection mode and/or handedness.
    ///
    /// Both fields are optional and latched independently — the most recent
    /// value for each field is the active value. Omitting a field does not
    /// reset it.
    SetDetectionMode {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<DetectionMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handed: Option<Handedness>,
    },
    /// Unknown protocol message kind — per spec, must be silently ignored.
    #[serde(other)]
    Unknown,
}

/// Alert severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_start() {
        let json = r#"{"kind":"start","version":["0.1.0"],"name":"My Dashboard"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert!(matches!(
            msg,
            FrpMessage::Protocol(FrpProtocolMessage::Start { .. })
        ));
    }

    #[test]
    fn parse_init() {
        let json = r#"{"kind":"init","version":"0.1.0"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        assert!(matches!(
            msg,
            FrpMessage::Protocol(FrpProtocolMessage::Init { version }) if version == "0.1.0"
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
                    FrpEvent::ShotTrigger { key } => {
                        assert_eq!(key.shot_number, 42);
                    }
                    _ => panic!("expected ShotTrigger"),
                }
            }
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
    }

    #[test]
    fn parse_ball_flight_envelope() {
        let json = r#"{
            "device": "EagleOne-X4K2",
            "event": {
                "kind": "ball_flight",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "ball": {
                    "launch_speed": "67.2mps",
                    "launch_azimuth": -1.3,
                    "launch_elevation": 14.2,
                    "carry_distance": "180.5m",
                    "total_distance": "195.0m",
                    "roll_distance": "14.5m",
                    "max_height": "28.3m",
                    "flight_time": 6.2,
                    "backspin_rpm": 3200,
                    "sidespin_rpm": -450
                }
            }
        }"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Envelope(env) => match env.event {
                FrpEvent::BallFlight { ball, .. } => {
                    assert_eq!(
                        ball.launch_speed,
                        Some(crate::Velocity::MetersPerSecond(67.2))
                    );
                    assert_eq!(ball.carry_distance, Some(crate::Distance::Meters(180.5)));
                    assert_eq!(ball.backspin_rpm, Some(3200));
                    assert_eq!(ball.sidespin_rpm, Some(-450));
                }
                _ => panic!("expected BallFlight"),
            },
            FrpMessage::Protocol(_) => panic!("expected Envelope"),
        }
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
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode { mode, handed }) => {
                assert_eq!(mode, Some(DetectionMode::Chipping));
                assert_eq!(handed, None);
            }
            _ => panic!("expected SetDetectionMode"),
        }
    }

    #[test]
    fn parse_set_detection_mode_with_handed() {
        let json = r#"{"kind":"set_detection_mode","mode":"full","handed":"lh"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode { mode, handed }) => {
                assert_eq!(mode, Some(DetectionMode::Full));
                assert_eq!(handed, Some(Handedness::Left));
            }
            _ => panic!("expected SetDetectionMode"),
        }
    }

    #[test]
    fn parse_set_detection_mode_handed_only() {
        let json = r#"{"kind":"set_detection_mode","handed":"rh"}"#;
        let msg = FrpMessage::parse(json).unwrap();
        match msg {
            FrpMessage::Protocol(FrpProtocolMessage::SetDetectionMode { mode, handed }) => {
                assert_eq!(mode, None);
                assert_eq!(handed, Some(Handedness::Right));
            }
            _ => panic!("expected SetDetectionMode"),
        }
    }

    #[test]
    fn set_detection_mode_roundtrip_mode_only() {
        let proto = FrpProtocolMessage::SetDetectionMode {
            mode: Some(DetectionMode::Putting),
            handed: None,
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
        };
        let msg = FrpMessage::Protocol(proto);
        let json = msg.to_json().unwrap();
        assert!(!json.contains(r#""mode""#));
        assert!(json.contains(r#""handed":"lh""#));
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
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
            version: vec!["0.1.0".into()],
            name: Some("Test".into()),
        };
        let msg = FrpMessage::Protocol(proto);
        let json = msg.to_json().unwrap();
        let back = FrpMessage::parse(&json).unwrap();
        assert_eq!(msg, back);
    }

    fn shot_fields_ball() -> BallFlight {
        BallFlight {
            launch_speed: Some(crate::Velocity::MetersPerSecond(67.2)),
            carry_distance: Some(crate::Distance::Meters(180.5)),
            carry_offline: Some(crate::Distance::Meters(-7.6)),
            total_offline: Some(crate::Distance::Yards(-9.2)),
            descent_angle: Some(38.4),
            backspin_rpm: Some(3200),
            sidespin_rpm: Some(-450),
            total_spin_rpm: Some(3231),
            spin_axis: Some(-8.0),
            ..Default::default()
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
                "kind": "ball_flight",
                "key": {
                    "shot_id": "550e8400-e29b-41d4-a716-446655440000",
                    "shot_number": 42
                },
                "ball": {
                    "carry_distance": "180.5m",
                    "carry_offline": "-7.6m",
                    "total_offline": "-8.4m",
                    "descent_angle": 38.4,
                    "total_spin_rpm": 3231,
                    "spin_axis": -8.0
                }
            }
        }"#;
        let FrpMessage::Envelope(env) = FrpMessage::parse(json).unwrap() else {
            panic!("expected Envelope");
        };
        let FrpEvent::BallFlight { ball, .. } = env.event else {
            panic!("expected BallFlight");
        };
        assert_eq!(ball.carry_offline, Some(crate::Distance::Meters(-7.6)));
        assert_eq!(ball.total_offline, Some(crate::Distance::Meters(-8.4)));
        assert_eq!(ball.descent_angle, Some(38.4));
        assert_eq!(ball.total_spin_rpm, Some(3231));
        assert_eq!(ball.spin_axis, Some(-8.0));
        assert_eq!(ball.backspin_rpm, None);

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
            FrpEvent::ball_flight(key.clone(), shot_fields_ball()),
            FrpEvent::club_path(key.clone(), shot_fields_club()),
        ] {
            let msg = FrpMessage::Envelope(FrpEnvelope::new("EagleOne-X4K2", event));
            let json = msg.to_json().unwrap();
            assert_eq!(FrpMessage::parse(&json).unwrap(), msg);
        }

        let json = serde_json::to_value(shot_fields_ball()).unwrap();
        assert_eq!(json["carry_offline"], "-7.6m");
        assert_eq!(json["total_offline"], "-9.2yd");
        assert_eq!(json["descent_angle"], 38.4);
        assert_eq!(json["total_spin_rpm"], 3231);
        assert_eq!(json["spin_axis"], -8.0);
        let json = serde_json::to_value(shot_fields_club()).unwrap();
        assert_eq!(json["spin_loft"], 22.1);
        assert_eq!(json["low_point_distance"], "-0.8in");
    }

    #[test]
    fn shot_fields_omitted_when_absent() {
        let ball = BallFlight {
            carry_distance: Some(crate::Distance::Meters(180.5)),
            backspin_rpm: Some(3200),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&ball).unwrap(),
            r#"{"carry_distance":"180.5m","backspin_rpm":3200}"#
        );
        let club = ClubData {
            path: Some(-2.1),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&club).unwrap(), r#"{"path":-2.1}"#);
        assert_eq!(serde_json::to_string(&BallFlight::default()).unwrap(), "{}");
        assert_eq!(serde_json::to_string(&ClubData::default()).unwrap(), "{}");
    }

    #[test]
    fn json_without_shot_fields_parses() {
        let ball: BallFlight = serde_json::from_str(
            r#"{"launch_speed":"67.2mps","launch_azimuth":-1.3,"launch_elevation":14.2,
                "carry_distance":"180.5m","total_distance":"195.0m","roll_distance":"14.5m",
                "max_height":"28.3m","flight_time":6.2,"backspin_rpm":3200,"sidespin_rpm":-450}"#,
        )
        .unwrap();
        assert_eq!(ball.carry_distance, Some(crate::Distance::Meters(180.5)));
        assert_eq!(ball.carry_offline, None);
        assert_eq!(ball.total_offline, None);
        assert_eq!(ball.descent_angle, None);
        assert_eq!(ball.total_spin_rpm, None);
        assert_eq!(ball.spin_axis, None);

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
}
