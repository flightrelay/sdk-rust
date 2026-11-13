use super::*;
use crate::kinds::{BusActor, DeviceInfo, ShotClub, ShotImpact, ShotLaunch, ShotTrigger};
use crate::message::Removal;
use crate::types::{AssumedLaunch, RawDir, RawFrame};
use crate::units::{Distance, Velocity};
use crate::{Club, GolferId, Handedness, Mode, ShotFinished, ShotFlight};

const LM: &str = "7d2f9a41c03be856";
const PHYSICS: &str = "e4a7136b9f02cd58";
const CLUBHOUSE: &str = "c19e04b7a2d5f318";

fn key() -> ShotKey {
    ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 42)
}

fn msg(from: &str, body: impl Into<FrpBody>) -> FrpMessage {
    FrpMessage::new(from, body)
}

fn trigger() -> FrpMessage {
    msg(LM, ShotTrigger::at(key(), 1_794_063_845_123))
}

fn finished() -> FrpMessage {
    msg(LM, ShotFinished { key: key() })
}

fn launch() -> Launch {
    Launch {
        launch_speed: Some(Velocity::MetersPerSecond(67.2)),
        launch_elevation: Some(14.2),
        backspin_rpm: Some(3200),
        sidespin_rpm: Some(-450),
        ..Default::default()
    }
}

fn flight(basis: Basis, model: &str, carry: f64) -> FlightResult {
    FlightResult {
        model: Some(model.into()),
        carry_distance: Some(Distance::Meters(carry)),
        ..FlightResult::new(basis)
    }
}

fn shot_flight(from: &str, f: FlightResult) -> FrpMessage {
    msg(
        from,
        ShotFlight {
            key: key(),
            flight: f,
        },
    )
}

fn finish(agg: &mut ShotAggregator) -> CompletedShot {
    match agg.feed(&finished()) {
        Some(ShotEvent::Finished(shot)) => shot,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_shot_is_composed_from_every_part_whoever_sends_it() {
    let mut agg = ShotAggregator::new();
    assert!(agg.feed(&trigger()).is_none());
    assert_eq!(agg.pending_count(), 1);
    agg.feed(&msg(
        LM,
        ShotLaunch {
            key: key(),
            launch: launch(),
        },
    ));
    let club = ClubData {
        club_speed: Some(Velocity::MetersPerSecond(42.1)),
        ..Default::default()
    };
    agg.feed(&msg(
        LM,
        ShotClub {
            key: key(),
            club: club.clone(),
        },
    ));
    let impact = FaceImpact {
        lateral: Some(Distance::Inches(0.31)),
        vertical: None,
    };
    // A part from another actor joins the same shot: shots are keyed by id.
    agg.feed(&msg(
        PHYSICS,
        ShotImpact {
            key: key(),
            impact: impact.clone(),
        },
    ));
    agg.feed(&shot_flight(
        PHYSICS,
        flight(Basis::Modeled, "Fairway Physics 1.3", 181.2),
    ));
    let shot = finish(&mut agg);
    assert_eq!(shot.from, LM);
    assert_eq!(shot.key, key());
    assert_eq!(shot.timestamp, Some(1_794_063_845_123));
    assert_eq!(shot.launch, Some(launch()));
    assert_eq!(shot.club, Some(club));
    assert_eq!(shot.impact, Some(impact));
    assert_eq!(shot.flights.len(), 1);
    assert_eq!(shot.flights[0].from, PHYSICS);
    assert_eq!(agg.pending_count(), 0);
}

#[test]
fn a_shot_keeps_every_flight() {
    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    let own = flight(Basis::Modeled, "Eagle One model 2.1", 180.0);
    let physics = flight(Basis::Modeled, "Fairway Physics 1.3", 181.2);
    let physics_v2 = flight(Basis::Modeled, "Fairway Physics 2.0", 183.0);
    agg.feed(&shot_flight(LM, own.clone()));
    agg.feed(&shot_flight(PHYSICS, physics.clone()));
    agg.feed(&shot_flight(PHYSICS, physics_v2.clone()));
    // The same sender and model again, and an exact repeat: both kept.
    let again = flight(Basis::Modeled, "Fairway Physics 1.3", 1.0);
    agg.feed(&shot_flight(PHYSICS, again.clone()));
    agg.feed(&shot_flight(PHYSICS, again.clone()));
    let shot = finish(&mut agg);
    let got: Vec<(&str, &FlightResult)> = shot
        .flights
        .iter()
        .map(|f| (f.from.as_str(), &f.flight))
        .collect();
    assert_eq!(
        got,
        [
            (LM, &own),
            (PHYSICS, &physics),
            (PHYSICS, &physics_v2),
            (PHYSICS, &again),
            (PHYSICS, &again),
        ]
    );
    assert_eq!(
        shot.flight(),
        Some(&own),
        "the launch monitor's own when none is tracked"
    );
}

#[test]
fn the_shown_flight_prefers_tracked() {
    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    agg.feed(&shot_flight(
        PHYSICS,
        flight(Basis::Modeled, "Fairway Physics 1.3", 181.2),
    ));
    let tracked = flight(Basis::Tracked, "Eagle One radar", 179.0);
    agg.feed(&shot_flight(LM, tracked.clone()));
    assert_eq!(finish(&mut agg).flight(), Some(&tracked));

    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    let other = flight(Basis::Modeled, "Fairway Physics 1.3", 181.2);
    agg.feed(&shot_flight(PHYSICS, other.clone()));
    assert_eq!(finish(&mut agg).flight(), Some(&other), "else the first");
}

#[test]
fn a_message_after_finished_is_attached_and_warned_about() {
    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    agg.feed(&msg(
        LM,
        ShotLaunch {
            key: key(),
            launch: launch(),
        },
    ));
    let first = finish(&mut agg);
    assert!(first.flights.is_empty());

    let late = flight(Basis::Modeled, "Fairway Physics 1.3", 181.2);
    let Some(ShotEvent::Late(warning)) = agg.feed(&shot_flight(PHYSICS, late.clone())) else {
        panic!("a late flight is surfaced");
    };
    assert_eq!(warning.from, PHYSICS);
    assert_eq!(warning.kind, "shot.flight");
    assert_eq!(warning.shot.flights[0].flight, late);
    assert_eq!(warning.shot.launch, first.launch);
    assert_eq!(
        warning.to_string(),
        "shot.flight from e4a7136b9f02cd58 arrived after shot 42 \
         (550e8400-e29b-41d4-a716-446655440000) finished"
    );
    // The remembered shot keeps it: a second late part sees both.
    let Some(ShotEvent::Late(again)) = agg.feed(&msg(
        LM,
        ShotImpact {
            key: key(),
            impact: FaceImpact::default(),
        },
    )) else {
        panic!()
    };
    assert_eq!(again.shot.flights.len(), 1);
    assert!(again.shot.impact.is_some());
    // A late part the shot already has changes nothing: no warning.
    assert_eq!(
        agg.feed(&msg(
            PHYSICS,
            ShotLaunch {
                key: key(),
                launch: Launch::default(),
            },
        )),
        None
    );
    assert_eq!(
        agg.feed(&msg(
            LM,
            ShotImpact {
                key: key(),
                impact: FaceImpact::default(),
            },
        )),
        None
    );
    // A repeated trigger or finish neither reopens nor warns.
    assert!(agg.feed(&trigger()).is_none());
    assert!(agg.feed(&finished()).is_none());
    assert_eq!(agg.pending_count(), 0);
}

#[test]
fn a_late_message_for_a_shot_no_longer_remembered_is_ignored() {
    let mut agg = ShotAggregator::new().with_timeout(Duration::from_secs(10));
    let t0 = Instant::now();
    agg.feed_at(&trigger(), t0);
    assert!(matches!(
        agg.feed_at(&finished(), t0 + Duration::from_secs(1)),
        Some(ShotEvent::Finished(_))
    ));
    let late = shot_flight(PHYSICS, flight(Basis::Modeled, "m", 1.0));
    assert!(matches!(
        agg.feed_at(&late, t0 + Duration::from_secs(11)),
        Some(ShotEvent::Late(_))
    ));
    assert!(agg.feed_at(&late, t0 + Duration::from_secs(12)).is_none());
    // Forgotten, so a trigger with the same id would start over.
    agg.feed_at(&trigger(), t0 + Duration::from_secs(12));
    assert_eq!(agg.pending_count(), 1);
}

#[test]
fn an_expired_shot_is_discarded_and_its_late_messages_ignored() {
    let mut agg = ShotAggregator::new();
    assert_eq!(agg.timeout(), DEFAULT_SHOT_TIMEOUT);
    let t0 = Instant::now();
    agg.feed_at(&trigger(), t0);
    let after = t0 + DEFAULT_SHOT_TIMEOUT + Duration::from_millis(1);
    assert_eq!(agg.discard_expired_at(after), 1);
    assert_eq!(agg.pending_count(), 0);
    assert!(
        agg.feed_at(
            &shot_flight(PHYSICS, flight(Basis::Modeled, "m", 1.0)),
            after
        )
        .is_none()
    );
    assert!(agg.feed_at(&finished(), after).is_none());
    agg.feed_at(&trigger(), after);
    assert_eq!(
        agg.pending_count(),
        0,
        "a discarded id does not start again"
    );
}

#[test]
fn a_shot_keeps_the_first_launch_club_and_impact() {
    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    agg.feed(&msg(
        LM,
        ShotLaunch {
            key: key(),
            launch: launch(),
        },
    ));
    agg.feed(&msg(
        PHYSICS,
        ShotLaunch {
            key: key(),
            launch: Launch::default(),
        },
    ));
    assert_eq!(finish(&mut agg).launch, Some(launch()));
}

#[test]
fn parts_before_the_trigger_or_for_unknown_shots_are_ignored() {
    let mut agg = ShotAggregator::new();
    assert!(
        agg.feed(&msg(
            LM,
            ShotLaunch {
                key: key(),
                launch: launch()
            }
        ))
        .is_none()
    );
    assert!(agg.feed(&finished()).is_none());
    agg.feed(&trigger());
    assert!(finish(&mut agg).launch.is_none());
}

#[test]
fn raw_frames_never_touch_a_shot() {
    let mut agg = ShotAggregator::new();
    let raw = RawFrame {
        key: Some(key()),
        ..RawFrame::new(RawDir::Rx, 0, vec![1, 2])
    };
    assert!(agg.feed(&msg(LM, FrpBody::ShotRaw(raw.clone()))).is_none());
    assert_eq!(agg.pending_count(), 0);
    agg.feed(&trigger());
    agg.feed(&msg(LM, FrpBody::ShotRaw(raw)));
    let shot = finish(&mut agg);
    assert!(shot.launch.is_none() && shot.flights.is_empty());
}

fn actor(source: Option<Source>) -> FrpMessage {
    let mut a = BusActor::new("Eagle One X4K2", 2.0).with_role("launch_monitor");
    if let Some(source) = source {
        a = a.with_device(DeviceInfo {
            source: Some(source),
            ..DeviceInfo::default()
        });
    }
    msg(LM, a)
}

#[test]
fn shots_carry_the_source_their_launch_monitor_declares() {
    let mut agg = ShotAggregator::new();
    agg.feed(&actor(Some(Source::Measured)));
    assert_eq!(agg.source(LM), Some(Source::Measured));
    agg.feed(&trigger());
    // Declared mid-shot: applies at finish.
    agg.feed(&actor(Some(Source::Virtual)));
    assert_eq!(finish(&mut agg).source, Some(Source::Virtual));
    // `bus.actor` is complete state: no device block, no source.
    agg.feed(&actor(None));
    assert_eq!(agg.source(LM), None);
}

fn sam() -> GolferId {
    "clubhouse.sam".parse().unwrap()
}

fn block(id: &str, golfer: Option<GolferId>) -> FrpMessage {
    msg(
        CLUBHOUSE,
        PracticeBlock {
            id: id.into(),
            golfer,
            club: Some(Club::Iron7),
            number: 1,
            session: 1,
            started_at: 0,
        },
    )
}

#[test]
fn shots_take_golfer_club_and_block_from_the_turn_current_at_the_trigger() {
    let mut agg = ShotAggregator::new();
    let turn = GolferTurn::new(Some(sam()), Some(Club::Iron7), Handedness::Left, Mode::Full);
    agg.feed(&msg(CLUBHOUSE, turn.clone()));
    agg.feed(&block("b-sam", Some(sam())));
    agg.feed(&block("b-none", None));
    agg.feed(&trigger());
    // A turn change after the trigger does not move the shot.
    agg.feed(&msg(
        CLUBHOUSE,
        GolferTurn::new(None, None, Handedness::Right, Mode::Putting),
    ));
    let shot = finish(&mut agg);
    assert_eq!(shot.turn, Some(turn));
    assert_eq!(shot.block.as_deref(), Some("b-sam"));

    // With no golfer selected, the block for no golfer.
    agg.feed(&msg(LM, ShotTrigger::new(ShotKey::new("second", 43))));
    let Some(ShotEvent::Finished(second)) = agg.feed(&msg(
        LM,
        ShotFinished {
            key: ShotKey::new("second", 43),
        },
    )) else {
        panic!()
    };
    assert_eq!(second.turn.unwrap().golfer, None);
    assert_eq!(second.block.as_deref(), Some("b-none"));
}

#[test]
fn removals_clear_the_turn_and_blocks() {
    let mut agg = ShotAggregator::new();
    agg.feed(&msg(
        CLUBHOUSE,
        GolferTurn::new(Some(sam()), None, Handedness::Right, Mode::Full),
    ));
    agg.feed(&block("b-sam", Some(sam())));
    // Another actor cannot remove what it doesn't own.
    agg.feed(&msg(PHYSICS, Removal::of(StateKind::GolferTurn)));
    assert!(agg.turn().is_some());
    agg.feed(&msg(
        CLUBHOUSE,
        Removal::of_id(StateKind::PracticeBlock, "b-sam"),
    ));
    agg.feed(&msg(CLUBHOUSE, Removal::of(StateKind::GolferTurn)));
    assert!(agg.turn().is_none());
    agg.feed(&trigger());
    let shot = finish(&mut agg);
    assert_eq!((shot.turn, shot.block), (None, None));
}

#[test]
fn an_actor_that_leaves_is_forgotten() {
    let mut agg = ShotAggregator::new();
    agg.feed(&actor(Some(Source::Measured)));
    agg.feed(&msg(
        CLUBHOUSE,
        GolferTurn::new(Some(sam()), None, Handedness::Right, Mode::Full),
    ));
    agg.feed(&trigger());
    // The launch monitor stops cleanly: its source goes and its open shot
    // is discarded.
    agg.feed(&msg(LM, Removal::of(StateKind::BusActor)));
    assert_eq!(agg.source(LM), None);
    assert_eq!(agg.pending_count(), 0);
    assert!(agg.turn().is_some(), "the session's turn stays");
    agg.forget_actor(CLUBHOUSE);
    assert!(agg.turn().is_none());
}

#[test]
fn a_gone_actors_open_shots_are_discarded_like_a_timeout() {
    let mut agg = ShotAggregator::new().with_timeout(Duration::from_secs(10));
    let t0 = Instant::now();
    agg.feed_at(&trigger(), t0);
    agg.forget_actor_at(LM, t0 + Duration::from_secs(1));
    assert_eq!(agg.pending_count(), 0);
    // Its late parts and a repeated trigger are ignored for one timeout.
    let late = shot_flight(PHYSICS, flight(Basis::Modeled, "m", 1.0));
    assert_eq!(agg.feed_at(&late, t0 + Duration::from_secs(2)), None);
    agg.feed_at(&trigger(), t0 + Duration::from_secs(2));
    assert_eq!(agg.pending_count(), 0);
    assert_eq!(agg.feed_at(&finished(), t0 + Duration::from_secs(3)), None);
    // After that the id is free again.
    agg.feed_at(&trigger(), t0 + Duration::from_secs(12));
    assert_eq!(agg.pending_count(), 1);
}

#[test]
fn only_the_triggering_actor_finishes_a_shot() {
    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    assert_eq!(agg.feed(&msg(PHYSICS, ShotFinished { key: key() })), None);
    assert_eq!(agg.pending_count(), 1, "still open");
    let shot = finish(&mut agg);
    assert_eq!(shot.from, LM);
}

#[test]
fn launch_for_flight_fills_from_the_shown_modeled_flight() {
    let mut agg = ShotAggregator::new();
    agg.feed(&trigger());
    let chip = Launch {
        launch_speed: Some(Velocity::MetersPerSecond(20.0)),
        launch_elevation: Some(30.0),
        ..Default::default()
    };
    agg.feed(&msg(
        LM,
        ShotLaunch {
            key: key(),
            launch: chip.clone(),
        },
    ));
    let modeled = FlightResult {
        assumed: Some(AssumedLaunch {
            backspin_rpm: Some(4500),
            launch_elevation: Some(99.0),
            ..Default::default()
        }),
        ..flight(Basis::Modeled, "Eagle One model 2.1", 31.2)
    };
    agg.feed(&shot_flight(LM, modeled));
    let shot = finish(&mut agg);
    let flown = shot.launch_for_flight().unwrap();
    assert_eq!(flown.backspin_rpm, Some(4500));
    assert_eq!(flown.launch_elevation, Some(30.0), "measured wins");
    assert_eq!(shot.launch, Some(chip), "the measured launch is unchanged");

    let mut empty = shot.clone();
    empty.launch = None;
    empty.flights.clear();
    assert_eq!(empty.launch_for_flight(), None);
}
