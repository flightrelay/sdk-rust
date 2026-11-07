# flightrelay

Rust SDK for the [Flight Relay Protocol (FRP)](https://github.com/flightrelay/spec) — golf launch monitor event streaming over WebSocket.

This crate implements FRP 0.2.0 (`SPEC_VERSION`). Its handshake proposes and accepts only that version, so it does not connect to FRP 0.1.0 peers.

[![CI](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/flightrelay/sdk-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/flightrelay.svg)](https://crates.io/crates/flightrelay)

## What's in the box

- **Message types** — strongly typed Rust representations of every FRP message, event, and command
- **Unit-tagged values** — `Distance` and `Velocity` types that serialize as unit-tagged strings (`"180.5m"`, `"67.2mps"`) with full unit conversion
- **Shot accumulator** — `ShotAggregator` collects lifecycle events (`shot_trigger` → `launch` + `flight_result` + `club_path` + `face_impact` → `shot_finished`) and yields a composed `CompletedShot`. Shots that don't finish within the accumulator timeout (30 s by default) are discarded, and their late events are ignored
- **Strike timestamp** — the optional `timestamp` on `shot_trigger`: when the device detected the strike, in milliseconds since the Unix epoch
- **Measured values and flight results** — `Launch` carries the measured launch; carry, total and the rest of the flight go in `FlightResult` with its `Basis` (`tracked`, `modeled`), and a modeled result can list the launch values its model assumed in `AssumedLaunch`; the device's `Source` (`measured`, `virtual`) goes on `device_telemetry`
- **Raw frames** — optional `RawFrame` debug events carrying the protocol messages behind a device's events, sent only to a controller that asks for them in `start`
- **Duplicate dropping for forwarders** — `ForwardFilter` drops repeated shot events, unchanged telemetry, repeated alerts and repeated raw frames before a bridge or hub forwards them, so an event that loops back dies out
- **WebSocket transport** — `FrpEndpoint` handles the FRP handshake for either role, in either transport direction, and yields `FrpConnection`s with synchronous send/recv

## Features

| Feature | Description |
|---|---|
| *(default)* | Message types, parsing, unit-tagged values |
| `controller` | `ShotAggregator` for accumulating shot lifecycle events |
| `device` | `ForwardFilter` for endpoints that forward events |
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
use flightrelay::{EndpointConfig, FrpEndpoint, FrpError, FrpMessage, Role, ShotAggregator, Transport};

let mut ep = FrpEndpoint::open(
    EndpointConfig::new(Role::Controller, Transport::connect("ws://192.168.1.50:5880/frp"))
        .with_name("My App"),
)?;
let mut conn = ep.establish()?;
let mut shots = ShotAggregator::new();

loop {
    let envelope = match conn.recv() {
        Ok(FrpMessage::Envelope(envelope)) => envelope,
        Ok(FrpMessage::Protocol(_)) | Err(FrpError::Json(_)) => continue, // ignore and keep reading
        Err(e) => return Err(e.into()), // the connection is gone
    };
    if let Some(shot) = shots.feed_envelope(&envelope) {
        if let Some(carry) = shot.flight.and_then(|flight| flight.carry_distance) {
            println!("{}: carry {carry}", shot.device);
        }
    }
}
```

```toml
[dependencies]
flightrelay = { version = "0.5", features = ["transport", "controller"] }
```

### Device serving controllers

```rust
use flightrelay::{Basis, Distance, EndpointConfig, FlightResult, FrpEndpoint, FrpEnvelope,
                  FrpEvent, Launch, Role, ShotKey, Transport, Velocity};

// Controllers connect to ws://host:5880/frp
let mut ep = FrpEndpoint::open(
    EndpointConfig::new(Role::Device, Transport::listen("0.0.0.0:5880")),
)?;
let mut conn = ep.establish()?;

// Send a shot
let key = ShotKey::new("550e8400-e29b-41d4-a716-446655440000", 1);
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::shot_trigger(key.clone())))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::launch(
    key.clone(),
    Launch {
        launch_speed: Some(Velocity::MetersPerSecond(67.2)),
        launch_elevation: Some(14.2),
        ..Default::default()
    },
)))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::flight_result(
    key.clone(),
    FlightResult {
        model: Some("MyDevice flight model 1.0 (Example Co)".into()),
        carry_distance: Some(Distance::Meters(180.5)),
        ..FlightResult::new(Basis::Modeled)
    },
)))?;
conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::shot_finished(key)))?;
```

```toml
[dependencies]
flightrelay = { version = "0.5", features = ["transport"] }
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

### Measured values and flight results

`Launch`, `ClubData` and `FaceImpact` carry measured values only. A device omits a value it doesn't have instead of sending a placeholder. `complete_spin()` translates one spin pair into the other, and the translated pair is measured too:

```rust
use flightrelay::{Basis, Distance, FlightResult, FrpEvent, Launch, Source};

let mut launch = Launch {
    total_spin_rpm: Some(3231),
    spin_axis: Some(-8.0),
    ..Default::default() // no launch_elevation: the hardware doesn't measure it
};
launch.complete_spin(); // adds backspin_rpm and sidespin_rpm

// Values that need a flight model go in a flight_result, at most one per shot
let flight = FlightResult {
    model: Some("Eagle One flight model 2.1 (Birdie Labs)".into()),
    carry_distance: Some(Distance::Meters(180.5)),
    ..FlightResult::new(Basis::Modeled)
};

// Declared per device on device_telemetry
let telemetry = FrpEvent::DeviceTelemetry {
    manufacturer: None,
    model: None,
    firmware: None,
    source: Some(Source::Measured),
    telemetry: None,
};
```

`basis` is required: a `flight_result` without `"tracked"` or `"modeled"` arrives as `FrpEvent::Unknown`, so controllers and forwarders ignore it, as they do any event of a known kind that doesn't match it. `ShotAggregator` keeps the first event of each kind for a shot, so `CompletedShot::flight` is the shot's first `flight_result`.

When the launch lacks a value the flight model needed, e.g. the spin of a chip whose speed and angles were measured but whose spin was not, a modeled result lists what the model assumed in `assumed`. An assumed value is not measured, so it never goes in `Launch`, and a field the shot's `launch` carries never goes in `assumed`. A controller that flies the ball itself reads `CompletedShot::launch_for_flight()`: the measured launch, with missing values filled from `assumed` of a modeled result (`Launch::fill_assumed` does the same without the aggregator). A field in both keeps the measured value, and `assumed` on a tracked result is ignored:

```rust
use flightrelay::{AssumedLaunch, Basis, Distance, FlightResult};

let flight = FlightResult {
    model: Some("Eagle One flight model 2.1 (Birdie Labs)".into()),
    carry_distance: Some(Distance::Meters(31.2)),
    assumed: Some(AssumedLaunch {
        backspin_rpm: Some(4500),
        sidespin_rpm: Some(0),
        ..Default::default()
    }),
    ..FlightResult::new(Basis::Modeled)
};

// On the controller
if let Some(launch) = shot.launch_for_flight() {
    my_model.fly(&launch);
}
```

A controller that needs measured data only (e.g. a tournament round) keeps shots from measured devices and ignores the flight result, assumed values included. `ShotAggregator` stamps each `CompletedShot` with the `source` its device last declared:

```rust
if let Some(mut shot) = shots.feed_envelope(&envelope) {
    if shot.source == Some(Source::Measured) {
        shot.flight = None;
        record(shot);
    }
}
```

### Strike timestamp and the accumulator timeout

A device can say when it detected the strike, in milliseconds since the Unix epoch (UTC) by its own clock:

```rust
use std::time::{SystemTime, UNIX_EPOCH};

let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
let trigger = FrpEvent::shot_trigger_at(key.clone(), now_ms);
```

`CompletedShot::timestamp` carries it. Clocks drift, so compare timestamps from different devices with a tolerance of your own.

`ShotAggregator` discards a shot that has no `shot_finished` within 30 s of its `shot_trigger`. After a shot finishes or is discarded, events for its `shot_id` are ignored for one more timeout, so a late event, or the same shot arriving by a second route, never starts a new partial shot:

```rust
use std::time::Duration;

let mut shots = ShotAggregator::new().with_timeout(Duration::from_secs(20));
shots.discard_expired(); // also runs on every feed
shots.forget_device("EagleOne-X4K2"); // e.g. after its connection closed
```

### Forwarding

A bridge or hub forwards events with their `device`, `shot_id`, `shot_number` and `timestamp` unchanged, and drops duplicates first. `ForwardFilter` (feature `device`) does the dropping:

```rust
use flightrelay::{ForwardFilter, FrpError, FrpMessage};

let mut filter = ForwardFilter::new();
loop {
    match upstream.recv() {
        Ok(FrpMessage::Envelope(envelope)) => {
            if filter.should_forward(&envelope) {
                downstream.send_envelope(&envelope)?;
            }
        }
        Ok(FrpMessage::Protocol(_)) | Err(FrpError::Json(_)) => {} // not forwarded
        Err(_) => break, // upstream closed: forget its devices, then reconnect
    }
}
```

- Shot events pass once per `(device, shot_id, kind)` within the window (30 s by default), so a shot keeps at most one `flight_result`.
- `device_telemetry` passes only when it changes the device's last-forwarded state. `filter.telemetry(device)` returns that state, for a controller that connects later.
- An alert with a device context passes once per identical `(device, severity, message)` within the window.
- A raw frame passes once per `(device, seq)` within the window. Its `key` plays no part: raw frames are not shot events.
- `FrpEvent::Unknown` never passes: an unknown kind, or a known kind whose fields don't match it.
- Telemetry state is kept per device until `filter.forget_device(device)`. Call it for each device of an upstream connection that closes.

A forwarder may add a `flight_result` from its own model to a shot whose device sent none. When the upstream `shot_finished` arrives, it builds one with `Basis::Modeled`, a `model` naming itself and, in `assumed`, any launch values its model filled in, and forwards it before the `shot_finished` if `should_forward` lets it through:

```rust
if filter.should_forward(&finished) {
    if let Some(flight) = my_model.fly(&launch) {
        let added = FrpEnvelope::new(finished.device.clone(), FrpEvent::flight_result(key.clone(), flight));
        if filter.should_forward(&added) { // false when the device sent one
            downstream.send_envelope(&added)?;
        }
    }
    downstream.send_envelope(&finished)?;
}
```

### Raw frames

For debugging, a device that speaks another protocol to its hardware can send the messages behind its events as `raw_frame` events: one message per frame, as bytes (lowercase hex on the wire), with its direction (`RawDir::Rx` received, `RawDir::Tx` sent), a per-device `seq` that increases by one per frame, and optionally a timestamp, a label and the shot it belongs to. Raw frames are off by default. A controller asks for them in `start`:

```rust
let mut ep = FrpEndpoint::open(
    EndpointConfig::new(Role::Controller, Transport::connect("ws://192.168.1.50:5880/frp"))
        .with_raw_frames(true),
)?;
```

A device sends them only when the controller asked, and never a frame that carries device or user identity (serial numbers, hardware addresses, network names, account data):

```rust
use flightrelay::{FrpEnvelope, FrpEvent, RawDir, RawFrame};

if conn.raw_frames_requested() && !carries_identity(&bytes) {
    let frame = RawFrame {
        label: Some("result".into()),
        key: Some(key.clone()),
        ..RawFrame::new(RawDir::Rx, next_seq(), bytes)
    };
    conn.send_envelope(&FrpEnvelope::new("MyDevice-001", FrpEvent::raw_frame(frame)))?;
}
```

Raw frames never affect a shot: `ShotAggregator` ignores them, even with a key. A `raw_frame` whose `dir`, `seq` or `data` is invalid (e.g. an odd number of hex digits) arrives as `FrpEvent::Unknown`. A forwarder may drop raw frames; one that forwards them passes them unchanged, only to a controller that asked for them.

### Messages only (no WebSocket)

```rust
use flightrelay::{FrpMessage, Distance};

let msg = FrpMessage::parse(r#"{"device":"EagleOne-X4K2","event":{"kind":"flight_result","key":{"shot_id":"...","shot_number":1},"flight":{"basis":"tracked","carry_distance":"180.5m"}}}"#)?;
let json = msg.to_json()?;

// Unit conversion
let d = Distance::Meters(180.5);
assert_eq!(d.as_yards(), 197.4); // approximate
```

```toml
[dependencies]
flightrelay = "0.5"
```

## Protocol

FRP is a minimal WebSocket protocol. After a `start`/`init` handshake, the device streams JSON events for each shot lifecycle stage:

```
shot_trigger → launch + flight_result + club_path + face_impact (any order) → shot_finished
```

Events arrive as they become available — ball launch data may arrive immediately while the flight result, club and face impact data take longer to process. Controllers accumulate events by `shot_id` until `shot_finished`.

See the [full spec](https://github.com/flightrelay/spec) for details.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT), at your option.
