//! Shot accumulator for collectors.
//!
//! Requires the `controller` feature. [`ShotAggregator`] composes a shot
//! from its `shot.*` messages and yields it when `shot.finished` arrives.
//!
//! - A shot is identified by `key.shot_id` alone: its messages are
//!   accumulated whoever sent them. `shot.trigger` opens it; the actor that
//!   sent the trigger is the shot's [`from`](CompletedShot::from), and only
//!   that actor's `shot.finished` finishes it (one from any other actor is
//!   ignored).
//! - A shot keeps the first `shot.launch`, `shot.club` and `shot.impact` it
//!   receives, and every `shot.flight`, in the order they arrive; each
//!   [`Flight`] records its sender, and its flight result its model.
//! - A shot carries the [`Source`] its launch monitor declares in the
//!   `device` block of its `bus.actor`, the `golfer.turn` current when its
//!   trigger arrived, and the open `practice.block` of that turn's golfer.
//! - A message for a shot that has finished is attached if the shot is still
//!   remembered (for one timeout after it finished). When it changed the
//!   shot (a flight, or a launch, club or impact the shot did not have yet)
//!   it yields a [`ShotEvent::Late`] for the caller to log as a warning; one
//!   that changed nothing yields nothing.
//! - A shot that has not finished within the accumulator timeout
//!   ([`DEFAULT_SHOT_TIMEOUT`] unless set with
//!   [`ShotAggregator::with_timeout`]) is discarded, and its messages are
//!   ignored for one more timeout, so a late message never starts a new
//!   partial shot. The open shots of an actor that is gone
//!   ([`ShotAggregator::forget_actor`], or its `bus.actor` removal) are
//!   discarded the same way.
//! - `shot.raw` is not part of a shot, even with a key.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use crate::DEFAULT_SHOT_TIMEOUT;
use crate::kinds::{GolferTurn, PracticeBlock};
use crate::message::{FrpBody, FrpMessage, StateKind};
use crate::types::{Basis, ClubData, FaceImpact, FlightResult, Launch, ShotKey, Source};

/// A shot with all the data its messages carried.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedShot {
    /// The actor that sent `shot.trigger`: the launch monitor that saw the
    /// strike.
    pub from: String,
    /// The shot.
    pub key: ShotKey,
    /// When the launch monitor detected the strike, in milliseconds since the
    /// Unix epoch (UTC) by its clock, if its trigger said.
    pub timestamp: Option<u64>,
    /// The measured launch, if one arrived.
    pub launch: Option<Launch>,
    /// The measured club delivery, if one arrived.
    pub club: Option<ClubData>,
    /// The impact location, if one arrived.
    pub impact: Option<FaceImpact>,
    /// Every flight result, in the order they arrived: the launch monitor's
    /// own and any flight model's, each one kept, even one equal to another.
    /// [`flight`](Self::flight) picks one.
    pub flights: Vec<Flight>,
    /// The source the launch monitor ([`from`](Self::from)) declares in its
    /// `bus.actor`, as of the latest message for the shot. `None` is unknown.
    pub source: Option<Source>,
    /// The `golfer.turn` current when the trigger arrived: who hit the shot,
    /// with what club, in which mode.
    pub turn: Option<GolferTurn>,
    /// The `id` of the open `practice.block` of the turn's golfer (or of the
    /// block for no golfer) when the trigger arrived.
    pub block: Option<String>,
}

/// One `shot.flight` of a shot.
#[derive(Debug, Clone, PartialEq)]
pub struct Flight {
    /// The actor that sent it.
    pub from: String,
    /// The flight result.
    pub flight: FlightResult,
}

impl CompletedShot {
    /// The flight result to show: the first [`Tracked`](Basis::Tracked) one,
    /// else the first from the shot's launch monitor, else the first.
    #[must_use]
    pub fn flight(&self) -> Option<&FlightResult> {
        self.flights
            .iter()
            .find(|f| f.flight.basis == Basis::Tracked)
            .or_else(|| self.flights.iter().find(|f| f.from == self.from))
            .or_else(|| self.flights.first())
            .map(|f| &f.flight)
    }

    /// Launch values for a receiver that flies the ball itself: each
    /// [`launch`](Self::launch) value, with a missing one filled from the
    /// `assumed` launch of [`flight`](Self::flight) when its basis is
    /// [`Modeled`](Basis::Modeled) (see [`Launch::fill_assumed`]). `None`
    /// when the shot has neither.
    ///
    /// The filled values are not measured. A measured-only receiver reads
    /// [`launch`](Self::launch) instead.
    #[must_use]
    pub fn launch_for_flight(&self) -> Option<Launch> {
        let mut launch = self.launch.clone().unwrap_or_default();
        if let Some(flight) = self.flight() {
            launch.fill_assumed(flight);
        }
        (self.launch.is_some() || launch != Launch::default()).then_some(launch)
    }

    /// Attach one shot message. Returns whether it changed the shot: a
    /// flight always does; a launch, club or impact does when the shot has
    /// none yet.
    fn attach(&mut self, from: &str, body: &FrpBody) -> bool {
        fn fill<T: Clone>(slot: &mut Option<T>, value: &T) -> bool {
            let empty = slot.is_none();
            if empty {
                *slot = Some(value.clone());
            }
            empty
        }
        match body {
            FrpBody::ShotLaunch(m) => fill(&mut self.launch, &m.launch),
            FrpBody::ShotClub(m) => fill(&mut self.club, &m.club),
            FrpBody::ShotImpact(m) => fill(&mut self.impact, &m.impact),
            FrpBody::ShotFlight(m) => {
                self.flights.push(Flight {
                    from: from.to_owned(),
                    flight: m.flight.clone(),
                });
                true
            }
            _ => false,
        }
    }
}

/// What feeding a message produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ShotEvent {
    /// `shot.finished` arrived for a shot in progress.
    Finished(CompletedShot),
    /// A message for a shot that has already finished arrived, was attached
    /// and changed the shot. Log it as a warning, and replace the shot you
    /// stored.
    Late(LateMessage),
}

/// A shot message that arrived after its shot's `shot.finished`. Its
/// `Display` is a warning line.
#[derive(Debug, Clone, PartialEq)]
pub struct LateMessage {
    /// The sender of the late message.
    pub from: String,
    /// Its kind, e.g. `"shot.flight"`.
    pub kind: String,
    /// The shot with the message attached.
    pub shot: CompletedShot,
}

impl fmt::Display for LateMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} from {} arrived after shot {} ({}) finished",
            self.kind, self.from, self.shot.key.shot_number, self.shot.key.shot_id
        )
    }
}

#[derive(Debug)]
struct InProgress {
    shot: CompletedShot,
    started: Instant,
}

/// Composes shots from `shot.*` messages, keyed by `shot_id`. See the
/// [module docs](self).
///
/// Feed every message with [`feed`](Self::feed). Besides shot messages it
/// reads `bus.actor` (each launch monitor's source), `golfer.turn` and
/// `practice.block`, and their removals. Call
/// [`forget_actor`](Self::forget_actor) for an actor that is gone, e.g. one
/// [`BusState::expire`](crate::BusState::expire) reports.
///
/// ```
/// use flightrelay::{FrpMessage, ShotAggregator, ShotEvent, ShotFinished, ShotKey, ShotTrigger};
///
/// let mut shots = ShotAggregator::new();
/// let key = ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 1);
/// shots.feed(&FrpMessage::new("7d2f9a41c03be856", ShotTrigger::new(key.clone())));
/// let done = shots.feed(&FrpMessage::new("7d2f9a41c03be856", ShotFinished { key }));
/// assert!(matches!(done, Some(ShotEvent::Finished(_))));
/// ```
#[derive(Debug)]
pub struct ShotAggregator {
    timeout: Duration,
    pending: HashMap<String, InProgress>,
    /// Finished shots, remembered for late messages, with when they finished.
    finished: HashMap<String, (CompletedShot, Instant)>,
    /// Discarded shots, with when they were discarded.
    discarded: HashMap<String, Instant>,
    sources: HashMap<String, Source>,
    turn: Option<(String, GolferTurn)>,
    blocks: HashMap<String, (String, PracticeBlock)>,
}

impl Default for ShotAggregator {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SHOT_TIMEOUT,
            pending: HashMap::new(),
            finished: HashMap::new(),
            discarded: HashMap::new(),
            sources: HashMap::new(),
            turn: None,
            blocks: HashMap::new(),
        }
    }
}

impl ShotAggregator {
    /// An aggregator with the [`DEFAULT_SHOT_TIMEOUT`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the accumulator timeout: how long a shot may take from
    /// `shot.trigger` to `shot.finished` before it is discarded, and how long
    /// a finished shot is remembered for late messages.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The accumulator timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Process one message.
    pub fn feed(&mut self, msg: &FrpMessage) -> Option<ShotEvent> {
        self.feed_at(msg, Instant::now())
    }

    fn feed_at(&mut self, msg: &FrpMessage, now: Instant) -> Option<ShotEvent> {
        self.discard_expired_at(now);
        let from = msg.from.as_str();
        match &msg.body {
            FrpBody::ShotTrigger(trigger) => {
                let id = &trigger.key.shot_id;
                if !self.pending.contains_key(id)
                    && !self.finished.contains_key(id)
                    && !self.discarded.contains_key(id)
                {
                    let shot = self.open(from, trigger.key.clone(), trigger.timestamp);
                    self.pending
                        .insert(id.clone(), InProgress { shot, started: now });
                }
                None
            }
            FrpBody::ShotFinished(finished) => {
                let id = &finished.key.shot_id;
                if self.pending.get(id)?.shot.from != from {
                    return None;
                }
                let mut done = self.pending.remove(id)?.shot;
                done.source = self.source(&done.from);
                self.finished.insert(id.clone(), (done.clone(), now));
                Some(ShotEvent::Finished(done))
            }
            body @ (FrpBody::ShotLaunch(_)
            | FrpBody::ShotClub(_)
            | FrpBody::ShotImpact(_)
            | FrpBody::ShotFlight(_)) => {
                let id = &body.shot_key()?.shot_id;
                if let Some(open) = self.pending.get_mut(id) {
                    open.shot.attach(from, body);
                    return None;
                }
                let (shot, _) = self.finished.get_mut(id)?;
                if !shot.attach(from, body) {
                    return None;
                }
                Some(ShotEvent::Late(LateMessage {
                    from: from.to_owned(),
                    kind: msg.kind().to_owned(),
                    shot: shot.clone(),
                }))
            }
            FrpBody::BusActor(actor) => {
                match actor.device.as_ref().and_then(|d| d.source) {
                    Some(source) => self.sources.insert(from.to_owned(), source),
                    None => self.sources.remove(from),
                };
                None
            }
            FrpBody::GolferTurn(turn) => {
                self.turn = Some((from.to_owned(), turn.clone()));
                None
            }
            FrpBody::PracticeBlock(block) => {
                self.blocks
                    .insert(block.id.clone(), (from.to_owned(), block.clone()));
                None
            }
            FrpBody::Removed(removal) => {
                match removal.kind {
                    StateKind::BusActor => self.forget_actor_at(from, now),
                    StateKind::GolferTurn => {
                        if self.turn.as_ref().is_some_and(|(owner, _)| owner == from) {
                            self.turn = None;
                        }
                    }
                    StateKind::PracticeBlock => {
                        if let Some(id) = &removal.id
                            && self.blocks.get(id).is_some_and(|(owner, _)| owner == from)
                        {
                            self.blocks.remove(id);
                        }
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }

    /// A new shot, stamped with the current turn and block.
    fn open(&self, from: &str, key: ShotKey, timestamp: Option<u64>) -> CompletedShot {
        let turn = self.turn.as_ref().map(|(_, turn)| turn.clone());
        let golfer = turn.as_ref().and_then(|t| t.golfer.as_ref());
        let block = self
            .blocks
            .values()
            .find(|(_, b)| b.golfer.as_ref() == golfer)
            .map(|(_, b)| b.id.clone());
        CompletedShot {
            from: from.to_owned(),
            key,
            timestamp,
            launch: None,
            club: None,
            impact: None,
            flights: Vec::new(),
            source: self.source(from),
            turn,
            block,
        }
    }

    /// Discards every shot that has not finished within the timeout of its
    /// trigger, and returns how many it discarded. Their later messages are
    /// ignored. Also forgets finished shots older than the timeout.
    pub fn discard_expired(&mut self) -> usize {
        self.discard_expired_at(Instant::now())
    }

    fn discard_expired_at(&mut self, now: Instant) -> usize {
        let timeout = self.timeout;
        let fresh = |at: &Instant| now.saturating_duration_since(*at) <= timeout;
        self.finished.retain(|_, (_, at)| fresh(at));
        self.discarded.retain(|_, at| fresh(at));
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| !fresh(&p.started))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.pending.remove(id);
            self.discarded.insert(id.clone(), now);
        }
        expired.len()
    }

    /// Forgets what came from `actor`: its declared source, its turn and
    /// blocks. The shots it triggered that have not finished are discarded,
    /// as on a timeout: their later messages are ignored for one timeout.
    /// For an actor that is gone.
    pub fn forget_actor(&mut self, actor: &str) {
        self.forget_actor_at(actor, Instant::now());
    }

    fn forget_actor_at(&mut self, actor: &str, now: Instant) {
        self.sources.remove(actor);
        if self.turn.as_ref().is_some_and(|(owner, _)| owner == actor) {
            self.turn = None;
        }
        self.blocks.retain(|_, (owner, _)| owner != actor);
        let gone: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.shot.from == actor)
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            self.pending.remove(&id);
            self.discarded.insert(id, now);
        }
    }

    /// The source `actor` declares, or `None` while it is unknown.
    #[must_use]
    pub fn source(&self, actor: &str) -> Option<Source> {
        self.sources.get(actor).copied()
    }

    /// The current `golfer.turn`, if any.
    #[must_use]
    pub fn turn(&self) -> Option<&GolferTurn> {
        self.turn.as_ref().map(|(_, turn)| turn)
    }

    /// Number of shots in progress.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests;
