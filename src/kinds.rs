//! One Rust type per FRP message kind.
//!
//! Each type holds the kind's own fields; the envelope fields `kind` and
//! `from` live on [`FrpMessage`](crate::FrpMessage). Optional fields are left
//! out on the wire while `None`. A field that is required but may be `null`
//! (e.g. `golfer.turn` `club`) is an `Option` that writes `null` for `None`.
//! A field that may be absent, `null` or a value (e.g. `golfer.select`
//! `club`) is an `Option<Option<_>>`: `None` absent, `Some(None)` null.
//!
//! Where a field holds a value from a closed set (a club code, handedness,
//! mode, sex, a reply error, a match result) or a golfer id, a value that is
//! not recognized reads as absent and the rest of the message still applies.
//! Unknown fields are ignored.

#![allow(clippy::option_option)] // absent, null and a value are three different things on the wire

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{ClubData, FaceImpact, FlightResult, Launch, Severity, ShotKey, Source};
use crate::{Club, Distance, GolferId, Handedness, Mode, Position, Sex};

// ------------------------------------------------------------------ bus

/// `bus.actor` (state, every actor): who the actor is, what it owns, and its
/// status. Doubles as the heartbeat: the actor republishes it every
/// [`heartbeat`](Self::heartbeat) seconds, even when nothing changed.
///
/// A receiver that hears no `bus.actor` from an actor for three heartbeats
/// treats it as gone and forgets all its state. An actor that stops cleanly
/// sends its removal ([`Removal`](crate::Removal) of kind
/// [`StateKind::BusActor`](crate::StateKind::BusActor)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusActor {
    /// Display name, for people.
    pub name: String,
    /// The protocol version the actor speaks, e.g. `"0.3.0"`.
    pub frp: String,
    /// The interval, in seconds, at which the actor republishes `bus.actor`.
    pub heartbeat: f64,
    /// The actor's roles; see [`roles`](crate::types::roles). Other strings
    /// are allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// The golfer id namespaces this actor issues.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// The actor's own reasons a shot shouldn't be taken now; empty when it
    /// sees none. See [`blockers`](crate::types::blockers). Display only: no
    /// actor is required to act on another actor's blockers.
    pub blockers: Vec<String>,
    /// For launch monitors: the device behind the actor. A value that is
    /// not an object reads as `None`; within it, each field that does not
    /// read reads as absent (see [`DeviceInfo`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub device: Option<DeviceInfo>,
    /// Free key/value pairs with string values, for display only. A value
    /// that is not an object reads as `None`, and an entry whose value is not
    /// a string is left out.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::map"
    )]
    pub telemetry: Option<BTreeMap<String, String>>,
}

impl BusActor {
    /// An actor named `name` that speaks [`SPEC_VERSION`](crate::SPEC_VERSION)
    /// and republishes every `heartbeat` seconds, with no roles, namespaces,
    /// blockers, device or telemetry.
    #[must_use]
    pub fn new(name: impl Into<String>, heartbeat: f64) -> Self {
        Self {
            name: name.into(),
            frp: crate::SPEC_VERSION.to_owned(),
            heartbeat,
            roles: Vec::new(),
            namespaces: Vec::new(),
            blockers: Vec::new(),
            device: None,
            telemetry: None,
        }
    }

    /// Add a role.
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.roles.push(role.into());
        self
    }

    /// Add a golfer id namespace this actor issues.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespaces.push(namespace.into());
        self
    }

    /// Set the device block.
    #[must_use]
    pub fn with_device(mut self, device: DeviceInfo) -> Self {
        self.device = Some(device);
        self
    }

    /// Whether the actor lists `role`.
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    /// Whether the actor sees no reason a shot shouldn't be taken now.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.blockers.is_empty()
    }

    /// Whether the actor lists the blocker `blocker`, e.g.
    /// [`blockers::NOT_ARMED`](crate::types::blockers::NOT_ARMED).
    #[must_use]
    pub fn has_blocker(&self, blocker: &str) -> bool {
        self.blockers.iter().any(|b| b == blocker)
    }
}

/// The `device` block of a launch monitor's [`BusActor`].
///
/// Every field is optional and read leniently: a value of the wrong type (a
/// number for a string, a string for a number) reads as `None`, and the rest
/// of the block and of the `bus.actor` still applies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// The device's manufacturer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub manufacturer: Option<String>,
    /// The device's model.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub model: Option<String>,
    /// The device's firmware version.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub firmware: Option<String>,
    /// Where the device's shots originate. A value that is not recognized
    /// reads as `None` (unknown).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub source: Option<Source>,
    /// Battery charge, 0 to 100 percent. A value that is not an integer in
    /// that range reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "battery"
    )]
    pub battery_pct: Option<u8>,
    /// Device attitude, degrees.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub tilt: Option<f64>,
    /// Device attitude, degrees.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub roll: Option<f64>,
}

fn battery<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u8>, D::Error> {
    Ok(crate::lenient::option::<D, u8>(deserializer)?.filter(|pct| *pct <= 100))
}

/// `bus.sync` (occurrence, any actor): "send me the current state". Every
/// actor that receives it republishes all of its state, `bus.actor` first.
/// An actor sends it when it connects or reconnects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusSync {}

impl BusSync {
    /// A `bus.sync`.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }
}

/// `bus.reply` (occurrence, the owner of a command): the answer to a
/// command. Everyone receives it; only [`to`](Self::to) acts on it. A
/// successful reply follows the changed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusReply {
    /// The `from` of the command.
    pub to: String,
    /// The command's kind, e.g. `"golfer.select"`.
    pub command: String,
    /// The command's `ref`.
    pub r#ref: String,
    /// Whether the owner applied the command.
    pub ok: bool,
    /// Why not, when [`ok`](Self::ok) is false. A value that is not
    /// recognized reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub error: Option<ReplyError>,
    /// With [`ReplyError::NotReady`]: the owner's own blockers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Text for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl BusReply {
    /// A successful reply to the command `kind` with `ref` from the actor
    /// `to`.
    #[must_use]
    pub fn ok(to: impl Into<String>, command: impl Into<String>, r#ref: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            command: command.into(),
            r#ref: r#ref.into(),
            ok: true,
            error: None,
            blockers: Vec::new(),
            message: None,
        }
    }

    /// A refusal of the command `kind` with `ref` from the actor `to`.
    #[must_use]
    pub fn refused(
        to: impl Into<String>,
        command: impl Into<String>,
        r#ref: impl Into<String>,
        error: ReplyError,
    ) -> Self {
        Self {
            ok: false,
            error: Some(error),
            ..Self::ok(to, command, r#ref)
        }
    }
}

/// Why an owner refused a command, on [`BusReply::error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyError {
    /// Invalid fields.
    BadRequest,
    /// The owner refuses, e.g. a club outside the golfer's bag.
    NotAllowed,
    /// The owner can't now; see the reply's `blockers`.
    NotReady,
    /// The owner doesn't do this.
    Unsupported,
}

/// `bus.alert` (occurrence, any actor). Informational only; no receiver
/// behavior is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusAlert {
    /// How bad it is.
    pub severity: Severity,
    /// Text for people.
    pub message: String,
}

impl BusAlert {
    /// An alert of `severity` with `message`.
    #[must_use]
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
        }
    }
}

// ----------------------------------------------------------------- shot

/// `shot.trigger` (occurrence, launch monitor): a strike was detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotTrigger {
    /// The shot.
    pub key: ShotKey,
    /// When the launch monitor detected the strike, in milliseconds since
    /// the Unix epoch (UTC), by its own clock. A value that is not a
    /// non-negative integer reads as `None`, so the trigger still applies.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub timestamp: Option<u64>,
}

impl ShotTrigger {
    /// A trigger with no timestamp.
    #[must_use]
    pub fn new(key: ShotKey) -> Self {
        Self {
            key,
            timestamp: None,
        }
    }

    /// A trigger at `timestamp`, in milliseconds since the Unix epoch (UTC).
    #[must_use]
    pub fn at(key: ShotKey, timestamp: u64) -> Self {
        Self {
            key,
            timestamp: Some(timestamp),
        }
    }
}

/// `shot.launch` (occurrence, launch monitor): the measured launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotLaunch {
    /// The shot.
    pub key: ShotKey,
    /// The measured launch values.
    pub launch: Launch,
}

/// `shot.club` (occurrence, launch monitor): the measured club delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotClub {
    /// The shot.
    pub key: ShotKey,
    /// The measured club values.
    pub club: ClubData,
}

/// `shot.impact` (occurrence, launch monitor): where on the face the ball
/// was struck.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotImpact {
    /// The shot.
    pub key: ShotKey,
    /// The impact location.
    pub impact: FaceImpact,
}

/// `shot.flight` (occurrence, launch monitor or any flight model): results
/// of the ball's flight. A shot may have several, told apart by `from` and
/// [`FlightResult::model`]; a launch monitor sends at most one of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotFlight {
    /// The shot.
    pub key: ShotKey,
    /// The flight result.
    pub flight: FlightResult,
}

/// `shot.finished` (occurrence, launch monitor): the launch monitor has sent
/// everything it will for the shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotFinished {
    /// The shot.
    pub key: ShotKey,
}

// --------------------------------------------------------------- golfer

/// `golfer.profile` (state, the actor that issues the id's namespace; keyed
/// by [`id`](Self::id)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GolferProfile {
    /// The golfer, in a namespace the sender lists in its `bus.actor`.
    pub id: GolferId,
    /// Display name.
    pub name: String,
    /// The golfer's handedness. A value that is not recognized reads as
    /// `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub handedness: Option<Handedness>,
    /// The golfer's clubs, in the order the golfer wants them shown. An
    /// unknown club code is left out.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::lenient::list"
    )]
    pub bag: Vec<Club>,
    /// For published norms that depend on it. A value that is not recognized
    /// reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub sex: Option<Sex>,
    /// The golfer's height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<Distance>,
}

impl GolferProfile {
    /// A profile with only `id` and `name`.
    #[must_use]
    pub fn new(id: GolferId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            handedness: None,
            bag: Vec::new(),
            sex: None,
            height: None,
        }
    }
}

/// `golfer.turn` (state, `session`): who is up and with what. Launch
/// monitors read `handedness`, `club` and `mode` as tracking hints, applying
/// each field they support and ignoring the rest; `club` never implies
/// `mode`, and `mode` wins if they conflict.
///
/// A turn is complete: a receiver never merges it with an earlier one. A
/// `null` club means no club (unknown or not applicable), and a `null` or
/// unknown value in any field is no hint for that field.
///
/// `handedness` and `mode` are required on the wire. A turn whose
/// [`handedness`](Self::handedness) or [`mode`](Self::mode) is `None`
/// serializes without that field, and receivers treat such a turn as
/// invalid; build turns to send with [`new`](Self::new), which sets both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GolferTurn {
    /// The golfer up, in any namespace; `None` (`null`) when no golfer is
    /// selected. An invalid id reads as `None`.
    #[serde(default, deserialize_with = "crate::lenient::option")]
    pub golfer: Option<GolferId>,
    /// The club; `None` (`null`) is no club, unknown or not applicable. An
    /// unknown code reads as `None`.
    #[serde(default, deserialize_with = "crate::lenient::option")]
    pub club: Option<Club>,
    /// The session's handedness. `None` only when a received value was
    /// missing or not recognized; a turn serialized with `None` leaves the
    /// field out, which makes it invalid to receivers.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub handedness: Option<Handedness>,
    /// The mode. `None` only when a received value was missing or not
    /// recognized; a turn serialized with `None` leaves the field out, which
    /// makes it invalid to receivers.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub mode: Option<Mode>,
}

impl GolferTurn {
    /// A turn with every field.
    #[must_use]
    pub fn new(
        golfer: Option<GolferId>,
        club: Option<Club>,
        handedness: Handedness,
        mode: Mode,
    ) -> Self {
        Self {
            golfer,
            club,
            handedness: Some(handedness),
            mode: Some(mode),
        }
    }
}

/// `golfer.select` (command, to `session`): changes the turn. A field that
/// is `None` stays as it is.
///
/// A value that is not recognized (an unknown club code, handedness or mode,
/// or an invalid golfer id) reads as absent (`None`), not as `null`: that
/// field stays as it is, and the rest of the command still applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GolferSelect {
    /// The sender's reference, echoed in the reply.
    pub r#ref: String,
    /// `Some(None)` clears the golfer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::change",
        serialize_with = "crate::lenient::write_change"
    )]
    pub golfer: Option<Option<GolferId>>,
    /// `Some(None)` sets no club.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::change",
        serialize_with = "crate::lenient::write_change"
    )]
    pub club: Option<Option<Club>>,
    /// The handedness to set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub handedness: Option<Handedness>,
    /// The mode to set.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub mode: Option<Mode>,
}

impl GolferSelect {
    /// A select with `ref` that changes nothing yet.
    #[must_use]
    pub fn new(r#ref: impl Into<String>) -> Self {
        Self {
            r#ref: r#ref.into(),
            golfer: None,
            club: None,
            handedness: None,
            mode: None,
        }
    }
}

// ------------------------------------------------------------- practice

/// `practice.block` (state, `practice`; keyed by [`id`](Self::id)): one open
/// block per golfer. Closing a block is its removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PracticeBlock {
    /// UUID v4.
    pub id: String,
    /// The golfer; `None` (`null`) for the block of shots with no golfer
    /// selected. An invalid id reads as `None`.
    #[serde(default, deserialize_with = "crate::lenient::option")]
    pub golfer: Option<GolferId>,
    /// The club; `None` (`null`) is no club, unknown or not applicable. An
    /// unknown code reads as `None`.
    #[serde(default, deserialize_with = "crate::lenient::option")]
    pub club: Option<Club>,
    /// Counts from 1 within the session, per golfer.
    pub number: u32,
    /// The practice session number.
    pub session: u32,
    /// When the block started, in milliseconds since the Unix epoch (UTC).
    pub started_at: u64,
}

/// `practice.new_block` (command, to `practice`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PracticeNewBlock {
    /// The sender's reference, echoed in the reply.
    pub r#ref: String,
    /// The golfer whose block to roll; `None` (absent) for the golfer in
    /// `golfer.turn`, `Some(None)` for the block of shots with no golfer.
    /// Ignored with [`new_session`](Self::new_session).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::change",
        serialize_with = "crate::lenient::write_change"
    )]
    pub golfer: Option<Option<GolferId>>,
    /// Close every open block and count the session up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new_session: bool,
}

// ----------------------------------------------------------------- game

/// `game.round` (state, `game`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameRound {
    /// The game's round id.
    pub id: String,
    /// Where the round is.
    pub status: RoundStatus,
    /// With [`RoundStatus::Ended`]: why. A value that is not recognized
    /// reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub end_reason: Option<EndReason>,
    /// The course.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub course: Option<Course>,
    /// The scoring format: `stroke`, `stableford`, `match_play` or another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<String>,
    /// The players, in playing order.
    pub players: Vec<RoundPlayer>,
    /// Whose turn it is in the game; `None` (`null`) for nobody. An invalid
    /// id reads as `None`.
    #[serde(default, deserialize_with = "crate::lenient::option")]
    pub up: Option<GolferId>,
    /// The hole being played; `None` (`null`) between holes.
    #[serde(default)]
    pub hole: Option<Hole>,
}

/// [`GameRound::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundStatus {
    /// The round is loading.
    Loading,
    /// The round is being played.
    Playing,
    /// The round is paused.
    Paused,
    /// The round is over.
    Ended,
}

/// [`GameRound::end_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Every hole was played.
    Completed,
    /// The round was given up.
    Abandoned,
}

/// [`GameRound::course`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Course {
    /// The course name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How many holes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holes: Option<u32>,
    /// The course par.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub par: Option<u32>,
}

/// One entry of [`GameRound::players`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundPlayer {
    /// The player, in the game's namespace.
    pub golfer: GolferId,
    /// Display name.
    pub name: String,
    /// The player's team.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<u32>,
    /// The player's handicap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handicap: Option<f64>,
    /// Whether the player plays at this simulator.
    pub local: bool,
}

/// [`GameRound::hole`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hole {
    /// The hole number.
    pub number: u32,
    /// The hole's par.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub par: Option<u32>,
    /// The hole's stroke index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_index: Option<u32>,
    /// The hole's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<Distance>,
    /// Where the pin is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Position>,
}

/// `game.scores` (state, `game`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameScores {
    /// The [`GameRound::id`].
    pub round: String,
    /// Per golfer.
    pub players: Vec<PlayerScore>,
}

/// One entry of [`GameScores::players`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerScore {
    /// The player, in the game's namespace.
    pub golfer: GolferId,
    /// Total strokes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strokes: Option<u32>,
    /// Score relative to par.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_par: Option<i32>,
    /// Net score relative to par.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_to_par: Option<i32>,
    /// Stableford points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i32>,
    /// Position in the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// Per hole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holes: Vec<HoleScore>,
}

/// One entry of [`PlayerScore::holes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoleScore {
    /// The hole number.
    pub number: u32,
    /// Strokes on the hole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strokes: Option<u32>,
    /// Putts on the hole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub putts: Option<u32>,
    /// Score relative to par.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_par: Option<i32>,
    /// Net score relative to par.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_to_par: Option<i32>,
    /// Stableford points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<i32>,
    /// Match play result. A value that is not recognized reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::option"
    )]
    pub result: Option<MatchResult>,
    /// Whether the ball is holed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holed_out: Option<bool>,
    /// Whether the score won't change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#final: Option<bool>,
}

/// [`HoleScore::result`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchResult {
    /// The hole was won.
    Won,
    /// The hole was lost.
    Lost,
    /// The hole was halved.
    Halved,
}

/// `game.ball` (state, `game`): the ball about to be played.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameBall {
    /// The golfer to play it, in the game's namespace.
    pub golfer: GolferId,
    /// The hole number.
    pub hole: u32,
    /// The stroke about to be played.
    pub stroke_number: u32,
    /// Command kinds the game accepts now, e.g. `["game.aim"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_commands: Option<Vec<String>>,
    /// Where the ball lies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lie: Option<Lie>,
    /// The ball's position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    /// Where the shot is aimed; `Some(None)` (`null`) when the game aims at
    /// its default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::lenient::change",
        serialize_with = "crate::lenient::write_change"
    )]
    pub aim: Option<Option<Position>>,
    /// Distance to the hole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance_to_hole: Option<Distance>,
    /// While the game's `bus.actor` lists
    /// [`blockers::AWAITING_DECISION`](crate::types::blockers::AWAITING_DECISION): text for people,
    /// e.g. "Take a drop?".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
}

/// Where a ball lies, on [`GameBall`] and [`GameStroke`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lie {
    /// `tee`, `fairway`, `rough`, `sand`, `fringe`, `green` or another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// Degrees, positive uphill toward the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slope_up_down: Option<f64>,
    /// Degrees, positive where the ground rises to the right of the target
    /// line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slope_left_right: Option<f64>,
}

/// `game.stroke` (occurrence, `game`): the game's view of one stroke, sent
/// once when the ball is at rest. A stroke counts unless a later stroke
/// [`replaces`](Self::replaces) it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameStroke {
    /// The game's id for the stroke.
    pub stroke_id: String,
    /// The launch monitor's `shot_id`; `None` for a stroke with no shot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shot_id: Option<String>,
    /// The `stroke_id` this stroke replaces (mulligan, re-hit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The golfer, in the game's namespace.
    pub golfer: GolferId,
    /// The hole number.
    pub hole: u32,
    /// The stroke number.
    pub stroke_number: u32,
    /// The launch the game flew, which may differ from the measured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flown: Option<Launch>,
    /// The game's flight, with `basis` modeled and `model` the game's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight: Option<FlightResult>,
    /// What happened.
    pub outcome: StrokeOutcome,
    /// Whether the stroke was a putt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub putt: Option<bool>,
    /// Penalty strokes; 0 when absent.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub penalty_strokes: u32,
    /// Where the ball came to rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lie: Option<Lie>,
    /// Where the stroke started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_position: Option<Position>,
    /// Where the ball came to rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_position: Option<Position>,
    /// Distance to the hole before the stroke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_distance_to_hole: Option<Distance>,
    /// Distance to the hole after the stroke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_distance_to_hole: Option<Distance>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // a `skip_serializing_if` function takes a reference
fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// [`GameStroke::outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrokeOutcome {
    /// In play.
    InPlay,
    /// On the green.
    Green,
    /// Holed.
    Holed,
    /// Out of bounds.
    OutOfBounds,
    /// In the water.
    Water,
    /// Lost.
    Lost,
    /// Unplayable.
    Unplayable,
    /// A penalty with no shot.
    PenaltyOnly,
    /// A conceded putt.
    Gimme,
    /// A putt the game played itself.
    AutoPutt,
}

/// `game.aim` (command, to `game`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameAim {
    /// The sender's reference, echoed in the reply.
    pub r#ref: String,
    /// What to do.
    pub action: AimAction,
    /// With [`AimAction::Nudge`]: −45 to 45 degrees, positive right (target
    /// line frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degrees: Option<f64>,
    /// With [`AimAction::Set`]: where to aim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Position>,
}

/// [`GameAim::action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AimAction {
    /// Turn the aim by `degrees`.
    Nudge,
    /// Aim at `target`.
    Set,
    /// Aim at the game's default.
    Reset,
}
