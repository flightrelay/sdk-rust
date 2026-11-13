//! Both transport directions, greeting, and a hub forwarding between peers.
//!
//! There is no handshake beyond the WebSocket upgrade: on connecting, each
//! side sends `bus.actor` for each actor it hosts, then the state they own,
//! then `bus.sync`, whichever side opened the socket.

#![cfg(feature = "transport")]

use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use flightrelay::{
    BusActor, Club, EndpointConfig, FrpBody, FrpEndpoint, FrpError, FrpMessage, GolferTurn,
    Handedness, Hub, HubEvent, Launch, Mode, PeerId, ShotKey, ShotLaunch, Transport, Velocity,
};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const LM: &str = "7d2f9a41c03be856";
const CLUBHOUSE: &str = "c19e04b7a2d5f318";
const GAME: &str = "5b80e2f7d14c9a63";
/// The flooding peer and the reading peer of the flood tests.
const B: &str = GAME;
const C: &str = CLUBHOUSE;

/// Bind an endpoint on an ephemeral port and report the address it got.
fn listener() -> (FrpEndpoint, String) {
    let ep =
        FrpEndpoint::open(EndpointConfig::new(Transport::listen("127.0.0.1:0"))).expect("bind");
    let addr = ep
        .local_addr()
        .expect("addr")
        .expect("listening")
        .to_string();
    (ep, addr)
}

fn dialer(addr: &str) -> FrpEndpoint {
    FrpEndpoint::open(EndpointConfig::new(Transport::connect(format!(
        "ws://{addr}/frp"
    ))))
    .expect("open")
}

fn actor(from: &str, name: &str) -> FrpMessage {
    FrpMessage::new(from, BusActor::new(name, 2.0))
}

fn shot() -> FrpMessage {
    FrpMessage::new(
        LM,
        ShotLaunch {
            key: ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 7),
            launch: Launch {
                launch_speed: Some(Velocity::MetersPerSecond(67.2)),
                ..Default::default()
            },
        },
    )
}

/// Both sides greet; each sees the other's `bus.actor`, then `bus.sync`.
fn greet_both_ways(mut listening: FrpEndpoint, addr: &str) {
    let side = thread::spawn(move || {
        let mut conn = listening.establish().expect("accept");
        conn.greet(&[actor(LM, "Eagle One X4K2")], &[])
            .expect("greet");
        let first = conn.recv().expect("recv");
        let second = conn.recv().expect("recv");
        conn.send(&shot()).expect("send");
        (first, second, conn)
    });
    let mut conn = dialer(addr).establish().expect("dial");
    conn.greet(
        &[actor(CLUBHOUSE, "Clubhouse"), actor(GAME, "Back Nine Golf")],
        &[],
    )
    .expect("greet");
    let got: Vec<FrpMessage> = (0..3).map(|_| conn.recv().expect("recv")).collect();
    assert_eq!(got[0], actor(LM, "Eagle One X4K2"));
    assert_eq!(got[1].kind(), "bus.sync");
    assert_eq!(got[1].from, LM);
    assert_eq!(got[2], shot());

    let (first, second, _conn) = side.join().expect("listening side");
    assert_eq!(first, actor(CLUBHOUSE, "Clubhouse"));
    assert_eq!(second, actor(GAME, "Back Nine Golf"));
}

#[test]
fn the_dialer_and_the_listener_greet_alike() {
    let (ep, addr) = listener();
    greet_both_ways(ep, &addr);
}

#[test]
fn a_greeting_sends_actors_then_state_then_one_sync_from_the_first_actor() {
    let (mut ep, addr) = listener();
    let side = thread::spawn(move || {
        let mut conn = ep.establish().expect("accept");
        (0..4)
            .map(|_| conn.recv().expect("recv"))
            .collect::<Vec<_>>()
    });
    let turn = FrpMessage::new(
        CLUBHOUSE,
        GolferTurn::new(None, Some(Club::Iron7), Handedness::Right, Mode::Full),
    );
    let mut conn = dialer(&addr).establish().expect("dial");
    conn.greet(
        &[actor(CLUBHOUSE, "Clubhouse"), actor(GAME, "Back Nine Golf")],
        std::slice::from_ref(&turn),
    )
    .expect("greet");
    let got = side.join().unwrap();
    assert_eq!(
        got.iter().map(FrpMessage::kind).collect::<Vec<_>>(),
        ["bus.actor", "bus.actor", "golfer.turn", "bus.sync"]
    );
    assert_eq!(got[2], turn);
    assert_eq!(got[3].from, CLUBHOUSE);
    conn.greet(&[], std::slice::from_ref(&turn))
        .expect("a greeting without actors sends nothing");
}

#[test]
fn endpoint_establishes_successive_connections() {
    let (mut ep, addr) = listener();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let mut conn = ep.establish().expect("accept");
            conn.send(&shot()).expect("send");
            let _ = conn.recv();
        }
    });
    for _ in 0..2 {
        let mut conn = dialer(&addr).establish().expect("dial");
        assert_eq!(conn.recv().expect("recv"), shot());
        let _ = conn.close(); // the server may drop its end first
    }
    server.join().unwrap();
}

#[test]
fn a_connection_keeps_reading_after_text_that_is_not_frp() {
    let (mut ep, addr) = listener();
    let peer = thread::spawn(move || {
        let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).expect("connect");
        for text in [
            "not json",
            r#"{"device":"old","event":{"kind":"launch"}}"#,
            r#"{"kind":"x.birdielabs.glow","from":"7d2f9a41c03be856","hue":120}"#,
        ] {
            ws.send(Message::text(text)).unwrap();
        }
        ws.send(Message::text(shot().to_json().unwrap())).unwrap();
        ws
    });
    let mut conn = ep.establish().expect("accept");
    assert!(matches!(conn.recv(), Err(FrpError::Json(_))));
    assert!(
        matches!(conn.recv(), Err(FrpError::Json(_))),
        "an object without kind and from is not FRP"
    );
    let ext = conn.recv().expect("an extension parses");
    assert!(matches!(&ext.body, FrpBody::Unknown(u) if u.is_extension()));
    assert_eq!(conn.recv().expect("recv"), shot());
    drop(peer.join().unwrap());
}

// ------------------------------------------------------------------ hub

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// A hub with `n` raw peers connected to it, all on the calling thread.
fn hub_with_peers(n: usize) -> (Hub, Vec<Ws>) {
    let (mut ep, addr) = listener();
    let dial = thread::spawn(move || {
        (0..n)
            .map(|_| tungstenite::connect(format!("ws://{addr}/frp")).unwrap().0)
            .collect::<Vec<Ws>>()
    });
    let mut hub = Hub::new();
    for _ in 0..n {
        hub.add(ep.establish().expect("accept")).unwrap();
    }
    (hub, dial.join().unwrap())
}

/// The text frames `ws` reads up to and including `last`.
fn read_through(ws: &mut Ws, last: &str) -> Vec<String> {
    let mut got = Vec::new();
    loop {
        if let Message::Text(text) = ws.read().unwrap() {
            got.push(text.to_string());
            if text.as_str() == last {
                return got;
            }
        }
    }
}

#[test]
fn a_hub_forwards_to_every_other_peer_and_never_back() {
    let (mut hub, mut peers) = hub_with_peers(3);
    let from_a = shot().to_json().unwrap();
    // An extension with fields nobody here knows travels unchanged.
    let from_b =
        r#"{"kind":"x.birdielabs.glow","from":"5b80e2f7d14c9a63","hue":120,"extra":[1,2]}"#;
    peers[0].send(Message::text(from_a.clone())).unwrap();
    peers[1].send(Message::text(from_b)).unwrap();
    peers[1].send(Message::text("not frp")).unwrap();
    let mut seen = Vec::new();
    let give_up = Instant::now() + Duration::from_secs(10);
    while seen.len() < 3 {
        assert!(Instant::now() < give_up, "saw only {seen:?}");
        for event in hub.poll() {
            seen.push(match event {
                HubEvent::Message { peer, message } => format!("{peer}: {}", message.kind()),
                HubEvent::Invalid { peer, .. } => format!("{peer}: invalid"),
                HubEvent::Left { peer, .. } => format!("{peer}: left"),
            });
        }
    }
    assert_eq!(
        seen,
        [
            "peer 0: shot.launch",
            "peer 1: x.birdielabs.glow",
            "peer 1: invalid"
        ]
    );
    // The hub's own message goes to everyone after what it forwarded.
    let mark = actor(CLUBHOUSE, "Clubhouse");
    assert!(hub.send(&mark).unwrap().is_empty());
    let mark = mark.to_json().unwrap();
    let got: Vec<Vec<String>> = peers.iter_mut().map(|p| read_through(p, &mark)).collect();
    assert_eq!(
        got[0],
        [from_b.to_owned(), mark.clone()],
        "a gets b's, not its own"
    );
    assert_eq!(
        got[1],
        [from_a.clone(), mark.clone()],
        "b gets a's, not its own"
    );
    assert_eq!(
        got[2],
        [from_a, from_b.to_owned(), mark],
        "c gets both, in order"
    );
}

#[test]
fn a_hub_publishes_its_own_messages_to_everyone_and_reports_leavers() {
    let (mut ep, addr) = listener();
    let accept = thread::spawn(move || {
        let a = ep.establish().unwrap();
        let b = ep.establish().unwrap();
        (a, b)
    });
    let mut wa = tungstenite::connect(format!("ws://{addr}/frp")).unwrap().0;
    let wb = tungstenite::connect(format!("ws://{addr}/frp")).unwrap().0;
    let (a, b) = accept.join().unwrap();
    let mut hub = Hub::new();
    let pa = hub.add(a).unwrap();
    let pb = hub.add(b).unwrap();
    assert_eq!(hub.len(), 2);
    assert!(hub.send(&shot()).unwrap().is_empty());
    assert!(
        hub.send_to(pa, &actor(CLUBHOUSE, "Clubhouse"))
            .unwrap()
            .is_none()
    );
    let mut got = Vec::new();
    let give_up = Instant::now() + Duration::from_secs(5);
    while got.len() < 2 {
        assert!(Instant::now() < give_up);
        let _ = hub.poll();
        if let Message::Text(t) = wa.read().unwrap() {
            got.push(FrpMessage::parse(&t).unwrap());
        }
    }
    assert_eq!(got, [shot(), actor(CLUBHOUSE, "Clubhouse")]);

    drop(wb);
    let give_up = Instant::now() + Duration::from_secs(5);
    let left = loop {
        assert!(Instant::now() < give_up, "the leaver was never reported");
        let events = hub.poll();
        if let Some(HubEvent::Left { peer, addr, .. }) = events
            .into_iter()
            .find(|e| matches!(e, HubEvent::Left { .. }))
        {
            assert!(addr.is_some());
            break peer;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(left, pb);
    assert_eq!(hub.peers().map(|(p, _)| p).collect::<Vec<_>>(), [pa]);
    assert!(hub.remove(pa).is_some());
    assert!(hub.is_empty());
}

/// An extension message from `from` padded to about `pad` bytes.
fn padded(from: &str, pad: usize, n: u64) -> String {
    format!(
        r#"{{"kind":"x.birdielabs.pad","from":"{from}","n":{n},"pad":"{}"}}"#,
        "a".repeat(pad)
    )
}

/// Whether a non-blocking socket call only found the socket busy.
fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

/// What the hub reported during a flood from B (and C).
#[derive(Default)]
struct Tally {
    left: Option<PeerId>,
    b_messages: u64,
    from_c_after_left: u64,
    misattributed: Vec<String>,
}

impl Tally {
    fn poll(&mut self, hub: &mut Hub, pa: PeerId, pb: PeerId) {
        for event in hub.poll() {
            match event {
                HubEvent::Message { peer, message } => {
                    let from = if peer == pb { B } else { C };
                    if message.from != from {
                        self.misattributed.push(format!("{peer}: {}", message.from));
                    }
                    if peer == pb {
                        self.b_messages += 1;
                    } else if self.left.is_some() {
                        self.from_c_after_left += 1;
                    }
                }
                HubEvent::Left { peer, .. } => {
                    assert_eq!(peer, pa, "only the stalled peer is dropped");
                    self.left = Some(peer);
                }
                HubEvent::Invalid { .. } => panic!("everything sent is FRP"),
            }
        }
    }
}

/// Peer A stops reading while peer B floods the hub with 64 KB messages
/// (and, with `with_c`, peer C sends small messages and reads everything).
/// With a write buffer limit far under the flood, the hub drops A in the
/// middle of forwarding B's messages; with one far over it, nobody is
/// dropped. The hub must not panic, must attribute every message to the peer
/// that sent it, and must never echo a message to its source.
///
/// Everything runs on the test thread, so the order is fixed: each round B
/// sends one message, C sends one, and the hub and C's reads alternate until
/// C has every B message the hub has reported. C's queue at the hub never
/// holds more than a message or two; A's grows by one per round.
// One scenario, read top to bottom: splitting it would scatter the setup.
#[allow(clippy::too_many_lines)]
fn a_flood_past_a_stalled_peer(with_c: bool, limit: usize) {
    // A hang guard: A is dropped long before this on any kernel.
    const ROUNDS: u64 = 2000;
    let dropping = limit < 16 * 1024 * 1024;
    let total_b = if dropping { ROUNDS } else { 160 };
    let n = if with_c { 3 } else { 2 };
    let (mut ep, addr) = {
        let ep = FrpEndpoint::open(
            EndpointConfig::new(Transport::listen("127.0.0.1:0")).with_write_buffer_limit(limit),
        )
        .expect("listen");
        let addr = ep.local_addr().unwrap().unwrap().to_string();
        (ep, addr)
    };
    let mut clients = Vec::new();
    let mut hub = Hub::new();
    let mut ids = Vec::new();
    for _ in 0..n {
        let dial = {
            let addr = addr.clone();
            thread::spawn(move || tungstenite::connect(format!("ws://{addr}/frp")).unwrap().0)
        };
        let conn = ep.establish().expect("accept");
        let ws: Ws = dial.join().unwrap();
        let MaybeTlsStream::Plain(tcp) = ws.get_ref() else {
            unreachable!("ws:// is plain TCP")
        };
        tcp.set_nonblocking(true).unwrap();
        clients.push(ws);
        ids.push(hub.add(conn).unwrap());
    }
    let (pa, pb) = (ids[0], ids[1]);
    let mut clients = clients.into_iter();
    // A never reads.
    let _stalled = clients.next().unwrap();
    let mut wb = clients.next().unwrap();
    let mut wc = clients.next();

    let mut seen = Tally::default();
    let (mut b_sent, mut c_got_b, mut echoed) = (0, 0, 0);
    for round in 0..ROUNDS * 2 {
        let done = if dropping {
            seen.left.is_some() && (!with_c || seen.from_c_after_left > 0)
        } else {
            seen.b_messages == total_b
        };
        if done {
            break;
        }
        assert!(round < ROUNDS * 2 - 1, "never done");
        // A send that finds the socket full stays queued in B's buffer.
        if b_sent < total_b {
            match wb.send(Message::text(padded(B, 64 * 1024, b_sent))) {
                Err(e) if !would_block(&e) => panic!("b: {e}"),
                _ => b_sent += 1,
            }
        } else if let Err(e) = wb.flush() {
            assert!(would_block(&e), "b: {e}");
        }
        if let Some(wc) = &mut wc {
            match wc.send(Message::text(padded(C, 16, round))) {
                Err(e) if !would_block(&e) => panic!("c: {e}"),
                _ => {}
            }
        }
        seen.poll(&mut hub, pa, pb);
        let Some(wc) = &mut wc else { continue };
        // Drain C completely: everything the hub has taken from B so far.
        while c_got_b < seen.b_messages {
            match wc.read() {
                Ok(Message::Text(text)) => match FrpMessage::parse(&text).unwrap().from.as_str() {
                    B => c_got_b += 1,
                    _ => echoed += 1,
                },
                Ok(_) => {}
                Err(e) if would_block(&e) => seen.poll(&mut hub, pa, pb),
                Err(e) => panic!("c: {e}"),
            }
        }
    }
    assert!(seen.misattributed.is_empty(), "{:?}", seen.misattributed);
    assert_eq!(echoed, 0, "c never gets its own messages back");
    if dropping {
        assert_eq!(seen.left, Some(pa));
        assert_eq!(hub.peers().map(|(p, _)| p).collect::<Vec<_>>(), ids[1..]);
        assert!(
            !with_c || seen.from_c_after_left > 0,
            "c is still served after a left"
        );
    } else {
        assert_eq!(seen.left, None, "every queue stays under the limit");
        assert_eq!(hub.len(), n);
    }
}

#[test]
fn a_stalled_peer_is_dropped_mid_forward_with_two_peers() {
    a_flood_past_a_stalled_peer(false, 1024 * 1024);
}

#[test]
fn a_stalled_peer_is_dropped_mid_forward_with_three_peers() {
    a_flood_past_a_stalled_peer(true, 1024 * 1024);
}

#[test]
fn a_slow_peer_under_a_large_limit_is_kept() {
    a_flood_past_a_stalled_peer(true, 256 * 1024 * 1024);
}
