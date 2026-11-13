//! A small random number generator for ids (actor ids, shot ids), seeded by
//! the standard library's randomly keyed hasher, the system clock and a
//! process-wide counter. Not for cryptography.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Counts the generators made in this process, so two made at once differ
/// even when the hasher keys and the clock repeat.
static SEEDS: AtomicU64 = AtomicU64::new(0);

/// xorshift128+.
#[derive(Debug, Clone)]
pub(crate) struct Rng([u64; 2]);

impl Rng {
    /// A generator with a random seed: two randomly keyed hashes of the
    /// current time in nanoseconds since the Unix epoch and a process-wide
    /// counter.
    pub(crate) fn random() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let count = SEEDS.fetch_add(1, Ordering::Relaxed);
        let seed = |lane: u8| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u8(lane);
            hasher.write_u128(nanos);
            hasher.write_u64(count);
            hasher.finish()
        };
        Self::from_seed([seed(1), seed(2)])
    }

    /// A generator with a fixed seed. An all-zero seed is replaced, because
    /// xorshift128+ must not start at all zeros.
    pub(crate) fn from_seed(seed: [u64; 2]) -> Self {
        Self(if seed == [0, 0] { [1, 2] } else { seed })
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let state = &mut self.0;
        let mut s1 = state[0];
        let s0 = state[1];
        state[0] = s0;
        s1 ^= s1 << 23;
        state[1] = s1 ^ s0 ^ (s1 >> 18) ^ (s0 >> 5);
        state[1].wrapping_add(s0)
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn generators_made_together_differ() {
        let firsts: std::collections::HashSet<u64> =
            (0..1000).map(|_| Rng::random().next_u64()).collect();
        assert_eq!(firsts.len(), 1000);
    }

    #[test]
    fn an_all_zero_seed_is_replaced() {
        let mut rng = Rng::from_seed([0, 0]);
        assert_ne!(rng.next_u64(), 0);
    }
}
