//! A simulated launch monitor served over FRP.
//!
//! ```text
//! cargo run --example frp-mock --features main,mock -- --set shot_interval_s=5
//! ```

use std::process::ExitCode;

use flightrelay::mock::MockDriver;

fn main() -> ExitCode {
    flightrelay::run::<MockDriver>()
}
