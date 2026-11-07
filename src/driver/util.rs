//! Small helpers every driver host and driver shares: [`Stop`], [`Backoff`],
//! [`Live`], [`ShotIds`] and [`connect_tcp`].

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::ShotKey;

/// A cloneable stop request. The host calls [`stop`](Self::stop) from any
/// thread; a driver checks it between steps and in long waits.
#[derive(Debug, Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

/// How often [`Stop::sleep`] looks at the flag.
const SLEEP_SLICE: Duration = Duration::from_millis(20);

impl Stop {
    /// A token that has not been stopped.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a stop. Every clone sees it.
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a stop was requested.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// The flag behind the token, for a transport layer that takes a
    /// `cancel: Option<&AtomicBool>` and needs no flightrelay types.
    #[must_use]
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }

    /// Sleep for `duration`, waking early on a stop. Returns `true` when the
    /// full duration passed and `false` when a stop cut it short (or was
    /// already requested).
    #[must_use]
    pub fn sleep(&self, duration: Duration) -> bool {
        let end = Instant::now() + duration;
        loop {
            if self.is_stopped() {
                return false;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(SLEEP_SLICE));
        }
    }
}

/// Growth per failed attempt.
const STEP: Duration = Duration::from_secs(1);

/// Shortest session that restarts a [`Backoff`] at its minimum. Longer than
/// any connect-time setup, so a link that is accepted and then drops during
/// setup counts as a failed attempt and keeps growing toward the cap.
pub const STABLE_SESSION: Duration = Duration::from_secs(10);

/// Linear reconnect backoff: starts at `min`, grows by 1 s per failed
/// attempt, capped at `max`. A session that lasted at least
/// [`STABLE_SESSION`] restarts the sequence at `min`, so one long outage does
/// not slow every later reconnect, while a link that keeps connecting and
/// dropping at once still backs off to `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// A backoff between `min` and `max`.
    #[must_use]
    pub const fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            next: min,
        }
    }

    /// Delay before the next attempt. `session` is how long the attempt that
    /// just ended stayed connected, `None` if it never connected.
    #[must_use]
    pub fn delay(&mut self, session: Option<Duration>) -> Duration {
        if session.is_some_and(|s| s >= STABLE_SESSION) {
            self.next = self.min;
        }
        let delay = self.next;
        self.next = (self.next + STEP).min(self.max);
        delay
    }
}

impl Default for Backoff {
    /// 1 s growing to 15 s.
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(15))
    }
}

/// A value shared between a driver and its sessions, for a config field the
/// driver applies without a restart. A small `Arc<Mutex<T>>`: clones share
/// the value.
#[derive(Debug)]
pub struct Live<T>(Arc<Mutex<T>>);

impl<T> Clone for Live<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Live<T> {
    /// A cell holding `value`.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }

    fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the value.
    pub fn set(&self, value: T) {
        *self.lock() = value;
    }

    /// Change the value in place and return what `f` returns.
    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.lock())
    }
}

impl<T: Clone> Live<T> {
    /// A copy of the current value.
    #[must_use]
    pub fn get(&self) -> T {
        self.lock().clone()
    }
}

impl<T: Default> Default for Live<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Makes [`ShotKey`]s: a fresh UUID v4 `shot_id` and a shot number that
/// counts up from 1.
///
/// Clones share the counter, so a driver keeps one in its own state and hands
/// a clone to each session, and shot numbers keep counting across
/// reconnects. The ids come from a generator seeded by the standard
/// library's randomly keyed hasher.
#[derive(Debug, Clone)]
pub struct ShotIds(Arc<IdState>);

#[derive(Debug)]
struct IdState {
    number: AtomicU32,
    rng: Mutex<[u64; 2]>,
}

impl ShotIds {
    /// A generator with a random seed.
    #[must_use]
    pub fn new() -> Self {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(1);
        let a = hasher.finish();
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(2);
        let b = hasher.finish();
        Self::from_seed([a, b])
    }

    /// A generator with a fixed seed, for tests that want repeatable ids.
    #[must_use]
    pub fn from_seed(seed: [u64; 2]) -> Self {
        // xorshift128+ must not start at all zeros.
        let seed = if seed == [0, 0] { [1, 2] } else { seed };
        Self(Arc::new(IdState {
            number: AtomicU32::new(0),
            rng: Mutex::new(seed),
        }))
    }

    /// The key of the next shot: number `n + 1` after `n` keys, shared by
    /// every clone.
    #[must_use]
    pub fn next_key(&self) -> ShotKey {
        let number = self.0.number.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let mut bytes = [0u8; 16];
        {
            let mut rng = self.0.rng.lock().unwrap_or_else(PoisonError::into_inner);
            for half in bytes.chunks_mut(8) {
                half.copy_from_slice(&xorshift128plus(&mut rng).to_be_bytes());
            }
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
        ShotKey::new(uuid_string(&bytes), number)
    }

    /// How many keys have been made.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.0.number.load(Ordering::SeqCst)
    }
}

impl Default for ShotIds {
    fn default() -> Self {
        Self::new()
    }
}

fn xorshift128plus(state: &mut [u64; 2]) -> u64 {
    let mut s1 = state[0];
    let s0 = state[1];
    state[0] = s0;
    s1 ^= s1 << 23;
    state[1] = s1 ^ s0 ^ (s1 >> 18) ^ (s0 >> 5);
    state[1].wrapping_add(s0)
}

fn uuid_string(b: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Connect to `addr` within `timeout`, giving up as soon as `stop` is
/// requested: `Ok(None)`. The connect runs on a thread of its own; one that
/// completes after the caller gave up closes the connection at once.
///
/// # Errors
///
/// Returns the connect error, e.g. a refusal or the timeout.
pub fn connect_tcp(
    addr: SocketAddr,
    timeout: Duration,
    stop: &Stop,
) -> std::io::Result<Option<TcpStream>> {
    crate::net::connect_cancelable(addr, timeout, || !stop.is_stopped())
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn backoff_grows_by_a_second_up_to_the_cap() {
        let mut b = Backoff::new(secs(1), secs(3));
        let delays: Vec<_> = (0..5).map(|_| b.delay(None)).collect();
        assert_eq!(delays, [secs(1), secs(2), secs(3), secs(3), secs(3)]);
    }

    #[test]
    fn backoff_resets_after_a_session_that_lasted() {
        let mut b = Backoff::new(secs(1), secs(15));
        for _ in 0..20 {
            let _ = b.delay(None);
        }
        assert_eq!(b.delay(None), secs(15));
        assert_eq!(b.delay(Some(STABLE_SESSION)), secs(1));
        assert_eq!(b.delay(None), secs(2));
        assert_eq!(b.delay(Some(secs(3600))), secs(1));
    }

    #[test]
    fn backoff_does_not_reset_for_a_session_that_dropped_at_once() {
        let mut b = Backoff::new(secs(1), secs(15));
        let delays: Vec<_> = (0..20).map(|_| b.delay(Some(Duration::ZERO))).collect();
        assert_eq!(delays[..3], [secs(1), secs(2), secs(3)]);
        assert_eq!(delays[19], secs(15));
        let just_short = Duration::from_millis(9_999);
        assert_eq!(b.delay(Some(just_short)), secs(15));
    }

    #[test]
    fn default_backoff_runs_one_to_fifteen_seconds() {
        let mut b = Backoff::default();
        assert_eq!(b.delay(None), secs(1));
        for _ in 0..30 {
            let _ = b.delay(None);
        }
        assert_eq!(b.delay(None), secs(15));
    }

    #[test]
    fn shot_ids_are_uuid_v4_and_count_up_across_clones() {
        let ids = ShotIds::new();
        let other = ids.clone();
        let a = ids.next_key();
        let b = other.next_key();
        let c = ids.next_key();
        assert_eq!([a.shot_number, b.shot_number, c.shot_number], [1, 2, 3]);
        assert_eq!(other.count(), 3);
        for key in [&a, &b, &c] {
            let id = &key.shot_id;
            assert_eq!(id.len(), 36, "{id}");
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                [8, 4, 4, 4, 12]
            );
            assert!(parts[2].starts_with('4'), "version nibble: {id}");
            assert!(
                matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
                "variant nibble: {id}"
            );
            assert!(id.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit()));
        }
        assert_ne!(a.shot_id, b.shot_id);
        assert_ne!(b.shot_id, c.shot_id);
    }

    #[test]
    fn shot_ids_are_random_per_generator_and_repeatable_from_a_seed() {
        assert_ne!(
            ShotIds::new().next_key().shot_id,
            ShotIds::new().next_key().shot_id
        );
        let a = ShotIds::from_seed([7, 9]).next_key();
        let b = ShotIds::from_seed([7, 9]).next_key();
        assert_eq!(a, b);
        // An all-zero seed still produces distinct ids.
        let zero = ShotIds::from_seed([0, 0]);
        assert_ne!(zero.next_key().shot_id, zero.next_key().shot_id);
    }

    #[test]
    fn stop_is_shared_by_clones_and_cuts_a_sleep_short() {
        let stop = Stop::new();
        assert!(!stop.is_stopped());
        assert!(stop.sleep(Duration::from_millis(10)));
        let other = stop.clone();
        let at = Instant::now();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            other.stop();
        });
        assert!(!stop.sleep(secs(10)));
        assert!(at.elapsed() < secs(2));
        assert!(stop.is_stopped());
        assert!(stop.flag().load(Ordering::SeqCst));
        assert!(!stop.sleep(secs(10)), "a stopped token never sleeps");
        handle.join().expect("thread");
    }

    #[test]
    fn live_cells_share_their_value() {
        let a = Live::new(1);
        let b = a.clone();
        b.set(5);
        assert_eq!(a.get(), 5);
        assert_eq!(a.update(|v| std::mem::replace(v, 7)), 5);
        assert_eq!(b.get(), 7);
        assert_eq!(Live::<u8>::default().get(), 0);
    }

    #[test]
    fn a_connect_returns_the_stream() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stream = connect_tcp(addr, secs(2), &Stop::new())
            .expect("connect")
            .expect("not stopped");
        assert_eq!(stream.peer_addr().expect("peer"), addr);
    }

    #[test]
    fn a_stopped_token_gives_up_a_connect_at_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stop = Stop::new();
        stop.stop();
        let at = Instant::now();
        let stream = connect_tcp(addr, secs(5), &stop).expect("no error");
        assert!(stream.is_none());
        assert!(at.elapsed() < secs(1));
    }

    #[test]
    fn a_refused_connect_is_an_error() {
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr")
        };
        assert!(connect_tcp(addr, secs(2), &Stop::new()).is_err());
    }
}
