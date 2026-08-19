//! Both transport directions, for both roles.
//!
//! FRP roles are independent of transport direction: the handshake always runs
//! controller→device (`start`) and device→controller (`init`), whichever side
//! opened the socket.

#![cfg(feature = "transport")]

use std::thread;

use flightrelay::{
    BallFlight, Distance, EndpointConfig, FrpEndpoint, FrpEnvelope, FrpEvent, FrpMessage, Role,
    ShotKey, Transport, Velocity,
};

/// Bind an endpoint on an ephemeral port and report the address it got.
fn listener(role: Role) -> (FrpEndpoint, String) {
    let ep = FrpEndpoint::open(EndpointConfig::new(role, Transport::listen("127.0.0.1:0")))
        .expect("bind");
    let addr = ep
        .local_addr()
        .expect("addr")
        .expect("listening")
        .to_string();
    (ep, addr)
}

fn shot() -> FrpEnvelope {
    FrpEnvelope::new(
        "EagleOne-X4K2",
        FrpEvent::ball_flight(
            ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 7),
            BallFlight {
                launch_speed: Some(Velocity::MetersPerSecond(67.2)),
                carry_distance: Some(Distance::Meters(180.5)),
                ..Default::default()
            },
        ),
    )
}

fn assert_is_our_shot(msg: &FrpMessage) {
    let FrpMessage::Envelope(env) = msg else {
        panic!("expected an envelope, got {msg:?}");
    };
    assert_eq!(env.device, "EagleOne-X4K2");
    let FrpEvent::BallFlight { key, ball } = &env.event else {
        panic!("expected ball_flight, got {:?}", env.event);
    };
    assert_eq!(key.shot_number, 7);
    assert_eq!(ball.carry_distance, Some(Distance::Meters(180.5)));
}

/// Standalone FRP: the device listens, the controller dials.
#[test]
fn device_listens_controller_dials() {
    let (mut device_ep, addr) = listener(Role::Device);

    let device = thread::spawn(move || {
        let mut conn = device_ep.establish().expect("device handshake");
        assert_eq!(conn.role(), Role::Device);
        assert_eq!(conn.peer_name(), Some("test-controller"));
        conn.send_envelope(&shot()).expect("send");
    });

    let mut controller_ep = FrpEndpoint::open(
        EndpointConfig::new(
            Role::Controller,
            Transport::connect(format!("ws://{addr}/frp")),
        )
        .with_name("test-controller"),
    )
    .expect("open");
    let mut conn = controller_ep.establish().expect("controller handshake");

    assert_eq!(conn.role(), Role::Controller);
    assert_eq!(conn.version(), flightrelay::SPEC_VERSION);
    assert_is_our_shot(&conn.recv().expect("recv"));

    device.join().expect("device thread");
}

/// Reverse-connect: the controller listens as a central bus, the device bridges
/// out to it. The handshake direction is unchanged.
#[test]
fn controller_listens_device_dials() {
    let (mut controller_ep, addr) = listener(Role::Controller);

    let controller = thread::spawn(move || {
        let mut conn = controller_ep.establish().expect("controller handshake");
        assert_eq!(conn.role(), Role::Controller);
        assert_is_our_shot(&conn.recv().expect("recv"));
    });

    let mut device_ep = FrpEndpoint::open(
        EndpointConfig::new(Role::Device, Transport::connect(format!("ws://{addr}/frp")))
            .with_name("ironsight"),
    )
    .expect("open");
    let mut conn = device_ep.establish().expect("device handshake");

    assert_eq!(conn.role(), Role::Device);
    assert_eq!(conn.version(), flightrelay::SPEC_VERSION);
    // The dialing device still receives `start`, so it still learns the peer name.
    assert_eq!(conn.peer_name(), Some("anonymous"));
    conn.send_envelope(&shot()).expect("send");

    controller.join().expect("controller thread");
}

/// An endpoint outlives its connections — `establish` again to reconnect.
#[test]
fn endpoint_establishes_successive_connections() {
    let (mut device_ep, addr) = listener(Role::Device);

    let device = thread::spawn(move || {
        for n in 1..=2u32 {
            let mut conn = device_ep.establish().expect("device handshake");
            conn.send_envelope(&FrpEnvelope::new(
                "EagleOne-X4K2",
                FrpEvent::shot_trigger(ShotKey::new(format!("shot-{n}"), n)),
            ))
            .expect("send");
        }
    });

    for n in 1..=2u32 {
        let mut ep = FrpEndpoint::open(EndpointConfig::new(
            Role::Controller,
            Transport::connect(format!("ws://{addr}/frp")),
        ))
        .expect("open");
        let mut conn = ep.establish().expect("controller handshake");
        let FrpMessage::Envelope(env) = conn.recv().expect("recv") else {
            panic!("expected envelope");
        };
        let FrpEvent::ShotTrigger { key } = &env.event else {
            panic!("expected shot_trigger");
        };
        assert_eq!(key.shot_number, n);
    }

    device.join().expect("device thread");
}

/// No mutually supported version: the device alerts and closes.
#[test]
fn incompatible_versions_are_rejected() {
    let ep = FrpEndpoint::open(
        EndpointConfig::new(Role::Device, Transport::listen("127.0.0.1:0"))
            .with_versions(&["9.9.9"]),
    )
    .expect("bind");
    let addr = ep
        .local_addr()
        .expect("addr")
        .expect("listening")
        .to_string();
    let mut device_ep = ep;

    let device = thread::spawn(move || {
        let err = device_ep.establish().expect_err("should reject");
        assert!(matches!(err, flightrelay::FrpError::Handshake(_)));
    });

    let mut controller_ep = FrpEndpoint::open(
        EndpointConfig::new(
            Role::Controller,
            Transport::connect(format!("ws://{addr}/frp")),
        )
        .with_versions(&["0.1.0"]),
    )
    .expect("open");

    let err = controller_ep.establish().expect_err("should be rejected");
    assert!(matches!(err, flightrelay::FrpError::Handshake(_)));

    device.join().expect("device thread");
}
