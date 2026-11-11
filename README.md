# flightrelay

Rust SDK for the [Flight Relay Protocol (FRP)](https://github.com/flightrelay/spec) — a message bus for golf simulators: launch monitors, games, flight models, sessions, lights and collectors, over WebSocket.

This crate implements FRP 0.3.0 (`SPEC_VERSION`).

[![CI](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/flightrelay.svg)](https://crates.io/crates/flightrelay)

## What's in the box

- **Message types** — one Rust type per FRP kind (all 21), in a flat `FrpMessage { from, body }`; unknown and extension kinds (`x.<vendor>.*`) parse as `FrpBody::Unknown` and serialize back unchanged
- **Unit-tagged values** — `Distance` and `Velocity` serialize as unit-tagged strings (`"180.5m"`, `"67.2mps"`) with unit conversion
- **Shot accumulator** — `ShotAggregator` composes a shot from its `shot.*` messages by `shot_id`, whoever sent each part, keeps every `shot.flight`, stamps the `golfer.turn` and `practice.block` current at the trigger, and reports a message that arrives after `shot.finished` as a warning
- **Bus state** — `BusState` keeps the current state of the bus by (kind, from, id), applies removals, and drops an actor after three missed heartbeats
- **WebSocket binding** — `FrpEndpoint` listens or dials and yields `FrpConnection`s with synchronous send/recv (`recv_timeout` waits no longer than a heartbeat is due); `greet` sends what a side sends on connecting (its `bus.actor`s, their state, then `bus.sync`)
- **Hub** — `Hub` forwards every message to every other connection, never back to its source
- **Device drivers** — a `DeviceDriver` trait for a launch monitor, `FrpServer` to run one as an actor on the bus, `run` for a complete `main`, and a simulated `MockDriver`

## Features

| Feature | Description |
|---|---|
| *(default)* | Message types, parsing, unit-tagged values, `new_actor_id` |
| `controller` | `ShotAggregator`, `BusState` |
| `transport` | `FrpEndpoint` / `FrpConnection`, `Hub` — the WebSocket binding |
| `config` | `DriverConfig`, `ConfigError`, `is_default` — serde settings for a driver |
| `driver` | the `DeviceDriver` / `DeviceSession` traits, `Events`, `TurnHints`, `DeviceStatus`, `StatusChange`, `Stop`, `Backoff`, `ShotIds`, `Live`, `connect_tcp` (implies `config`) |
| `server` | `FrpServer` (implies `driver` and `transport`) |
| `main` | `run`, `run_with`, `run_args`, `config_actor_id` (implies `server`) |
| `mock` | `MockDriver`, a simulated launch monitor (implies `driver`) |

The `transport` feature adds a dependency on `tungstenite`, and `main` adds `toml` and `ctrlc`. `config` and `driver` add no dependencies, and `config` builds for `wasm32-unknown-unknown`.

## The bus

FRP is a multi-producer, multi-consumer bus of **actors**. Every actor receives everything. Every message is one flat JSON object:

```json
{ "kind": "golfer.turn", "from": "c19e04b7a2d5f318", "golfer": "backnine.local-3", "club": "7I", "handedness": "rh", "mode": "full" }
```

- **State** (`bus.actor`, `golfer.profile`, `golfer.turn`, `practice.block`, `game.round`, `game.scores`, `game.ball`) is a complete object its owner republishes when it changes, when it starts, and in answer to `bus.sync`. The owner removes a state with a last message marked `"removed": true` (`FrpBody::Removed`).
- **Occurrences** (`bus.sync`, `bus.reply`, `bus.alert`, `shot.*`, `game.stroke`) are sent once.
- **Commands** (`golfer.select`, `practice.new_block`, `game.aim`) carry a `ref`; the actor holding the owning role (`session`, `practice`, `game`) applies one, publishes the changed state, and answers with `bus.reply`.

`bus.actor` doubles as the heartbeat: an actor republishes it every `heartbeat` seconds, and an actor not heard for three heartbeats is gone, with all its state. An actor id is a random string generated once by the host that runs the actor and kept across restarts (`new_actor_id()`); the host decides where to keep it.

There is no handshake. On connecting, each side sends `bus.actor` for each actor it hosts, then `bus.sync`, and the other side's actors republish their state. A hub forwards every message to every other connection, never back to the one it came from; hubs connected in a loop are a configuration error.

## Usage

### A collector

A collector is an actor like any other: it greets with its `bus.actor`, answers `bus.sync` by republishing it, and republishes it every heartbeat so the bus doesn't take it for gone. `recv_timeout` waits for the next message no longer than the next heartbeat is due, so one blocking loop does both.

```rust
use std::time::Instant;
use flightrelay::{BusActor, BusState, DEFAULT_HEARTBEAT, EndpointConfig, FrpBody, FrpEndpoint,
                  FrpMessage, ShotAggregator, ShotEvent, Transport};
use flightrelay::types::roles;

let me = FrpMessage::new(
    my_actor_id,
    BusActor::new("Shot Log", DEFAULT_HEARTBEAT.as_secs_f64()).with_role(roles::COLLECTOR),
);
let mut ep = FrpEndpoint::open(EndpointConfig::new(Transport::connect("ws://192.168.1.50:5880/frp")))?;
let mut conn = ep.establish()?;
conn.greet(&[me.clone()], &[])?;

let mut bus = BusState::new();
let mut shots = ShotAggregator::new();
let mut last_beat = Instant::now();
loop {
    let left = DEFAULT_HEARTBEAT.saturating_sub(last_beat.elapsed());
    let msg = match conn.recv_timeout(left) {
        Ok(msg) => msg,
        Err(e) if !e.is_fatal() => None, // not FRP: ignore it and keep reading
        Err(e) => return Err(e.into()), // the connection is gone
    };
    if let Some(msg) = msg {
        if matches!(msg.body, FrpBody::BusSync(_)) {
            conn.send(&me)?;
        }
        bus.apply(&msg);
        match shots.feed(&msg) {
            Some(ShotEvent::Finished(shot)) => {
                let golfer = shot.turn.as_ref().and_then(|t| t.golfer.as_ref());
                if let Some(carry) = shot.flight().and_then(|f| f.carry_distance) {
                    println!("{golfer:?}: carry {carry}");
                }
            }
            Some(ShotEvent::Late(late)) => eprintln!("warning: {late}"),
            None => {}
        }
    }
    if last_beat.elapsed() >= DEFAULT_HEARTBEAT {
        conn.send(&me)?; // the heartbeat
        last_beat = Instant::now();
    }
    for gone in bus.expire() {
        shots.forget_actor(&gone);
    }
}
```

```toml
[dependencies]
flightrelay = { version = "0.6", features = ["transport", "controller"] }
```

### A launch monitor

A launch monitor publishes its `bus.actor` (with a `device` block and its `blockers`), then each shot as `shot.trigger`, `shot.launch`, `shot.club`, `shot.impact`, at most one `shot.flight` of its own, and `shot.finished`. It reads `golfer.turn` for its tracking hints: handedness, club and mode, each applied if the device supports it. Every turn is complete: a `null` club means no club, and a `null` or unknown value in any field is no hint for that field.

```rust
use flightrelay::{BusActor, Basis, DeviceInfo, Distance, FlightResult, FrpMessage, Launch,
                  ShotFinished, ShotFlight, ShotKey, ShotLaunch, ShotTrigger, Source, Velocity};
use flightrelay::types::{blockers, roles};

let me = "7d2f9a41c03be856"; // generated once, kept by the host
let mut actor = BusActor::new("Eagle One X4K2", 2.0)
    .with_role(roles::LAUNCH_MONITOR)
    .with_device(DeviceInfo { source: Some(Source::Measured), ..DeviceInfo::default() });
actor.blockers = vec![blockers::NO_BALL.into()];
conn.greet(&[FrpMessage::new(me, actor)], &[])?;

let key = ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 1);
conn.send(&FrpMessage::new(me, ShotTrigger::new(key.clone())))?;
conn.send(&FrpMessage::new(me, ShotLaunch {
    key: key.clone(),
    launch: Launch { launch_speed: Some(Velocity::MetersPerSecond(67.2)), ..Default::default() },
}))?;
conn.send(&FrpMessage::new(me, ShotFlight {
    key: key.clone(),
    flight: FlightResult {
        model: Some("Eagle One flight model 2.1".into()),
        carry_distance: Some(Distance::Meters(180.5)),
        ..FlightResult::new(Basis::Modeled)
    },
}))?;
conn.send(&FrpMessage::new(me, ShotFinished { key }))?;
```

A launch monitor driver gets all of this — greeting, heartbeat, answering `bus.sync`, turn hints, raw frame numbering — from `FrpServer` (below).

### A hub

```rust
use flightrelay::{EndpointConfig, FrpEndpoint, Hub, HubEvent, Transport};

let mut ep = FrpEndpoint::open(EndpointConfig::new(Transport::listen("0.0.0.0:5880")))?;
let mut hub = Hub::new();
// For each connection: greet it with your own actors, then `hub.add(conn)?`.
// Then, regularly:
for event in hub.poll() {
    match event {
        HubEvent::Message { message, .. } => { /* already forwarded to every other peer */ }
        HubEvent::Invalid { .. } => {}          // not FRP: not forwarded
        HubEvent::Left { reason, .. } => eprintln!("peer left: {reason}"),
    }
}
hub.send(&my_message)?; // publish your own message to every peer
```

### Measured values and flight results

`Launch`, `ClubData` and `FaceImpact` carry measured values only. A sender omits a value it doesn't have instead of sending a placeholder. `complete_spin()` translates one spin pair into the other, and the translated pair is measured too. Carry, total and the rest of the flight go in a `FlightResult` with its required `Basis` (`tracked`, `modeled`); a modeled result can list the launch values its model assumed in `AssumedLaunch`.

A shot may have several flights: its launch monitor's own and any flight model's, told apart by `from` and `model`. `CompletedShot::flights` keeps every one, in arrival order; `CompletedShot::flight()` picks one to show (the first tracked, else the launch monitor's, else the first). A receiver that flies the ball itself reads `CompletedShot::launch_for_flight()`: the measured launch, with missing values filled from the shown flight's `assumed` values when it is modeled.

A receiver that needs measured data only (e.g. a tournament round) keeps shots whose `source` is `Source::Measured` — the source the launch monitor declares in its `bus.actor` — and ignores the flights.

### Late messages and the accumulator timeout

`ShotAggregator` discards a shot that has no `shot.finished` within 30 s of its trigger. A message for a finished shot that arrives within the timeout after it finished (e.g. a slow flight model's `shot.flight`) is attached; when it changed the shot, `feed` returns `ShotEvent::Late` with the updated shot, for the caller to log as a warning. Later than that, or for a discarded shot, it is ignored. Only the actor that sent a shot's `shot.trigger` can finish it, and the open shots of an actor that leaves are discarded.

### Raw frames

A launch monitor can publish the protocol messages behind its shots as `shot.raw` (`RawFrame`): one message per frame, as bytes (lowercase hex on the wire), with its direction, a per-actor `seq`, and optionally a timestamp, a label and the shot it belongs to. It does so only when its own configuration turns them on, and never for a frame that carries device or user identity. Raw frames never affect a shot. Relays may drop them.

### Device drivers

A driver connects one launch monitor to a host, as a launch monitor actor on the bus.

A driver is built in two steps. `DeviceDriver::new(config)` validates a config and builds the driver, with no I/O, and the driver keeps whatever outlives one connection (a shot counter, a retry clock). `connect(&mut ctx)` opens one `DeviceSession`: transport, handshake with the device, identity. The host then calls `poll(timeout, &mut events)` until it fails or the host stops, and `close` to release the device. The host owns the reconnect loop and asks the driver for the delay (`retry_delay`). Everything is synchronous and caller-driven.

A driver emits `DeviceEvent`s into the `Events` sink: `shot.*` messages and alerts (`out.launch(..)`, `out.flight(..)`, `out.alert(..)`), its status (`out.blockers([..])`, `out.ready(..)`, `out.battery(..)`, `out.attitude(..)`, `out.telemetry(..)`, `out.clear_telemetry(..)`, or the whole `DeviceStatus` at once with `out.status(..)`), and three host-only kinds (a phase, a log line, a discarded shot). The host holds the driver's current status and its `bus.actor` carries exactly that, so a driver takes a value off the same way it sets one:

```rust
use flightrelay::driver::DeviceStatus;

out.battery(80);
out.battery(None);                 // no reading any more: the battery leaves the actor
out.attitude(0.4, -0.2);
out.clear_telemetry(["radar_mode"]);
out.status(DeviceStatus::new()     // the whole status: anything left out is off the actor
    .with_blockers(["no_ball"])
    .with_battery(72)
    .with_telemetry("radar_mode", "armed"));
```

 It reads `golfer.turn` as `TurnHints`: each `golfer.turn` replaces all the hints, and a `null` or unrecognized field is `None`, no hint for that field, with nothing kept from an earlier turn. `DeviceSession::hints` gets the new set whenever it changes, and `ConnectCtx::hints()` shows the last complete turn's during a connect. The host keeps that set when the session actor goes away; what the hardware does for a field with no hint is the driver's business.

To run a driver on the bus, give it to `FrpServer` with the actor id the host keeps:

```rust
use flightrelay::driver::Stop;
use flightrelay::{FrpServer, ServerConfig, Transport};

let driver = Beacon::new(Config::default())?;
let stop = Stop::new();
FrpServer::new(driver, ServerConfig::new(Transport::listen("0.0.0.0:5880"), actor_id))
    .on_event(|event| eprintln!("{event:?}"))
    .run(&stop)?;
```

The server greets every peer with the actor's `bus.actor` and `bus.sync`, republishes `bus.actor` on change, every heartbeat (at least 0.1 s) and in answer to `bus.sync`, publishes the driver's messages under the actor id, forwards between its peers, finishes a shot a lost session left open, goes back to `DeviceStatus::not_armed()` (blocker `not_armed`, no battery, attitude or telemetry) while no session is up, and sends the actor's removal when it stops. `run` returns an error if its network thread dies. `Transport::Connect` dials a hub instead of listening.

`run` is a whole `main`:

```rust
fn main() -> std::process::ExitCode {
    flightrelay::run::<Beacon>()
}
```

Its command line is named flags, in any order: `--config <path>` (a TOML file with the driver's config at the top level and the runner's own `[frp]` table), `--target <listen-addr | ws://host:port/frp>`, repeatable `--set key=value` over the file (`--set frp.raw_frames=true` for the runner), `--log <info|debug|trace>`, and `--help`, which lists every config key with its default. The `[frp]` table holds `actor_id` — generated on the first run and written into the file, so the actor keeps its id; a file whose `[frp]` table is written in a shape the runner can't safely edit is left as it is, and the runner prints the line to add — and `raw_frames`. Ctrl-C closes the session, removes the actor and exits.

A binary that builds its own `FrpServer` keeps its actor id the same way with `config_actor_id(path)`: it reads `frp.actor_id` from the TOML file, or generates one and writes it in by the same rules, and says which (`ActorIdOrigin::Read`, `Saved`, `NotSaved`, `SaveFailed`).

```rust
let kept = flightrelay::config_actor_id("launch-monitor.toml")?;
let config = ServerConfig::new(Transport::listen("0.0.0.0:5880"), kept.id);
```

`MockDriver` (feature `mock`) is a virtual launch monitor that fires a shot at an interval, in the mode the turn asks for, for testing a host without hardware:

```text
cargo run --example frp-mock --features main,mock -- --set shot_interval_s=5
```

### Messages only (no WebSocket)

```rust
use flightrelay::{Distance, FrpBody, FrpMessage};

let msg = FrpMessage::parse(r#"{"kind":"shot.flight","from":"e4a7136b9f02cd58","key":{"shot_id":"550e8400-e29b-41d4-a716-446655440000","shot_number":42},"flight":{"basis":"modeled","model":"Fairway Physics 1.3","carry_distance":"181.2m"}}"#)?;
if let FrpBody::ShotFlight(f) = &msg.body {
    println!("{} carry {:?}", msg.from, f.flight.carry_distance);
}
let json = msg.to_json()?;

let d = Distance::Meters(180.5);
assert!((d.as_yards() - 197.4).abs() < 0.1);
```

```toml
[dependencies]
flightrelay = "0.6"
```

See the [full spec](https://github.com/flightrelay/spec) for details.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT), at your option.
