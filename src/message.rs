//! FRP messages: the flat envelope and its parsing.
//!
//! Every message is one JSON object with its fields at the top level:
//! `kind` (the message kind, e.g. `shot.launch`), `from` (the sending
//! actor's id) and the kind's own fields. [`FrpMessage`] holds `from` and an
//! [`FrpBody`], which has one variant per kind, each holding that kind's
//! type from [`kinds`](crate::kinds).
//!
//! # Parsing rules
//!
//! - A message must be a JSON object with a string `kind` and a string
//!   `from`; anything else is an [`FrpError::Json`] for that message.
//! - Unknown fields are ignored.
//! - A kind this crate doesn't know, including every extension kind
//!   (`x.<vendor>.<name>`), parses as [`FrpBody::Unknown`], which keeps the
//!   message's fields so it serializes back unchanged. So does a known kind
//!   whose fields don't match it (e.g. a `shot.flight` without a recognized
//!   `basis`). A receiver ignores it; a hub forwards it like any other.
//! - A state kind with `"removed": true` parses as [`FrpBody::Removed`],
//!   whatever its other fields.

use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::FrpError;
use crate::kinds::{
    BusActor, BusAlert, BusReply, BusSync, GameAim, GameBall, GameRound, GameScores, GameStroke,
    GolferProfile, GolferSelect, GolferTurn, PracticeBlock, PracticeNewBlock, ShotClub,
    ShotFinished, ShotFlight, ShotImpact, ShotLaunch, ShotTrigger,
};
use crate::types::{RawFrame, ShotKey};

/// One FRP message: the sending actor and the kind's body.
///
/// ```
/// use flightrelay::{FrpBody, FrpMessage, ShotKey, ShotTrigger};
///
/// let msg = FrpMessage::new("7d2f9a41c03be856", ShotTrigger::new(ShotKey::new("id", 1)));
/// let json = msg.to_json()?;
/// assert_eq!(
///     json,
///     r#"{"kind":"shot.trigger","from":"7d2f9a41c03be856","key":{"shot_id":"id","shot_number":1}}"#
/// );
/// assert_eq!(FrpMessage::parse(&json)?, msg);
/// assert!(matches!(msg.body, FrpBody::ShotTrigger(_)));
/// # Ok::<(), flightrelay::FrpError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct FrpMessage {
    /// The sending actor's id.
    pub from: String,
    /// The kind and its fields.
    pub body: FrpBody,
}

impl FrpMessage {
    /// A message from the actor `from`.
    #[must_use]
    pub fn new(from: impl Into<String>, body: impl Into<FrpBody>) -> Self {
        Self {
            from: from.into(),
            body: body.into(),
        }
    }

    /// Parse one message from JSON text. See the [module docs](self) for the
    /// rules.
    ///
    /// # Errors
    ///
    /// Returns [`FrpError::Json`] for text that is not a JSON object with a
    /// string `kind` and a string `from`.
    pub fn parse(json: &str) -> Result<Self, FrpError> {
        let value: Value = serde_json::from_str(json)?;
        Ok(Self::from_value(value)?)
    }

    /// Serialize this message to JSON text: `kind` first, then `from`, then
    /// the kind's fields.
    ///
    /// # Errors
    ///
    /// Returns [`FrpError::Json`] if serialization fails.
    pub fn to_json(&self) -> Result<String, FrpError> {
        Ok(serde_json::to_string(self)?)
    }

    /// The message kind, e.g. `"shot.launch"`.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.body.kind()
    }

    /// The identity of the state this message carries or removes, or `None`
    /// for an occurrence, a command or an unknown kind.
    #[must_use]
    pub fn state_key(&self) -> Option<StateKey> {
        let (kind, id) = self.body.state_identity()?;
        Some(StateKey {
            kind,
            from: self.from.clone(),
            id: id.map(str::to_owned),
        })
    }

    fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let Value::Object(mut fields) = value else {
            return Err(serde_json::Error::custom("an FRP message is a JSON object"));
        };
        let Some(Value::String(kind)) = fields.remove("kind") else {
            return Err(serde_json::Error::custom("missing string field `kind`"));
        };
        let Some(Value::String(from)) = fields.remove("from") else {
            return Err(serde_json::Error::custom("missing string field `from`"));
        };
        let body = FrpBody::from_fields(kind, fields);
        Ok(Self { from, body })
    }
}

impl Serialize for FrpMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Flat<'a, B: Serialize> {
            kind: &'a str,
            from: &'a str,
            #[serde(flatten)]
            body: &'a B,
        }
        let kind = self.body.kind();
        let from = self.from.as_str();
        macro_rules! flat {
            ($body:expr) => {
                Flat {
                    kind,
                    from,
                    body: $body,
                }
                .serialize(serializer)
            };
        }
        match &self.body {
            FrpBody::BusActor(b) => flat!(b),
            FrpBody::BusSync(b) => flat!(b),
            FrpBody::BusReply(b) => flat!(b),
            FrpBody::BusAlert(b) => flat!(b),
            FrpBody::ShotTrigger(b) => flat!(b),
            FrpBody::ShotLaunch(b) => flat!(b),
            FrpBody::ShotClub(b) => flat!(b),
            FrpBody::ShotImpact(b) => flat!(b),
            FrpBody::ShotFlight(b) => flat!(b),
            FrpBody::ShotFinished(b) => flat!(b),
            FrpBody::ShotRaw(b) => flat!(b),
            FrpBody::GolferProfile(b) => flat!(b),
            FrpBody::GolferTurn(b) => flat!(b),
            FrpBody::GolferSelect(b) => flat!(b),
            FrpBody::PracticeBlock(b) => flat!(b),
            FrpBody::PracticeNewBlock(b) => flat!(b),
            FrpBody::GameRound(b) => flat!(b),
            FrpBody::GameScores(b) => flat!(b),
            FrpBody::GameBall(b) => flat!(b),
            FrpBody::GameStroke(b) => flat!(b),
            FrpBody::GameAim(b) => flat!(b),
            FrpBody::Removed(r) => flat!(&RemovalFields {
                id: r.id.as_deref(),
                removed: true,
            }),
            FrpBody::Unknown(u) => flat!(&u.fields),
        }
    }
}

#[derive(Serialize)]
struct RemovalFields<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    removed: bool,
}

impl<'de> Deserialize<'de> for FrpMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(D::Error::custom)
    }
}

macro_rules! bodies {
    ($( $(#[$doc:meta])* $variant:ident($ty:ty) = $kind:literal, )*) => {
        /// The kind of an [`FrpMessage`] and its fields: one variant per kind.
        ///
        /// Matches need a wildcard arm only for kinds added in later
        /// versions: they arrive as [`Unknown`](Self::Unknown).
        // Messages are parsed, handled and dropped one at a time, so the size
        // gap between variants costs nothing.
        #[allow(clippy::large_enum_variant)]
        #[derive(Debug, Clone, PartialEq)]
        pub enum FrpBody {
            $( $(#[$doc])* $variant($ty), )*
            /// The removal of a state: the owner's last message for it, with
            /// `"removed": true` and nothing else but its identity.
            Removed(Removal),
            /// A kind this crate doesn't know (an extension, a newer kind), or
            /// a known kind whose fields don't match it. Receivers ignore it;
            /// hubs forward it.
            Unknown(UnknownBody),
        }

        impl FrpBody {
            /// The kind on the wire, e.g. `"shot.launch"`.
            #[must_use]
            pub fn kind(&self) -> &str {
                match self {
                    $( Self::$variant(_) => $kind, )*
                    Self::Removed(r) => r.kind.as_str(),
                    Self::Unknown(u) => &u.kind,
                }
            }

            fn from_fields(kind: String, fields: Map<String, Value>) -> Self {
                if fields.get("removed") == Some(&Value::Bool(true))
                    && let Some(state) = StateKind::from_kind(&kind)
                {
                    let id = match fields.get("id") {
                        Some(Value::String(id)) => Some(id.clone()),
                        _ => None,
                    };
                    return Self::Removed(Removal { kind: state, id });
                }
                let parsed = match kind.as_str() {
                    $( $kind => serde_json::from_value::<$ty>(Value::Object(fields.clone()))
                        .ok()
                        .map(Self::$variant), )*
                    _ => None,
                };
                parsed.unwrap_or(Self::Unknown(UnknownBody { kind, fields }))
            }
        }

        $(
            impl From<$ty> for FrpBody {
                fn from(body: $ty) -> Self {
                    Self::$variant(body)
                }
            }
        )*

        /// Every kind this crate knows, in the order of the spec.
        pub const KINDS: &[&str] = &[ $( $kind, )* ];
    };
}

bodies! {
    /// `bus.actor` (state).
    BusActor(BusActor) = "bus.actor",
    /// `bus.sync` (occurrence).
    BusSync(BusSync) = "bus.sync",
    /// `bus.reply` (occurrence).
    BusReply(BusReply) = "bus.reply",
    /// `bus.alert` (occurrence).
    BusAlert(BusAlert) = "bus.alert",
    /// `shot.trigger` (occurrence).
    ShotTrigger(ShotTrigger) = "shot.trigger",
    /// `shot.launch` (occurrence).
    ShotLaunch(ShotLaunch) = "shot.launch",
    /// `shot.club` (occurrence).
    ShotClub(ShotClub) = "shot.club",
    /// `shot.impact` (occurrence).
    ShotImpact(ShotImpact) = "shot.impact",
    /// `shot.flight` (occurrence).
    ShotFlight(ShotFlight) = "shot.flight",
    /// `shot.finished` (occurrence).
    ShotFinished(ShotFinished) = "shot.finished",
    /// `shot.raw` (occurrence): one protocol message behind a launch
    /// monitor's messages, for debugging. Not part of a shot, even with a key.
    ShotRaw(RawFrame) = "shot.raw",
    /// `golfer.profile` (state, keyed by `id`).
    GolferProfile(GolferProfile) = "golfer.profile",
    /// `golfer.turn` (state).
    GolferTurn(GolferTurn) = "golfer.turn",
    /// `golfer.select` (command).
    GolferSelect(GolferSelect) = "golfer.select",
    /// `practice.block` (state, keyed by `id`).
    PracticeBlock(PracticeBlock) = "practice.block",
    /// `practice.new_block` (command).
    PracticeNewBlock(PracticeNewBlock) = "practice.new_block",
    /// `game.round` (state).
    GameRound(GameRound) = "game.round",
    /// `game.scores` (state).
    GameScores(GameScores) = "game.scores",
    /// `game.ball` (state).
    GameBall(GameBall) = "game.ball",
    /// `game.stroke` (occurrence).
    GameStroke(GameStroke) = "game.stroke",
    /// `game.aim` (command).
    GameAim(GameAim) = "game.aim",
}

impl From<Removal> for FrpBody {
    fn from(removal: Removal) -> Self {
        Self::Removed(removal)
    }
}

impl FrpBody {
    /// Whether this is a state, an occurrence or a command; `None` for an
    /// unknown kind.
    #[must_use]
    pub fn class(&self) -> Option<Class> {
        Some(match self {
            Self::BusActor(_)
            | Self::GolferProfile(_)
            | Self::GolferTurn(_)
            | Self::PracticeBlock(_)
            | Self::GameRound(_)
            | Self::GameScores(_)
            | Self::GameBall(_)
            | Self::Removed(_) => Class::State,
            Self::GolferSelect(_) | Self::PracticeNewBlock(_) | Self::GameAim(_) => Class::Command,
            Self::BusSync(_)
            | Self::BusReply(_)
            | Self::BusAlert(_)
            | Self::ShotTrigger(_)
            | Self::ShotLaunch(_)
            | Self::ShotClub(_)
            | Self::ShotImpact(_)
            | Self::ShotFlight(_)
            | Self::ShotFinished(_)
            | Self::ShotRaw(_)
            | Self::GameStroke(_) => Class::Occurrence,
            Self::Unknown(_) => return None,
        })
    }

    /// The `ref` of a command, or `None` for anything else.
    #[must_use]
    pub fn command_ref(&self) -> Option<&str> {
        match self {
            Self::GolferSelect(c) => Some(&c.r#ref),
            Self::PracticeNewBlock(c) => Some(&c.r#ref),
            Self::GameAim(c) => Some(&c.r#ref),
            _ => None,
        }
    }

    /// The shot a `shot.*` message belongs to: its key, or for `shot.raw`
    /// none (raw frames are not part of a shot).
    #[must_use]
    pub fn shot_key(&self) -> Option<&ShotKey> {
        match self {
            Self::ShotTrigger(m) => Some(&m.key),
            Self::ShotLaunch(m) => Some(&m.key),
            Self::ShotClub(m) => Some(&m.key),
            Self::ShotImpact(m) => Some(&m.key),
            Self::ShotFlight(m) => Some(&m.key),
            Self::ShotFinished(m) => Some(&m.key),
            _ => None,
        }
    }

    /// The state kind and, for kinds with several, the id.
    fn state_identity(&self) -> Option<(StateKind, Option<&str>)> {
        Some(match self {
            Self::BusActor(_) => (StateKind::BusActor, None),
            Self::GolferProfile(p) => (StateKind::GolferProfile, Some(p.id.as_str())),
            Self::GolferTurn(_) => (StateKind::GolferTurn, None),
            Self::PracticeBlock(b) => (StateKind::PracticeBlock, Some(b.id.as_str())),
            Self::GameRound(_) => (StateKind::GameRound, None),
            Self::GameScores(_) => (StateKind::GameScores, None),
            Self::GameBall(_) => (StateKind::GameBall, None),
            Self::Removed(r) => (r.kind, r.id.as_deref()),
            _ => return None,
        })
    }
}

/// The message classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// A complete object its owner republishes on change, at start and in
    /// answer to `bus.sync`. The newest replaces the older one.
    State,
    /// Sent once, never republished.
    Occurrence,
    /// Names the thing it changes; its owner applies it and answers with
    /// `bus.reply`.
    Command,
}

/// The state kinds: the kinds a [`Removal`] can remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateKind {
    /// `bus.actor`.
    BusActor,
    /// `golfer.profile`, keyed by `id`.
    GolferProfile,
    /// `golfer.turn`.
    GolferTurn,
    /// `practice.block`, keyed by `id`.
    PracticeBlock,
    /// `game.round`.
    GameRound,
    /// `game.scores`.
    GameScores,
    /// `game.ball`.
    GameBall,
}

impl StateKind {
    /// Every state kind.
    pub const ALL: &[StateKind] = &[
        Self::BusActor,
        Self::GolferProfile,
        Self::GolferTurn,
        Self::PracticeBlock,
        Self::GameRound,
        Self::GameScores,
        Self::GameBall,
    ];

    /// The kind on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BusActor => "bus.actor",
            Self::GolferProfile => "golfer.profile",
            Self::GolferTurn => "golfer.turn",
            Self::PracticeBlock => "practice.block",
            Self::GameRound => "game.round",
            Self::GameScores => "game.scores",
            Self::GameBall => "game.ball",
        }
    }

    /// Whether a state of this kind is keyed by `id` as well as its owner.
    #[must_use]
    pub fn keyed_by_id(self) -> bool {
        matches!(self, Self::GolferProfile | Self::PracticeBlock)
    }

    /// The state kind named `kind` on the wire.
    #[must_use]
    pub fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == kind)
    }
}

impl fmt::Display for StateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of a state on the bus: its kind, its owner, and for kinds
/// with several, its `id`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateKey {
    /// The state kind.
    pub kind: StateKind,
    /// The owning actor.
    pub from: String,
    /// The `id`, for [`StateKind::GolferProfile`] and
    /// [`StateKind::PracticeBlock`].
    pub id: Option<String>,
}

/// The removal of a state, sent by its owner as a last message with
/// `"removed": true` and nothing else but its identity. Receivers forget the
/// state; the removal of `bus.actor` means the actor stopped cleanly, and
/// receivers forget all its state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Removal {
    /// The kind of the state removed.
    pub kind: StateKind,
    /// Its `id`, for kinds keyed by one.
    pub id: Option<String>,
}

impl Removal {
    /// The removal of the state of `kind` that is not keyed by an id, e.g.
    /// `golfer.turn`, or of the actor itself (`bus.actor`).
    #[must_use]
    pub fn of(kind: StateKind) -> Self {
        Self { kind, id: None }
    }

    /// The removal of the state of `kind` with `id`, e.g. a closed
    /// `practice.block`.
    #[must_use]
    pub fn of_id(kind: StateKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: Some(id.into()),
        }
    }
}

/// The body of a message whose kind this crate doesn't know, or whose
/// fields don't match its kind. Keeps every field but `kind` and `from`, so
/// the message serializes back as it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownBody {
    /// The kind on the wire.
    pub kind: String,
    /// The other fields.
    pub fields: Map<String, Value>,
}

impl UnknownBody {
    /// Whether the kind is an extension kind (`x.<vendor>.<name>`).
    #[must_use]
    pub fn is_extension(&self) -> bool {
        self.kind.starts_with("x.")
    }
}

#[cfg(test)]
mod tests;
