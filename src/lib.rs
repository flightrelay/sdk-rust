//! Flight Relay Protocol (FRP) — golf launch monitor event streaming.
//!
//! This crate provides message schemas and optional WebSocket transport for the
//! [Flight Relay Protocol](https://github.com/flightrelay/spec).
//!
//! # Features
//!
//! - **`controller`** — [`ShotAggregator`] for accumulating shot lifecycle events,
//!   with the accumulator timeout
//! - **`device`** — [`ForwardFilter`], which drops duplicate events before an
//!   endpoint forwards them
//! - **`transport`** — [`FrpEndpoint`] / [`FrpConnection`] WebSocket transport for either role
//!
//! # Measured values and flight results
//!
//! [`Launch`], [`ClubData`] and [`FaceImpact`] carry measured values
//! only. Carry, total distance and the rest of the ball's flight travel in a
//! [`FlightResult`] (`flight_result`), whose required [`Basis`] says whether
//! the device tracked the flight or a flight model produced the values. A
//! modeled result may list the launch values its model assumed, because the
//! shot's [`Launch`] lacks them, in an [`AssumedLaunch`]; they are not
//! measured. A device declares its [`Source`] on `device_telemetry`.
//!
//! # Raw frames
//!
//! A device can show a controller the protocol messages behind its events as
//! [`RawFrame`]s (`raw_frame`), for debugging. A controller asks for them with
//! `raw_frames` in `start` (`EndpointConfig::with_raw_frames`, feature
//! `transport`); a device sends none otherwise
//! (`FrpConnection::raw_frames_requested`). Raw frames never carry device or
//! user identity, and never affect a shot.
//!
//! # Ignored messages
//!
//! An envelope whose event has an unknown kind, or a known kind whose fields
//! don't match it (e.g. a `flight_result` without a recognized `basis`),
//! parses with an [`FrpEvent::Unknown`] event, which controllers ignore and
//! forwarders drop. A message that isn't valid FRP JSON at all is an
//! [`FrpError::Json`] for that message only; the connection stays usable.
//!
//! # Forwarding
//!
//! An endpoint that forwards events passes `device`, `shot_id`,
//! `shot_number` and the `shot_trigger` `timestamp` through unchanged, and
//! drops duplicates before forwarding. `ForwardFilter` (feature `device`)
//! does the dropping.

use std::time::Duration;

pub mod error;
pub mod message;
pub mod types;
pub mod units;

#[cfg(feature = "controller")]
pub mod accumulator;

#[cfg(feature = "device")]
pub mod forward;

#[cfg(feature = "transport")]
pub mod endpoint;

pub use error::FrpError;
pub use message::{
    Club, DetectionMode, FrpEnvelope, FrpEvent, FrpMessage, FrpProtocolMessage, Handedness,
    Severity, Source,
};
pub use types::{
    AssumedLaunch, Basis, ClubData, FaceImpact, FlightResult, Launch, RawDir, RawFrame, ShotKey,
};
pub use units::{Distance, Velocity};

#[cfg(feature = "controller")]
pub use accumulator::{CompletedShot, ShotAggregator};

#[cfg(feature = "device")]
pub use forward::ForwardFilter;

#[cfg(feature = "transport")]
pub use endpoint::{EndpointConfig, FrpConnection, FrpEndpoint, Role, Transport};

/// Recommended default port for standalone FRP connections.
pub const DEFAULT_PORT: u16 = 5880;

/// Recommended default WebSocket path for FRP connections.
pub const DEFAULT_PATH: &str = "/frp";

/// Recommended default URL for standalone FRP connections.
pub const DEFAULT_URL: &str = "ws://localhost:5880/frp";

/// Recommended accumulator timeout: how long a shot may take from
/// `shot_trigger` to `shot_finished` before a controller discards it. Some
/// launch monitors deliver their full flight data several seconds after the
/// strike. `ForwardFilter` uses it as its default window too.
pub const DEFAULT_SHOT_TIMEOUT: Duration = Duration::from_secs(30);

/// The FRP spec version this crate implements, and the only one its handshake
/// proposes or accepts.
pub const SPEC_VERSION: &str = "0.2.0";
