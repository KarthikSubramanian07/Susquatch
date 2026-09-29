//! Exponential backoff with jitter, driven by a tiny splitmix64 PRNG (no `rand` dependency).

use std::time::Duration;

/// splitmix64: fast, statistically decent, and seedable. Only used for retry jitter.
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        const SCALE: f64 = 1.0 / (1u64 << 53) as f64;
        (self.next_u64() >> 11) as f64 * SCALE
    }
}

/// Retry delays: the ceiling doubles from `min` up to `max`; each delay is drawn uniformly from
/// the upper half of the current ceiling ("equal jitter"), so it lies in `[min/2, max]`.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    attempt: u32,
    rng: SplitMix64,
}

impl Backoff {
    /// `max` is raised to `min` if smaller. `seed` should differ per instance (e.g. the clock).
    pub fn new(min: Duration, max: Duration, seed: u64) -> Self {
        Self {
            min,
            max: max.max(min),
            attempt: 0,
            rng: SplitMix64(seed),
        }
    }

    /// Delay before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        let half = ceiling / 2;
        half + (ceiling - half).mul_f64(self.rng.unit())
    }

    /// Back to the shortest delay (after a healthy period or a success).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    fn ceiling(&self) -> Duration {
        let factor = 1u32 << self.attempt.min(31);
        self.min.saturating_mul(factor).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_secs(5);

    #[test]
    fn delays_stay_within_bounds_forever() {
        let mut b = Backoff::new(MIN, MAX, 42);
        for _ in 0..500 {
            let d = b.next_delay();
            assert!(d >= MIN / 2 && d <= MAX, "{d:?}");
        }
    }

    #[test]
    fn ceiling_grows_then_caps() {
        let mut b = Backoff::new(MIN, MAX, 7);
        let first = b.next_delay();
        assert!(first >= MIN / 2 && first <= MIN);
        for _ in 0..10 {
            b.next_delay();
        }
        assert!(b.next_delay() >= MAX / 2);
    }

    #[test]
    fn reset_returns_to_minimum() {
        let mut b = Backoff::new(MIN, MAX, 1);
        for _ in 0..20 {
            b.next_delay();
        }
        b.reset();
        assert!(b.next_delay() <= MIN);
    }

    #[test]
    fn jitter_differs_by_seed_and_max_below_min_is_clamped() {
        let a: Vec<_> = (0..5)
            .map({
                let mut b = Backoff::new(MIN, MAX, 1);
                move |_| b.next_delay()
            })
            .collect();
        let c: Vec<_> = (0..5)
            .map({
                let mut b = Backoff::new(MIN, MAX, 2);
                move |_| b.next_delay()
            })
            .collect();
        assert_ne!(a, c);
        let mut b = Backoff::new(MIN, Duration::ZERO, 3);
        assert!(b.next_delay() <= MIN);
    }

    #[test]
    fn unit_is_in_range() {
        let mut r = SplitMix64(0);
        for _ in 0..1_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }
}
