//! Flight Relay Protocol (FRP) — a message bus for golf simulators.
//!
//! This crate provides the message types of the
//! [Flight Relay Protocol](https://github.com/flightrelay/spec) and optional
//! WebSocket transport, a hub, and a framework for launch monitor drivers.
//!
//! # The bus
//!
//! FRP is a multi-producer, multi-consumer bus of **actors**. Every message
//! is one flat JSON object, `{"kind": "...", "from": "<actor id>", ...}`
//! ([`FrpMessage`]), and every actor receives everything. Messages come in
//! three classes ([`Class`]):
//!
//! - **State** (`bus.actor`, `golfer.profile`, `golfer.turn`,
//!   `practice.block`, `game.round`, `game.scores`, `game.ball`): a complete
//!   object its owner republishes when it changes, when it starts, and in
//!   answer to `bus.sync`. The owner removes a state with a last message
//!   marked `"removed": true` ([`Removal`]).
//! - **Occurrences** (`bus.sync`, `bus.reply`, `bus.alert`, `shot.*`,
//!   `game.stroke`): sent once.
//! - **Commands** (`golfer.select`, `practice.new_block`, `game.aim`): carry a
//!   `ref`; the actor holding the owning role applies one and answers with
//!   `bus.reply`.
//!
//! An actor announces itself with `bus.actor`, which doubles as its
//! heartbeat: an actor not heard for three heartbeats is gone, and receivers
//! forget all its state. An actor id is a random string generated once by
//! the host that runs the actor and kept across restarts ([`new_actor_id`]).
//!
//! # Features
//!
//! - **`controller`** — [`ShotAggregator`], which composes a shot from its
//!   `shot.*` messages, and [`BusState`], the current state of the bus with
//!   removals and heartbeat expiry applied
//! - **`transport`** — [`FrpEndpoint`] / [`FrpConnection`], the WebSocket
//!   binding, and [`Hub`], which forwards every message to every other
//!   connection
//! - **`config`** — [`config::DriverConfig`], the serde settings of a device driver
//! - **`driver`** — the [`driver::DeviceDriver`] contract between a launch
//!   monitor and its host (implies `config`)
//! - **`server`** — [`FrpServer`], which runs a driver as a launch monitor
//!   actor on the bus (implies `driver` and `transport`)
//! - **`main`** — [`run`], a complete `main` for a driver: config file,
//!   `--set` overrides, `--help`, Ctrl-C (implies `server`; adds `toml` and
//!   `ctrlc`)
//! - **`mock`** — [`mock::MockDriver`], a simulated launch monitor (implies
//!   `driver`)
//!
//! # Shots
//!
//! A launch monitor publishes a shot as `shot.trigger`, then `shot.launch`,
//! `shot.club`, `shot.impact` and `shot.flight` as they become available,
//! then `shot.finished`. A shot is identified by `key.shot_id` alone, so a
//! collector accumulates by `shot_id`, whoever sent each part: flight models
//! publish their own `shot.flight` for shots they saw launched, and a shot
//! may have several, told apart by `from` and [`FlightResult::model`].
//!
//! [`Launch`], [`ClubData`] and [`FaceImpact`] carry measured values only.
//! Carry, total distance and the rest of the ball's flight travel in a
//! [`FlightResult`], whose required [`Basis`] says whether the flight was
//! tracked or modeled. A modeled result may list the launch values its model
//! assumed in an [`AssumedLaunch`]; they are not measured. A launch monitor
//! declares its [`Source`] in the `device` block of its `bus.actor`.
//!
//! Golfer and club are not on shots: a collector takes them from the
//! `golfer.turn` current when `shot.trigger` arrived.
//!
//! # Raw frames
//!
//! A launch monitor can publish the protocol messages behind its shots as
//! [`RawFrame`]s (`shot.raw`), for debugging, when its own configuration
//! turns them on. Raw frames never carry device or user identity, and never
//! affect a shot.

use std::time::Duration;

#[cfg(feature = "main")]
mod cli;
mod lenient;
#[cfg(any(feature = "driver", feature = "transport"))]
mod net;
mod rand;

pub mod error;
pub mod kinds;
pub mod message;
pub mod types;
pub mod units;

#[cfg(feature = "controller")]
pub mod accumulator;

#[cfg(feature = "controller")]
pub mod bus;

#[cfg(feature = "transport")]
pub mod endpoint;

#[cfg(feature = "transport")]
pub mod hub;

#[cfg(feature = "config")]
pub mod config;

#[cfg(feature = "driver")]
pub mod driver;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "mock")]
pub mod mock;

pub use error::FrpError;
pub use kinds::{
    AimAction, BusActor, BusAlert, BusReply, BusSync, Course, DeviceInfo, EndReason, GameAim,
    GameBall, GameRound, GameScores, GameStroke, GolferProfile, GolferSelect, GolferTurn, Hole,
    HoleScore, Lie, MatchResult, PlayerScore, PracticeBlock, PracticeNewBlock, ReplyError,
    RoundPlayer, RoundStatus, ShotClub, ShotFinished, ShotFlight, ShotImpact, ShotLaunch,
    ShotTrigger, StrokeOutcome,
};
pub use message::{Class, FrpBody, FrpMessage, Removal, StateKey, StateKind, UnknownBody};
pub use types::{
    AssumedLaunch, Basis, Club, ClubData, FaceImpact, FlightResult, GolferId, Handedness,
    InvalidGolferId, Launch, Mode, Position, RawDir, RawFrame, Severity, Sex, ShotKey, Source,
};
pub use units::{Distance, Velocity};

#[cfg(feature = "controller")]
pub use accumulator::{CompletedShot, Flight, LateMessage, ShotAggregator, ShotEvent};

#[cfg(feature = "controller")]
pub use bus::BusState;

#[cfg(feature = "transport")]
pub use endpoint::{EndpointConfig, FrpConnection, FrpEndpoint, Transport};

#[cfg(feature = "transport")]
pub use hub::{Hub, HubEvent, PeerId};

#[cfg(feature = "server")]
pub use server::{FrpServer, MIN_HEARTBEAT, ServerConfig, ServerError, ServerEvent};

#[cfg(feature = "main")]
pub use cli::{RunOptions, run, run_args, run_with};

/// Recommended default port for standalone FRP connections.
pub const DEFAULT_PORT: u16 = 5880;

/// Recommended default WebSocket path for FRP connections.
pub const DEFAULT_PATH: &str = "/frp";

/// Recommended default URL for standalone FRP connections.
pub const DEFAULT_URL: &str = "ws://localhost:5880/frp";

/// Recommended accumulator timeout: how long a shot may take from
/// `shot.trigger` to `shot.finished` before a collector discards it, and how
/// long it remembers a finished shot for messages that arrive late. Some
/// launch monitors deliver their full flight data several seconds after the
/// strike.
pub const DEFAULT_SHOT_TIMEOUT: Duration = Duration::from_secs(30);

/// The heartbeat this crate's hosts use unless told otherwise: how often an
/// actor republishes its `bus.actor`.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(2);

/// How many heartbeats a receiver waits without a `bus.actor` from an actor
/// before it treats the actor as gone.
pub const MISSED_HEARTBEATS: u32 = 3;

/// The FRP spec version this crate implements, sent as `frp` in `bus.actor`.
pub const SPEC_VERSION: &str = "0.3.0";

/// A new actor id: 16 random lowercase hexadecimal digits.
///
/// Generate one once per actor and keep it across restarts; the host that
/// runs the actor decides where. The id identifies the actor, never the
/// machine or the person.
///
/// ```
/// let id = flightrelay::new_actor_id();
/// assert_eq!(id.len(), 16);
/// assert!(id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
/// ```
#[must_use]
pub fn new_actor_id() -> String {
    format!("{:016x}", rand::Rng::random().next_u64())
}
