//! [`BusState`]: the current state of the bus, for collectors and UIs.
//!
//! Requires the `controller` feature. Feed it every message; it keeps the
//! newest state message for each (kind, from, id), applies removals, and
//! forgets everything from an actor whose `bus.actor` it has not heard for
//! three heartbeats. Occurrences and commands pass through untouched.
//!
//! An actor has a heartbeat clock once its `bus.actor` arrives with a
//! positive `heartbeat`. State from an actor with no clock (no `bus.actor`
//! heard yet, or one whose `heartbeat` is zero, negative or not a number)
//! never expires: it is kept until it is removed, the actor's `bus.actor`
//! is removed, [`BusState::forget_actor`] is called, or the actor sends a
//! `bus.actor` with a positive `heartbeat`, which starts its clock.
//!
//! ```
//! use flightrelay::{BusActor, BusState, FrpMessage, Removal, StateKind};
//!
//! let mut bus = BusState::new();
//! let actor = FrpMessage::new("7d2f9a41c03be856", BusActor::new("Eagle One X4K2", 2.0));
//! assert!(bus.apply(&actor));
//! assert!(!bus.apply(&actor), "a heartbeat changes nothing");
//! assert_eq!(bus.actor("7d2f9a41c03be856").map(|a| a.name.as_str()), Some("Eagle One X4K2"));
//! bus.apply(&FrpMessage::new("7d2f9a41c03be856", Removal::of(StateKind::BusActor)));
//! assert!(bus.is_empty());
//! ```

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::MISSED_HEARTBEATS;
use crate::kinds::BusActor;
use crate::message::{FrpBody, FrpMessage, StateKey, StateKind};

/// The current state of the bus. See the [module docs](self).
#[derive(Debug, Default)]
pub struct BusState {
    states: BTreeMap<StateKey, FrpMessage>,
    /// When each actor's `bus.actor` was last heard, and how long it may
    /// then stay silent.
    heard: HashMap<String, (Instant, Option<Duration>)>,
}

impl BusState {
    /// An empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one message: store a state (the newest replaces the older one),
    /// forget a removed state, or, for the removal of `bus.actor`, forget the
    /// actor and all its state. A `bus.actor` also restarts the actor's
    /// heartbeat clock. Returns whether the stored state changed.
    pub fn apply(&mut self, msg: &FrpMessage) -> bool {
        self.apply_at(msg, Instant::now())
    }

    fn apply_at(&mut self, msg: &FrpMessage, now: Instant) -> bool {
        let Some(key) = msg.state_key() else {
            return false;
        };
        match &msg.body {
            FrpBody::Removed(_) if key.kind == StateKind::BusActor => self.forget_actor(&msg.from),
            FrpBody::Removed(_) => self.states.remove(&key).is_some(),
            body => {
                if let FrpBody::BusActor(actor) = body {
                    self.heard
                        .insert(msg.from.clone(), (now, silence_allowed(actor)));
                }
                if self.states.get(&key) == Some(msg) {
                    return false;
                }
                self.states.insert(key, msg.clone());
                true
            }
        }
    }

    /// Forgets every actor whose `bus.actor` has not been heard for three of
    /// its heartbeats, with all its state, and returns their ids. An actor
    /// with no heartbeat clock (see the [module docs](self)) is never
    /// forgotten here. Call it
    /// regularly, e.g. once a second, and pass each id to anything else that
    /// keeps per-actor state (such as
    /// [`ShotAggregator::forget_actor`](crate::ShotAggregator::forget_actor)).
    pub fn expire(&mut self) -> Vec<String> {
        self.expire_at(Instant::now())
    }

    fn expire_at(&mut self, now: Instant) -> Vec<String> {
        let gone: Vec<String> = self
            .heard
            .iter()
            .filter(|(_, (at, allowed))| {
                allowed.is_some_and(|allowed| now.saturating_duration_since(*at) > allowed)
            })
            .map(|(actor, _)| actor.clone())
            .collect();
        for actor in &gone {
            self.forget_actor(actor);
        }
        gone
    }

    /// Forgets `actor` and all its state. Returns whether anything was
    /// stored.
    pub fn forget_actor(&mut self, actor: &str) -> bool {
        let before = self.states.len();
        self.states.retain(|key, _| key.from != actor);
        self.heard.remove(actor);
        self.states.len() != before
    }

    /// The stored state with `key`.
    #[must_use]
    pub fn get(&self, key: &StateKey) -> Option<&FrpMessage> {
        self.states.get(key)
    }

    /// The `bus.actor` of `actor`, if stored.
    #[must_use]
    pub fn actor(&self, actor: &str) -> Option<&BusActor> {
        let key = StateKey {
            kind: StateKind::BusActor,
            from: actor.to_owned(),
            id: None,
        };
        match self.states.get(&key).map(|m| &m.body) {
            Some(FrpBody::BusActor(a)) => Some(a),
            _ => None,
        }
    }

    /// Every stored `bus.actor`, by actor id.
    pub fn actors(&self) -> impl Iterator<Item = (&str, &BusActor)> {
        self.states.values().filter_map(|m| match &m.body {
            FrpBody::BusActor(a) => Some((m.from.as_str(), a)),
            _ => None,
        })
    }

    /// Every stored state message, ordered by kind, then owner, then id.
    pub fn states(&self) -> impl Iterator<Item = &FrpMessage> {
        self.states.values()
    }

    /// Every stored state message of `kind`.
    pub fn of_kind(&self, kind: StateKind) -> impl Iterator<Item = &FrpMessage> {
        self.states
            .iter()
            .filter(move |(key, _)| key.kind == kind)
            .map(|(_, m)| m)
    }

    /// How many states are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Whether no state is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }
}

/// How long an actor may stay silent: three heartbeats. `None` (never
/// expires) for a heartbeat that is not a positive, finite number of seconds.
fn silence_allowed(actor: &BusActor) -> Option<Duration> {
    Duration::try_from_secs_f64(actor.heartbeat)
        .ok()
        .filter(|d| !d.is_zero())
        .and_then(|d| d.checked_mul(MISSED_HEARTBEATS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::{GolferProfile, GolferTurn, PracticeBlock, ShotTrigger};
    use crate::message::Removal;
    use crate::types::ShotKey;
    use crate::{Handedness, Mode};

    const LM: &str = "7d2f9a41c03be856";
    const CLUBHOUSE: &str = "c19e04b7a2d5f318";

    fn actor(from: &str, heartbeat: f64) -> FrpMessage {
        FrpMessage::new(from, BusActor::new(from, heartbeat))
    }

    fn turn(mode: Mode) -> FrpMessage {
        FrpMessage::new(
            CLUBHOUSE,
            GolferTurn::new(None, None, Handedness::Right, mode),
        )
    }

    fn profile(id: &str) -> FrpMessage {
        FrpMessage::new(CLUBHOUSE, GolferProfile::new(id.parse().unwrap(), "Sam"))
    }

    #[test]
    fn the_newest_state_replaces_the_older_and_occurrences_pass() {
        let mut bus = BusState::new();
        assert!(bus.apply(&turn(Mode::Full)));
        assert!(!bus.apply(&turn(Mode::Full)));
        assert!(bus.apply(&turn(Mode::Putting)));
        assert_eq!(bus.len(), 1);
        let stored = bus.of_kind(StateKind::GolferTurn).next().unwrap();
        assert_eq!(stored, &turn(Mode::Putting));
        let shot = FrpMessage::new(LM, ShotTrigger::new(ShotKey::new("a", 1)));
        assert!(!bus.apply(&shot));
        assert_eq!(bus.len(), 1);
    }

    #[test]
    fn states_are_keyed_by_kind_owner_and_id() {
        let mut bus = BusState::new();
        bus.apply(&profile("clubhouse.a"));
        bus.apply(&profile("clubhouse.b"));
        bus.apply(&turn(Mode::Full));
        // The same kind from another owner is another state.
        bus.apply(&FrpMessage::new(
            LM,
            GolferTurn::new(None, None, Handedness::Left, Mode::Full),
        ));
        assert_eq!(bus.of_kind(StateKind::GolferProfile).count(), 2);
        assert_eq!(bus.of_kind(StateKind::GolferTurn).count(), 2);
        let key = profile("clubhouse.b").state_key().unwrap();
        assert_eq!(bus.get(&key), Some(&profile("clubhouse.b")));
    }

    #[test]
    fn a_removal_forgets_its_state_only() {
        let mut bus = BusState::new();
        bus.apply(&profile("clubhouse.a"));
        bus.apply(&profile("clubhouse.b"));
        let block = FrpMessage::new(
            CLUBHOUSE,
            PracticeBlock {
                id: "blk".into(),
                golfer: None,
                club: None,
                number: 1,
                session: 1,
                started_at: 0,
            },
        );
        bus.apply(&block);
        let gone = FrpMessage::new(
            CLUBHOUSE,
            Removal::of_id(StateKind::GolferProfile, "clubhouse.a"),
        );
        assert!(bus.apply(&gone));
        assert!(!bus.apply(&gone), "already gone");
        assert_eq!(bus.of_kind(StateKind::GolferProfile).count(), 1);
        assert!(bus.apply(&FrpMessage::new(
            CLUBHOUSE,
            Removal::of_id(StateKind::PracticeBlock, "blk")
        )));
        assert_eq!(bus.len(), 1);
    }

    #[test]
    fn removing_bus_actor_forgets_the_actor_and_all_its_state() {
        let mut bus = BusState::new();
        bus.apply(&actor(CLUBHOUSE, 2.0));
        bus.apply(&turn(Mode::Full));
        bus.apply(&profile("clubhouse.a"));
        bus.apply(&actor(LM, 2.0));
        assert!(bus.apply(&FrpMessage::new(
            CLUBHOUSE,
            Removal::of(StateKind::BusActor)
        )));
        assert_eq!(bus.len(), 1);
        assert!(bus.actor(LM).is_some());
        assert!(bus.actor(CLUBHOUSE).is_none());
    }

    #[test]
    fn an_actor_is_dropped_after_three_missed_heartbeats() {
        let mut bus = BusState::new();
        let t0 = Instant::now();
        bus.apply_at(&actor(CLUBHOUSE, 2.0), t0);
        bus.apply_at(&turn(Mode::Full), t0);
        bus.apply_at(&actor(LM, 10.0), t0);
        let secs = |s| t0 + Duration::from_secs(s);
        assert!(
            bus.expire_at(secs(6)).is_empty(),
            "three heartbeats is not yet gone"
        );
        // A heartbeat restarts the clock; other messages don't.
        bus.apply_at(&turn(Mode::Putting), secs(5));
        bus.apply_at(&actor(CLUBHOUSE, 2.0), secs(5));
        assert!(bus.expire_at(secs(11)).is_empty());
        assert_eq!(bus.expire_at(secs(12)), [CLUBHOUSE]);
        assert!(bus.actor(CLUBHOUSE).is_none());
        assert_eq!(
            bus.of_kind(StateKind::GolferTurn).count(),
            0,
            "its state goes too"
        );
        assert!(bus.actor(LM).is_some());
        assert_eq!(bus.expire_at(secs(31)), [LM]);
        assert!(bus.is_empty());
        // A returning actor is simply new again.
        bus.apply_at(&actor(CLUBHOUSE, 2.0), secs(40));
        assert_eq!(bus.actors().count(), 1);
    }

    #[test]
    fn state_from_an_actor_never_heard_does_not_expire() {
        let mut bus = BusState::new();
        let t0 = Instant::now();
        bus.apply_at(&turn(Mode::Full), t0);
        bus.apply_at(&actor(LM, 0.0), t0);
        assert!(bus.expire_at(t0 + Duration::from_secs(3600)).is_empty());
        assert_eq!(bus.len(), 2);
        assert!(bus.forget_actor(CLUBHOUSE));
        assert!(!bus.forget_actor(CLUBHOUSE));
    }
}
