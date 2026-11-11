//! [`MockDriver`]: a simulated launch monitor.
//!
//! Requires the `mock` feature. The mock is a virtual device (its
//! [`Source`] is [`Virtual`](Source::Virtual)) that arms, reports a ball
//! ready and fires a shot at a fixed interval, with realistic values for the
//! mode in play. It needs no hardware, so it serves for tests of a
//! host, for `FrpServer` (feature `server`) in development, and for a demo.
//!
//! A shot carries the measured launch (`shot.launch`), the model-produced
//! flight (`shot.flight`, [`Basis::Modeled`]) and the club (`shot.club`).
//! Values follow the mode in the hints when the shot fires: full swing,
//! chipping or putting. The mode comes from the host's `golfer.turn` hints
//! at connect and from [`hints`](DeviceSession::hints) afterwards; no mode
//! hint means a full swing, as before any turn. The mock ignores handedness
//! and club.
//!
//! Its blockers follow the simulated device: `not_armed` from connecting
//! until it arms and again right after a shot, `no_ball` while armed with no
//! ball, none while the ball is ready.
//!
//! A host can hold shots back, e.g. between rounds: the mock asks
//! [`Events::shots_allowed`] before it numbers a shot or raises a blocker for
//! it, and asks again on the next poll, so a held shot takes no number and
//! fires as soon as shots are allowed again.
//!
//! ```
//! use flightrelay::driver::DeviceDriver;
//! use flightrelay::mock::{MockConfig, MockDriver};
//! use flightrelay::Source;
//!
//! // A shot every 5 seconds, for a demo.
//! let driver = MockDriver::new(MockConfig { shot_interval_s: 5.0, ..MockConfig::default() })?;
//! assert_eq!(MockDriver::info(driver.config()).source, Source::Virtual);
//! # Ok::<(), flightrelay::config::ConfigError>(())
//! ```
//!
//! With the `server` feature, `FrpServer::new(driver, config)` runs it as a
//! launch monitor actor on the bus.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, DriverConfig, is_default_value};
use crate::driver::{
    ConnectCtx, DeviceDriver, DeviceIdentity, DeviceSession, DriverInfo, Events, Level, NoAction,
    ShotIds, Stopped, TurnHints,
};
use crate::types::blockers;
use crate::{Basis, ClubData, Distance, FlightResult, Launch, Mode, Source, Velocity};

const DEFAULT_SHOT_INTERVAL_S: f64 = 30.0;
const DEFAULT_ARM_DELAY_S: f64 = 2.0;
const DEFAULT_BALL_READY_DELAY_S: f64 = 1.0;
const DEFAULT_POST_SHOT_DELAY_S: f64 = 1.0;

/// The longest any delay may be, in seconds.
const MAX_DELAY_S: f64 = 86_400.0;

/// Settings of the [`MockDriver`]. All delays are in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MockConfig {
    /// Time from the ball being ready to the shot. Default 30.
    #[serde(skip_serializing_if = "is_default_interval")]
    pub shot_interval_s: f64,
    /// Time from connecting (or re-arming) to being armed. Default 2.
    #[serde(skip_serializing_if = "is_default_arm")]
    pub arm_delay_s: f64,
    /// Time from armed to the ball being ready. Default 1.
    #[serde(skip_serializing_if = "is_default_ball_ready")]
    pub ball_ready_delay_s: f64,
    /// Time from a shot to re-arming. Default 1.
    #[serde(skip_serializing_if = "is_default_post_shot")]
    pub post_shot_delay_s: f64,
    /// The serial number the device reports (`MockLM-<serial>`). Default:
    /// unset, which reports `0001`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // a `skip_serializing_if` function takes a reference
fn is_default_interval(v: &f64) -> bool {
    is_default_value(v, &DEFAULT_SHOT_INTERVAL_S)
}
#[allow(clippy::trivially_copy_pass_by_ref)] // a `skip_serializing_if` function takes a reference
fn is_default_arm(v: &f64) -> bool {
    is_default_value(v, &DEFAULT_ARM_DELAY_S)
}
#[allow(clippy::trivially_copy_pass_by_ref)] // a `skip_serializing_if` function takes a reference
fn is_default_ball_ready(v: &f64) -> bool {
    is_default_value(v, &DEFAULT_BALL_READY_DELAY_S)
}
#[allow(clippy::trivially_copy_pass_by_ref)] // a `skip_serializing_if` function takes a reference
fn is_default_post_shot(v: &f64) -> bool {
    is_default_value(v, &DEFAULT_POST_SHOT_DELAY_S)
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            shot_interval_s: DEFAULT_SHOT_INTERVAL_S,
            arm_delay_s: DEFAULT_ARM_DELAY_S,
            ball_ready_delay_s: DEFAULT_BALL_READY_DELAY_S,
            post_shot_delay_s: DEFAULT_POST_SHOT_DELAY_S,
            serial: None,
        }
    }
}

impl DriverConfig for MockConfig {
    fn example() -> Self {
        Self {
            serial: Some(String::new()),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("shot_interval_s", self.shot_interval_s),
            ("arm_delay_s", self.arm_delay_s),
            ("ball_ready_delay_s", self.ball_ready_delay_s),
            ("post_shot_delay_s", self.post_shot_delay_s),
        ] {
            if !(0.0..=MAX_DELAY_S).contains(&value) {
                return Err(ConfigError::field(field, "must be 0 to 86400"));
            }
        }
        Ok(())
    }
}

impl MockConfig {
    /// A delay from a config value; one that is not a finite number (which
    /// `apply_config` does not validate) is no delay.
    fn secs(value: f64) -> Duration {
        if value.is_finite() {
            Duration::from_secs_f64(value.clamp(0.0, MAX_DELAY_S))
        } else {
            Duration::ZERO
        }
    }
}

/// Why a mock connect or session ended. It never fails on its own: only a
/// stop ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MockError {
    /// The host stopped.
    #[error("stopped")]
    Stopped,
}

impl From<Stopped> for MockError {
    fn from(_: Stopped) -> Self {
        Self::Stopped
    }
}

/// A simulated launch monitor. See the [module docs](self).
#[derive(Debug)]
pub struct MockDriver {
    config: MockConfig,
    ids: ShotIds,
}

impl DeviceDriver for MockDriver {
    type Config = MockConfig;
    type Error = MockError;
    type Action = NoAction;
    type Session = MockSession;

    fn info(_: &MockConfig) -> DriverInfo {
        DriverInfo::new("mock", "Mock launch monitor", Source::Virtual)
    }

    fn new(config: MockConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            ids: ShotIds::new(),
        })
    }

    fn config(&self) -> &MockConfig {
        &self.config
    }

    fn connect(&mut self, ctx: &mut ConnectCtx<'_>) -> Result<MockSession, MockError> {
        ctx.tick()?;
        let mode = ctx.hints().mode.unwrap_or(Mode::Full);
        Ok(MockSession {
            identity: DeviceIdentity::new(format!(
                "MockLM-{}",
                self.config.serial.as_deref().unwrap_or("0001")
            ))
            .with_model("Mock Launch Monitor")
            .with_label("Mock Launch Monitor (simulated)"),
            config: self.config.clone(),
            ids: self.ids.clone(),
            mode,
            phase: Phase::Start,
        })
    }

    fn apply_config(&mut self, new: &MockConfig) {
        self.config = new.clone();
    }
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Connected, nothing reported yet.
    Start,
    /// Just connected or re-arming, waiting to arm.
    Idle { since: Instant },
    /// Armed, waiting for the ball.
    Armed { since: Instant },
    /// Armed and the ball is detected, waiting for the shot timer.
    Ready { since: Instant },
    /// A shot just went out, waiting to re-arm.
    PostShot { since: Instant },
}

/// A connection to the [`MockDriver`].
#[derive(Debug)]
pub struct MockSession {
    identity: DeviceIdentity,
    config: MockConfig,
    ids: ShotIds,
    mode: Mode,
    phase: Phase,
}

impl MockSession {
    fn mode_telemetry(&self, out: &mut Events<'_>, radar: &str) {
        out.telemetry([
            ("tracking_mode", self.mode.to_string()),
            ("radar_mode", radar.to_owned()),
        ]);
    }

    fn fire(&mut self, out: &mut Events<'_>) {
        let key = self.ids.next_key();
        let shot = generate_shot(key.shot_number, self.mode);
        out.log(
            Level::Info,
            format!(
                "mock shot #{}: ball {:.1} mph, launch {:.1} deg, carry {:.1} yd",
                key.shot_number,
                shot.launch.launch_speed.map_or(0.0, Velocity::as_mph),
                shot.launch.launch_elevation.unwrap_or(0.0),
                shot.flight.carry_distance.map_or(0.0, Distance::as_yards),
            ),
        );
        out.blockers([blockers::NOT_ARMED]);
        self.mode_telemetry(out, "idle");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok());
        match now {
            Some(ms) => out.shot_trigger_at(key.clone(), ms),
            None => out.shot_trigger(key.clone()),
        }
        out.launch(key.clone(), shot.launch);
        out.flight(key.clone(), shot.flight);
        out.club(key.clone(), shot.club);
        out.shot_finished(key);
    }
}

impl DeviceSession for MockSession {
    type Driver = MockDriver;

    fn poll(&mut self, timeout: Duration, out: &mut Events<'_>) -> Result<(), MockError> {
        let c = self.config.clone();
        match self.phase {
            Phase::Start => {
                out.blockers([blockers::NOT_ARMED]);
                self.mode_telemetry(out, "idle");
                out.log(Level::Info, "mock: connected");
                self.phase = Phase::Idle {
                    since: Instant::now(),
                };
            }
            Phase::Idle { since } if since.elapsed() >= MockConfig::secs(c.arm_delay_s) => {
                out.blockers([blockers::NO_BALL]);
                self.mode_telemetry(out, "armed");
                self.phase = Phase::Armed {
                    since: Instant::now(),
                };
            }
            Phase::Armed { since } if since.elapsed() >= MockConfig::secs(c.ball_ready_delay_s) => {
                out.ready(true);
                self.phase = Phase::Ready {
                    since: Instant::now(),
                };
            }
            Phase::Ready { since }
                if since.elapsed() >= MockConfig::secs(c.shot_interval_s)
                    && out.shots_allowed() =>
            {
                self.fire(out);
                self.phase = Phase::PostShot {
                    since: Instant::now(),
                };
            }
            Phase::PostShot { since }
                if since.elapsed() >= MockConfig::secs(c.post_shot_delay_s) =>
            {
                self.phase = Phase::Idle {
                    since: Instant::now(),
                };
            }
            _ => {}
        }
        std::thread::sleep(timeout.min(Duration::from_millis(100)));
        Ok(())
    }

    fn hints(&mut self, hints: &TurnHints) -> Result<(), MockError> {
        // The new mode shows in the telemetry of the next arm; a mode change
        // leaves the timers alone. No mode hint is a full swing.
        self.mode = hints.mode.unwrap_or(Mode::Full);
        Ok(())
    }

    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    fn close(self, out: &mut Events<'_>) -> Result<(), MockError> {
        out.blockers([blockers::NOT_ARMED]);
        Ok(())
    }
}

/// The values of one simulated shot.
struct Shot {
    launch: Launch,
    flight: FlightResult,
    club: ClubData,
}

/// A shot for shot number `n` in `mode`: values vary smoothly with `n`.
#[allow(clippy::many_single_char_names, clippy::cast_possible_truncation)]
fn generate_shot(n: u32, mode: Mode) -> Shot {
    let v = (f64::from(n) * 0.7).sin(); // -1..1 variation seed

    let (ball_speed, vla, hla, carry, height, backspin, sidespin, club_speed, aoa, loft) =
        match mode {
            Mode::Full => (
                53.0 + 3.0 * v,
                17.0 + 1.5 * v,
                0.8 * v,
                155.0 + 10.0 * v,
                26.0 + 2.0 * v,
                5500.0 + 300.0 * v,
                80.0 * v,
                36.0 + 2.0 * v,
                -4.0 + 0.8 * v,
                22.0 + 1.0 * v,
            ),
            Mode::Chipping => (
                18.0 + 2.0 * v,
                35.0 + 3.0 * v,
                0.5 * v,
                27.0 + 4.0 * v,
                8.0 + 1.5 * v,
                5000.0 + 400.0 * v,
                50.0 * v,
                25.0 + 2.0 * v,
                -5.0 + 1.0 * v,
                38.0 + 2.0 * v,
            ),
            Mode::Putting => (
                3.5 + 0.5 * v,
                2.0 + 0.3 * v,
                0.3 * v,
                4.5 + 1.0 * v,
                0.1 + 0.02 * v,
                200.0 + 40.0 * v,
                20.0 * v,
                3.0 + 0.4 * v,
                -2.0 + 0.5 * v,
                4.0 + 0.5 * v,
            ),
        };

    let launch = Launch {
        launch_speed: Some(Velocity::MetersPerSecond(ball_speed)),
        launch_elevation: Some(vla),
        launch_azimuth: Some(hla),
        backspin_rpm: Some(backspin as i32),
        sidespin_rpm: Some(sidespin as i32),
        ..Launch::default()
    };
    let flight = FlightResult {
        model: Some("Mock flight model".to_owned()),
        carry_distance: Some(Distance::Meters(carry)),
        total_distance: Some(Distance::Meters(carry * 1.1)),
        roll_distance: Some(Distance::Meters(carry * 0.1)),
        max_height: Some(Distance::Meters(height)),
        flight_time: Some(5.0),
        ..FlightResult::new(Basis::Modeled)
    };
    let club = ClubData {
        club_speed: Some(Velocity::MetersPerSecond(club_speed)),
        club_speed_post: Some(Velocity::MetersPerSecond(club_speed * 0.7)),
        path: Some(hla * 0.4),
        attack_angle: Some(aoa),
        face_angle: Some(hla * 0.7),
        dynamic_loft: Some(loft),
        smash_factor: Some(ball_speed / club_speed),
        swing_plane_horizontal: Some(0.0),
        swing_plane_vertical: Some(45.0),
        ..ClubData::default()
    };
    Shot {
        launch,
        flight,
        club,
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::FrpBody;
    use crate::driver::{ConnectHost, DeviceEvent, HostFlags, StatusChange, Stop};

    struct Host(TurnHints);

    impl ConnectHost for Host {
        fn tick(&mut self) {}
        fn hints(&self) -> &TurnHints {
            &self.0
        }
    }

    fn fast() -> MockConfig {
        MockConfig {
            shot_interval_s: 0.02,
            arm_delay_s: 0.01,
            ball_ready_delay_s: 0.01,
            post_shot_delay_s: 0.01,
            serial: None,
        }
    }

    fn connect(driver: &mut MockDriver, mode: Option<Mode>) -> MockSession {
        let stop = Stop::new();
        let flags = HostFlags::new();
        let mut sink = |_| {};
        let mut host = Host(TurnHints {
            mode,
            ..TurnHints::default()
        });
        let mut ctx = ConnectCtx::new(&stop, Events::new(&mut sink, &flags), &mut host);
        driver.connect(&mut ctx).expect("connect")
    }

    /// Polls until `done` says so, collecting the messages and status.
    fn drive(
        session: &mut MockSession,
        flags: &HostFlags,
        done: impl Fn(&[DeviceEvent]) -> bool,
    ) -> Vec<DeviceEvent> {
        let mut seen: Vec<DeviceEvent> = Vec::new();
        let give_up = Instant::now() + Duration::from_secs(5);
        while !done(&seen) {
            assert!(Instant::now() < give_up, "timed out; saw {seen:?}");
            let mut sink = |e: DeviceEvent| {
                if matches!(
                    e,
                    DeviceEvent::Frp(_) | DeviceEvent::Status(_) | DeviceEvent::StatusChange(_)
                ) {
                    seen.push(e);
                }
            };
            session
                .poll(Duration::from_millis(5), &mut Events::new(&mut sink, flags))
                .expect("poll");
        }
        seen
    }

    fn kinds(events: &[DeviceEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                DeviceEvent::Frp(FrpBody::ShotTrigger(_)) => "trigger",
                DeviceEvent::Frp(FrpBody::ShotLaunch(_)) => "launch",
                DeviceEvent::Frp(FrpBody::ShotFlight(_)) => "flight",
                DeviceEvent::Frp(FrpBody::ShotClub(_)) => "club",
                DeviceEvent::Frp(FrpBody::ShotFinished(_)) => "finished",
                DeviceEvent::Status(_) | DeviceEvent::StatusChange(_) => "status",
                _ => "other",
            })
            .collect()
    }

    /// Each blocker list the driver set, joined; `""` for none.
    fn blocker_values(events: &[DeviceEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                DeviceEvent::StatusChange(StatusChange::Blockers(b)) => Some(b.join(",")),
                _ => None,
            })
            .collect()
    }

    fn bodies(events: &[DeviceEvent]) -> impl Iterator<Item = &FrpBody> {
        events.iter().filter_map(|e| match e {
            DeviceEvent::Frp(b) => Some(b),
            _ => None,
        })
    }

    #[test]
    fn it_is_a_virtual_device_with_the_mock_card() {
        let info = MockDriver::info(&MockConfig::default());
        assert_eq!(info.source, Source::Virtual);
        assert!(info.raw.is_none() && info.fixed_mode.is_none() && !info.exclusive);
        let mut driver = MockDriver::new(MockConfig::default()).unwrap();
        let session = connect(&mut driver, None);
        assert_eq!(
            session.identity().label(),
            "Mock Launch Monitor (simulated)"
        );
    }

    #[test]
    fn a_session_arms_signals_ready_and_fires_a_complete_shot() {
        let mut driver = MockDriver::new(fast()).unwrap();
        let mut session = connect(&mut driver, Some(Mode::Chipping));
        let flags = HostFlags::new();
        let seen = drive(&mut session, &flags, |s| kinds(s).contains(&"finished"));

        // Not armed on connect, no ball once armed, nothing in the way with
        // the ball ready, then not armed again as the shot goes out.
        assert_eq!(
            blocker_values(&seen),
            ["not_armed", "no_ball", "", "not_armed"]
        );
        let order = kinds(&seen);
        let shot: Vec<&str> = order.iter().copied().filter(|k| *k != "status").collect();
        assert_eq!(shot, ["trigger", "launch", "flight", "club", "finished"]);

        // The telemetry keys come from the driver, in the mode the host gave.
        let tracking: Vec<&str> = seen
            .iter()
            .filter_map(|e| match e {
                DeviceEvent::StatusChange(StatusChange::Telemetry(t)) => {
                    t.get("tracking_mode").and_then(Option::as_deref)
                }
                _ => None,
            })
            .collect();
        assert!(
            !tracking.is_empty() && tracking.iter().all(|m| *m == "chipping"),
            "{tracking:?}"
        );

        // One key throughout; the flight is modeled and carries the distances.
        let keys: Vec<_> = bodies(&seen).filter_map(FrpBody::shot_key).collect();
        assert_eq!(keys.len(), 5);
        assert!(keys.iter().all(|k| *k == keys[0]));
        assert_eq!(keys[0].shot_number, 1);
        let flight = bodies(&seen).find_map(|b| match b {
            FrpBody::ShotFlight(m) => Some(&m.flight),
            _ => None,
        });
        let flight = flight.unwrap();
        assert_eq!(flight.basis, Basis::Modeled);
        assert!(flight.carry_distance.is_some() && flight.total_distance.is_some());
        assert!(flight.max_height.is_some() && flight.flight_time.is_some());
        assert!(flight.roll_distance.is_some());
        let launch = bodies(&seen).find_map(|b| match b {
            FrpBody::ShotLaunch(m) => Some(&m.launch),
            _ => None,
        });
        let launch = launch.unwrap();
        assert!(launch.launch_speed.is_some() && launch.launch_elevation.is_some());
        assert!(launch.backspin_rpm.is_some() && launch.sidespin_rpm.is_some());
        assert!(
            bodies(&seen).any(|b| matches!(b, FrpBody::ShotTrigger(t) if t.timestamp.is_some())),
            "the trigger carries the clock"
        );
    }

    #[test]
    fn values_follow_the_mode_and_shot_numbers_keep_counting_across_sessions() {
        let mut driver = MockDriver::new(fast()).unwrap();
        let flags = HostFlags::new();
        let speed = |mode| {
            generate_shot(1, mode)
                .launch
                .launch_speed
                .map(Velocity::as_mps)
                .unwrap()
        };
        assert!(speed(Mode::Full) > speed(Mode::Chipping));
        assert!(speed(Mode::Chipping) > speed(Mode::Putting));

        let mut first = connect(&mut driver, None);
        let seen = drive(&mut first, &flags, |s| kinds(s).contains(&"finished"));
        drop(first);
        let mut second = connect(&mut driver, Some(Mode::Putting));
        let again = drive(&mut second, &flags, |s| kinds(s).contains(&"finished"));
        let number = |events: &[DeviceEvent]| {
            bodies(events).find_map(|b| match b {
                FrpBody::ShotTrigger(t) => Some(t.key.shot_number),
                _ => None,
            })
        };
        assert_eq!((number(&seen), number(&again)), (Some(1), Some(2)));
    }

    #[test]
    fn hints_change_the_mode_of_the_next_shot() {
        let mut driver = MockDriver::new(MockConfig {
            shot_interval_s: 0.2,
            ..fast()
        })
        .unwrap();
        let mut session = connect(&mut driver, Some(Mode::Full));
        let flags = HostFlags::new();
        let _ = drive(&mut session, &flags, |s| blocker_values(s).len() >= 2);
        session
            .hints(
                &TurnHints::default()
                    .with_mode(Mode::Putting)
                    .with_handedness(crate::Handedness::Left),
            )
            .unwrap();
        let seen = drive(&mut session, &flags, |s| kinds(s).contains(&"finished"));
        let launch = bodies(&seen).find_map(|b| match b {
            FrpBody::ShotLaunch(m) => Some(&m.launch),
            _ => None,
        });
        let speed = launch.unwrap().launch_speed.unwrap().as_mps();
        assert!(speed < 5.0, "a putt: {speed}");
    }

    #[test]
    fn no_mode_hint_is_a_full_swing() {
        let mut driver = MockDriver::new(MockConfig {
            shot_interval_s: 0.2,
            ..fast()
        })
        .unwrap();
        let mut session = connect(&mut driver, Some(Mode::Putting));
        let flags = HostFlags::new();
        let _ = drive(&mut session, &flags, |s| blocker_values(s).len() >= 2);
        session.hints(&TurnHints::default()).unwrap();
        let seen = drive(&mut session, &flags, |s| kinds(s).contains(&"finished"));
        let launch = bodies(&seen).find_map(|b| match b {
            FrpBody::ShotLaunch(m) => Some(&m.launch),
            _ => None,
        });
        let speed = launch.unwrap().launch_speed.unwrap().as_mps();
        assert!(speed > 5.0, "a full swing: {speed}");
    }

    #[test]
    fn a_held_shot_takes_no_number_and_fires_when_allowed() {
        let mut driver = MockDriver::new(fast()).unwrap();
        let mut session = connect(&mut driver, None);
        let flags = HostFlags::new();
        flags.set_shots_allowed(false);
        // Ready and past the interval, but held: no trigger, no new blocker.
        let held = drive(&mut session, &flags, |s| {
            blocker_values(s).contains(&String::new())
        });
        let more = {
            let mut seen = Vec::new();
            for _ in 0..20 {
                let mut sink = |e: DeviceEvent| {
                    if matches!(
                        e,
                        DeviceEvent::Frp(_) | DeviceEvent::Status(_) | DeviceEvent::StatusChange(_)
                    ) {
                        seen.push(e);
                    }
                };
                session
                    .poll(
                        Duration::from_millis(5),
                        &mut Events::new(&mut sink, &flags),
                    )
                    .unwrap();
            }
            seen
        };
        assert!(!kinds(&held).contains(&"trigger") && !kinds(&more).contains(&"trigger"));
        assert!(blocker_values(&more).is_empty(), "no blocker while held");
        assert_eq!(session.ids.count(), 0, "no shot number taken");

        flags.set_shots_allowed(true);
        let seen = drive(&mut session, &flags, |s| kinds(s).contains(&"finished"));
        let trigger = bodies(&seen).find_map(|b| match b {
            FrpBody::ShotTrigger(t) => Some(t.key.shot_number),
            _ => None,
        });
        assert_eq!(trigger, Some(1));
    }

    #[test]
    fn close_reports_not_armed() {
        let mut driver = MockDriver::new(MockConfig::default()).unwrap();
        let session = connect(&mut driver, None);
        let flags = HostFlags::new();
        let mut seen = Vec::new();
        let mut sink = |e: DeviceEvent| seen.push(e);
        session.close(&mut Events::new(&mut sink, &flags)).unwrap();
        assert_eq!(blocker_values(&seen), ["not_armed"]);
    }

    #[test]
    fn the_serial_is_optional_and_names_the_device() {
        let mut driver = MockDriver::new(fast()).unwrap();
        assert_eq!(connect(&mut driver, None).identity().device, "MockLM-0001");
        let mut driver = MockDriver::new(MockConfig {
            serial: Some("ABC9".to_owned()),
            ..fast()
        })
        .unwrap();
        assert_eq!(connect(&mut driver, None).identity().device, "MockLM-ABC9");
        let config = MockConfig::from_map(
            serde_json::json!({ "serial": "7" })
                .as_object()
                .cloned()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(config.serial.as_deref(), Some("7"));
        assert_eq!(config.to_map().keys().collect::<Vec<_>>(), ["serial"]);
        assert!(MockConfig::default().to_map().is_empty());
    }

    #[test]
    fn config_checks_its_ranges_and_writes_only_what_differs() {
        #[cfg(feature = "main")]
        {
            let help = MockConfig::help_toml();
            for line in [
                "shot_interval_s = 30.0",
                "arm_delay_s = 2.0",
                "ball_ready_delay_s = 1.0",
                "post_shot_delay_s = 1.0",
                "# serial = (unset)",
            ] {
                assert!(help.contains(line), "{help}");
            }
        }
        assert!(MockConfig::default().to_map().is_empty());
        let config = MockConfig::from_map(
            serde_json::json!({ "shot_interval_s": 5, "arm_delay_s": 2 })
                .as_object()
                .cloned()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(config.shot_interval_s, 5.0);
        assert_eq!(
            config.to_map().keys().collect::<Vec<_>>(),
            ["shot_interval_s"]
        );
        for bad in [
            "shot_interval_s",
            "arm_delay_s",
            "ball_ready_delay_s",
            "post_shot_delay_s",
        ] {
            for value in [-1.0, 100_000.0] {
                let map = serde_json::json!({ bad: value })
                    .as_object()
                    .cloned()
                    .unwrap();
                let err = MockConfig::from_map(map).unwrap_err();
                assert_eq!(err.field.as_deref(), Some(bad));
                assert_eq!(err.message, "must be 0 to 86400");
            }
        }
        assert!(
            MockDriver::new(MockConfig {
                arm_delay_s: f64::NAN,
                ..MockConfig::default()
            })
            .is_err()
        );
    }

    #[test]
    fn a_config_applied_without_validation_cannot_panic_a_session() {
        let mut driver = MockDriver::new(fast()).unwrap();
        driver.apply_config(&MockConfig {
            shot_interval_s: f64::INFINITY,
            arm_delay_s: f64::NAN,
            ball_ready_delay_s: f64::NAN,
            serial: None,
            post_shot_delay_s: f64::NEG_INFINITY,
        });
        let mut session = connect(&mut driver, None);
        let flags = HostFlags::new();
        let seen = drive(&mut session, &flags, |s| kinds(s).contains(&"finished"));
        assert!(kinds(&seen).contains(&"launch"), "a shot, without a panic");
    }

    #[test]
    fn apply_config_replaces_the_driver_config() {
        let mut driver = MockDriver::new(MockConfig::default()).unwrap();
        driver.apply_config(&fast());
        assert_eq!(driver.config(), &fast());
    }
}
