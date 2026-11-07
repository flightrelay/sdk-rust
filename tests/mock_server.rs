//! The mock driver served by `FrpServer`, read by a controller the way an
//! application would.

#![cfg(all(feature = "mock", feature = "server", feature = "controller"))]

use std::time::{Duration, Instant};

use flightrelay::driver::{DeviceDriver, Stop};
use flightrelay::mock::{MockConfig, MockDriver};
use flightrelay::{
    Basis, EndpointConfig, FrpEndpoint, FrpMessage, FrpProtocolMessage, FrpServer, Role,
    ServerConfig, ServerEvent, ShotAggregator, Source, Transport,
};

#[test]
fn a_controller_receives_complete_virtual_shots_from_the_mock() {
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
        FrpServer::new(driver, ServerConfig::new(Transport::listen("127.0.0.1:0")))
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

    let mut controller = FrpEndpoint::open(EndpointConfig::new(
        Role::Controller,
        Transport::connect(format!("ws://{addr}/frp")),
    ))
    .expect("open");
    let mut conn = controller.establish().expect("handshake");

    let mut shots = ShotAggregator::new();
    let mut done = Vec::new();
    let give_up = Instant::now() + Duration::from_secs(10);
    while done.len() < 2 {
        assert!(Instant::now() < give_up, "only {} shots", done.len());
        match conn.recv() {
            Ok(FrpMessage::Envelope(env)) => {
                assert_eq!(env.device, "MockLM-0001");
                if let Some(shot) = shots.feed_envelope(&env) {
                    done.push(shot);
                }
            }
            Ok(FrpMessage::Protocol(FrpProtocolMessage::Alert { message, .. })) => {
                panic!("unexpected alert: {message}")
            }
            Ok(FrpMessage::Protocol(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }
    stop.stop();
    server
        .join()
        .expect("server thread")
        .expect("server result");

    for (i, shot) in done.iter().enumerate() {
        assert_eq!(shot.key.shot_number as usize, i + 1);
        assert_eq!(shot.source, Some(Source::Virtual));
        assert!(shot.timestamp.is_some());
        assert!(
            shot.launch
                .as_ref()
                .is_some_and(|l| l.launch_speed.is_some())
        );
        assert!(shot.club.is_some());
        let flight = shot.flight.as_ref().expect("a flight result");
        assert_eq!(flight.basis, Basis::Modeled);
        assert!(flight.carry_distance.is_some());
    }
}
