//! Retry policy: exponential backoff with jitter, honouring `Retry-After` and the 90 s
//! `REQUEST_IN_FLIGHT` wait. Only [`RetryClass::Retry`] failures are retried, always with the same
//! idempotency key (the caller resends the identical request).

use std::time::Duration;

use susq_platform::RetryClass;

/// Backoff settings.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
    /// Give up (outcome unknown → reconcile) after this much time spent retrying.
    pub max_elapsed: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            base: Duration::from_millis(250),
            cap: Duration::from_secs(20),
            max_elapsed: Duration::from_secs(300),
        }
    }
}

impl RetryPolicy {
    /// Delay before attempt `attempt + 1` after failure number `attempt` (1-based), or `None` to stop.
    pub fn next_delay(
        &self,
        attempt: u32,
        class: RetryClass,
        elapsed: Duration,
        rng: &mut Jitter,
    ) -> Option<Duration> {
        let RetryClass::Retry { min_wait } = class else {
            return None;
        };
        if attempt >= self.max_attempts {
            return None;
        }
        let exp = self
            .base
            .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
            .min(self.cap);
        // "Equal jitter": half fixed, half random, so retries never collapse to zero.
        let half = exp / 2;
        let delay = (half + half.mul_f64(rng.next_unit())).max(min_wait.unwrap_or_default());
        (elapsed + delay <= self.max_elapsed).then_some(delay)
    }
}

/// Tiny deterministic PRNG (splitmix64) for jitter; no external dependency, seedable in tests.
#[derive(Debug, Clone)]
pub struct Jitter(u64);

impl Jitter {
    /// Seeded generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_platform::REQUEST_IN_FLIGHT_WAIT;

    const RETRY: RetryClass = RetryClass::Retry { min_wait: None };

    #[test]
    fn terminal_and_unknown_are_not_retried() {
        let p = RetryPolicy::default();
        let mut j = Jitter::new(1);
        assert_eq!(
            p.next_delay(1, RetryClass::Terminal, Duration::ZERO, &mut j),
            None
        );
        assert_eq!(p.next_delay(1, RetryClass::Unknown, Duration::ZERO, &mut j), None);
    }

    #[test]
    fn exponential_with_jitter_and_bounds() {
        let p = RetryPolicy::default();
        let mut j = Jitter::new(7);
        for attempt in 1..p.max_attempts {
            let d = p
                .next_delay(attempt, RETRY, Duration::ZERO, &mut j)
                .expect("retry");
            let exp = p.base.saturating_mul(1 << (attempt - 1)).min(p.cap);
            assert!(d >= exp / 2 && d <= exp, "attempt {attempt}: {d:?} vs {exp:?}");
        }
        assert_eq!(p.next_delay(p.max_attempts, RETRY, Duration::ZERO, &mut j), None);
    }

    #[test]
    fn honours_retry_after_and_in_flight() {
        let p = RetryPolicy::default();
        let mut j = Jitter::new(3);
        let ra = RetryClass::Retry {
            min_wait: Some(Duration::from_secs(9)),
        };
        assert!(p.next_delay(1, ra, Duration::ZERO, &mut j).expect("retry") >= Duration::from_secs(9));
        let inflight = RetryClass::Retry {
            min_wait: Some(REQUEST_IN_FLIGHT_WAIT),
        };
        assert!(p.next_delay(1, inflight, Duration::ZERO, &mut j).expect("retry") >= Duration::from_secs(90));
        // Out of time budget: stop and let reconcile resolve it.
        assert_eq!(p.next_delay(1, inflight, Duration::from_secs(250), &mut j), None);
    }

    #[test]
    fn jitter_is_uniform_enough() {
        let mut j = Jitter::new(42);
        let xs: Vec<f64> = (0..10_000).map(|_| j.next_unit()).collect();
        assert!(xs.iter().all(|x| (0.0..1.0).contains(x)));
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!((mean - 0.5).abs() < 0.02, "{mean}");
    }
}
