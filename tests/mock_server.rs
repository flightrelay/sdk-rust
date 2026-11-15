//! The mock driver run by `FrpServer`, read by a collector the way an
//! application would: greet, publish a turn, collect shots and bus state.

#![cfg(all(feature = "mock", feature = "server", feature = "collector"))]

use std::time::{Duration, Instant};

use flightrelay::driver::{FrpDriver, Stop};
use flightrelay::mock::{MockConfig, MockDriver};
use flightrelay::{
    Basis, BusActor, BusState, EndpointConfig, FrpBody, FrpEndpoint, FrpMessage, FrpServer,
    GolferTurn, Handedness, Mode, ServerConfig, ServerEvent, ShotAccumulator, ShotEvent, Source,
    Target, roles,
};

const MOCK: &str = "7d2f9a41c03be856";
const COLLECTOR: &str = "c19e04b7a2d5f318";

#[test]
fn a_collector_receives_complete_virtual_shots_from_the_mock() {
    let mut config = MockConfig::default();
    config.shot_interval_s = 0.05;
    config.arm_delay_s = 0.02;
    config.ball_ready_delay_s = 0.02;
    config.post_shot_delay_s = 0.02;
    let driver = MockDriver::new(config).expect("config");
    let stop = Stop::new();
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        FrpServer::new(
            driver,
            ServerConfig::new(Target::listen("127.0.0.1:0"), MOCK),
        )
        .on_event(move |event| {
            if let ServerEvent::Listening(addr) = event {
                let _ = addr_tx.send(*addr);
            }
        })
        .run(&server_stop)
    });
    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listening");

    let mut endpoint = FrpEndpoint::open(EndpointConfig::new(Target::connect(format!(
        "ws://{addr}/frp"
    ))))
    .expect("open");
    let mut conn = endpoint.establish().expect("connect");
    let me = BusActor::new("Clubhouse", 2.0)
        .with_role(roles::SESSION)
        .with_role(roles::COLLECTOR);
    conn.greet(&[FrpMessage::new(COLLECTOR, me)], &[])
        .expect("greet");
    let turn = GolferTurn::new(
        Some("clubhouse.sam".parse().unwrap()),
        None,
        Handedness::Left,
        Mode::Putting,
    );
    conn.send(&FrpMessage::new(COLLECTOR, turn.clone()))
        .expect("turn");

    let mut bus = BusState::new();
    let mut shots = ShotAccumulator::new();
    // Our own turn, as the accumulator would have heard it on the bus.
    shots.feed(&FrpMessage::new(COLLECTOR, turn.clone()));
    let mut done = Vec::new();
    let give_up = Instant::now() + Duration::from_secs(10);
    while done.len() < 2 {
        assert!(Instant::now() < give_up, "only {} shots", done.len());
        let msg = conn.recv().expect("recv");
        assert_eq!(msg.from, MOCK, "only the mock's messages come back");
        if let FrpBody::BusAlert(alert) = &msg.body {
            panic!("unexpected alert: {}", alert.message);
        }
        bus.apply(&msg);
        // The mock shoots on its own clock from the moment it listens, so a
        // shot can finish before our turn reaches it. Count only shots made
        // under our turn.
        if let Some(ShotEvent::Finished(shot)) = shots.feed(&msg)
            && shot.turn.as_ref() == Some(&turn)
        {
            done.push(shot);
        }
    }
    let actor = bus.actor(MOCK).expect("the mock's bus.actor");
    assert_eq!(actor.name, "Mock Launch Monitor");
    assert!(actor.has_role(roles::LAUNCH_MONITOR));

    stop.stop();
    server
        .join()
        .expect("server thread")
        .expect("server result");
    // The rest of the stream ends with the actor's removal.
    while let Ok(msg) = conn.recv() {
        bus.apply(&msg);
    }
    assert!(bus.actor(MOCK).is_none(), "the actor removed itself");

    assert_eq!(done[1].key.shot_number, done[0].key.shot_number + 1);
    for shot in &done {
        assert_eq!(shot.from, MOCK);
        assert_eq!(shot.source, Some(Source::Virtual));
        assert_eq!(shot.turn.as_ref(), Some(&turn));
        assert!(shot.timestamp.is_some());
        let speed = shot
            .launch
            .as_ref()
            .and_then(|l| l.launch_speed)
            .expect("a launch speed")
            .as_mps();
        assert!(
            speed < 5.0,
            "the turn's putting mode reached the mock: {speed}"
        );
        assert!(shot.club.is_some());
        let flight = shot.flight().expect("a flight result");
        assert_eq!(flight.basis, Basis::Modeled);
        assert!(flight.carry_distance.is_some());
    }
}
