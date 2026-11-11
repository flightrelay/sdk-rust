//! A simulated launch monitor, run as a launch monitor actor on the bus.
//!
//! ```text
//! cargo run --example frp-mock --features main,mock -- --set shot_interval_s=5
//! ```
//!
//! Pass `--config <file>` to keep the actor's id across runs: the first run
//! writes it into the file's `[frp]` table.

use std::process::ExitCode;

use flightrelay::mock::MockDriver;

fn main() -> ExitCode {
    flightrelay::run::<MockDriver>()
}
