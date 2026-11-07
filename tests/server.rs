//! `FrpServer` over loopback, with a scripted fake driver and raw tungstenite
//! peers (the way `tests/topology.rs` talks to the endpoint).

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
    Backoff, ConfigError, ConnectCtx, DeviceCommand, DeviceDriver, DeviceIdentity, DeviceSession,
    DriverInfo, Events, NoAction, SessionEnd, Stop, Stopped,
};
use flightrelay::{
    Club, DetectionMode, EndpointConfig, FrpEndpoint, FrpServer, Handedness, Launch, RawDir, Role,
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
    /// Tick until a club is latched, then connect.
    BlockUntilClub,
    /// Tick until released, then emit these telemetry maps and an alert
    /// while still connecting, and connect with this identity.
    Announce {
        release: Arc<AtomicBool>,
        identity: DeviceIdentity,
        telemetry: Vec<Vec<(&'static str, &'static str)>>,
        alert: &'static str,
    },
}

#[derive(Default)]
struct Inner {
    plans: Mutex<VecDeque<Plan>>,
    latched_at_connect: Mutex<Vec<DeviceCommand>>,
    commands: Mutex<Vec<DeviceCommand>>,
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
        DriverInfo::new("fake", "Fake", Source::Virtual)
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
            Plan::Announce {
                release,
                identity,
                telemetry,
                alert,
            } => {
                while !release.load(Ordering::SeqCst) {
                    ctx.tick()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                for pairs in telemetry {
                    ctx.out.telemetry(pairs);
                }
                ctx.out.alert(Severity::Warn, alert);
                identity
            }
            Plan::BlockUntilClub => {
                while ctx.latched().club.is_none() {
                    ctx.tick()?;
                    std::thread::sleep(Duration::from_millis(10));
                }
                identity()
            }
        };
        self.inner
            .latched_at_connect
            .lock()
            .unwrap()
            .push(*ctx.latched());
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
    fn command(&mut self, cmd: &DeviceCommand) -> Result<(), FakeError> {
        self.inner.commands.lock().unwrap().push(*cmd);
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
        let mut config = ServerConfig::new(target);
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

    /// A handshaken controller that the server has registered, so events
    /// emitted from now on reach it.
    fn peer(&self) -> Peer {
        self.named_peer("peer")
    }

    fn named_peer(&self, name: &str) -> Peer {
        let before = self
            .log()
            .iter()
            .filter(|l| l.starts_with("controller+ "))
            .count();
        let peer = Peer::connect_as(&self.addr(), false, name);
        self.wait_for_nth("controller+ ", before + 1);
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
        ServerEvent::ControllerConnected { peer } => format!("controller+ {}", peer.unwrap_or("-")),
        ServerEvent::ControllerLeft { peer, reason } => {
            format!("controller- {}: {reason}", peer.unwrap_or("-"))
        }
        ServerEvent::DeviceConnected(id) => format!("device+ {}", id.device),
        ServerEvent::DeviceLost { error, retry_in } => {
            format!(
                "lost {:?} retry={retry_in:?}",
                error.map(ToString::to_string)
            )
        }
        ServerEvent::Device(event) => format!("device {event:?}"),
        ServerEvent::Command(cmd) => format!("command {cmd:?}"),
        other => format!("{other:?}"),
    }
}

struct Peer {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl Peer {
    fn connect(addr: &str, raw_frames: bool) -> Self {
        Self::connect_as(addr, raw_frames, "peer")
    }

    fn connect_as(addr: &str, raw_frames: bool, name: &str) -> Self {
        let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).expect("connect");
        let mut start = json!({ "kind": "start", "version": ["0.2.0"], "name": name });
        if raw_frames {
            start["raw_frames"] = json!(true);
        }
        ws.send(Message::text(start.to_string())).expect("start");
        let mut peer = Self { ws };
        let init = peer.next(Duration::from_secs(5)).expect("init");
        assert_eq!(init, json!({ "kind": "init", "version": "0.2.0" }));
        peer
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
    v["event"]["kind"]
        .as_str()
        .or_else(|| v["kind"].as_str())
        .unwrap_or("?")
}

fn kinds(vs: &[Value]) -> Vec<&str> {
    vs.iter().map(kind).collect()
}

fn telemetry<'a>(vs: &'a [Value], key: &str) -> Vec<&'a str> {
    vs.iter()
        .filter(|v| kind(v) == "device_telemetry")
        .filter_map(|v| v["event"]["telemetry"][key].as_str())
        .collect()
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
fn a_listening_server_handshakes_and_sends_identity_telemetry_with_source_first() {
    let h = Harness::listen();
    h.control.plan(Plan::Block {
        release: Arc::new(AtomicBool::new(true)),
        alert: None,
    });
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.ready(true);
        out.shot_trigger(key(1));
        out.launch(key(1), launch());
        out.shot_finished(key(1));
    });
    let seen = peer.until(|v| kind(v) == "shot_finished");
    let order = kinds(&seen);
    // The identity telemetry (with `source`) comes before every shot event.
    let first_shot = order.iter().position(|k| *k == "shot_trigger").unwrap();
    let identity = seen
        .iter()
        .take(first_shot)
        .find(|v| v["event"]["manufacturer"] == "Birdie Labs")
        .expect("identity telemetry before the first shot");
    assert_eq!(identity["device"], "EagleOne-X4K2");
    assert_eq!(identity["event"]["model"], "Eagle One");
    assert_eq!(identity["event"]["firmware"], "2.1");
    assert_eq!(identity["event"]["source"], "virtual");
    // Every telemetry carries `source`, the driver's delta included.
    for v in seen.iter().filter(|v| kind(v) == "device_telemetry") {
        assert_eq!(v["event"]["source"], "virtual", "{v}");
    }
    assert_eq!(
        order[first_shot..],
        ["shot_trigger", "launch", "shot_finished"]
    );
}

#[test]
fn a_controller_that_connects_first_gets_the_identity_when_the_device_appears() {
    let h = Harness::listen();
    let release = Arc::new(AtomicBool::new(false));
    h.control.plan(Plan::Block {
        release: Arc::clone(&release),
        alert: None,
    });
    let mut peer = h.peer();
    assert!(
        peer.next(Duration::from_millis(300)).is_none(),
        "nothing before a device exists"
    );
    release.store(true, Ordering::SeqCst);
    let first = peer.next(Duration::from_secs(5)).expect("telemetry");
    assert_eq!(kind(&first), "device_telemetry");
    assert_eq!(first["event"]["source"], "virtual");
}

#[test]
fn a_driver_alert_before_identification_is_a_protocol_alert() {
    let h = Harness::listen();
    let release = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    h.control.plan(Plan::Block {
        release: Arc::clone(&release),
        alert: Some(("not found yet", Arc::clone(&go))),
    });
    let mut peer = h.peer();
    go.store(true, Ordering::SeqCst);
    let alert = peer.next(Duration::from_secs(5)).expect("alert");
    // Sent as a protocol-level alert: there is no device id to put on an envelope yet.
    assert_eq!(
        alert,
        json!({ "kind": "alert", "severity": "warn", "message": "not found yet" })
    );
    release.store(true, Ordering::SeqCst);
    assert_eq!(
        kind(&peer.next(Duration::from_secs(5)).unwrap()),
        "device_telemetry"
    );
}

#[test]
fn a_late_controller_gets_the_merged_state_and_no_shots() {
    let h = Harness::listen();
    let mut first = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.ready(true);
        out.telemetry([("battery_pct", "80")]);
        out.shot_trigger(key(1));
        out.shot_finished(key(1));
        out.telemetry([("battery_pct", "79")]);
    });
    first.until(|v| v["event"]["telemetry"]["battery_pct"] == "79");

    let mut late = h.peer();
    let replay = late.drain(Duration::from_millis(300));
    assert_eq!(
        kinds(&replay),
        ["device_telemetry"],
        "one merged telemetry: {replay:#?}"
    );
    let t = &replay[0]["event"];
    assert_eq!(t["telemetry"]["ready"], "true");
    assert_eq!(t["telemetry"]["battery_pct"], "79");
    assert_eq!(t["manufacturer"], "Birdie Labs");
    assert_eq!(t["source"], "virtual");
}

#[test]
fn every_controller_gets_every_envelope() {
    let h = Harness::listen();
    let mut a = h.peer();
    let mut b = h.peer();
    h.wait_for_nth("controller+ ", 2);
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.shot_trigger(key(1));
        out.launch(key(1), launch());
        out.shot_finished(key(1));
    });
    for peer in [&mut a, &mut b] {
        let seen = peer.until(|v| kind(v) == "shot_finished");
        let shot: Vec<&str> = kinds(&seen)
            .into_iter()
            .filter(|k| *k != "device_telemetry")
            .collect();
        assert_eq!(shot, ["shot_trigger", "launch", "shot_finished"]);
    }
}

#[test]
fn a_driver_that_sends_everything_twice_is_deduplicated() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.ready(true);
        out.ready(true);
        out.shot_trigger(key(1));
        out.launch(key(1), launch());
        out.launch(key(1), launch());
        out.flight_result(
            key(1),
            flightrelay::FlightResult::new(flightrelay::Basis::Modeled),
        );
        out.flight_result(
            key(1),
            flightrelay::FlightResult::new(flightrelay::Basis::Modeled),
        );
        out.club_path(key(1), flightrelay::ClubData::default());
        out.club_path(key(1), flightrelay::ClubData::default());
        out.shot_finished(key(1));
        out.shot_finished(key(1));
        out.shot_trigger(key(2));
        out.shot_finished(key(2));
    });
    let seen = peer.until(|v| kind(v) == "shot_finished" && v["event"]["key"]["shot_number"] == 2);
    let shot: Vec<&str> = kinds(&seen)
        .into_iter()
        .filter(|k| *k != "device_telemetry")
        .collect();
    assert_eq!(
        shot,
        [
            "shot_trigger",
            "launch",
            "flight_result",
            "club_path",
            "shot_finished",
            "shot_trigger",
            "shot_finished"
        ]
    );
    assert_eq!(
        telemetry(&seen, "ready"),
        ["true"],
        "an unchanged value is sent once"
    );
}

#[test]
fn a_shot_left_open_by_a_lost_session_is_finished_then_the_device_is_marked_lost() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.ready(true);
        out.shot_trigger(key(1));
        out.launch(key(1), launch());
    });
    peer.until(|v| kind(v) == "launch");
    h.control.fail("cable pulled");
    let seen = peer.until(|v| v["message"] == "failed: cable pulled");
    let order = kinds(&seen);
    let finished = order
        .iter()
        .position(|k| *k == "shot_finished")
        .expect("shot_finished");
    assert_eq!(seen[finished]["event"]["key"]["shot_number"], 1);
    assert_eq!(telemetry(&seen, "ready"), ["false"]);
    let not_ready = seen
        .iter()
        .position(|v| v["event"]["telemetry"]["ready"] == "false")
        .unwrap();
    assert!(
        finished < not_ready,
        "the shot is closed before the device is marked lost: {order:?}"
    );
    // The driver's alert is the only one: the server adds none of its own.
    // It comes after the session ended, so it is protocol-level.
    let alert = seen.last().unwrap();
    assert_eq!(alert["severity"], "warn");
    assert!(alert["device"].is_null(), "{alert}");
    assert_eq!(kinds(&seen).iter().filter(|k| **k == "alert").count(), 1);
    // The log shows the loss, and the driver reconnects.
    h.wait_for("lost ");
    h.wait_for_nth("device+ ", 2);
    assert!(
        h.log().iter().any(|l| l.contains("finishing shot 1")),
        "{:#?}",
        h.log()
    );
}

#[test]
fn a_shot_that_stays_open_is_finished_after_the_shot_timeout() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.shot_timeout = Duration::from_millis(300);
    });
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| {
        out.shot_trigger(key(1));
        out.launch(key(1), launch());
    });
    let seen = peer.until(|v| kind(v) == "shot_finished");
    assert_eq!(seen.last().unwrap()["event"]["key"]["shot_number"], 1);
    wait_until("the warning", || {
        h.log().iter().any(|l| l.contains("stayed open too long"))
    });
    // The session is still up: the watchdog finishes the shot, not the device.
    assert_eq!(h.control.connects(), 1);
}

#[test]
fn a_different_device_after_a_reconnect_replaces_the_old_one() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ EagleOne-X4K2");
    h.control.run(|out| out.telemetry([("battery_pct", "80")]));
    peer.until(|v| v["event"]["telemetry"]["battery_pct"] == "80");
    h.control.plan(Plan::Session(
        DeviceIdentity::new("EagleTwo-Q7Z9").with_model("Eagle Two"),
    ));
    h.control.fail("swapped");
    let seen = peer.until(|v| v["device"] == "EagleTwo-Q7Z9");
    assert_eq!(seen.last().unwrap()["event"]["model"], "Eagle Two");
    // A late controller is told about the new device only: the old state is gone.
    let mut late = h.peer();
    let replay = late.drain(Duration::from_millis(300));
    assert_eq!(replay.len(), 1, "{replay:#?}");
    assert_eq!(replay[0]["device"], "EagleTwo-Q7Z9");
    assert!(replay[0]["event"]["telemetry"]["battery_pct"].is_null());
}

#[test]
fn a_controller_that_joins_while_the_device_is_lost_sees_ready_false() {
    let h = Harness::listen();
    let mut first = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| out.ready(true));
    first.until(|v| v["event"]["telemetry"]["ready"] == "true");
    // The next connect blocks, so the device stays lost.
    h.control.plan(Plan::Block {
        release: Arc::new(AtomicBool::new(false)),
        alert: None,
    });
    h.control.fail("gone");
    first.until(|v| v["event"]["telemetry"]["ready"] == "false");
    h.wait_for("lost ");
    let mut late = h.peer();
    let replay = late.drain(Duration::from_millis(300));
    assert_eq!(telemetry(&replay, "ready"), ["false"], "{replay:#?}");
}

#[test]
fn commands_and_pings_are_served_while_a_connect_is_blocked() {
    let h = Harness::listen();
    h.control.plan(Plan::BlockUntilClub);
    let mut peer = h.peer();
    peer.send(&json!({ "kind": "set_detection_mode", "club": "PW" }));
    peer.ws
        .send(Message::Ping(b"are you there".to_vec().into()))
        .unwrap();
    peer.set_timeout(Duration::from_secs(5));
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        match peer.ws.read() {
            Ok(Message::Pong(payload)) => {
                assert_eq!(&payload[..], b"are you there");
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("no pong: {e}"),
        }
        assert!(Instant::now() < give_up);
    }
    // The command reached `ctx.latched()` during the blocked connect, which
    // then finished.
    h.wait_for("device+ ");
    let latched = h.control.inner.latched_at_connect.lock().unwrap().clone();
    assert_eq!(latched[0].club, Some(Club::PitchingWedge));
}

#[test]
fn latched_commands_reach_the_next_connect_each_field_on_its_own() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    peer.send(
        &json!({ "kind": "set_detection_mode", "mode": "putting", "handed": "lh", "club": "7I" }),
    );
    peer.send(&json!({ "kind": "set_detection_mode", "club": "PW" }));
    peer.send(&json!({ "kind": "set_detection_mode", "mode": "nonsense", "handed": "rh" }));
    wait_until("three commands", || {
        h.control.inner.commands.lock().unwrap().len() == 3
    });
    let commands = h.control.inner.commands.lock().unwrap().clone();
    // The session sees only the fields each message carried; an unrecognized value is dropped.
    assert_eq!(
        commands[0],
        DeviceCommand::default()
            .with_mode(DetectionMode::Putting)
            .with_handed(Handedness::Left)
            .with_club(Club::Iron7)
    );
    assert_eq!(
        commands[1],
        DeviceCommand::default().with_club(Club::PitchingWedge)
    );
    assert_eq!(
        commands[2],
        DeviceCommand::default().with_handed(Handedness::Right)
    );

    h.control.fail("reconnect please");
    h.wait_for_nth("device+ ", 2);
    let latched = h.control.inner.latched_at_connect.lock().unwrap().clone();
    assert_eq!(latched.len(), 2);
    assert_eq!(
        latched[0],
        DeviceCommand::default(),
        "nothing latched at the first connect"
    );
    assert_eq!(
        latched[1],
        DeviceCommand::default()
            .with_mode(DetectionMode::Putting)
            .with_handed(Handedness::Right)
            .with_club(Club::PitchingWedge),
        "the last writer wins per field"
    );
}

#[test]
fn a_stop_ends_a_blocked_connect_within_stop_wait() {
    let h = Harness::listen();
    h.control.plan(Plan::Block {
        release: Arc::new(AtomicBool::new(false)),
        alert: None,
    });
    wait_until("the connect to start", || h.control.connects() == 1);
    let at = Instant::now();
    let result = h.stop_and_join();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        at.elapsed() < <Fake as DeviceDriver>::STOP_WAIT,
        "{:?}",
        at.elapsed()
    );
}

#[test]
fn a_stop_closes_the_session_and_tells_controllers_the_device_is_not_ready() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ ");
    h.control.run(|out| out.ready(true));
    peer.until(|v| v["event"]["telemetry"]["ready"] == "true");
    let control = h.control.clone();
    h.stop.stop();
    let rest = peer.until(|v| v["event"]["telemetry"]["ready"] == "false");
    assert_eq!(telemetry(&rest, "ready"), ["false"]);
    assert!(
        peer.drain(Duration::from_millis(300)).is_empty(),
        "then the connection closes"
    );
    h.stop_and_join().unwrap();
    assert_eq!(control.inner.closes.load(Ordering::SeqCst), 1);
}

#[test]
fn raw_frames_go_only_to_controllers_that_asked_and_seq_continues_across_sessions() {
    let h = Harness::listen();
    let addr = h.addr();
    let wanted = Arc::new(Mutex::new(Vec::<bool>::new()));
    let probe = |wanted: &Arc<Mutex<Vec<bool>>>| {
        let wanted = Arc::clone(wanted);
        move |out: &mut Events<'_>| wanted.lock().unwrap().push(out.wants_raw())
    };
    let latest = |wanted: &Arc<Mutex<Vec<bool>>>| wanted.lock().unwrap().last().copied();

    let mut quiet = Peer::connect(&addr, false);
    h.wait_for("device+ ");
    h.control.run(probe(&wanted));
    wait_until("a probe", || latest(&wanted) == Some(false));

    // A second controller asks: `wants_raw()` flips on the live session.
    let mut asking = Peer::connect(&addr, true);
    h.wait_for_nth("controller+ ", 2);
    h.control.run(probe(&wanted));
    wait_until("wants_raw to flip", || latest(&wanted) == Some(true));

    let frames = |first: u8| {
        move |out: &mut Events<'_>| {
            for i in 0..3 {
                out.raw(RawDir::Rx, format!("rx result {i}"), &[first, i]);
            }
        }
    };
    h.control.run(frames(0xa0));
    let got = asking.until(|v| kind(v) == "raw_frame" && v["event"]["seq"] == 2);
    let raw: Vec<&Value> = got.iter().filter(|v| kind(v) == "raw_frame").collect();
    assert_eq!(raw.len(), 3);
    for (i, v) in raw.iter().enumerate() {
        assert_eq!(v["event"]["seq"], i);
        assert_eq!(v["event"]["dir"], "rx");
        assert_eq!(v["event"]["label"], format!("rx result {i}"));
        assert_eq!(v["event"]["data"], format!("a0{i:02x}"));
    }
    assert!(
        !kinds(&quiet.drain(Duration::from_millis(300))).contains(&"raw_frame"),
        "a controller that did not ask gets no frames"
    );

    // A new session of the same device keeps numbering.
    h.control.fail("reconnect");
    h.wait_for_nth("device+ ", 2);
    h.control.run(frames(0xb0));
    let got = asking.until(|v| kind(v) == "raw_frame" && v["event"]["seq"] == 5);
    let seqs: Vec<u64> = got
        .iter()
        .filter(|v| kind(v) == "raw_frame")
        .map(|v| v["event"]["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [3, 4, 5]);

    // The asking controller leaves: `wants_raw()` flips back.
    drop(asking);
    wait_until("wants_raw to flip back", || {
        h.control.run(probe(&wanted));
        std::thread::sleep(Duration::from_millis(60));
        latest(&wanted) == Some(false)
    });
}

/// A device that emits `count` alerts of about 100 KB each, every one different.
fn flood(control: &Control, count: usize) {
    control.run(move |out| {
        for i in 0..count {
            out.alert(Severity::Warn, format!("{i}:{}", "x".repeat(100_000)));
        }
    });
}

/// Counts what a controller reads, on a thread of its own, until told to stop.
fn count_alerts(mut peer: Peer, want: usize) -> JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut count = 0;
        let give_up = Instant::now() + Duration::from_secs(40);
        while count < want && Instant::now() < give_up {
            if let Some(v) = peer.next(Duration::from_millis(200))
                && kind(&v) == "alert"
            {
                count += 1;
            }
        }
        count
    })
}

#[test]
fn a_controller_that_does_not_read_is_queued_and_kept_while_the_others_are_unaffected() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.write_buffer_limit = 256 * 1024 * 1024;
    });
    let stalled = h.named_peer("stalled"); // never reads again
    let reader = count_alerts(h.named_peer("reader"), 160);
    h.wait_for("device+ ");
    flood(&h.control, 160); // 16 MB: more than the kernel buffers hold
    assert_eq!(
        reader.join().unwrap(),
        160,
        "the reading controller gets everything"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !h.log().iter().any(|l| l.starts_with("controller- stalled")),
        "a full kernel buffer alone drops nobody: {:#?}",
        h.log()
            .iter()
            .filter(|l| !l.starts_with("device"))
            .collect::<Vec<_>>()
    );
    drop(stalled);
}

#[test]
fn a_controller_that_falls_behind_the_write_buffer_cap_is_dropped_alone() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| {
        c.write_buffer_limit = 512 * 1024;
    });
    let stalled = h.named_peer("stalled");
    let reader = count_alerts(h.named_peer("reader"), 160);
    h.wait_for("device+ ");
    flood(&h.control, 160);
    let left = h.wait_for("controller- stalled");
    assert!(left.contains("write buffer full"), "{left}");
    assert_eq!(
        reader.join().unwrap(),
        160,
        "the other controller is unaffected"
    );
    assert!(
        !h.log()
            .iter()
            .any(|l| l.starts_with("controller- reader") && l.contains("buffer")),
        "{:#?}",
        h.log()
            .iter()
            .filter(|l| !l.starts_with("device"))
            .collect::<Vec<_>>()
    );
    drop(stalled);
}

#[test]
fn a_silent_client_cannot_hold_up_the_acceptor() {
    let h = Harness::listen();
    let addr = h.addr();
    let _silent = TcpStream::connect(&addr).unwrap(); // connects, says nothing
    let at = Instant::now();
    let mut peer = Peer::connect(&addr, false);
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
    h.wait_for("device+ ");
    assert_eq!(
        kind(&peer.next(Duration::from_secs(5)).unwrap()),
        "device_telemetry"
    );
}

#[test]
fn controllers_past_the_limit_are_refused() {
    let h = Harness::start(Transport::listen("127.0.0.1:0"), |c| c.max_controllers = 1);
    let addr = h.addr();
    let _first = Peer::connect(&addr, false);
    h.wait_for("controller+ ");
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/frp")).unwrap();
    ws.send(Message::text(
        json!({ "kind": "start", "version": ["0.2.0"] }).to_string(),
    ))
    .ok();
    let mut refused = false;
    for _ in 0..50 {
        match ws.read() {
            Ok(Message::Text(t)) if t.contains("\"init\"") => {}
            Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_)) => {}
            Ok(_) | Err(_) => {
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the second controller is closed");
    assert_eq!(
        h.log()
            .iter()
            .filter(|l| l.starts_with("controller+ "))
            .count(),
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
    let result = h.handle.as_ref().map(|j| {
        let give_up = Instant::now() + Duration::from_secs(5);
        while !j.is_finished() && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(5));
        }
        j.is_finished()
    });
    assert_eq!(result, Some(true), "the server returned by itself");
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

#[test]
fn a_bridge_dials_the_controller_and_redials_when_the_link_drops() {
    let controller = FrpEndpoint::open(EndpointConfig::new(
        Role::Controller,
        Transport::listen("127.0.0.1:0"),
    ))
    .unwrap();
    let addr = controller.local_addr().unwrap().unwrap();
    // The controller accepts twice: the server dials in, then dials again.
    let (tx, rx) = channel();
    let accepting = std::thread::spawn(move || {
        let mut controller = controller;
        for _ in 0..2 {
            let conn = controller.establish().expect("the server dialed");
            let _ = tx.send(conn);
        }
    });
    let h = Harness::start(Transport::connect(format!("ws://{addr}/frp")), |_| {});
    let mut conn = rx.recv_timeout(Duration::from_secs(10)).expect("dialed");
    assert_eq!(conn.role(), Role::Controller);
    h.wait_for("device+ ");
    h.control.run(|out| out.ready(true));
    let mut ready = false;
    let give_up = Instant::now() + Duration::from_secs(5);
    while !ready && Instant::now() < give_up {
        if let Ok(flightrelay::FrpMessage::Envelope(env)) = conn.recv() {
            ready |= matches!(
                env.event,
                flightrelay::FrpEvent::DeviceTelemetry { telemetry: Some(ref t), .. }
                    if t.get("ready").map(String::as_str) == Some("true")
            );
        }
    }
    assert!(ready, "events flow over the dialed link");
    drop(conn); // the link drops
    let mut conn = rx.recv_timeout(Duration::from_secs(15)).expect("redialed");
    // The new link starts with the device's merged state.
    let first = loop {
        match conn.recv().unwrap() {
            flightrelay::FrpMessage::Envelope(env) => break env,
            flightrelay::FrpMessage::Protocol(_) => {}
        }
    };
    assert_eq!(first.device, "EagleOne-X4K2");
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
fn a_bridge_that_cannot_reach_its_controller_says_so_once_at_the_default_level() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let url = format!("ws://{closed}/frp");
    let lines = |h: &Harness| -> Vec<String> {
        h.log()
            .into_iter()
            .filter(|l| l.contains("cannot reach the controller"))
            .collect()
    };

    // At the default level the failure shows once, however often it repeats.
    let info = Harness::start(Transport::connect(url.clone()), |_| {});
    info.wait_for("device Log(Info, \"cannot reach the controller");
    std::thread::sleep(Duration::from_millis(1600));
    assert_eq!(lines(&info).len(), 1, "{:#?}", info.log());
    drop(info);

    // At debug level the repeats show too, marked as debug.
    let debug = Harness::start(Transport::connect(url), |c| {
        c.log_level = flightrelay::driver::Level::Debug;
    });
    debug.wait_for("device Log(Debug, \"cannot reach the controller");
    let all = lines(&debug);
    assert!(all[0].starts_with("device Log(Info, "), "{all:#?}");
    assert!(
        all[1..].iter().all(|l| l.starts_with("device Log(Debug, ")),
        "{all:#?}"
    );
}

#[test]
fn telemetry_sent_before_the_device_is_identified_arrives_merged() {
    let h = Harness::listen();
    let release = Arc::new(AtomicBool::new(false));
    h.control.plan(Plan::Announce {
        release: Arc::clone(&release),
        identity: identity(),
        telemetry: vec![
            vec![("phase", "searching")],
            vec![("phase", "searching")],
            vec![("phase", "found"), ("battery_pct", "80")],
        ],
        alert: "found it",
    });
    let mut peer = h.peer();
    release.store(true, Ordering::SeqCst);
    let seen = peer.drain(Duration::from_millis(500));
    let telemetry_events: Vec<&Value> = seen
        .iter()
        .filter(|v| kind(v) == "device_telemetry")
        .collect();
    assert_eq!(
        telemetry_events.len(),
        2,
        "identity, then one merged: {seen:#?}"
    );
    assert_eq!(telemetry_events[0]["event"]["manufacturer"], "Birdie Labs");
    assert_eq!(telemetry_events[1]["event"]["telemetry"]["phase"], "found");
    assert_eq!(
        telemetry_events[1]["event"]["telemetry"]["battery_pct"],
        "80"
    );
}

#[test]
fn what_a_driver_emits_while_reconnecting_is_not_the_old_devices() {
    let h = Harness::listen();
    let mut peer = h.peer();
    h.wait_for("device+ EagleOne-X4K2");
    h.control.run(|out| out.telemetry([("battery_pct", "80")]));
    peer.until(|v| v["event"]["telemetry"]["battery_pct"] == "80");

    // The next connect finds a different unit, and says things on the way.
    let release = Arc::new(AtomicBool::new(false));
    h.control.plan(Plan::Announce {
        release: Arc::clone(&release),
        identity: DeviceIdentity::new("EagleTwo-Q7Z9").with_model("Eagle Two"),
        telemetry: vec![vec![("phase", "pairing")]],
        alert: "from the new unit",
    });
    h.control.fail("swapped");
    peer.until(|v| v["message"] == "failed: swapped");
    release.store(true, Ordering::SeqCst);
    let seen = peer
        .until(|v| v["device"] == "EagleTwo-Q7Z9" && v["event"]["telemetry"]["phase"] == "pairing");

    // The alert is protocol-level, the telemetry waits for the new identity.
    let alert = seen
        .iter()
        .find(|v| v["message"] == "from the new unit")
        .expect("the alert reaches the controller");
    assert!(alert["device"].is_null(), "no device id: {alert}");
    for v in &seen {
        if v["device"] == "EagleOne-X4K2" {
            assert!(
                v["event"]["telemetry"]["phase"].is_null()
                    && v["event"]["message"] != "from the new unit",
                "attributed to the old device: {v}"
            );
        }
    }
    let identity_at = seen
        .iter()
        .position(|v| v["device"] == "EagleTwo-Q7Z9" && v["event"]["model"] == "Eagle Two")
        .unwrap();
    assert_eq!(
        identity_at,
        seen.len() - 2,
        "identity, then the held telemetry: {seen:#?}"
    );
}

#[test]
fn a_bridge_dials_again_after_a_controller_that_closes_at_once() {
    let controller = FrpEndpoint::open(EndpointConfig::new(
        Role::Controller,
        Transport::listen("127.0.0.1:0"),
    ))
    .unwrap();
    let addr = controller.local_addr().unwrap().unwrap();
    let (tx, rx) = channel();
    let accepting = std::thread::spawn(move || {
        let mut controller = controller;
        for n in 0..2 {
            let conn = controller.establish().expect("the server dialed");
            if n == 0 {
                drop(conn); // gone right after the handshake
            } else {
                let _ = tx.send(conn);
            }
        }
    });
    let h = Harness::start(Transport::connect(format!("ws://{addr}/frp")), |_| {});
    let mut conn = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("dialed again");
    let first = loop {
        match conn.recv().unwrap() {
            flightrelay::FrpMessage::Envelope(env) => break env,
            flightrelay::FrpMessage::Protocol(_) => {}
        }
    };
    assert_eq!(first.device, "EagleOne-X4K2");
    h.stop.stop();
    drop(h);
    accepting.join().unwrap();
}
