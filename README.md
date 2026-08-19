# flightrelay

Rust SDK for the [Flight Relay Protocol (FRP)](https://github.com/flightrelay/spec) — golf launch monitor event streaming over WebSocket.

[![CI](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/flightrelay.svg)](https://crates.io/crates/flightrelay)

## What's in the box

- **Message types** — strongly typed Rust representations of every FRP message, event, and command
- **Unit-tagged values** — `Distance` and `Velocity` types that serialize as unit-tagged strings (`"180.5m"`, `"67.2mps"`) with full unit conversion
- **Shot accumulator** — `ShotAggregator` collects lifecycle events (`shot_trigger` → `ball_flight` + `club_path` + `face_impact` → `shot_finished`) and yields a composed `CompletedShot`
- **WebSocket transport** — `FrpEndpoint` handles the FRP handshake for either role, in either transport direction, and yields `FrpConnection`s with synchronous send/recv

## Features

| Feature | Description |
|---|---|
| *(default)* | Message types, parsing, unit-tagged values |
| `controller` | `ShotAggregator` for accumulating shot lifecycle events |
| `device` | Reserved for future device-side helpers |
| `transport` | `FrpEndpoint` / `FrpConnection` — WebSocket transport for either role |

The `transport` feature adds a dependency on `tungstenite`.

## Roles and transport direction

FRP roles are **entity names, not directions**. A **device** emits shot data and accepts commands; a **controller** receives shot data and sends commands. Either role may open the WebSocket or accept it, and the handshake always runs controller→device (`start`) and device→controller (`init`) regardless of who dialed.

`FrpEndpoint` keeps these axes separate — `Role` picks the handshake direction, `Transport` picks who opens the socket:

| | `Transport::Listen` | `Transport::Connect` |
|---|---|---|
| `Role::Device` | launch monitor serving simulators | bridge dialing into a central bus |
| `Role::Controller` | bus accepting bridges | simulator connecting to a monitor |

An endpoint is long-lived and outlives its connections. Call `establish()` again to reconnect — the caller owns any retry or backoff policy.

## Usage

### Controller connecting to a device

```rust
use flightrelay::{EndpointConfig, FrpEndpoint, FrpMessage, Role, ShotAggregator, Transport};

let mut ep = FrpEndpoint::open(
    EndpointConfig::new(Role::Controller, Transport::connect("ws://192.168.1.50:5880/frp"))
        .with_name("My App"),
)?;
let mut conn = ep.establish()?;
let mut shots = ShotAggregator::new();

loop {
    if let FrpMessage::Envelope(envelope) = conn.recv()? {
        if let Some(shot) = shots.feed_envelope(&envelope) {
            println!("{}: carry {}", shot.device, shot.ball.unwrap().carry_distance.unwrap());
        }
    }
}
```

```toml
[dependencies]
flightrelay = { version = "0.1", features = ["transport", "controller"] }
```

### Device serving controllers

```rust
use flightrelay::{BallFlight, Distance, EndpointConfig, FrpEndpoint, FrpEnvelope, FrpEvent,
                  Role, ShotKey, Transport, Velocity};

// Controllers connect to ws://host:5880/frp
let mut ep = FrpEndpoint::open(
    EndpointConfig::new(Role::Device, Transport::listen("0.0.0.0:5880")),
)?;
let mut conn = ep.establish()?;

// Send a shot
let key = ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 1);
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::shot_trigger(key.clone())))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::ball_flight(
    key.clone(),
    BallFlight {
        launch_speed: Some(Velocity::MetersPerSecond(67.2)),
        carry_distance: Some(Distance::Meters(180.5)),
        ..Default::default()
    },
)))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::shot_finished(key)))?;
```

```toml
[dependencies]
flightrelay = { version = "0.1", features = ["transport"] }
```

### Device bridging out to a central controller

The same device code, dialing out instead of listening — useful for a launch
monitor bridge reporting into a central bus:

```rust
use flightrelay::{EndpointConfig, FrpEndpoint, Role, Transport};

let mut ep = FrpEndpoint::open(
    EndpointConfig::new(Role::Device, Transport::connect("ws://flighthook:5880/frp"))
        .with_name("ironsight"),
)?;

loop {
    let mut conn = ep.establish()?;   // dials, waits for `start`, replies `init`
    pump_shots(&mut conn);            // returns when the connection drops
    std::thread::sleep(backoff);
}
```

### Messages only (no WebSocket)

```rust
use flightrelay::{FrpMessage, Distance};

let msg = FrpMessage::parse(r#"{"device":"EagleOne-X4K2","event":{"kind":"ball_flight","key":{"shot_id":"...","shot_number":1},"ball":{"carry_distance":"180.5m"}}}"#)?;
let json = msg.to_json()?;

// Unit conversion
let d = Distance::Meters(180.5);
assert_eq!(d.as_yards(), 197.4); // approximate
```

```toml
[dependencies]
flightrelay = "0.0.1"
```

## Protocol

FRP is a minimal WebSocket protocol. After a `start`/`init` handshake, the device streams JSON events for each shot lifecycle stage:

```
shot_trigger → ball_flight + club_path + face_impact (any order) → shot_finished
```

Events arrive as they become available — ball flight data may arrive immediately while club and face impact data take longer to process. Controllers accumulate events by `shot_id` until `shot_finished`.

See the [full spec](https://github.com/flightrelay/spec) for details.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT), at your option.
