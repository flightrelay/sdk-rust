//! A host for driver tests: one thread, no sockets, time in steps.
//!
//! Requires the `testing` feature. [`TestHost`] holds the host's flags, the
//! hints, a stop token, a clock that moves only when the test moves it, and
//! a sink that collects every event. A test connects through it, steps the
//! session with an explicit `now`, and reads what the driver emitted:
//!
//! ```
//! # use std::time::Duration;
//! # use flightrelay::driver::{FrpDriver, FrpSession};
//! # use flightrelay::mock::{MockConfig, MockDriver};
//! use flightrelay::testing::TestHost;
//!
//! let mut host = TestHost::new();
//! let mut driver = MockDriver::new(MockConfig::default()).unwrap();
//! let mut session = host.connect(&mut driver).unwrap();
//! // Five virtual seconds, in microseconds of real time.
//! host.run_for(Duration::from_secs(5), |now, out| session.step(now, out)).unwrap();
//! assert!(!host.take_frp().is_empty());
//! ```
//!
//! The free functions read a slice of events the way a test asks about
//! them: the message [`bodies`] and their [`kinds`], the [`alerts`],
//! [`launches`], [`flights`], [`clubs`], [`raw_frames`] and [`logs`], and
//! the device's [`status`] as a host holds it (or every one in turn,
//! [`statuses`]).

use std::time::{Duration, Instant};

use crate::driver::{
    Activity, ConnectCtx, ConnectHost, DeviceEvent, DeviceStatus, Events, FrpDriver, FrpSession,
    HostFlags, IDLE, Level, Stop, TurnHints,
};
use crate::{BusAlert, ClubData, FlightResult, FrpBody, FrpMessage, Launch, RawFrame};

/// How many [`Activity::Active`] steps in a row [`TestHost::run_for`] takes before
/// it decides the session never idles.
const MAX_BUSY_STEPS: u32 = 10_000;

/// The host side of a driver test. See the [module docs](self).
#[derive(Debug)]
pub struct TestHost {
    /// The flags the driver reads through [`Events`].
    pub flags: HostFlags,
    clock: Clock,
    events: Vec<DeviceEvent>,
}

#[derive(Debug)]
struct Clock {
    now: Instant,
    hints: TurnHints,
    ticks: u32,
    tick_takes: Duration,
    stop_after: Option<u32>,
    stop: Stop,
}

impl ConnectHost for Clock {
    fn tick(&mut self) {
        self.ticks += 1;
        self.now += self.tick_takes;
        if self.stop_after.is_some_and(|n| self.ticks >= n) {
            self.stop.stop();
        }
    }

    fn hints(&self) -> &TurnHints {
        &self.hints
    }

    fn now(&self) -> Instant {
        self.now
    }

    fn wait(&mut self, d: Duration) {
        self.now += d;
    }
}

impl Default for TestHost {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHost {
    /// A host at the current time (the clock is read once, here), with
    /// default flags, no hints, and ticks that take no time.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flags: HostFlags::new(),
            clock: Clock {
                now: Instant::now(),
                hints: TurnHints::default(),
                ticks: 0,
                tick_takes: Duration::ZERO,
                stop_after: None,
                stop: Stop::new(),
            },
            events: Vec::new(),
        }
    }

    /// The hints a connect sees.
    #[must_use]
    pub fn with_hints(mut self, hints: TurnHints) -> Self {
        self.clock.hints = hints;
        self
    }

    /// Stop on the `n`th tick, so a connect that ticks ends with `Stopped`
    /// without a second thread.
    #[must_use]
    pub fn stop_after_ticks(mut self, n: u32) -> Self {
        self.clock.stop_after = Some(n);
        self
    }

    /// Each tick moves the clock by `d`, so time passes for a connect that
    /// ticks but never waits (a device that keeps talking).
    #[must_use]
    pub fn tick_takes(mut self, d: Duration) -> Self {
        self.clock.tick_takes = d;
        self
    }

    /// The host's stop token.
    #[must_use]
    pub fn stop(&self) -> &Stop {
        &self.clock.stop
    }

    /// The time now.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.clock.now
    }

    /// Move the clock by `d`.
    pub fn advance(&mut self, d: Duration) {
        self.clock.now += d;
    }

    /// How many times a connect has ticked (a wait ticks once per slice).
    #[must_use]
    pub fn ticks(&self) -> u32 {
        self.clock.ticks
    }

    /// `driver.connect(ctx)` against this host.
    ///
    /// # Errors
    ///
    /// Returns what the connect failed with.
    pub fn connect<D: FrpDriver>(&mut self, driver: &mut D) -> Result<D::Session, D::Error> {
        self.connect_with(|ctx| driver.connect(ctx))
    }

    /// Any connect-shaped call against this host, e.g.
    /// `|ctx| driver.connect_with(ctx, fake)`.
    ///
    /// # Errors
    ///
    /// Returns what `f` returns.
    pub fn connect_with<T, E>(
        &mut self,
        f: impl FnOnce(&mut ConnectCtx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let stop = self.clock.stop.clone();
        let events = &mut self.events;
        let mut sink = |e: DeviceEvent| events.push(e);
        let mut ctx = ConnectCtx::new(&stop, Events::new(&mut sink, &self.flags), &mut self.clock);
        f(&mut ctx)
    }

    /// Hand `msg` to `session` as a host routing a bus message to it.
    ///
    /// # Errors
    ///
    /// Returns what the session's [`bus`](crate::driver::FrpSession::bus) returns.
    pub fn bus<S: FrpSession>(
        &mut self,
        session: &mut S,
        msg: &FrpMessage,
    ) -> Result<(), S::Error> {
        self.with(|_, out| session.bus(msg, out))
    }

    /// Call `f` once with the current time and the sink: one step
    /// (`|now, out| session.step(now, out)`), a close
    /// (`|_, out| session.close(out)`), anything that takes an [`Events`].
    pub fn with<T>(&mut self, f: impl FnOnce(Instant, &mut Events<'_>) -> T) -> T {
        let events = &mut self.events;
        let mut sink = |e: DeviceEvent| events.push(e);
        f(self.clock.now, &mut Events::new(&mut sink, &self.flags))
    }

    /// Step through `d` of time as a host does: after [`Activity::Active`]
    /// step again at the same instant, after [`Activity::Idle`] move the
    /// clock by [`IDLE`]. A session whose clock is
    /// its input's (a fast replay that returns `Active` on every step) is
    /// stepped with [`with`](Self::with) instead.
    ///
    /// # Errors
    ///
    /// Returns the first error `f` returns; the clock stays at that step.
    ///
    /// # Panics
    ///
    /// Panics after 10,000 `Active` steps in a row: a session that never idles.
    pub fn run_for<E>(
        &mut self,
        d: Duration,
        mut f: impl FnMut(Instant, &mut Events<'_>) -> Result<Activity, E>,
    ) -> Result<(), E> {
        let end = self.clock.now + d;
        let mut busy = 0;
        while self.clock.now < end {
            match self.with(&mut f)? {
                Activity::Active => {
                    busy += 1;
                    assert!(busy < MAX_BUSY_STEPS, "the session never idles");
                }
                Activity::Idle => {
                    busy = 0;
                    self.clock.now += IDLE;
                }
            }
        }
        Ok(())
    }

    /// Every event since the last `take`.
    pub fn take(&mut self) -> Vec<DeviceEvent> {
        std::mem::take(&mut self.events)
    }

    /// The message bodies and status changes since the last `take`; the
    /// phases, logs and discards in between are dropped.
    pub fn take_frp(&mut self) -> Vec<DeviceEvent> {
        self.take()
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    DeviceEvent::Frp(_) | DeviceEvent::Status(_) | DeviceEvent::StatusChange(_)
                )
            })
            .collect()
    }
}

/// The message bodies, in order.
#[must_use]
pub fn bodies(events: &[DeviceEvent]) -> Vec<&FrpBody> {
    events
        .iter()
        .filter_map(|e| match e {
            DeviceEvent::Frp(body) => Some(body),
            _ => None,
        })
        .collect()
}

/// The kind of each message body, in order: `"shot.trigger"`,
/// `"bus.alert"`, … ([`FrpBody::kind`]).
#[must_use]
pub fn kinds(events: &[DeviceEvent]) -> Vec<&str> {
    bodies(events).into_iter().map(FrpBody::kind).collect()
}

/// The `bus.alert`s, in order.
#[must_use]
pub fn alerts(events: &[DeviceEvent]) -> Vec<&BusAlert> {
    bodies(events)
        .into_iter()
        .filter_map(|b| match b {
            FrpBody::BusAlert(alert) => Some(alert),
            _ => None,
        })
        .collect()
}

/// The launch of each `shot.launch`, in order.
#[must_use]
pub fn launches(events: &[DeviceEvent]) -> Vec<&Launch> {
    bodies(events)
        .into_iter()
        .filter_map(|b| match b {
            FrpBody::ShotLaunch(m) => Some(&m.launch),
            _ => None,
        })
        .collect()
}

/// The flight result of each `shot.flight`, in order.
#[must_use]
pub fn flights(events: &[DeviceEvent]) -> Vec<&FlightResult> {
    bodies(events)
        .into_iter()
        .filter_map(|b| match b {
            FrpBody::ShotFlight(m) => Some(&m.flight),
            _ => None,
        })
        .collect()
}

/// The club data of each `shot.club`, in order.
#[must_use]
pub fn clubs(events: &[DeviceEvent]) -> Vec<&ClubData> {
    bodies(events)
        .into_iter()
        .filter_map(|b| match b {
            FrpBody::ShotClub(m) => Some(&m.club),
            _ => None,
        })
        .collect()
}

/// The `shot.raw` frames, in order.
#[must_use]
pub fn raw_frames(events: &[DeviceEvent]) -> Vec<&RawFrame> {
    bodies(events)
        .into_iter()
        .filter_map(|b| match b {
            FrpBody::ShotRaw(frame) => Some(frame),
            _ => None,
        })
        .collect()
}

/// The log lines, in order.
#[must_use]
pub fn logs(events: &[DeviceEvent]) -> Vec<(Level, &str)> {
    events
        .iter()
        .filter_map(|e| match e {
            DeviceEvent::Log(level, line) => Some((*level, line.as_str())),
            _ => None,
        })
        .collect()
}

/// The status a host holds after each [`DeviceEvent::Status`] and
/// [`DeviceEvent::StatusChange`], in order, starting from
/// [`DeviceStatus::not_armed`] as a host does for a new session. Pass every
/// event since the connect for the values to match the host's.
#[must_use]
pub fn statuses(events: &[DeviceEvent]) -> Vec<DeviceStatus> {
    let mut status = DeviceStatus::not_armed();
    events
        .iter()
        .filter_map(|e| {
            match e {
                DeviceEvent::Status(whole) => status = whole.clone(),
                DeviceEvent::StatusChange(change) => status.apply(change.clone()),
                _ => return None,
            }
            Some(status.clone())
        })
        .collect()
}

/// The status a host holds after `events`: the last of [`statuses`], or
/// [`DeviceStatus::not_armed`] when there is no status event.
#[must_use]
pub fn status(events: &[DeviceEvent]) -> DeviceStatus {
    statuses(events)
        .pop()
        .unwrap_or_else(DeviceStatus::not_armed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DriverConfig;
    use crate::driver::StatusChange;
    use crate::driver::{ConfigError, ConnectCtx, DeviceIdentity, DriverInfo, NoAction, Stopped};
    use crate::{Club, Severity, ShotKey, Source};

    #[test]
    fn a_wait_moves_the_clock_in_ticked_slices_and_ends_with_a_stop() {
        let mut host = TestHost::new().stop_after_ticks(4);
        let t0 = host.now();
        let waited = host.connect_with(|ctx| {
            ctx.wait(IDLE * 2 + Duration::from_millis(1))?;
            Ok::<_, Stopped>(ctx.now())
        });
        // Three slices (5, 5, 1 ms), a tick before each and one after.
        assert_eq!(waited, Ok(t0 + IDLE * 2 + Duration::from_millis(1)));
        assert_eq!(host.ticks(), 4);
        assert!(host.stop().is_stopped());
        assert_eq!(
            host.connect_with(|ctx| ctx.wait(Duration::ZERO)),
            Err(Stopped)
        );
    }

    #[test]
    fn a_tick_takes_the_time_it_is_given_and_shows_the_hints() {
        let hints = TurnHints::default().with_club(Club::Driver);
        let mut host = TestHost::new()
            .with_hints(hints)
            .tick_takes(Duration::from_millis(1));
        let t0 = host.now();
        let seen = host.connect_with(|ctx| {
            for _ in 0..3 {
                ctx.tick()?;
            }
            Ok::<_, Stopped>((ctx.now(), *ctx.hints()))
        });
        assert_eq!(seen, Ok((t0 + Duration::from_millis(3), hints)));
    }

    #[test]
    fn run_for_steps_again_at_once_after_active_and_waits_after_idle() {
        let mut host = TestHost::new();
        let t0 = host.now();
        let mut steps = Vec::new();
        host.run_for(IDLE * 2, |now, out| {
            steps.push(now - t0);
            out.ready(true);
            Ok::<_, Stopped>(if steps.len() == 1 {
                Activity::Active
            } else {
                Activity::Idle
            })
        })
        .unwrap();
        assert_eq!(steps, [Duration::ZERO, Duration::ZERO, IDLE]);
        assert_eq!(host.now(), t0 + IDLE * 2);
        assert_eq!(host.take_frp().len(), 3);
        assert!(host.take().is_empty());
    }

    #[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Nothing {}
    impl DriverConfig for Nothing {}

    struct Listener(Nothing);
    struct ListenerSession {
        identity: DeviceIdentity,
        heard: Vec<FrpMessage>,
    }

    impl FrpDriver for Listener {
        type Config = Nothing;
        type Error = Stopped;
        type Action = NoAction;
        type Session = ListenerSession;
        fn info(_: &Nothing) -> DriverInfo {
            DriverInfo::new("listener", "Listener", Source::Virtual)
        }
        fn new(config: Nothing) -> Result<Self, ConfigError> {
            Ok(Self(config))
        }
        fn config(&self) -> &Nothing {
            &self.0
        }
        fn apply_config(&mut self, new: &Nothing) {
            self.0 = new.clone();
        }
        fn connect(&mut self, _: &mut ConnectCtx<'_>) -> Result<ListenerSession, Stopped> {
            Ok(ListenerSession {
                identity: DeviceIdentity::new("Listener-1"),
                heard: Vec::new(),
            })
        }
    }

    impl FrpSession for ListenerSession {
        type Error = Stopped;
        type Action = NoAction;
        fn step(&mut self, _: Instant, _: &mut Events<'_>) -> Result<Activity, Stopped> {
            Ok(Activity::Idle)
        }
        fn bus(&mut self, msg: &FrpMessage, out: &mut Events<'_>) -> Result<(), Stopped> {
            out.log(crate::driver::Level::Info, "heard");
            self.heard.push(msg.clone());
            Ok(())
        }
        fn identity(&self) -> &DeviceIdentity {
            &self.identity
        }
        fn close(self, _: &mut Events<'_>) {}
    }

    #[test]
    fn a_session_receives_the_bus_message_the_host_hands_it() {
        let mut host = TestHost::new();
        let mut driver = Listener::new(Nothing {}).unwrap();
        let mut session = host.connect(&mut driver).unwrap();
        let msg = FrpMessage::new("a1", crate::BusActor::new("Other", 2.0));
        host.bus(&mut session, &msg).unwrap();
        assert_eq!(session.heard.len(), 1);
        assert_eq!(session.heard[0].from, "a1");
        assert!(matches!(session.heard[0].body, FrpBody::BusActor(_)));
        assert!(matches!(&host.take()[..], [DeviceEvent::Log(_, line)] if line == "heard"));
    }

    #[test]
    fn the_readers_pick_their_part_of_the_events() {
        let flags = HostFlags::new();
        flags.set_wants_raw(true);
        let mut events = Vec::new();
        let mut sink = |e| events.push(e);
        let mut out = Events::new(&mut sink, &flags);
        let key = ShotKey::new("id", 1);
        out.shot_trigger(key.clone(), None);
        out.launch(key.clone(), Launch::default());
        out.club(key.clone(), ClubData::default());
        out.alert(Severity::Warn, "careful");
        out.wire(crate::RawDir::Rx, "result", vec![1], None, false);
        out.log(Level::Info, "line");
        out.shot_finished(key);
        assert_eq!(
            kinds(&events),
            [
                "shot.trigger",
                "shot.launch",
                "shot.club",
                "bus.alert",
                "shot.raw",
                "shot.finished"
            ]
        );
        assert_eq!(launches(&events).len(), 1);
        assert_eq!(clubs(&events).len(), 1);
        assert!(flights(&events).is_empty());
        assert_eq!(alerts(&events)[0].message, "careful");
        assert_eq!(raw_frames(&events)[0].label.as_deref(), Some("result"));
        assert_eq!(logs(&events), [(Level::Info, "line")]);
    }

    #[test]
    fn the_status_folds_whole_statuses_and_changes_from_not_armed() {
        assert_eq!(status(&[]), DeviceStatus::not_armed());
        let events = [
            DeviceEvent::StatusChange(StatusChange::Battery(Some(80))),
            DeviceEvent::Phase("arming"),
            DeviceEvent::StatusChange(StatusChange::Blockers(vec![])),
            DeviceEvent::Status(DeviceStatus::new().with_telemetry("mode", "full")),
            DeviceEvent::StatusChange(StatusChange::Battery(Some(70))),
        ];
        let seen: Vec<(bool, Option<u8>)> = statuses(&events)
            .iter()
            .map(|s| (s.blockers.is_empty(), s.battery_pct))
            .collect();
        assert_eq!(
            seen,
            [
                (false, Some(80)),
                (true, Some(80)),
                (true, None),
                (true, Some(70))
            ]
        );
        assert_eq!(
            status(&events),
            DeviceStatus::new()
                .with_telemetry("mode", "full")
                .with_battery(70)
        );
    }
}
