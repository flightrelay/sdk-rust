//! The endpoint and connection behavior the server relies on: stoppable
//! establish, upgrade timeouts, timed receives, and queued sends on a
//! non-blocking socket.

#![cfg(feature = "transport")]

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use flightrelay::{
    BusAlert, EndpointConfig, FrpEndpoint, FrpError, FrpMessage, Severity, Transport,
};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

fn listener(tweak: impl FnOnce(EndpointConfig) -> EndpointConfig) -> (FrpEndpoint, String) {
    let ep = FrpEndpoint::open(tweak(EndpointConfig::new(Transport::listen("127.0.0.1:0"))))
        .expect("bind");
    let addr = ep
        .local_addr()
        .expect("addr")
        .expect("listening")
        .to_string();
    (ep, addr)
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn dial(addr: &str) -> Ws {
    tungstenite::connect(format!("ws://{addr}/frp"))
        .expect("connect")
        .0
}

const SYNC: &str = r#"{"kind":"bus.sync","from":"c19e04b7a2d5f318"}"#;

#[test]
fn establish_until_returns_none_on_a_stop_and_leaves_the_endpoint_usable() {
    let (mut ep, addr) = listener(|c| c);
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
        let mut ws = dial(&addr);
        ws.send(Message::text(SYNC)).expect("send");
        ws
    });
    let mut conn = ep.establish().expect("establish after a stop");
    assert_eq!(conn.recv().expect("recv").kind(), "bus.sync");
    drop(peer.join().unwrap());
}

#[test]
fn establish_until_serves_a_connection_that_arrives() {
    let (mut ep, addr) = listener(|c| c);
    let stop = AtomicBool::new(false);
    let peer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        let mut ws = dial(&addr);
        ws.send(Message::text(SYNC)).expect("send");
        ws
    });
    let mut conn = ep
        .establish_until(&stop)
        .expect("no error")
        .expect("a connection");
    assert!(conn.peer_addr().is_some());
    assert_eq!(conn.recv().expect("recv").kind(), "bus.sync");
    drop(peer.join().unwrap());
}

#[test]
fn a_stopped_dial_gives_up_at_once() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/frp", taken.local_addr().unwrap());
    let mut ep = FrpEndpoint::open(EndpointConfig::new(Transport::connect(url))).unwrap();
    let stop = AtomicBool::new(true);
    let at = Instant::now();
    assert!(matches!(ep.establish_until(&stop), Ok(None)));
    assert!(at.elapsed() < Duration::from_secs(1));
}

#[test]
fn only_ws_urls_can_be_dialed() {
    let mut ep = FrpEndpoint::open(EndpointConfig::new(Transport::connect(
        "wss://127.0.0.1:1/frp",
    )))
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
fn a_silent_client_times_out_the_upgrade() {
    let (mut ep, addr) = listener(|c| c.with_handshake_timeout(Duration::from_millis(300)));
    let _silent = TcpStream::connect(&addr).unwrap();
    let at = Instant::now();
    assert!(timed_out(&ep.establish()));
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
}

#[test]
fn a_peer_that_never_answers_times_out_a_dial() {
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/frp", silent.local_addr().unwrap());
    let mut ep = FrpEndpoint::open(
        EndpointConfig::new(Transport::connect(url))
            .with_handshake_timeout(Duration::from_millis(300)),
    )
    .unwrap();
    let at = Instant::now();
    assert!(timed_out(&ep.establish()));
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
}

#[test]
fn the_upgrade_timeout_does_not_limit_the_connection() {
    let (mut ep, addr) = listener(|c| c.with_handshake_timeout(Duration::from_millis(200)));
    let peer = thread::spawn(move || {
        let mut ws = dial(&addr);
        thread::sleep(Duration::from_millis(500));
        ws.send(Message::text(SYNC)).unwrap();
        ws
    });
    let mut conn = ep.establish().expect("upgrade");
    thread::sleep(Duration::from_millis(100));
    // Blocks for longer than the upgrade timeout and still succeeds.
    let msg = conn.recv().expect("recv after the timeout window");
    assert_eq!(msg.kind(), "bus.sync");
    drop(peer.join().unwrap());
}

fn big_alert(i: usize) -> FrpMessage {
    FrpMessage::new(
        "7d2f9a41c03be856",
        BusAlert::new(Severity::Warn, format!("{i}:{}", "x".repeat(100_000))),
    )
}

#[test]
fn a_full_socket_queues_sends_until_the_write_buffer_limit() {
    let (mut ep, addr) = listener(|c| c.with_write_buffer_limit(512 * 1024));
    let peer = thread::spawn(move || dial(&addr)); // never reads
    let mut conn = ep.establish().expect("upgrade");
    let mut peer_ws = peer.join().unwrap();
    conn.set_nonblocking(true).unwrap();

    let mut sent = 0;
    let failure = loop {
        match conn.send(&big_alert(sent)) {
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
    let (mut ep, addr) = listener(|c| c);
    let peer = thread::spawn(move || dial(&addr));
    let conn = ep.establish().expect("upgrade");
    let _ws = peer.join().unwrap();
    conn.set_nonblocking(true).unwrap();
    let at = Instant::now();
    conn.close().expect("close");
    assert!(at.elapsed() < Duration::from_secs(1));
}

/// Connects a raw client to a fresh listener: the server's connection and
/// the client.
fn pair() -> (flightrelay::FrpConnection, Ws) {
    let (mut ep, addr) = listener(|c| c);
    let peer = thread::spawn(move || dial(&addr));
    let conn = ep.establish().expect("upgrade");
    (conn, peer.join().unwrap())
}

#[test]
fn recv_timeout_returns_none_in_time_and_then_the_next_message() {
    let (mut conn, mut ws) = pair();
    let at = Instant::now();
    assert!(
        conn.recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    let took = at.elapsed();
    assert!(
        took >= Duration::from_millis(90) && took < Duration::from_secs(2),
        "{took:?}"
    );
    assert!(conn.recv_timeout(Duration::ZERO).unwrap().is_none());

    ws.send(Message::text(SYNC)).expect("send");
    let msg = conn
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("the message");
    assert_eq!(msg.kind(), "bus.sync");

    // A plain `recv` still blocks without a limit afterwards.
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        ws.send(Message::text(SYNC)).expect("send");
        ws
    });
    assert_eq!(conn.recv().expect("recv").kind(), "bus.sync");
    drop(sender.join().unwrap());
}

#[test]
fn recv_timeout_keeps_a_message_that_arrives_in_parts() {
    let (mut conn, mut ws) = pair();
    // A masked text frame, written by hand in two parts. A zero mask key
    // leaves the payload as it is.
    let payload = SYNC.as_bytes();
    let mut frame = vec![
        0x81,
        0x80 | u8::try_from(payload.len()).unwrap(),
        0,
        0,
        0,
        0,
    ];
    frame.extend_from_slice(payload);
    let MaybeTlsStream::Plain(tcp) = ws.get_mut() else {
        panic!("plain");
    };
    let (first, rest) = frame.split_at(10);
    std::io::Write::write_all(tcp, first).unwrap();
    assert!(
        conn.recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    std::io::Write::write_all(tcp, rest).unwrap();
    let msg = conn
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("the whole message");
    assert_eq!(msg.kind(), "bus.sync");
}

#[test]
fn recv_timeout_reports_a_bad_message_and_a_close() {
    let (mut conn, mut ws) = pair();
    ws.send(Message::text("not json")).expect("send");
    assert!(matches!(
        conn.recv_timeout(Duration::from_secs(5)),
        Err(FrpError::Json(_))
    ));
    ws.close(None).expect("close");
    let _ = ws.flush();
    assert!(matches!(
        conn.recv_timeout(Duration::from_secs(5)),
        Err(FrpError::Closed)
    ));
}

#[test]
fn recv_timeout_returns_at_once_on_a_non_blocking_connection() {
    let (mut conn, _ws) = pair();
    conn.set_nonblocking(true).unwrap();
    let at = Instant::now();
    assert!(conn.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
    assert!(at.elapsed() < Duration::from_secs(1));
}
