//! The endpoint and connection behavior the server relies on: stoppable
//! establish, handshake timeouts, and queued sends on a non-blocking socket.

#![cfg(feature = "transport")]

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use flightrelay::{
    EndpointConfig, FrpEndpoint, FrpEnvelope, FrpError, FrpEvent, Role, Severity, Transport,
};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

fn listener(
    role: Role,
    tweak: impl FnOnce(EndpointConfig) -> EndpointConfig,
) -> (FrpEndpoint, String) {
    let ep = FrpEndpoint::open(tweak(EndpointConfig::new(
        role,
        Transport::listen("127.0.0.1:0"),
    )))
    .expect("bind");
    let addr = ep
        .local_addr()
        .expect("addr")
        .expect("listening")
        .to_string();
    (ep, addr)
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn dial(addr: &str, start: bool) -> Ws {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).expect("connect");
    if start {
        ws.send(Message::text(r#"{"kind":"start","version":["0.2.0"]}"#))
            .expect("start");
    }
    ws
}

#[test]
fn establish_until_returns_none_on_a_stop_and_leaves_the_endpoint_usable() {
    let (mut ep, addr) = listener(Role::Device, |c| c);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let waiter = thread::spawn(move || {
        let at = Instant::now();
        let result = ep.establish_until(&flag);
        (ep, result.map(|c| c.is_some()), at.elapsed())
    });
    thread::sleep(Duration::from_millis(150));
    stop.store(true, Ordering::SeqCst);
    let (mut ep, result, took) = waiter.join().unwrap();
    assert!(matches!(result, Ok(false)), "{result:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");

    // The listener is blocking again, so `establish` serves a connection.
    let peer = thread::spawn(move || {
        let mut ws = dial(&addr, true);
        ws.read().expect("init")
    });
    let conn = ep.establish().expect("establish after a stop");
    assert_eq!(conn.role(), Role::Device);
    assert!(peer.join().unwrap().is_text());
}

#[test]
fn establish_until_serves_a_connection_that_arrives() {
    let (mut ep, addr) = listener(Role::Device, |c| c);
    let stop = AtomicBool::new(false);
    let peer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        dial(&addr, true).read().expect("init")
    });
    let conn = ep
        .establish_until(&stop)
        .expect("no error")
        .expect("a connection");
    assert_eq!(conn.version(), flightrelay::SPEC_VERSION);
    assert!(peer.join().unwrap().is_text());
}

#[test]
fn a_stopped_dial_gives_up_at_once() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/frp", taken.local_addr().unwrap());
    let mut ep =
        FrpEndpoint::open(EndpointConfig::new(Role::Device, Transport::connect(url))).unwrap();
    let stop = AtomicBool::new(true);
    let at = Instant::now();
    assert!(matches!(ep.establish_until(&stop), Ok(None)));
    assert!(at.elapsed() < Duration::from_secs(1));
}

#[test]
fn only_ws_urls_can_be_dialed() {
    let mut ep = FrpEndpoint::open(EndpointConfig::new(
        Role::Device,
        Transport::connect("wss://127.0.0.1:1/frp"),
    ))
    .unwrap();
    let stop = AtomicBool::new(false);
    assert!(
        matches!(ep.establish_until(&stop), Err(FrpError::Handshake(m)) if m.contains("only ws://"))
    );
}

fn timed_out(result: &Result<flightrelay::FrpConnection, FrpError>) -> bool {
    matches!(result, Err(FrpError::Handshake(m)) if m == "handshake timed out")
}

#[test]
fn a_silent_client_times_out_the_handshake() {
    let (mut ep, addr) = listener(Role::Device, |c| {
        c.with_handshake_timeout(Duration::from_millis(300))
    });
    let _silent = TcpStream::connect(&addr).unwrap();
    let at = Instant::now();
    assert!(timed_out(&ep.establish()));
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
}

#[test]
fn a_client_that_upgrades_and_never_sends_start_times_out_too() {
    let (mut ep, addr) = listener(Role::Device, |c| {
        c.with_handshake_timeout(Duration::from_millis(300))
    });
    let upgraded = thread::spawn(move || {
        let ws = dial(&addr, false);
        thread::sleep(Duration::from_secs(2)); // holds the upgraded socket open
        drop(ws);
    });
    let at = Instant::now();
    assert!(timed_out(&ep.establish()));
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
    upgraded.join().unwrap();
}

#[test]
fn a_peer_that_never_answers_times_out_a_dialing_controller() {
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/frp", silent.local_addr().unwrap());
    let mut ep = FrpEndpoint::open(
        EndpointConfig::new(Role::Controller, Transport::connect(url))
            .with_handshake_timeout(Duration::from_millis(300)),
    )
    .unwrap();
    let at = Instant::now();
    assert!(timed_out(&ep.establish()));
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
}

#[test]
fn the_handshake_timeout_does_not_limit_the_connection() {
    let (mut ep, addr) = listener(Role::Device, |c| {
        c.with_handshake_timeout(Duration::from_millis(200))
    });
    let peer = thread::spawn(move || {
        let mut ws = dial(&addr, true);
        let _init = ws.read().expect("init");
        thread::sleep(Duration::from_millis(500));
        ws.send(Message::text(
            r#"{"kind":"set_detection_mode","club":"PW"}"#,
        ))
        .unwrap();
        ws
    });
    let mut conn = ep.establish().expect("handshake");
    thread::sleep(Duration::from_millis(100));
    // Blocks for longer than the handshake timeout and still succeeds.
    let msg = conn.recv().expect("recv after the timeout window");
    assert!(matches!(msg, flightrelay::FrpMessage::Protocol(_)));
    drop(peer.join().unwrap());
}

fn big_alert(i: usize) -> FrpEnvelope {
    FrpEnvelope::new(
        "EagleOne-X4K2",
        FrpEvent::Alert {
            severity: Severity::Warn,
            message: format!("{i}:{}", "x".repeat(100_000)),
        },
    )
}

#[test]
fn a_full_socket_queues_sends_until_the_write_buffer_limit() {
    let (mut ep, addr) = listener(Role::Device, |c| c.with_write_buffer_limit(512 * 1024));
    let peer = thread::spawn(move || {
        let mut ws = dial(&addr, true);
        let _init = ws.read().expect("init");
        ws // does not read again
    });
    let mut conn = ep.establish().expect("handshake");
    let mut peer_ws = peer.join().unwrap();
    conn.set_nonblocking(true).unwrap();

    let mut sent = 0;
    let failure = loop {
        match conn.send_envelope(&big_alert(sent)) {
            Ok(()) => sent += 1,
            Err(e) => break e,
        }
        assert!(sent < 1000, "the cap never tripped");
    };
    assert!(
        matches!(&failure, FrpError::WebSocket(e) if matches!(**e, tungstenite::Error::WriteBufferFull(_))),
        "{failure:?}"
    );
    // Past what the kernel holds, the rest was queued, not refused.
    assert!(sent >= 5, "only {sent} sends before the cap");
    // Flushing while the peer still reads nothing is not an error.
    conn.flush().expect("flush on a full socket");

    // Once the peer reads, the queue drains.
    let drained = thread::spawn(move || {
        let mut count = 0;
        loop {
            match peer_ws.read() {
                Ok(Message::Text(_)) => count += 1,
                Ok(_) => {}
                Err(_) => return count,
            }
            if count == sent {
                return count;
            }
        }
    });
    let give_up = Instant::now() + Duration::from_secs(10);
    while !drained.is_finished() {
        conn.flush().expect("flush");
        assert!(Instant::now() < give_up, "the queue never drained");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        drained.join().unwrap(),
        sent,
        "every queued message arrived, in full"
    );
}

#[test]
fn closing_a_non_blocking_connection_does_not_wait_for_the_peer() {
    let (mut ep, addr) = listener(Role::Device, |c| c);
    let peer = thread::spawn(move || {
        let mut ws = dial(&addr, true);
        let _init = ws.read().expect("init");
        ws
    });
    let conn = ep.establish().expect("handshake");
    let _ws = peer.join().unwrap();
    conn.set_nonblocking(true).unwrap();
    let at = Instant::now();
    conn.close().expect("close");
    assert!(at.elapsed() < Duration::from_secs(1));
}
