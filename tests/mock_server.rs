//! The mock driver run by `FrpServer`, read by a collector the way an
//! application would: greet, publish a turn, collect shots and bus state.

#![cfg(all(feature = "mock", feature = "server", feature = "controller"))]

use std::time::{Duration, Instant};

use flightrelay::driver::{DeviceDriver, Stop};
use flightrelay::mock::{MockConfig, MockDriver};
use flightrelay::types::roles;
use flightrelay::{
    Basis, BusActor, BusState, EndpointConfig, FrpBody, FrpEndpoint, FrpMessage, FrpServer,
    GolferTurn, Handedness, Mode, ServerConfig, ServerEvent, ShotAggregator, ShotEvent, Source,
    Transport,
};

const MOCK: &str = "7d2f9a41c03be856";
const COLLECTOR: &str = "c19e04b7a2d5f318";

#[test]
fn a_collector_receives_complete_virtual_shots_from_the_mock() {
    let driver = MockDriver::new(MockConfig {
        shot_interval_s: 0.05,
        arm_delay_s: 0.02,
        ball_ready_delay_s: 0.02,
        post_shot_delay_s: 0.02,
        ..MockConfig::default()
    })
    .expect("config");
    let stop = Stop::new();
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        FrpServer::new(
            driver,
            ServerConfig::new(Transport::listen("127.0.0.1:0"), MOCK),
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

    let mut endpoint = FrpEndpoint::open(EndpointConfig::new(Transport::connect(format!(
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
    let mut shots = ShotAggregator::new();
    // Our own turn, as the aggregator would have heard it on the bus.
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
        if let Some(ShotEvent::Finished(shot)) = shots.feed(&msg) {
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

    for (i, shot) in done.iter().enumerate() {
        assert_eq!(shot.from, MOCK);
        assert_eq!(shot.key.shot_number as usize, i + 1);
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
