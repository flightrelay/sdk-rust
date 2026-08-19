//! Flight Relay Protocol (FRP) — golf launch monitor event streaming.
//!
//! This crate provides message schemas and optional WebSocket transport for the
//! [Flight Relay Protocol](https://github.com/flightrelay/spec).
//!
//! # Features
//!
//! - **`controller`** — [`ShotAggregator`] for accumulating shot lifecycle events
//! - **`device`** — (reserved for future device-side helpers)
//! - **`transport`** — [`FrpEndpoint`] / [`FrpConnection`] WebSocket transport for either role

pub mod error;
pub mod message;
pub mod types;
pub mod units;

#[cfg(feature = "controller")]
pub mod accumulator;

#[cfg(feature = "transport")]
pub mod endpoint;

pub use error::FrpError;
pub use message::{
    DetectionMode, FrpEnvelope, FrpEvent, FrpMessage, FrpProtocolMessage, Handedness, Severity,
};
pub use types::{BallFlight, ClubData, FaceImpact, ShotKey};
pub use units::{Distance, Velocity};

#[cfg(feature = "controller")]
pub use accumulator::{CompletedShot, ShotAggregator};

#[cfg(feature = "transport")]
pub use endpoint::{EndpointConfig, FrpConnection, FrpEndpoint, Role, Transport};

/// Recommended default port for standalone FRP connections.
pub const DEFAULT_PORT: u16 = 5880;

/// Recommended default WebSocket path for FRP connections.
pub const DEFAULT_PATH: &str = "/frp";

/// Recommended default URL for standalone FRP connections.
pub const DEFAULT_URL: &str = "ws://localhost:5880/frp";

/// The FRP spec version this crate implements.
pub const SPEC_VERSION: &str = "0.1.0";
