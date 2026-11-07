//! Both transport directions, for both roles.
//!
//! FRP roles are independent of transport direction: the handshake always runs
//! controller→device (`start`) and device→controller (`init`), whichever side
//! opened the socket.

#![cfg(feature = "transport")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use flightrelay::{
    EndpointConfig, FrpEndpoint, FrpEnvelope, FrpEvent, FrpMessage, FrpProtocolMessage, Launch,
    RawDir, RawFrame, Role, Severity, ShotKey, Transport, Velocity,
};
use tungstenite::{Message, WebSocket};

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
        FrpEvent::launch(
            ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 7),
            Launch {
                launch_speed: Some(Velocity::MetersPerSecond(67.2)),
                launch_elevation: Some(14.2),
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
    let FrpEvent::Launch { key, launch } = &env.event else {
        panic!("expected launch, got {:?}", env.event);
    };
    assert_eq!(key.shot_number, 7);
    assert_eq!(launch.launch_elevation, Some(14.2));
}

/// Standalone FRP: the device listens, the controller dials.
#[test]
fn device_listens_controller_dials() {
    let (mut device_ep, addr) = listener(Role::Device);

    let device = thread::spawn(move || {
        let mut conn = device_ep.establish().expect("device handshake");
        assert_eq!(conn.role(), Role::Device);
        assert_eq!(conn.peer_name(), Some("test-controller"));
        assert!(!conn.raw_frames_requested());
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
        let FrpEvent::ShotTrigger { key, .. } = &env.event else {
            panic!("expected shot_trigger");
        };
        assert_eq!(key.shot_number, n);
    }

    device.join().expect("device thread");
}

/// The next text frame from a raw WebSocket peer.
fn read_text<S: Read + Write>(socket: &mut WebSocket<S>) -> String {
    loop {
        if let Message::Text(text) = socket.read().expect("read") {
            return text.to_string();
        }
    }
}

fn critical(message: &str) -> FrpMessage {
    FrpMessage::Protocol(FrpProtocolMessage::Alert {
        severity: Severity::Critical,
        message: message.into(),
    })
}

/// A controller that doesn't propose 0.2.0: the device alerts and closes.
#[test]
fn device_rejects_a_controller_without_its_version() {
    let (mut device_ep, addr) = listener(Role::Device);

    let device = thread::spawn(move || {
        let err = device_ep.establish().expect_err("should reject");
        assert!(matches!(err, flightrelay::FrpError::Handshake(_)));
    });

    let (mut socket, _) = tungstenite::connect(format!("ws://{addr}/frp")).expect("connect");
    socket
        .send(Message::text(
            r#"{"kind":"start","version":["0.1.0"],"name":"old controller"}"#,
        ))
        .expect("send");
    let reply = FrpMessage::parse(&read_text(&mut socket)).expect("parse");
    assert_eq!(reply, critical("No compatible FRP version"));

    device.join().expect("device thread");
}

/// The controller proposes only 0.2.0, and a device that answers with
/// another version is refused.
#[test]
fn controller_rejects_a_device_with_another_version() {
    let tcp = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = tcp.local_addr().expect("addr");

    let device = thread::spawn(move || {
        let (stream, _) = tcp.accept().expect("accept");
        let mut socket = tungstenite::accept(stream).expect("websocket");
        let start = read_text(&mut socket);
        // A controller that didn't ask for raw frames leaves the flag out.
        assert!(!start.contains("raw_frames"), "{start}");
        assert_eq!(
            FrpMessage::parse(&start).expect("parse"),
            FrpMessage::Protocol(FrpProtocolMessage::Start {
                version: vec!["0.2.0".into()],
                name: Some("anonymous".into()),
                raw_frames: false,
            })
        );
        socket
            .send(Message::text(r#"{"kind":"init","version":"0.1.0"}"#))
            .expect("send");
        let reply = FrpMessage::parse(&read_text(&mut socket)).expect("parse");
        assert_eq!(reply, critical(r#"Unsupported FRP version "0.1.0""#));
    });

    let mut controller_ep = FrpEndpoint::open(EndpointConfig::new(
        Role::Controller,
        Transport::connect(format!("ws://{addr}/frp")),
    ))
    .expect("open");
    let err = controller_ep.establish().expect_err("should be rejected");
    assert!(matches!(err, flightrelay::FrpError::Handshake(_)));

    device.join().expect("device thread");
}

/// A message the spec has a controller ignore doesn't end the connection: a
/// `flight_result` without a recognized `basis` arrives as an `Unknown` event,
/// invalid JSON as `FrpError::Json`, and the next message still arrives.
#[test]
fn controller_keeps_reading_after_a_message_it_ignores() {
    let tcp = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = tcp.local_addr().expect("addr");

    let device = thread::spawn(move || {
        let (stream, _) = tcp.accept().expect("accept");
        let mut socket = tungstenite::accept(stream).expect("websocket");
        read_text(&mut socket); // start
        for text in [
            r#"{"kind":"init","version":"0.2.0"}"#,
            r#"{"device":"EagleOne-X4K2","event":{"kind":"flight_result","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":7},"flight":{"basis":"estimated","carry_distance":"180.5m"}}}"#,
            "{not json",
        ] {
            socket.send(Message::text(text)).expect("send");
        }
        socket
            .send(Message::text(serde_json::to_string(&shot()).expect("json")))
            .expect("send");
        read_text(&mut socket); // wait for the controller to finish reading
    });

    let mut controller_ep = FrpEndpoint::open(EndpointConfig::new(
        Role::Controller,
        Transport::connect(format!("ws://{addr}/frp")),
    ))
    .expect("open");
    let mut conn = controller_ep.establish().expect("controller handshake");

    assert_eq!(
        conn.recv().expect("ignored event"),
        FrpMessage::Envelope(FrpEnvelope::new("EagleOne-X4K2", FrpEvent::Unknown))
    );
    assert!(matches!(conn.recv(), Err(flightrelay::FrpError::Json(_))));
    assert_is_our_shot(&conn.recv().expect("recv after ignored messages"));

    conn.send_protocol(&FrpProtocolMessage::Alert {
        severity: Severity::Warn,
        message: "done".into(),
    })
    .expect("send");
    device.join().expect("device thread");
}

/// A controller that asks for raw frames in `start`: the device sees the
/// request and its raw frames arrive in order with its other events.
#[test]
fn device_sees_a_raw_frames_request() {
    let (mut device_ep, addr) = listener(Role::Device);

    let device = thread::spawn(move || {
        let mut conn = device_ep.establish().expect("device handshake");
        assert!(conn.raw_frames_requested());
        let frame = RawFrame {
            key: Some(ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 7)),
            ..RawFrame::new(RawDir::Rx, 1, vec![0x0a, 0x1b])
        };
        conn.send_envelope(&FrpEnvelope::new(
            "EagleOne-X4K2",
            FrpEvent::raw_frame(frame),
        ))
        .expect("send");
        conn.send_envelope(&shot()).expect("send");
    });

    let mut controller_ep = FrpEndpoint::open(
        EndpointConfig::new(
            Role::Controller,
            Transport::connect(format!("ws://{addr}/frp")),
        )
        .with_raw_frames(true),
    )
    .expect("open");
    let mut conn = controller_ep.establish().expect("controller handshake");
    assert!(conn.raw_frames_requested());

    let FrpMessage::Envelope(env) = conn.recv().expect("raw frame") else {
        panic!("expected envelope");
    };
    let FrpEvent::RawFrame(frame) = &env.event else {
        panic!("expected raw_frame, got {:?}", env.event);
    };
    assert_eq!((frame.dir, frame.seq), (RawDir::Rx, 1));
    assert_eq!(frame.data, [0x0a, 0x1b]);
    assert_is_our_shot(&conn.recv().expect("shot after the frame"));

    device.join().expect("device thread");
}
