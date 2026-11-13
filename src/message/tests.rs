// Test data: exact values that round-trip through text, and one long table of examples.
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use serde_json::{Value, json};

use super::*;
use crate::kinds::{AimAction, ReplyError, RoundStatus, StrokeOutcome};
use crate::types::{Basis, RawDir};
use crate::{Club, Distance, GolferId, Handedness, Mode, Severity, Source, Velocity};

const LM: &str = "7d2f9a41c03be856";
const CLUBHOUSE: &str = "c19e04b7a2d5f318";
const GAME: &str = "5b80e2f7d14c9a63";
const PHYSICS: &str = "e4a7136b9f02cd58";

/// One example per kind: the spec's where it has one, fictional otherwise.
fn examples() -> Vec<Value> {
    vec![
        json!({
            "kind": "bus.actor", "from": LM, "name": "Eagle One X4K2", "frp": "0.3.0",
            "heartbeat": 2.0, "roles": ["launch_monitor"], "blockers": [],
            "device": {
                "manufacturer": "Birdie Labs", "model": "Eagle One", "firmware": "2.4.1",
                "source": "measured", "battery_pct": 82, "tilt": 0.4, "roll": -0.2
            }
        }),
        json!({ "kind": "bus.sync", "from": PHYSICS }),
        json!({
            "kind": "bus.reply", "from": CLUBHOUSE, "to": GAME, "command": "golfer.select",
            "ref": "sel-118", "ok": true
        }),
        json!({ "kind": "bus.alert", "from": LM, "severity": "warn", "message": "Battery low" }),
        json!({
            "kind": "shot.trigger", "from": LM,
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 },
            "timestamp": 1_794_063_845_123_u64
        }),
        json!({
            "kind": "shot.launch", "from": LM,
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 },
            "launch": {
                "launch_speed": "67.2mps", "launch_azimuth": -1.3, "launch_elevation": 14.2,
                "backspin_rpm": 3200, "sidespin_rpm": -450
            }
        }),
        json!({
            "kind": "shot.club", "from": LM,
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 },
            "club": { "club_speed": "42.1mps", "path": -2.1, "face_angle": 1.2, "club_offset": "0.47in" }
        }),
        json!({
            "kind": "shot.impact", "from": LM,
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 },
            "impact": { "lateral": "0.31in", "vertical": "0.15in" }
        }),
        json!({
            "kind": "shot.flight", "from": PHYSICS,
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 },
            "flight": {
                "basis": "modeled", "model": "Fairway Physics 1.3", "carry_distance": "181.2m",
                "carry_offline": "-7.1m", "total_distance": "196.4m", "max_height": "28.0m"
            }
        }),
        json!({
            "kind": "shot.finished", "from": LM,
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 }
        }),
        json!({
            "kind": "shot.raw", "from": LM, "dir": "rx", "seq": 1042,
            "timestamp": 1_794_063_845_200_u64, "label": "result", "data": "0a1b2c",
            "key": { "shot_id": "550e8400-e29b-41d4-a716-446655440000", "shot_number": 42 }
        }),
        json!({
            "kind": "golfer.profile", "from": CLUBHOUSE,
            "id": "clubhouse.0f8c2d1a-77b4-4e1e-9a50-3c6d2b8e4f10", "name": "Sam",
            "handedness": "lh",
            "bag": ["DR", "3W", "5H", "6I", "7I", "8I", "9I", "PW", "SW", "PT"],
            "height": "175cm"
        }),
        json!({
            "kind": "golfer.turn", "from": CLUBHOUSE, "golfer": "backnine.local-3",
            "club": "7I", "handedness": "rh", "mode": "full"
        }),
        json!({
            "kind": "golfer.select", "from": GAME, "ref": "sel-118",
            "golfer": "backnine.local-3", "handedness": "rh"
        }),
        json!({
            "kind": "practice.block", "from": CLUBHOUSE,
            "id": "9b2e6f04-1c3d-4a8b-b7e5-2f0d9c6a1e33",
            "golfer": "clubhouse.0f8c2d1a-77b4-4e1e-9a50-3c6d2b8e4f10", "club": "7I",
            "number": 4, "session": 12, "started_at": 1_794_063_845_123_u64
        }),
        json!({ "kind": "practice.new_block", "from": CLUBHOUSE, "ref": "nb-7", "new_session": true }),
        json!({
            "kind": "game.round", "from": GAME, "id": "r-20261110-1", "status": "playing",
            "course": { "name": "Pine Hollow", "holes": 18, "par": 72 },
            "scoring": "stroke",
            "players": [
                { "golfer": "backnine.local-3", "name": "Sam", "handicap": 12.4, "local": true },
                { "golfer": "backnine.remote-1", "name": "Alex", "local": false }
            ],
            "up": "backnine.local-3",
            "hole": {
                "number": 7, "par": 4, "stroke_index": 3, "length": "391yd",
                "pin": ["12.0m", "355.2m", "3.1m"]
            }
        }),
        json!({
            "kind": "game.scores", "from": GAME, "round": "r-20261110-1",
            "players": [{
                "golfer": "backnine.local-3", "strokes": 27, "to_par": 3,
                "holes": [
                    { "number": 6, "strokes": 4, "putts": 2, "to_par": 0, "holed_out": true, "final": true },
                    { "number": 7, "strokes": 1, "holed_out": false, "final": false }
                ]
            }]
        }),
        json!({
            "kind": "game.ball", "from": GAME, "golfer": "backnine.local-3", "hole": 7,
            "stroke_number": 2, "allowed_commands": ["game.aim"],
            "lie": { "surface": "fairway", "slope_up_down": 1.5, "slope_left_right": -0.8 },
            "position": ["8.4m", "212.6m", "1.2m"], "aim": null, "distance_to_hole": "157yd"
        }),
        json!({
            "kind": "game.stroke", "from": GAME, "stroke_id": "r-20261110-1/7/1",
            "shot_id": "550e8400-e29b-41d4-a716-446655440000", "golfer": "backnine.local-3",
            "hole": 7, "stroke_number": 1,
            "flown": {
                "launch_speed": "67.2mps", "launch_azimuth": -1.3, "launch_elevation": 14.2,
                "backspin_rpm": 3200, "sidespin_rpm": -450
            },
            "flight": {
                "basis": "modeled", "model": "Back Nine Golf", "carry_distance": "198yd",
                "total_distance": "234yd", "max_height": "31yd"
            },
            "outcome": "in_play",
            "lie": { "surface": "fairway", "slope_up_down": 1.5, "slope_left_right": -0.8 },
            "start_distance_to_hole": "391yd", "end_distance_to_hole": "157yd"
        }),
        json!({ "kind": "game.aim", "from": CLUBHOUSE, "ref": "aim-3", "action": "nudge", "degrees": -2.0 }),
    ]
}

/// Whether `a` and `b` are the same JSON, where two strings that are the same
/// unit-tagged distance or velocity count as equal (`"28.0m"` is `"28m"`).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|other| same(v, other)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y))
        }
        (Value::String(a), Value::String(b)) => {
            a == b
                || matches!((a.parse::<Distance>(), b.parse::<Distance>()), (Ok(x), Ok(y)) if x.as_meters() == y.as_meters())
                || matches!((a.parse::<Velocity>(), b.parse::<Velocity>()), (Ok(x), Ok(y)) if x.as_mps() == y.as_mps())
        }
        _ => a == b,
    }
}

#[test]
fn every_kind_round_trips_with_the_spec_examples() {
    let examples = examples();
    let mut seen: Vec<String> = Vec::new();
    for example in &examples {
        let msg = FrpMessage::parse(&example.to_string()).unwrap();
        assert!(
            !matches!(msg.body, FrpBody::Unknown(_)),
            "{example} parsed as unknown"
        );
        assert_eq!(msg.kind(), example["kind"]);
        assert_eq!(msg.from, example["from"]);
        let json = msg.to_json().unwrap();
        assert!(json.starts_with(r#"{"kind":"#), "{json}");
        let back: Value = serde_json::from_str(&json).unwrap();
        assert!(same(&back, example), "\n{back}\n{example}");
        assert_eq!(FrpMessage::parse(&json).unwrap(), msg);
        seen.push(msg.kind().to_owned());
    }
    assert_eq!(seen, KINDS, "one example per kind, in spec order");
    assert_eq!(KINDS.len(), 21);
}

#[test]
fn every_kind_has_a_class() {
    for example in examples() {
        let msg = FrpMessage::parse(&example.to_string()).unwrap();
        let class = msg.body.class().unwrap();
        let state = msg.state_key();
        assert_eq!(class == Class::State, state.is_some(), "{}", msg.kind());
        assert_eq!(
            class == Class::Command,
            msg.body.command_ref().is_some(),
            "{}",
            msg.kind()
        );
    }
}

fn parse(v: &Value) -> FrpMessage {
    FrpMessage::parse(&v.to_string()).unwrap()
}

#[test]
fn spec_examples_parse_to_the_expected_values() {
    let ex = examples();
    let FrpBody::BusActor(actor) = parse(&ex[0]).body else {
        panic!()
    };
    assert_eq!(actor.heartbeat, 2.0);
    assert!(actor.is_clear());
    assert!(actor.has_role(crate::types::roles::LAUNCH_MONITOR));
    let device = actor.device.unwrap();
    assert_eq!(device.source, Some(Source::Measured));
    assert_eq!(device.battery_pct, Some(82));

    let FrpBody::BusReply(reply) = parse(&ex[2]).body else {
        panic!()
    };
    assert_eq!(reply, BusReply::ok(GAME, "golfer.select", "sel-118"));

    let FrpBody::ShotFlight(flight) = parse(&ex[8]).body else {
        panic!()
    };
    assert_eq!(flight.flight.basis, Basis::Modeled);
    assert_eq!(flight.flight.max_height, Some(Distance::Meters(28.0)));

    let FrpBody::ShotRaw(raw) = parse(&ex[10]).body else {
        panic!()
    };
    assert_eq!(
        (raw.dir, raw.seq, raw.data),
        (RawDir::Rx, 1042, vec![0x0a, 0x1b, 0x2c])
    );

    let FrpBody::GolferProfile(profile) = parse(&ex[11]).body else {
        panic!()
    };
    assert_eq!(profile.id.namespace(), "clubhouse");
    assert_eq!(profile.handedness, Some(Handedness::Left));
    assert_eq!(profile.bag.len(), 10);
    assert_eq!(profile.height, Some(Distance::Centimeters(175.0)));

    let FrpBody::GolferTurn(turn) = parse(&ex[12]).body else {
        panic!()
    };
    assert_eq!(
        turn,
        GolferTurn::new(
            Some("backnine.local-3".parse().unwrap()),
            Some(Club::Iron7),
            Handedness::Right,
            Mode::Full
        )
    );

    let FrpBody::GolferSelect(select) = parse(&ex[13]).body else {
        panic!()
    };
    assert_eq!(
        select.golfer,
        Some(Some("backnine.local-3".parse().unwrap()))
    );
    assert_eq!(select.club, None, "absent stays as it is");
    assert_eq!(select.mode, None);

    let FrpBody::GameRound(round) = parse(&ex[16]).body else {
        panic!()
    };
    assert_eq!(round.status, RoundStatus::Playing);
    assert_eq!(round.players.len(), 2);
    let hole = round.hole.unwrap();
    assert_eq!(hole.length, Some(Distance::Yards(391.0)));
    assert_eq!(hole.pin.unwrap().y(), Distance::Meters(355.2));

    let FrpBody::GameBall(ball) = parse(&ex[18]).body else {
        panic!()
    };
    assert_eq!(ball.aim, Some(None), "null aim: the game's default");

    let FrpBody::GameStroke(stroke) = parse(&ex[19]).body else {
        panic!()
    };
    assert_eq!(stroke.outcome, StrokeOutcome::InPlay);
    assert_eq!(stroke.penalty_strokes, 0);

    let FrpBody::GameAim(aim) = parse(&ex[20]).body else {
        panic!()
    };
    assert_eq!((aim.action, aim.degrees), (AimAction::Nudge, Some(-2.0)));
}

#[test]
fn unknown_and_extension_kinds_parse_as_unknown_and_serialize_back() {
    for kind in [
        "x.birdielabs.lights",
        "shot_trigger",
        "golfer.shot",
        "launch",
    ] {
        let v = json!({ "kind": kind, "from": LM, "color": "green", "n": 3 });
        let msg = parse(&v);
        let FrpBody::Unknown(unknown) = &msg.body else {
            panic!("{kind}")
        };
        assert_eq!(unknown.kind, kind);
        assert_eq!(unknown.is_extension(), kind.starts_with("x."));
        assert_eq!(msg.kind(), kind);
        assert_eq!(msg.body.class(), None);
        let back: Value = serde_json::from_str(&msg.to_json().unwrap()).unwrap();
        assert_eq!(back, v);
    }
}

#[test]
fn unknown_fields_are_ignored() {
    let msg = parse(&json!({
        "kind": "bus.alert", "from": LM, "severity": "error", "message": "x",
        "future_field": { "a": 1 }, "seq": 9
    }));
    assert_eq!(
        msg.body,
        FrpBody::BusAlert(BusAlert::new(Severity::Error, "x"))
    );
}

#[test]
fn a_known_kind_whose_fields_dont_match_is_unknown() {
    for v in [
        // No recognized basis.
        json!({ "kind": "shot.flight", "from": LM, "key": { "shot_id": "a", "shot_number": 1 },
                "flight": { "carry_distance": "180m" } }),
        json!({ "kind": "shot.flight", "from": LM, "key": { "shot_id": "a", "shot_number": 1 },
                "flight": { "basis": "estimated" } }),
        // Odd number of hex digits.
        json!({ "kind": "shot.raw", "from": LM, "dir": "rx", "seq": 1, "data": "abc" }),
        // Missing required fields.
        json!({ "kind": "bus.actor", "from": LM, "name": "x" }),
        json!({ "kind": "game.stroke", "from": GAME, "stroke_id": "s" }),
        json!({ "kind": "golfer.profile", "from": CLUBHOUSE, "id": "Not An Id", "name": "x" }),
    ] {
        let msg = parse(&v);
        assert!(matches!(msg.body, FrpBody::Unknown(_)), "{v}");
        assert_eq!(msg.kind(), v["kind"]);
    }
}

#[test]
fn a_message_without_kind_or_from_is_an_error() {
    for text in [
        r#"{"from":"a"}"#,
        r#"{"kind":"bus.sync"}"#,
        r#"{"kind":7,"from":"a"}"#,
        r#"{"kind":"bus.sync","from":null}"#,
        r#"["bus.sync"]"#,
        "not json",
    ] {
        assert!(
            matches!(FrpMessage::parse(text), Err(FrpError::Json(_))),
            "{text}"
        );
    }
}

#[test]
fn removal_of_each_state_kind() {
    for &kind in StateKind::ALL {
        let mut v = json!({ "kind": kind.as_str(), "from": CLUBHOUSE, "removed": true });
        if kind.keyed_by_id() {
            v["id"] = json!("the-id");
        }
        let msg = parse(&v);
        let expected = if kind.keyed_by_id() {
            Removal::of_id(kind, "the-id")
        } else {
            Removal::of(kind)
        };
        assert_eq!(msg.body, FrpBody::Removed(expected), "{kind}");
        assert_eq!(msg.kind(), kind.as_str());
        assert_eq!(msg.body.class(), Some(Class::State));
        let key = msg.state_key().unwrap();
        assert_eq!(key.kind, kind);
        assert_eq!(key.id.is_some(), kind.keyed_by_id());
        let back: Value = serde_json::from_str(&msg.to_json().unwrap()).unwrap();
        assert_eq!(back, v);
    }
}

#[test]
fn removed_true_wins_over_other_fields_and_other_values_do_not_remove() {
    let full = examples()[14].clone();
    let mut removed = full.clone();
    removed["removed"] = json!(true);
    assert_eq!(
        parse(&removed).body,
        FrpBody::Removed(Removal::of_id(
            StateKind::PracticeBlock,
            "9b2e6f04-1c3d-4a8b-b7e5-2f0d9c6a1e33"
        ))
    );
    for other in [json!(false), json!("true"), json!(1)] {
        let mut v = full.clone();
        v["removed"] = other;
        assert!(matches!(parse(&v).body, FrpBody::PracticeBlock(_)), "{v}");
    }
    // `removed` on an occurrence is just an unknown field.
    let v = json!({ "kind": "bus.sync", "from": LM, "removed": true });
    assert_eq!(parse(&v).body, FrpBody::BusSync(BusSync {}));
}

#[test]
fn state_keys_identify_owner_and_id() {
    let profile = parse(&examples()[11]).state_key().unwrap();
    assert_eq!(profile.kind, StateKind::GolferProfile);
    assert_eq!(profile.from, CLUBHOUSE);
    assert_eq!(
        profile.id.as_deref(),
        Some("clubhouse.0f8c2d1a-77b4-4e1e-9a50-3c6d2b8e4f10")
    );
    let turn = parse(&examples()[12]).state_key().unwrap();
    assert_eq!((turn.kind, turn.id), (StateKind::GolferTurn, None));
    assert!(parse(&examples()[4]).state_key().is_none());
}

#[test]
fn unknown_club_codes_are_absent_and_the_rest_applies() {
    let turn = parse(&json!({
        "kind": "golfer.turn", "from": CLUBHOUSE, "golfer": null, "club": "11W",
        "handedness": "rh", "mode": "full"
    }));
    let FrpBody::GolferTurn(turn) = turn.body else {
        panic!()
    };
    assert_eq!(turn.club, None);
    assert_eq!(turn.golfer, None);
    assert_eq!(turn.mode, Some(Mode::Full));

    let profile = parse(&json!({
        "kind": "golfer.profile", "from": CLUBHOUSE, "id": "clubhouse.1", "name": "Sam",
        "bag": ["DR", "XX", "7i", "PT"], "handedness": "both", "sex": "x"
    }));
    let FrpBody::GolferProfile(profile) = profile.body else {
        panic!()
    };
    assert_eq!(profile.bag, [Club::Driver, Club::Putter]);
    assert_eq!(profile.handedness, None);
    assert_eq!(profile.sex, None);

    // In a select an unknown club is absent: the club stays as it is.
    let select = parse(&json!({ "kind": "golfer.select", "from": GAME, "ref": "1", "club": "ZZ" }));
    let FrpBody::GolferSelect(select) = select.body else {
        panic!()
    };
    assert_eq!(select.club, None);
}

#[test]
fn unknown_handedness_and_mode_on_a_turn_leave_the_rest() {
    let turn = parse(&json!({
        "kind": "golfer.turn", "from": CLUBHOUSE, "golfer": "backnine.local-3",
        "club": "PW", "handedness": "ambi", "mode": "flop"
    }));
    let FrpBody::GolferTurn(turn) = turn.body else {
        panic!()
    };
    assert_eq!(turn.club, Some(Club::PitchingWedge));
    assert_eq!(turn.handedness, None);
    assert_eq!(turn.mode, None);
    assert_eq!(turn.golfer.unwrap().local(), "local-3");
}

#[test]
fn a_turn_writes_null_for_no_golfer_and_unknown_club() {
    let turn = FrpMessage::new(
        CLUBHOUSE,
        GolferTurn::new(None, None, Handedness::Left, Mode::Putting),
    );
    let v: Value = serde_json::from_str(&turn.to_json().unwrap()).unwrap();
    assert_eq!(
        v,
        json!({ "kind": "golfer.turn", "from": CLUBHOUSE, "golfer": null, "club": null,
                "handedness": "lh", "mode": "putting" })
    );
}

#[test]
fn select_and_new_block_tell_null_from_absent() {
    let mut select = GolferSelect::new("r1");
    select.golfer = Some(None);
    select.club = Some(Some(Club::SandWedge));
    let v: Value =
        serde_json::from_str(&FrpMessage::new(GAME, select.clone()).to_json().unwrap()).unwrap();
    assert_eq!(
        v,
        json!({ "kind": "golfer.select", "from": GAME, "ref": "r1", "golfer": null, "club": "SW" })
    );
    assert_eq!(parse(&v).body, FrpBody::GolferSelect(select));

    let nb =
        parse(&json!({ "kind": "practice.new_block", "from": GAME, "ref": "n", "golfer": null }));
    let FrpBody::PracticeNewBlock(nb) = nb.body else {
        panic!()
    };
    assert_eq!(nb.golfer, Some(None));
    assert!(!nb.new_session);
    let nb = parse(&json!({ "kind": "practice.new_block", "from": GAME, "ref": "n" }));
    let FrpBody::PracticeNewBlock(nb) = nb.body else {
        panic!()
    };
    assert_eq!(nb.golfer, None);
}

#[test]
fn lenient_device_fields_and_reply_errors() {
    let actor = parse(&json!({
        "kind": "bus.actor", "from": LM, "name": "x", "frp": "0.3.0", "heartbeat": 1.5,
        "blockers": ["not_armed", "warming_up"],
        "device": { "source": "imagined", "battery_pct": 140, "model": "M" }
    }));
    let FrpBody::BusActor(actor) = actor.body else {
        panic!()
    };
    let device = actor.device.clone().unwrap();
    assert_eq!(device.source, None);
    assert_eq!(device.battery_pct, None);
    assert_eq!(device.model.as_deref(), Some("M"));
    assert!(actor.has_blocker(crate::types::blockers::NOT_ARMED));
    assert!(
        actor.has_blocker("warming_up"),
        "unknown blockers are kept as is"
    );

    let reply = parse(&json!({
        "kind": "bus.reply", "from": CLUBHOUSE, "to": GAME, "command": "golfer.select",
        "ref": "r", "ok": false, "error": "not_ready", "blockers": ["paused"]
    }));
    let FrpBody::BusReply(reply) = reply.body else {
        panic!()
    };
    assert_eq!(reply.error, Some(ReplyError::NotReady));
    assert_eq!(reply.blockers, ["paused"]);
    let odd = parse(&json!({
        "kind": "bus.reply", "from": CLUBHOUSE, "to": GAME, "command": "c", "ref": "r",
        "ok": false, "error": "overloaded"
    }));
    let FrpBody::BusReply(odd) = odd.body else {
        panic!()
    };
    assert_eq!(odd.error, None);
}

#[test]
fn bad_device_values_and_telemetry_never_make_a_bus_actor_unknown() {
    let actor = parse(&json!({
        "kind": "bus.actor", "from": LM, "name": "x", "frp": "0.3.0", "heartbeat": 1.5,
        "blockers": [],
        "device": {
            "manufacturer": 7, "model": ["M"], "firmware": "2.4.1",
            "tilt": "level", "roll": -0.2, "battery_pct": "full"
        },
        "telemetry": { "radar_mode": "armed", "temp": 31, "flags": null }
    }));
    let FrpBody::BusActor(actor) = actor.body else {
        panic!("a bad optional field leaves the bus.actor known: {actor:?}")
    };
    let device = actor.device.clone().unwrap();
    assert_eq!(device.manufacturer, None);
    assert_eq!(device.model, None);
    assert_eq!(device.firmware.as_deref(), Some("2.4.1"));
    assert_eq!(device.tilt, None);
    assert_eq!(device.roll, Some(-0.2));
    assert_eq!(device.battery_pct, None);
    let telemetry = actor.telemetry.unwrap();
    assert_eq!(telemetry.len(), 1, "non-string entries are left out");
    assert_eq!(telemetry["radar_mode"], "armed");

    for (device, telemetry) in [
        (json!("Eagle One"), json!("hot")),
        (json!(5), json!(["a"])),
        (json!(null), json!(null)),
    ] {
        let actor = parse(&json!({
            "kind": "bus.actor", "from": LM, "name": "x", "frp": "0.3.0", "heartbeat": 1.5,
            "blockers": [], "device": device, "telemetry": telemetry
        }));
        let FrpBody::BusActor(actor) = actor.body else {
            panic!("{actor:?}")
        };
        assert_eq!((actor.device, actor.telemetry), (None, None));
    }
}

#[test]
fn malformed_optional_shot_fields_are_ignored() {
    let trigger = parse(&json!({
        "kind": "shot.trigger", "from": LM, "key": { "shot_id": "a", "shot_number": 1 },
        "timestamp": -5
    }));
    assert_eq!(
        trigger.body,
        FrpBody::ShotTrigger(ShotTrigger::new(ShotKey::new("a", 1)))
    );
    let raw = parse(&json!({
        "kind": "shot.raw", "from": LM, "dir": "tx", "seq": 2, "data": "ABCD",
        "label": 5, "timestamp": "now", "key": "nope"
    }));
    let FrpBody::ShotRaw(raw) = raw.body else {
        panic!()
    };
    assert_eq!(raw.data, [0xab, 0xcd]);
    assert_eq!((raw.label, raw.timestamp, raw.key), (None, None, None));
}

#[test]
fn shot_keys_come_from_shot_messages_only() {
    let ex = examples();
    for (i, v) in ex.iter().enumerate() {
        let msg = parse(v);
        let has = msg.body.shot_key().is_some();
        let is_shot = msg.kind().starts_with("shot.") && msg.kind() != "shot.raw";
        assert_eq!(has, is_shot, "{i}: {}", msg.kind());
    }
}

#[test]
fn messages_deserialize_through_serde_too() {
    let v = examples()[3].clone();
    let msg: FrpMessage = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(msg, parse(&v));
    assert!(serde_json::from_value::<FrpMessage>(json!({ "kind": "bus.sync" })).is_err());
}

#[test]
fn golfer_ids_validate_their_namespace() {
    for good in [
        "a.b",
        "back-nine9.x.y",
        "clubhouse.0f8c",
        "n.ID with spaces",
    ] {
        assert!(good.parse::<GolferId>().is_ok(), "{good}");
    }
    for bad in ["", "a", "a.", ".b", "A.b", "a b.c", "ä.b"] {
        assert!(bad.parse::<GolferId>().is_err(), "{bad}");
    }
    let id = GolferId::new("backnine", "remote.1").unwrap();
    assert_eq!(id.as_str(), "backnine.remote.1");
    assert_eq!(id.local(), "remote.1");
    assert!(GolferId::new("x", "").is_err());
}

#[test]
fn every_club_code_round_trips() {
    for &club in Club::ALL {
        let code = club.to_string();
        assert_eq!(code.parse::<Club>().unwrap(), club);
        assert_eq!(serde_json::to_value(club).unwrap(), json!(code));
    }
    assert!("dr".parse::<Club>().is_err(), "codes are case-sensitive");
}

#[test]
fn builders_make_the_matching_messages() {
    let actor = BusActor::new("Clubhouse", 2.0)
        .with_role(crate::types::roles::SESSION)
        .with_role(crate::types::roles::PRACTICE)
        .with_namespace("clubhouse");
    let v: Value =
        serde_json::from_str(&FrpMessage::new(CLUBHOUSE, actor).to_json().unwrap()).unwrap();
    assert_eq!(
        v,
        json!({ "kind": "bus.actor", "from": CLUBHOUSE, "name": "Clubhouse", "frp": "0.3.0",
                "heartbeat": 2.0, "roles": ["session", "practice"], "namespaces": ["clubhouse"],
                "blockers": [] })
    );
    let refused = BusReply::refused(GAME, "golfer.select", "s", ReplyError::NotAllowed);
    assert!(!refused.ok);
    let launch = FrpMessage::new(
        LM,
        ShotLaunch {
            key: ShotKey::new("a", 1),
            launch: crate::Launch {
                launch_speed: Some(Velocity::MetersPerSecond(67.2)),
                ..Default::default()
            },
        },
    );
    assert_eq!(
        launch.to_json().unwrap(),
        r#"{"kind":"shot.launch","from":"7d2f9a41c03be856","key":{"shot_id":"a","shot_number":1},"launch":{"launch_speed":"67.2mps"}}"#
    );
}
