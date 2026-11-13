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
    BusActor, Club, EndpointConfig, FrpBody, FrpConnection, FrpEndpoint, FrpError, FrpMessage,
    GolferTurn, Handedness, Hub, HubEvent, Launch, Mode, ShotKey, ShotLaunch, Transport, Velocity,
};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const LM: &str = "7d2f9a41c03be856";
const CLUBHOUSE: &str = "c19e04b7a2d5f318";
const GAME: &str = "5b80e2f7d14c9a63";

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

/// A hub on its own thread with `n` raw peers connected to it.
struct Rig {
    peers: Vec<Ws>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hub: thread::JoinHandle<Vec<String>>,
}

fn rig(n: usize) -> Rig {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (mut ep, addr) = listener();
    let accepted = thread::spawn(move || {
        (0..n)
            .map(|_| ep.establish().expect("accept"))
            .collect::<Vec<FrpConnection>>()
    });
    let peers: Vec<Ws> = (0..n)
        .map(|_| {
            let (ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).expect("connect");
            if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
                tcp.set_read_timeout(Some(Duration::from_millis(300)))
                    .unwrap();
            }
            ws
        })
        .collect();
    let conns = accepted.join().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let hub = thread::spawn(move || {
        let mut hub = Hub::new();
        for conn in conns {
            hub.add(conn).unwrap();
        }
        let mut seen = Vec::new();
        while !flag.load(Ordering::SeqCst) {
            for event in hub.poll() {
                match event {
                    HubEvent::Message { peer, message } => {
                        seen.push(format!("{peer}: {}", message.kind()));
                    }
                    HubEvent::Invalid { peer, .. } => seen.push(format!("{peer}: invalid")),
                    HubEvent::Left { peer, .. } => seen.push(format!("{peer}: left")),
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        seen
    });
    Rig { peers, stop, hub }
}

impl Rig {
    fn finish(self) -> Vec<String> {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(self.peers);
        self.hub.join().unwrap()
    }
}

/// Every text frame `ws` receives within its read timeout.
fn drain(ws: &mut Ws) -> Vec<String> {
    let mut got = Vec::new();
    while let Ok(msg) = ws.read() {
        if let Message::Text(text) = msg {
            got.push(text.to_string());
        }
    }
    got
}

#[test]
fn a_hub_forwards_to_every_other_peer_and_never_back() {
    let mut rig = rig(3);
    let from_a = shot().to_json().unwrap();
    // An extension with fields nobody here knows travels unchanged.
    let from_b =
        r#"{"kind":"x.birdielabs.glow","from":"5b80e2f7d14c9a63","hue":120,"extra":[1,2]}"#;
    rig.peers[0].send(Message::text(from_a.clone())).unwrap();
    rig.peers[1].send(Message::text(from_b)).unwrap();
    rig.peers[1].send(Message::text("not frp")).unwrap();
    thread::sleep(Duration::from_millis(100));
    let got: Vec<Vec<String>> = rig.peers.iter_mut().map(drain).collect();
    assert_eq!(got[0], [from_b.to_owned()], "a gets b's, not its own");
    assert_eq!(
        got[1],
        std::slice::from_ref(&from_a),
        "b gets a's, not its own"
    );
    assert_eq!(got[2], [from_a, from_b.to_owned()], "c gets both, in order");
    let seen = rig.finish();
    assert_eq!(
        seen[..3],
        [
            "peer 0: shot.launch",
            "peer 1: x.birdielabs.glow",
            "peer 1: invalid"
        ]
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

/// Peer A stops reading while peer B floods the hub (and, with `with_c`, peer
/// C sends small messages and reads everything). The hub drops A once its
/// queue passes the write buffer limit, in the middle of forwarding B's
/// messages. The limit holds 16 of B's messages, so C, which reads
/// everything, never fills its queue on a slow machine. The hub must not panic, must attribute every message to the
/// peer that sent it, and must never echo a message to its source.
// One scenario, read top to bottom: splitting it would scatter the setup.
#[allow(clippy::too_many_lines)]
fn a_stalled_peer_is_dropped_mid_forward(with_c: bool) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const B: &str = "5b80e2f7d14c9a63";
    const C: &str = "c19e04b7a2d5f318";
    let n = if with_c { 3 } else { 2 };
    let (mut ep, addr) = {
        let ep = FrpEndpoint::open(
            EndpointConfig::new(Transport::listen("127.0.0.1:0"))
                .with_write_buffer_limit(1024 * 1024),
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
        clients.push(dial.join().unwrap());
        ids.push(hub.add(conn).unwrap());
    }
    let (pa, pb) = (ids[0], ids[1]);
    let mut clients = clients.into_iter();
    // A never reads.
    let _stalled = clients.next().unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    let mut wb = clients.next().unwrap();
    let flag = Arc::clone(&stop);
    let b = thread::spawn(move || {
        let mut sent = 0;
        while !flag.load(Ordering::SeqCst) && sent < 2000 {
            if wb.send(Message::text(padded(B, 64 * 1024, sent))).is_err() {
                break;
            }
            sent += 1;
        }
        wb
    });

    // C writes and reads on two threads over one socket, so it always has
    // messages waiting at the hub while it reads everything forwarded to it.
    let c = clients.next().map(|mut wc| {
        let MaybeTlsStream::Plain(tcp) = wc.get_ref() else {
            unreachable!("ws:// is plain TCP")
        };
        tcp.set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let mut reader = WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(tcp.try_clone().unwrap()),
            tungstenite::protocol::Role::Client,
            None,
        );
        let flag = Arc::clone(&stop);
        let writer = thread::spawn(move || {
            let mut sent = 0;
            while !flag.load(Ordering::SeqCst) {
                if wc.send(Message::text(padded(C, 16, sent))).is_err() {
                    break;
                }
                sent += 1;
                thread::sleep(Duration::from_micros(200));
            }
            sent
        });
        let flag = Arc::clone(&stop);
        let reader = thread::spawn(move || {
            let mut echoed = 0;
            while !flag.load(Ordering::SeqCst) {
                match reader.read() {
                    Ok(Message::Text(text)) => {
                        if FrpMessage::parse(&text).is_ok_and(|m| m.from == C) {
                            echoed += 1;
                        }
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
            echoed
        });
        (writer, reader)
    });

    let from_of = |peer| if peer == pb { B } else { C };
    let mut misattributed = Vec::new();
    let mut left = None;
    let mut seen_from_c_after = 0;
    let give_up = Instant::now() + Duration::from_secs(30);
    let mut after = None;
    while after.is_none_or(|t: Instant| t.elapsed() < Duration::from_millis(300)) {
        assert!(Instant::now() < give_up, "a was never dropped");
        for event in hub.poll() {
            match event {
                HubEvent::Message { peer, message } => {
                    if message.from != from_of(peer) {
                        misattributed.push(format!("{peer}: {}", message.from));
                    }
                    if left.is_some() && peer != pb {
                        seen_from_c_after += 1;
                    }
                }
                HubEvent::Left { peer, .. } => {
                    assert_eq!(peer, pa, "only the stalled peer is dropped");
                    left = Some(peer);
                    after.get_or_insert_with(Instant::now);
                }
                HubEvent::Invalid { .. } => panic!("everything sent is FRP"),
            }
        }
    }
    assert_eq!(hub.peers().map(|(p, _)| p).collect::<Vec<_>>(), ids[1..]);
    stop.store(true, Ordering::SeqCst);
    // Keep serving B and C until their threads end, so neither blocks.
    while !b.is_finished()
        || c.as_ref()
            .is_some_and(|(w, r)| !w.is_finished() || !r.is_finished())
    {
        let _ = hub.poll();
        thread::sleep(Duration::from_millis(1));
    }
    let _wb = b.join().unwrap();
    assert_eq!(left, Some(pa));
    assert!(misattributed.is_empty(), "{misattributed:?}");
    if let Some((writer, reader)) = c {
        let (sent, echoed) = (writer.join().unwrap(), reader.join().unwrap());
        assert!(sent > 0);
        assert_eq!(echoed, 0, "c never gets its own messages back");
        assert!(seen_from_c_after > 0, "c is still served after a left");
    }
}

#[test]
fn a_stalled_peer_is_dropped_mid_forward_with_two_peers() {
    a_stalled_peer_is_dropped_mid_forward(false);
}

#[test]
fn a_stalled_peer_is_dropped_mid_forward_with_three_peers() {
    a_stalled_peer_is_dropped_mid_forward(true);
}
