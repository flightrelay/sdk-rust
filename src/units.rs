//! Unit-tagged types for FRP distance and velocity values.
//!
//! All velocity and distance fields in FRP are unit-tagged strings: a numeric
//! value immediately followed by a unit suffix with no space (e.g. `"67.2mps"`,
//! `"180.5m"`). These types handle serde roundtripping to/from that format.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// A distance value with unit. Serializes as a suffix string: `"1.5in"`,
/// `"9ft"`, `"3m"`, `"30cm"`, `"100yd"`, `"42mm"`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Distance {
    Feet(f64),
    Inches(f64),
    Meters(f64),
    Centimeters(f64),
    Yards(f64),
    Millimeters(f64),
}

impl Serialize for Distance {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Distance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl std::str::FromStr for Distance {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // Try each suffix longest-first to avoid "m" matching before "mm" or "cm"
        for (suffix, ctor) in &[
            ("mm", Self::Millimeters as fn(f64) -> Self),
            ("cm", Self::Centimeters as fn(f64) -> Self),
            ("ft", Self::Feet as fn(f64) -> Self),
            ("in", Self::Inches as fn(f64) -> Self),
            ("yd", Self::Yards as fn(f64) -> Self),
            ("m", Self::Meters as fn(f64) -> Self),
        ] {
            if let Some(num) = s.strip_suffix(suffix) {
                let v: f64 = num
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid number in distance: {s:?}"))?;
                return Ok(ctor(v));
            }
        }
        Err(format!(
            "invalid distance {s:?}: expected number with suffix (ft, in, m, cm, yd, mm)"
        ))
    }
}

impl Distance {
    #[must_use]
    pub fn value(self) -> f64 {
        match self {
            Self::Feet(v)
            | Self::Inches(v)
            | Self::Meters(v)
            | Self::Centimeters(v)
            | Self::Yards(v)
            | Self::Millimeters(v) => v,
        }
    }

    #[must_use]
    pub fn unit_suffix(self) -> &'static str {
        match self {
            Self::Feet(_) => "ft",
            Self::Inches(_) => "in",
            Self::Meters(_) => "m",
            Self::Centimeters(_) => "cm",
            Self::Yards(_) => "yd",
            Self::Millimeters(_) => "mm",
        }
    }

    #[must_use]
    pub fn as_feet(self) -> f64 {
        match self {
            Self::Feet(v) => v,
            Self::Inches(v) => v / 12.0,
            Self::Meters(v) => v / 0.3048,
            Self::Centimeters(v) => v / 30.48,
            Self::Yards(v) => v * 3.0,
            Self::Millimeters(v) => v / 304.8,
        }
    }

    #[must_use]
    pub fn as_inches(self) -> f64 {
        match self {
            Self::Feet(v) => v * 12.0,
            Self::Inches(v) => v,
            Self::Meters(v) => v / 0.0254,
            Self::Centimeters(v) => v / 2.54,
            Self::Yards(v) => v * 36.0,
            Self::Millimeters(v) => v / 25.4,
        }
    }

    #[must_use]
    pub fn as_meters(self) -> f64 {
        match self {
            Self::Feet(v) => v * 0.3048,
            Self::Inches(v) => v * 0.0254,
            Self::Meters(v) => v,
            Self::Centimeters(v) => v / 100.0,
            Self::Yards(v) => v * 0.9144,
            Self::Millimeters(v) => v / 1000.0,
        }
    }

    #[must_use]
    pub fn as_centimeters(self) -> f64 {
        match self {
            Self::Feet(v) => v * 30.48,
            Self::Inches(v) => v * 2.54,
            Self::Meters(v) => v * 100.0,
            Self::Centimeters(v) => v,
            Self::Yards(v) => v * 91.44,
            Self::Millimeters(v) => v / 10.0,
        }
    }

    #[must_use]
    pub fn as_yards(self) -> f64 {
        match self {
            Self::Feet(v) => v / 3.0,
            Self::Inches(v) => v / 36.0,
            Self::Meters(v) => v / 0.9144,
            Self::Centimeters(v) => v / 91.44,
            Self::Yards(v) => v,
            Self::Millimeters(v) => v / 914.4,
        }
    }

    #[must_use]
    pub fn as_millimeters(self) -> f64 {
        match self {
            Self::Feet(v) => v * 304.8,
            Self::Inches(v) => v * 25.4,
            Self::Meters(v) => v * 1000.0,
            Self::Centimeters(v) => v * 10.0,
            Self::Yards(v) => v * 914.4,
            Self::Millimeters(v) => v,
        }
    }
}

impl fmt::Display for Distance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.value(), self.unit_suffix())
    }
}

/// A velocity value with unit. Serializes as a suffix string: `"67.2mps"`,
/// `"150.3mph"`, `"108.0kph"`, `"100.0fps"`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Velocity {
    MilesPerHour(f64),
    FeetPerSecond(f64),
    MetersPerSecond(f64),
    KilometersPerHour(f64),
}

impl Serialize for Velocity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Velocity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl std::str::FromStr for Velocity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // Try each suffix longest-first to avoid "mps" matching before "mph"
        for (suffix, ctor) in &[
            ("mph", Self::MilesPerHour as fn(f64) -> Self),
            ("mps", Self::MetersPerSecond as fn(f64) -> Self),
            ("kph", Self::KilometersPerHour as fn(f64) -> Self),
            ("fps", Self::FeetPerSecond as fn(f64) -> Self),
        ] {
            if let Some(num) = s.strip_suffix(suffix) {
                let v: f64 = num
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid number in velocity: {s:?}"))?;
                return Ok(ctor(v));
            }
        }
        Err(format!(
            "invalid velocity {s:?}: expected number with suffix (mph, mps, kph, fps)"
        ))
    }
}

impl Velocity {
    #[must_use]
    pub fn value(self) -> f64 {
        match self {
            Self::MilesPerHour(v)
            | Self::FeetPerSecond(v)
            | Self::MetersPerSecond(v)
            | Self::KilometersPerHour(v) => v,
        }
    }

    #[must_use]
    pub fn unit_suffix(self) -> &'static str {
        match self {
            Self::MilesPerHour(_) => "mph",
            Self::FeetPerSecond(_) => "fps",
            Self::MetersPerSecond(_) => "mps",
            Self::KilometersPerHour(_) => "kph",
        }
    }

    #[must_use]
    pub fn as_mph(self) -> f64 {
        match self {
            Self::MilesPerHour(v) => v,
            Self::FeetPerSecond(v) => v * 0.681_818,
            Self::MetersPerSecond(v) => v * 2.23694,
            Self::KilometersPerHour(v) => v * 0.621_371,
        }
    }

    #[must_use]
    pub fn as_fps(self) -> f64 {
        match self {
            Self::MilesPerHour(v) => v * 1.46667,
            Self::FeetPerSecond(v) => v,
            Self::MetersPerSecond(v) => v * 3.28084,
            Self::KilometersPerHour(v) => v * 0.911_344,
        }
    }

    #[must_use]
    pub fn as_mps(self) -> f64 {
        match self {
            Self::MilesPerHour(v) => v * 0.44704,
            Self::FeetPerSecond(v) => v * 0.3048,
            Self::MetersPerSecond(v) => v,
            Self::KilometersPerHour(v) => v / 3.6,
        }
    }

    #[must_use]
    pub fn as_kph(self) -> f64 {
        match self {
            Self::MilesPerHour(v) => v * 1.60934,
            Self::FeetPerSecond(v) => v * 1.09728,
            Self::MetersPerSecond(v) => v * 3.6,
            Self::KilometersPerHour(v) => v,
        }
    }
}

impl fmt::Display for Velocity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.value(), self.unit_suffix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_roundtrip() {
        let cases = [
            Distance::Meters(180.5),
            Distance::Yards(197.4),
            Distance::Feet(9.0),
            Distance::Inches(0.47),
            Distance::Centimeters(30.0),
            Distance::Millimeters(42.67),
        ];
        for d in cases {
            let s = d.to_string();
            let parsed: Distance = s.parse().unwrap();
            assert_eq!(d, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn velocity_roundtrip() {
        let cases = [
            Velocity::MetersPerSecond(67.2),
            Velocity::MilesPerHour(150.3),
            Velocity::KilometersPerHour(108.0),
            Velocity::FeetPerSecond(100.0),
        ];
        for v in cases {
            let s = v.to_string();
            let parsed: Velocity = s.parse().unwrap();
            assert_eq!(v, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn distance_serde_json() {
        let d = Distance::Meters(180.5);
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"180.5m\"");
        let back: Distance = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn velocity_serde_json() {
        let v = Velocity::MetersPerSecond(67.2);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"67.2mps\"");
        let back: Velocity = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn distance_conversions() {
        let d = Distance::Meters(1.0);
        assert!((d.as_feet() - 3.28084).abs() < 0.001);
        assert!((d.as_yards() - 1.09361).abs() < 0.001);
        assert!((d.as_millimeters() - 1000.0).abs() < 0.001);
    }
}
