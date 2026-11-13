//! `FrpServer` over loopback, with a scripted fake driver and raw tungstenite
//! peers.

#![cfg(feature = "server")]

use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flightrelay::config::DriverConfig;
use flightrelay::driver::{
    Backoff, ConfigError, ConnectCtx, DeviceDriver, DeviceIdentity, DeviceSession, DriverInfo,
    Events, NoAction, SessionEnd, Stop, Stopped, TurnHints,
};
use flightrelay::{
    Club, EndpointConfig, FrpEndpoint, FrpMessage, FrpServer, Handedness, Launch, Mode, RawDir,
    ServerConfig, ServerError, ServerEvent, Severity, ShotKey, Source, Transport, Velocity,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

// ---------------------------------------------------------------- fake driver

type Job = Box<dyn FnOnce(&mut Events<'_>) + Send>;

enum Msg {
    Run(Job),
    Fail(&'static str),
}

enum Plan {
    /// Connect at once with this identity.
    Session(DeviceIdentity),
    /// Fail the attempt.
    Fail(&'static str),
    /// Tick until released, then connect. Emits one alert first, if given.
    Block {
        release: Arc<AtomicBool>,
        /// An alert to send, once its flag is set.
        alert: Option<(&'static str, Arc<AtomicBool>)>,
    },
    /// Tick until a club is in the hints, then connect.
    BlockUntilClub,
}

#[derive(Default)]
struct Inner {
    plans: Mutex<VecDeque<Plan>>,
    hints_at_connect: Mutex<Vec<TurnHints>>,
    hints: Mutex<Vec<TurnHints>>,
    closes: AtomicU32,
    connects: AtomicU32,
    give_up: AtomicBool,
}

#[derive(Clone)]
struct Control {
    inner: Arc<Inner>,
    jobs: Sender<Msg>,
}

impl Control {
    fn plan(&self, plan: Plan) {
        self.inner.plans.lock().unwrap().push_back(plan);
    }

    /// Run `f` on the device thread, inside the session's next `poll`.
    fn run(&self, f: impl FnOnce(&mut Events<'_>) + Send + 'static) {
        self.jobs.send(Msg::Run(Box::new(f))).unwrap();
    }

    fn fail(&self, why: &'static str) {
        self.jobs.send(Msg::Fail(why)).unwrap();
    }

    fn connects(&self) -> u32 {
        self.inner.connects.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct FakeConfig {}
impl DriverConfig for FakeConfig {}

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("stopped")]
    Stopped,
    #[error("failed: {0}")]
    Failed(&'static str),
}

impl From<Stopped> for FakeError {
    fn from(_: Stopped) -> Self {
        Self::Stopped
    }
}

struct Fake {
    config: FakeConfig,
    inner: Arc<Inner>,
    jobs: Arc<Mutex<Receiver<Msg>>>,
}

struct FakeSession {
    identity: DeviceIdentity,
    inner: Arc<Inner>,
    jobs: Arc<Mutex<Receiver<Msg>>>,
}

const ACTOR: &str = "7d2f9a41c03be856";
const CLUBHOUSE: &str = "c19e04b7a2d5f318";
const GAME: &str = "5b80e2f7d14c9a63";

fn identity() -> DeviceIdentity {
    DeviceIdentity::new("EagleOne-X4K2")
        .with_manufacturer("Birdie Labs")
        .with_model("Eagle One")
        .with_firmware("2.1")
}

fn fake() -> (Fake, Control) {
    let inner = Arc::new(Inner::default());
    let (tx, rx) = channel();
    let driver = Fake {
        config: FakeConfig::default(),
        inner: Arc::clone(&inner),
        jobs: Arc::new(Mutex::new(rx)),
    };
    (driver, Control { inner, jobs: tx })
}

impl DeviceDriver for Fake {
    type Config = FakeConfig;
    type Error = FakeError;
    type Action = NoAction;
    type Session = FakeSession;

    const STOP_WAIT: Duration = Duration::from_secs(1);

    fn info(_: &FakeConfig) -> DriverInfo {
        DriverInfo::new("fake", "Fake launch monitor", Source::Virtual)
    }
    fn new(config: FakeConfig) -> Result<Self, ConfigError> {
        let (mut driver, _) = fake();
        driver.config = config;
        Ok(driver)
    }
    fn config(&self) -> &FakeConfig {
        &self.config
    }
    fn apply_config(&mut self, new: &FakeConfig) {
        self.config = new.clone();
    }

    fn connect(&mut self, ctx: &mut ConnectCtx<'_>) -> Result<FakeSession, FakeError> {
        self.inner.connects.fetch_add(1, Ordering::SeqCst);
        ctx.tick()?;
        let plan = self.inner.plans.lock().unwrap().pop_front();
        let id = match plan.unwrap_or_else(|| Plan::Session(identity())) {
            Plan::Session(id) => id,
            Plan::Fail(why) => return Err(FakeError::Failed(why)),
            Plan::Block { release, alert } => {
                let mut alert = alert;
                while !release.load(Ordering::SeqCst) {
                    if alert
                        .as_ref()
                        .is_some_and(|(_, go)| go.load(Ordering::SeqCst))
                        && let Some((message, _)) = alert.take()
                    {
                        ctx.out.alert(Severity::Warn, message);
                    }
                    ctx.tick()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                identity()
            }
            Plan::BlockUntilClub => {
                while ctx.hints().club.is_none() {
                    ctx.tick()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                identity()
            }
        };
        self.inner
            .hints_at_connect
            .lock()
            .unwrap()
            .push(*ctx.hints());
        Ok(FakeSession {
            identity: id,
            inner: Arc::clone(&self.inner),
            jobs: Arc::clone(&self.jobs),
        })
    }

    fn retry_delay(
        &mut self,
        ended: &SessionEnd<'_, FakeError>,
        _: &mut Backoff,
        out: &mut Events<'_>,
    ) -> Option<Duration> {
        if let Some(error) = ended.error {
            out.alert(Severity::Warn, error.to_string());
        }
        if self.inner.give_up.load(Ordering::SeqCst) {
            None
        } else {
            Some(Duration::from_millis(50))
        }
    }
}

impl DeviceSession for FakeSession {
    type Driver = Fake;

    fn poll(&mut self, timeout: Duration, out: &mut Events<'_>) -> Result<(), FakeError> {
        let msg = self.jobs.lock().unwrap().recv_timeout(timeout);
        match msg {
            Ok(Msg::Run(job)) => job(out),
            Ok(Msg::Fail(why)) => return Err(FakeError::Failed(why)),
            Err(_) => {}
        }
        Ok(())
    }
    fn hints(&mut self, hints: &TurnHints) -> Result<(), FakeError> {
        self.inner.hints.lock().unwrap().push(*hints);
        Ok(())
    }
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }
    fn close(self, _: &mut Events<'_>) -> Result<(), FakeError> {
        self.inner.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// -------------------------------------------------------------------- harness

type Log = Arc<Mutex<Vec<String>>>;

struct Harness {
    control: Control,
    stop: Stop,
    log: Log,
    handle: Option<JoinHandle<Result<(), ServerError<FakeError>>>>,
}

impl Harness {
    fn start(target: Transport, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let (driver, control) = fake();
        Self::start_with(driver, control, target, tweak)
    }

    fn start_with(
        driver: Fake,
        control: Control,
        target: Transport,
        tweak: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let mut config = ServerConfig::new(target, ACTOR);
        config.handshake_timeout = Duration::from_millis(500);
        tweak(&mut config);
        let stop = Stop::new();
        let log: Log = Arc::default();
        let (thread_stop, thread_log) = (stop.clone(), Arc::clone(&log));
        let handle = std::thread::spawn(move || {
            FrpServer::new(driver, config)
                .on_event(move |event| thread_log.lock().unwrap().push(render(event)))
                .run(&thread_stop)
        });
        Self {
            control,
            stop,
            log,
            handle: Some(handle),
        }
    }

    fn listen() -> Self {
        Self::start(Transport::listen("127.0.0.1:0"), |_| {})
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Wait until a log line starts with `prefix`; returns it.
    fn wait_for(&self, prefix: &str) -> String {
        self.wait_for_nth(prefix, 1)
    }

    fn wait_for_nth(&self, prefix: &str, n: usize) -> String {
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            let lines: Vec<String> = self
                .log()
                .into_iter()
                .filter(|l| l.starts_with(prefix))
                .collect();
            if let Some(line) = lines.get(n - 1) {
                return line.clone();
            }
            assert!(
                Instant::now() < give_up,
                "no {prefix:?} #{n} in {:#?}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn addr(&self) -> String {
        self.wait_for("listening ")["listening ".len()..].to_owned()
    }

    /// A peer the server has greeted and taken in, so what is published
    /// from now on reaches it. Its greeting is read.
    fn peer(&self) -> Peer {
        let before = self
            .log()
            .iter()
            .filter(|l| l.starts_with("peer+ "))
            .count();
        let mut peer = Peer::connect(&self.addr());
        self.wait_for_nth("peer+ ", before + 1);
        let hello = peer.until(|v| kind(v) == "bus.sync");
        assert_eq!(kinds(&hello), ["bus.actor", "bus.sync"]);
        peer
    }

    fn stop_and_join(mut self) -> Result<(), ServerError<FakeError>> {
        self.stop.stop();
        self.handle.take().unwrap().join().unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn render(event: &ServerEvent<'_>) -> String {
    match event {
        ServerEvent::Listening(addr) => format!("listening {addr}"),
        ServerEvent::PeerConnected { addr } => format!("peer+ {addr:?}"),
        ServerEvent::PeerLeft { addr, reason } => format!("peer- {addr:?}: {reason}"),
        ServerEvent::DeviceConnected(id) => format!("device+ {}", id.device),
        ServerEvent::DeviceLost { error, retry_in } => {
            format!(
                "lost {:?} retry={retry_in:?}",
                error.map(ToString::to_string)
            )
        }
        ServerEvent::Device(event) => format!("device {event:?}"),
        ServerEvent::Hints(hints) => format!("hints {hints:?}"),
        other => format!("{other:?}"),
    }
}

struct Peer {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Peer {
    /// A raw WebSocket peer; it sends nothing until told.
    fn connect(addr: &str) -> Self {
        let (ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).expect("connect");
        Self { ws }
    }

    fn set_timeout(&self, wait: Duration) {
        if let MaybeTlsStream::Plain(tcp) = self.ws.get_ref() {
            tcp.set_read_timeout(Some(wait.max(Duration::from_millis(1))))
                .unwrap();
        }
    }

    /// The next text message as JSON, or `None` after `wait`.
    fn next(&mut self, wait: Duration) -> Option<Value> {
        self.set_timeout(wait);
        loop {
            match self.ws.read() {
                Ok(Message::Text(text)) => return Some(serde_json::from_str(&text).unwrap()),
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(_) => return None,
            }
        }
    }

    /// Everything that arrives until `quiet` passes with nothing.
    fn drain(&mut self, quiet: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(v) = self.next(quiet) {
            out.push(v);
        }
        out
    }

    /// Read until `pred` matches a message; returns everything read.
    fn until(&mut self, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
        let give_up = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < give_up {
            if let Some(v) = self.next(Duration::from_millis(200)) {
                let hit = pred(&v);
                seen.push(v);
                if hit {
                    return seen;
                }
            }
        }
        panic!("timed out; saw {seen:#?}");
    }

    fn send(&mut self, v: &Value) {
        self.ws.send(Message::text(v.to_string())).unwrap();
    }
}

fn kind(v: &Value) -> &str {
    v["kind"].as_str().unwrap_or("?")
}

fn kinds(vs: &[Value]) -> Vec<&str> {
    vs.iter().map(kind).collect()
}

/// The `bus.actor` messages among `vs`.
fn actors(vs: &[Value]) -> Vec<&Value> {
    vs.iter().filter(|v| kind(v) == "bus.actor").collect()
}

fn turn(club: Option<&str>, hand: &str, mode: &str) -> Value {
    json!({ "kind": "golfer.turn", "from": CLUBHOUSE, "golfer": "clubhouse.sam",
            "club": club, "handedness": hand, "mode": mode })
}

fn key(n: u32) -> ShotKey {
    ShotKey::new(format!("550e8400-e29b-41d4-a716-4466554400{n:02}"), n)
}

fn launch() -> Launch {
    Launch {
        launch_speed: Some(Velocity::MetersPerSecond(67.2)),
        launch_elevation: Some(14.2),
        ..Launch::default()
    }
}

/// Wait for a condition on the device thread (e.g. a flag a job recorded).
fn wait_until(what: &str, f: impl Fn() -> bool) {
    let give_up = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < give_up, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------- tests

#[test]
fn a_peer_is_greeted_with_the_actor_then_a_sync() {
    let h = Harness::listen();
    let mut peer = Peer::connect(&h.addr());
    let hello = peer.until(|v| kind(v) == "bus.sync");
    assert_eq!(kinds(&hello), ["bus.actor", "bus.sync"]);
    let actor = &hello[0];
    assert_eq!(actor["from"], ACTOR);
    assert_eq!(actor["frp"], "0.3.0");
    assert_eq!(actor["heartbeat"], 2.0);
    assert_eq!(actor["roles"], json!(["launch_monitor"]));
    assert_eq!(actor["device"]["source"], "virtual");
    assert_eq!(hello[1]["from"], ACTOR);
}

#[test]
fn the_actor_takes_the_identity_and_never_publishes_the_device_id() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    let seen = peer.until(|v| v["device"]["model"] == "Eagle One");
    let actor = actors(&seen).pop().unwrap().clone();
    assert_eq!(actor["name"], "Birdie Labs Eagle One");
    assert_eq!(actor["device"]["manufacturer"], "Birdie Labs");
    assert_eq!(actor["device"]["firmware"], "2.1");
    assert_eq!(actor["blockers"], json!(["not_armed"]));
    h.control.run(|out| out.ready(true));
    let seen = peer.until(|v| kind(v) == "bus.actor" && v["blockers"] == json!([]));
    for v in &seen {
        assert!(!v.to_string().contains("EagleOne-X4K2"), "{v}");
    }
}

#[test]
fn a_named_actor_keeps_its_name() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.name = Some("Bay 3".into());
    });
    let mut peer = Peer::connect(&h.addr());
    let hello = peer.until(|v| kind(v) == "bus.actor");
    assert_eq!(hello[0]["name"], "Bay 3");
    h.wait_for("device+ ");
    assert!(
        peer.drain(Duration::from_millis(300))
            .iter()
            .all(|v| kind(v) != "bus.actor" || v["name"] == "Bay 3")
    );
}

#[test]
fn a_sync_from_any_peer_is_answered_with_the_actor() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.heartbeat = Duration::from_secs(60);
    });
    let mut peer = h.peer();
    h.wait_for("device+ ");
    let _ = peer.drain(Duration::from_millis(200));
    peer.send(&json!({ "kind": "bus.sync", "from": GAME }));
    let answer = peer.until(|v| kind(v) == "bus.actor");
    assert_eq!(answer.last().unwrap()["from"], ACTOR);
}

#[test]
fn the_actor_is_republished_every_heartbeat() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.heartbeat = Duration::from_millis(100);
    });
    let mut peer = h.peer();
    h.wait_for("device+ ");
    let _ = peer.drain(Duration::from_millis(50));
    let at = Instant::now();
    let mut beats = 0;
    while at.elapsed() < Duration::from_millis(1000) {
        if peer
            .next(Duration::from_millis(50))
            .is_some_and(|v| kind(&v) == "bus.actor")
        {
            beats += 1;
        }
    }
    assert!((6..=14).contains(&beats), "{beats} heartbeats in a second");
}

#[test]
fn shots_and_alerts_go_out_under_the_actor_id_to_every_peer() {
    let h = Harness::listen();
    let mut a = h.peer();
    let mut b = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.shot_trigger_at(key(1), 1_794_063_845_123);
        out.launch(key(1), launch());
        out.alert(Severity::Error, "lens fogged");
        out.shot_finished(key(1));
    });
    for peer in [&mut a, &mut b] {
        let seen = peer.until(|v| kind(v) == "shot.finished");
        let shot: Vec<&Value> = seen.iter().filter(|v| kind(v) != "bus.actor").collect();
        assert_eq!(
            shot.iter().map(|v| kind(v)).collect::<Vec<_>>(),
            ["shot.trigger", "shot.launch", "bus.alert", "shot.finished"]
        );
        assert!(shot.iter().all(|v| v["from"] == ACTOR));
        assert_eq!(shot[0]["timestamp"], 1_794_063_845_123_u64);
        assert_eq!(shot[1]["launch"]["launch_speed"], "67.2mps");
        assert_eq!(shot[2]["severity"], "error");
    }
}

#[test]
fn kinds_that_are_not_a_launch_monitors_are_not_published() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.frp(flightrelay::GolferTurn::new(
            None,
            None,
            Handedness::Left,
            Mode::Full,
        ));
        out.frp(flightrelay::BusSync {});
        out.shot_trigger(key(1));
    });
    let seen = peer.until(|v| kind(v) == "shot.trigger");
    assert!(
        seen.iter()
            .all(|v| matches!(kind(v), "bus.actor" | "shot.trigger")),
        "{seen:#?}"
    );
}

#[test]
fn the_server_forwards_between_peers_never_back_to_the_source() {
    let h = Harness::listen();
    let mut session = h.peer();
    let mut game = h.peer();
    let mut lights = h.peer();
    for p in [&mut session, &mut game, &mut lights] {
        let _ = p.drain(Duration::from_millis(200));
    }
    let profile = json!({ "kind": "golfer.profile", "from": CLUBHOUSE, "id": "clubhouse.sam",
                          "name": "Sam", "x-extra": 1 });
    session.send(&profile);
    let ext = json!({ "kind": "x.birdielabs.glow", "from": GAME, "hue": 120 });
    game.send(&ext);
    let not_actor = |v: &Value| kind(v) != "bus.actor";
    let at_game: Vec<Value> = game
        .drain(Duration::from_millis(300))
        .into_iter()
        .filter(not_actor)
        .collect();
    let at_lights: Vec<Value> = lights
        .drain(Duration::from_millis(300))
        .into_iter()
        .filter(not_actor)
        .collect();
    let at_session: Vec<Value> = session
        .drain(Duration::from_millis(300))
        .into_iter()
        .filter(not_actor)
        .collect();
    assert_eq!(
        at_game,
        std::slice::from_ref(&profile),
        "unknown fields travel too"
    );
    assert_eq!(at_lights, [profile, ext.clone()]);
    assert_eq!(at_session, [ext], "never back to the source");
}

#[test]
fn a_turn_reaches_the_driver_as_hints_that_replace_the_last() {
    let h = Harness::listen();
    let mut session = h.peer();
    h.wait_for("device+ ");
    session.send(&turn(Some("7I"), "lh", "full"));
    wait_until("the first hints", || {
        h.control.inner.hints.lock().unwrap().len() == 1
    });
    // The same turn again changes nothing, so the driver hears nothing.
    session.send(&turn(Some("7I"), "lh", "full"));
    // A null club is no club, and an unknown mode is no mode: nothing is
    // kept from the turn before.
    session.send(&turn(None, "rh", "flop"));
    wait_until("the second hints", || {
        h.control.inner.hints.lock().unwrap().len() == 2
    });
    std::thread::sleep(Duration::from_millis(100));
    let hints = h.control.inner.hints.lock().unwrap().clone();
    assert_eq!(
        hints,
        [
            TurnHints::default()
                .with_club(Club::Iron7)
                .with_handedness(Handedness::Left)
                .with_mode(Mode::Full),
            TurnHints::default().with_handedness(Handedness::Right),
        ]
    );
    assert!(h.log().iter().any(|l| l.starts_with("hints ")));
}

#[test]
fn the_last_turns_hints_reach_the_next_connect_and_outlive_the_session_actor() {
    let (driver, control) = fake();
    control.plan(Plan::BlockUntilClub);
    let h = Harness::start_with(driver, control, Transport::listen("127.0.0.1:0"), |_| {});
    let mut session = h.peer();
    // A turn served while the connect blocks.
    session.send(&turn(Some("PW"), "rh", "chipping"));
    h.wait_for("device+ ");
    // The session actor goes away; the launch monitor keeps the last turn's hints.
    session.send(&json!({ "kind": "bus.actor", "from": CLUBHOUSE, "removed": true }));
    drop(session);
    h.control.fail("unplugged");
    h.wait_for_nth("device+ ", 2);
    let at_connect = h.control.inner.hints_at_connect.lock().unwrap().clone();
    let expected = TurnHints::default()
        .with_club(Club::PitchingWedge)
        .with_handedness(Handedness::Right)
        .with_mode(Mode::Chipping);
    assert_eq!(at_connect, [expected, expected]);
}

#[test]
fn a_sync_is_answered_while_a_connect_blocks() {
    let (driver, control) = fake();
    let release = Arc::new(AtomicBool::new(false));
    control.plan(Plan::Block {
        release: Arc::clone(&release),
        alert: None,
    });
    let h = Harness::start_with(driver, control, Transport::listen("127.0.0.1:0"), |c| {
        c.heartbeat = Duration::from_secs(60);
    });
    let mut peer = h.peer();
    peer.send(&json!({ "kind": "bus.sync", "from": GAME }));
    let answer = peer.until(|v| kind(v) == "bus.actor");
    assert_eq!(answer.last().unwrap()["blockers"], json!(["not_armed"]));
    assert_eq!(h.control.connects(), 1);
    release.store(true, Ordering::SeqCst);
    h.wait_for("device+ ");
}

#[test]
fn an_alert_while_connecting_goes_out_at_once() {
    let (driver, control) = fake();
    let release = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    control.plan(Plan::Block {
        release: Arc::clone(&release),
        alert: Some(("press the pairing button", Arc::clone(&go))),
    });
    let h = Harness::start_with(driver, control, Transport::listen("127.0.0.1:0"), |_| {});
    let mut peer = h.peer();
    go.store(true, Ordering::SeqCst);
    let seen = peer.until(|v| kind(v) == "bus.alert");
    let alert = seen.last().unwrap();
    assert_eq!(alert["from"], ACTOR);
    assert_eq!(alert["message"], "press the pairing button");
    release.store(true, Ordering::SeqCst);
}

#[test]
fn driver_status_is_merged_into_the_actor() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.blockers(["no_ball"]);
        out.battery(82);
        out.attitude(0.4, -0.2);
        out.telemetry([("radar_mode", "armed")]);
    });
    let seen = peer.until(|v| v["telemetry"]["radar_mode"] == "armed");
    let actor = seen.last().unwrap();
    assert_eq!(actor["blockers"], json!(["no_ball"]));
    assert_eq!(actor["device"]["battery_pct"], 82);
    assert_eq!(actor["device"]["tilt"], 0.4);
    assert_eq!(actor["device"]["roll"], -0.2);
    // Telemetry keys merge; the rest stays.
    h.control.run(|out| out.telemetry([("temp_c", "21")]));
    let seen = peer.until(|v| v["telemetry"]["temp_c"] == "21");
    let actor = seen.last().unwrap();
    assert_eq!(actor["telemetry"]["radar_mode"], "armed");
    assert_eq!(actor["device"]["battery_pct"], 82);
    // A status that changes nothing publishes nothing.
    h.control.run(|out| out.battery(82));
    let quiet = peer.drain(Duration::from_millis(300));
    assert!(actors(&quiet).is_empty(), "{quiet:#?}");
}

#[test]
fn a_lost_session_finishes_its_open_shot_and_raises_not_armed() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.ready(true);
        out.shot_trigger(key(3));
    });
    peer.until(|v| kind(v) == "shot.trigger");
    h.control.fail("cable pulled");
    let seen = peer.until(|v| kind(v) == "bus.actor" && v["blockers"] == json!(["not_armed"]));
    let finished = seen
        .iter()
        .position(|v| kind(v) == "shot.finished")
        .expect("the open shot is finished");
    assert_eq!(seen[finished]["key"]["shot_number"], 3);
    assert_eq!(seen[finished]["from"], ACTOR);
    // The driver's own alert about the loss, from `retry_delay`.
    let rest = peer.until(|v| kind(v) == "bus.alert");
    assert_eq!(rest.last().unwrap()["message"], "failed: cable pulled");
}

#[test]
fn a_shot_that_stays_open_is_finished_after_the_shot_timeout() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.shot_timeout = Duration::from_millis(200);
    });
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| out.shot_trigger(key(4)));
    let seen = peer.until(|v| kind(v) == "shot.finished");
    assert_eq!(seen.last().unwrap()["key"]["shot_number"], 4);
}

#[test]
fn a_stop_closes_the_session_and_removes_the_actor() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| out.shot_trigger(key(5)));
    peer.until(|v| kind(v) == "shot.trigger");
    let control = h.control.clone();
    assert!(h.stop_and_join().is_ok());
    let seen = peer.drain(Duration::from_millis(500));
    assert!(kinds(&seen).contains(&"shot.finished"), "{seen:#?}");
    let last = seen.last().expect("the removal");
    assert_eq!(
        last,
        &json!({ "kind": "bus.actor", "from": ACTOR, "removed": true })
    );
    assert_eq!(control.inner.closes.load(Ordering::SeqCst), 1);
}

#[test]
fn raw_frames_go_out_only_when_configured_and_count_on_across_sessions() {
    let off = Harness::listen();
    let mut peer = off.peer();
    off.wait_for("device+ ");
    off.control.run(|out| {
        out.raw(RawDir::Rx, "result", &[1, 2, 3]);
        out.shot_trigger(key(1));
    });
    let seen = peer.until(|v| kind(v) == "shot.trigger");
    assert!(!kinds(&seen).contains(&"shot.raw"), "{seen:#?}");
    drop(off);

    let on = Harness::start(Transport::listen("127.0.0.1:0"), |c| c.raw_frames = true);
    let mut peer = on.peer();
    on.wait_for("device+ ");
    on.control.run(|out| {
        out.raw(RawDir::Rx, "a", &[0xaa]);
        out.raw(RawDir::Tx, "b", &[0xbb]);
    });
    peer.until(|v| v["label"] == "b");
    on.control.fail("reconnect");
    on.wait_for_nth("device+ ", 2);
    on.control.run(|out| out.raw(RawDir::Rx, "c", &[0xcc]));
    let seen = peer.until(|v| v["label"] == "c");
    let raws: Vec<&Value> = seen.iter().filter(|v| kind(v) == "shot.raw").collect();
    let last = raws.last().unwrap();
    assert_eq!(last["seq"], 2, "numbered per actor across sessions");
    assert_eq!(last["data"], "cc");
    assert_eq!(last["from"], ACTOR);
}

/// A device that emits `count` alerts of about 100 KB each, every one different.
fn flood(control: &Control, count: usize) {
    control.run(move |out| {
        for i in 0..count {
            out.alert(Severity::Warn, format!("{i}:{}", "x".repeat(100_000)));
        }
    });
}

/// Counts the alerts a peer reads, on a thread of its own.
fn count_alerts(mut peer: Peer, want: usize) -> JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut count = 0;
        let give_up = Instant::now() + Duration::from_secs(40);
        while count < want && Instant::now() < give_up {
            if let Some(v) = peer.next(Duration::from_millis(200))
                && kind(&v) == "bus.alert"
            {
                count += 1;
            }
        }
        count
    })
}

fn peer_left_lines(h: &Harness) -> Vec<String> {
    h.log()
        .into_iter()
        .filter(|l| l.starts_with("peer- "))
        .collect()
}

#[test]
fn a_peer_that_does_not_read_is_queued_and_kept_while_the_others_are_unaffected() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.write_buffer_limit = 256 * 1024 * 1024;
    });
    let stalled = h.peer(); // never reads again
    let reader = count_alerts(h.peer(), 160);
    h.wait_for("device+ ");
    flood(&h.control, 160); // 16 MB: more than the kernel buffers hold
    assert_eq!(
        reader.join().unwrap(),
        160,
        "the reading peer gets everything"
    );
    std::thread::sleep(Duration::from_millis(300));
    // The reader has hung up by now; the stalled peer is still there.
    assert!(
        !peer_left_lines(&h)
            .iter()
            .any(|l| l.contains("write buffer")),
        "a full kernel buffer alone drops nobody: {:#?}",
        peer_left_lines(&h)
    );
    assert!(peer_left_lines(&h).len() <= 1, "{:#?}", peer_left_lines(&h));
    drop(stalled);
}

#[test]
fn a_peer_that_falls_behind_the_write_buffer_cap_is_dropped_alone() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.write_buffer_limit = 512 * 1024;
    });
    let stalled = h.peer();
    let reader = count_alerts(h.peer(), 160);
    h.wait_for("device+ ");
    flood(&h.control, 160);
    let left = h.wait_for("peer- ");
    assert!(left.contains("write buffer full"), "{left}");
    assert_eq!(reader.join().unwrap(), 160, "the other peer is unaffected");
    let full: Vec<String> = peer_left_lines(&h)
        .into_iter()
        .filter(|l| l.contains("write buffer"))
        .collect();
    assert_eq!(full.len(), 1, "{full:#?}");
    drop(stalled);
}

#[test]
fn a_silent_client_cannot_hold_up_the_acceptor() {
    let h = Harness::listen();
    let addr = h.addr();
    let _silent = TcpStream::connect(&addr).unwrap(); // connects, says nothing
    let at = Instant::now();
    let mut peer = Peer::connect(&addr);
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
    assert_eq!(
        kind(&peer.next(Duration::from_secs(5)).unwrap()),
        "bus.actor"
    );
}

#[test]
fn peers_past_the_limit_are_refused() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| c.max_peers = 1);
    let _first = h.peer();
    let (mut ws, _) = tungstenite::connect(format!("ws://{}/frp", h.addr())).unwrap();
    let mut refused = false;
    for _ in 0..50 {
        if !matches!(
            ws.read(),
            Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))
        ) {
            refused = true;
            break;
        }
    }
    assert!(refused, "the second peer is closed");
    assert_eq!(
        h.log().iter().filter(|l| l.starts_with("peer+ ")).count(),
        1
    );
}

#[test]
fn a_driver_that_gives_up_ends_the_server_with_its_error() {
    let (driver, control) = fake();
    control.inner.give_up.store(true, Ordering::SeqCst);
    control.plan(Plan::Fail("no such device"));
    let h = Harness::start_with(driver, control, Transport::listen("127.0.0.1:0"), |_| {});
    let line = h.wait_for("lost ");
    assert!(
        line.contains("failed: no such device") && line.ends_with("retry=None"),
        "{line}"
    );
    let give_up = Instant::now() + Duration::from_secs(5);
    while !h.handle.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < give_up, "the server returns by itself");
        std::thread::sleep(Duration::from_millis(5));
    }
    match h.stop_and_join() {
        Err(ServerError::Driver(FakeError::Failed("no such device"))) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_failed_connect_is_retried_after_the_advised_delay() {
    let (driver, control) = fake();
    control.plan(Plan::Fail("first"));
    control.plan(Plan::Fail("second"));
    let h = Harness::start_with(driver, control, Transport::listen("127.0.0.1:0"), |_| {});
    h.wait_for("device+ ");
    let lost: Vec<String> = h
        .log()
        .into_iter()
        .filter(|l| l.starts_with("lost "))
        .collect();
    assert_eq!(lost.len(), 2, "{lost:?}");
    assert!(
        lost[0].contains("first") && lost[0].contains("retry=Some(50ms)"),
        "{lost:?}"
    );
}

#[test]
fn an_endpoint_that_cannot_open_is_a_fatal_error() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let h = Harness::start(Transport::listen(addr), |_| {});
    let give_up = Instant::now() + Duration::from_secs(5);
    while !h.handle.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < give_up);
        std::thread::sleep(Duration::from_millis(5));
    }
    match h.stop_and_join() {
        Err(ServerError::Endpoint(_)) => {}
        other => panic!("{other:?}"),
    }
}

/// A listening hub stand-in that hands over the first `n` connections.
fn hub_endpoint(
    n: usize,
    drop_first: bool,
) -> (String, Receiver<flightrelay::FrpConnection>, JoinHandle<()>) {
    let mut hub = FrpEndpoint::open(EndpointConfig::new(Transport::listen("127.0.0.1:0"))).unwrap();
    let addr = hub.local_addr().unwrap().unwrap();
    let (tx, rx) = channel();
    let accepting = std::thread::spawn(move || {
        for i in 0..n {
            let conn = hub.establish().expect("the server dialed");
            if drop_first && i == 0 {
                drop(conn);
            } else {
                let _ = tx.send(conn);
            }
        }
    });
    (format!("ws://{addr}/frp"), rx, accepting)
}

#[test]
fn a_bridge_dials_greets_and_redials_when_the_link_drops() {
    let (url, rx, accepting) = hub_endpoint(2, false);
    let h = Harness::start(Transport::connect(url), |_| {});
    let mut conn = rx.recv_timeout(Duration::from_secs(10)).expect("dialed");
    let hello = conn.recv().unwrap();
    assert_eq!(hello.kind(), "bus.actor");
    assert_eq!(hello.from, ACTOR);
    assert_eq!(conn.recv().unwrap().kind(), "bus.sync");
    h.wait_for("device+ ");
    // The hub side's own turn reaches the driver over the dialed link.
    conn.send(&FrpMessage::parse(&turn(Some("DR"), "rh", "full").to_string()).unwrap())
        .unwrap();
    wait_until("hints over the bridge", || {
        !h.control.inner.hints.lock().unwrap().is_empty()
    });
    drop(conn); // the link drops
    let mut conn = rx.recv_timeout(Duration::from_secs(15)).expect("redialed");
    let hello = conn.recv().unwrap();
    assert_eq!(hello.kind(), "bus.actor");
    h.stop.stop();
    drop(h);
    accepting.join().unwrap();
}

#[test]
fn a_bridge_dials_again_after_a_hub_that_closes_at_once() {
    let (url, rx, accepting) = hub_endpoint(2, true);
    let h = Harness::start(Transport::connect(url), |_| {});
    let mut conn = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("dialed again");
    assert_eq!(conn.recv().unwrap().from, ACTOR);
    h.stop.stop();
    drop(h);
    accepting.join().unwrap();
}

#[test]
fn a_wss_target_is_refused_before_anything_starts() {
    let h = Harness::start(Transport::connect("wss://127.0.0.1:1/frp"), |_| {});
    let give_up = Instant::now() + Duration::from_secs(5);
    while !h.handle.as_ref().unwrap().is_finished() {
        assert!(
            Instant::now() < give_up,
            "the server keeps trying a wss:// target"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let connects = h.control.connects();
    match h.stop_and_join() {
        Err(ServerError::Endpoint(e)) => {
            assert!(e.to_string().contains("only ws:// is supported"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(connects, 0, "the driver never connected");
}

#[test]
fn a_bridge_that_cannot_reach_the_bus_says_so_once_at_the_default_level() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let url = format!("ws://{closed}/frp");
    let lines = |h: &Harness| -> Vec<String> {
        h.log()
            .into_iter()
            .filter(|l| l.contains("cannot reach the bus"))
            .collect()
    };
    let info = Harness::start(Transport::connect(url.clone()), |_| {});
    info.wait_for("device Log(Info, \"cannot reach the bus");
    std::thread::sleep(Duration::from_millis(1600));
    assert_eq!(lines(&info).len(), 1, "{:#?}", info.log());
    drop(info);

    let debug = Harness::start(Transport::connect(url), |c| {
        c.log_level = flightrelay::driver::Level::Debug;
    });
    debug.wait_for("device Log(Debug, \"cannot reach the bus");
    let all = lines(&debug);
    assert!(all[0].starts_with("device Log(Info, "), "{all:#?}");
    assert!(
        all[1..].iter().all(|l| l.starts_with("device Log(Debug, ")),
        "{all:#?}"
    );
}
