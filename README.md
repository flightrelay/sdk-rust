# flightrelay

Rust SDK for the [Flight Relay Protocol (FRP)](https://github.com/flightrelay/spec) — golf launch monitor event streaming over WebSocket.

[![CI](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/flightrelay.svg)](https://crates.io/crates/flightrelay)

## What's in the box

- **Message types** — strongly typed Rust representations of every FRP message, event, and command
- **Unit-tagged values** — `Distance` and `Velocity` types that serialize as unit-tagged strings (`"180.5m"`, `"67.2mps"`) with full unit conversion
- **Shot accumulator** — `ShotAggregator` collects lifecycle events (`shot_trigger` → `ball_flight` + `club_path` + `face_impact` → `shot_finished`) and yields a composed `CompletedShot`
- **WebSocket transport** — `FrpClient` and `FrpListener`/`FrpConnection` handle the FRP handshake and provide synchronous send/recv

## Features

| Feature | Description |
|---|---|
| *(default)* | Message types, parsing, unit-tagged values |
| `controller` | `ShotAggregator` for accumulating shot lifecycle events |
| `device` | Reserved for future device-side helpers |
| `client` | `FrpClient` — WebSocket client (connects to a device) |
| `server` | `FrpListener` / `FrpConnection` — WebSocket server (accepts controllers) |

The `client` and `server` features add a dependency on `tungstenite`.

## Usage

### Controller (receiving shots)

```rust
use flightrelay::{FrpClient, ShotAggregator, FrpMessage};

// Connect to a device and perform the FRP handshake
let mut client = FrpClient::connect("192.168.1.50:5880", "My App")?;
let mut shots = ShotAggregator::new();

loop {
    let msg = client.recv()?;
    if let FrpMessage::Envelope(envelope) = msg {
        if let Some(shot) = shots.feed_envelope(&envelope) {
            println!("{}: carry {}", shot.device, shot.ball.unwrap().carry_distance.unwrap());
        }
    }
}
```

```toml
[dependencies]
flightrelay = { version = "0.1", features = ["client", "controller"] }
```

### Device (emitting shots)

```rust
use flightrelay::{FrpListener, FrpEnvelope, FrpEvent, ShotKey, BallFlight, Velocity, Distance};

// Listen for controllers
let listener = FrpListener::bind("0.0.0.0:5880", &["0.1.0"])?;
let mut conn = listener.accept()?;

// Send a shot
let key = ShotKey::new(1);
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::shot_trigger(key.clone())))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::ball_flight(
    key.clone(),
    BallFlight {
        launch_speed: Some(Velocity::mps(67.2)),
        carry_distance: Some(Distance::meters(180.5)),
        ..Default::default()
    },
)))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::shot_finished(key)))?;
```

```toml
[dependencies]
flightrelay = { version = "0.1", features = ["server"] }
```

### Messages only (no WebSocket)

```rust
use flightrelay::{FrpMessage, Distance};

let msg = FrpMessage::parse(r#"{"device":"EagleOne-X4K2","event":{"kind":"ball_flight","key":{"shot_id":"...","shot_number":1},"ball":{"carry_distance":"180.5m"}}}"#)?;
let json = msg.to_json()?;

// Unit conversion
let d = Distance::meters(180.5);
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
