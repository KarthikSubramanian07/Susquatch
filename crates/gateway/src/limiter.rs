//! Adaptive rate limiter: token bucket with AIMD on `429` and a reserved write budget.
//!
//! The platform publishes no numbers, so the rate starts at `initial_rps`, grows additively on
//! success, halves on every `429` (and pauses for `Retry-After`), bounded by `[min_rps, max_rps]`.
//! Reads may only spend tokens above a reserve of `order_budget_frac` of the bucket, so market data
//! polling can never starve order placement and cancels.

use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

/// What a request is, for budget purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Market data, portfolio reads.
    Read,
    /// Place / cancel.
    Write,
}

/// Limiter settings (from `[platform]`).
#[derive(Debug, Clone, PartialEq)]
pub struct LimiterConfig {
    pub initial_rps: f64,
    pub min_rps: f64,
    pub max_rps: f64,
    /// Share of the bucket reserved for writes.
    pub order_budget_frac: f64,
    /// Additive increase per successful request, in requests/second.
    pub increase_per_success: f64,
    /// Pause after a 429 without `Retry-After`.
    pub default_backoff: Duration,
}

impl LimiterConfig {
    /// From `[platform]` with the default AIMD step (0.02 rps per success) and 1 s default pause.
    pub fn from_cfg(p: &susq_core::config::PlatformCfg) -> Self {
        Self {
            initial_rps: p.initial_rps,
            min_rps: p.min_rps,
            max_rps: p.max_rps,
            order_budget_frac: p.order_budget_frac,
            increase_per_success: 0.02,
            default_backoff: Duration::from_secs(1),
        }
    }
}

#[derive(Debug)]
struct State {
    rate: f64,
    tokens: f64,
    last: Instant,
    paused_until: Option<Instant>,
    rate_limited: u64,
}

/// The shared limiter.
#[derive(Debug)]
pub struct RateLimiter {
    cfg: LimiterConfig,
    state: Mutex<State>,
}

impl RateLimiter {
    /// Start at `initial_rps` with a full bucket.
    pub fn new(cfg: LimiterConfig) -> Self {
        let rate = cfg.initial_rps.clamp(cfg.min_rps, cfg.max_rps);
        let state = State {
            rate,
            tokens: rate.max(1.0),
            last: Instant::now(),
            paused_until: None,
            rate_limited: 0,
        };
        Self {
            cfg,
            state: Mutex::new(state),
        }
    }

    fn burst(rate: f64) -> f64 {
        rate.max(1.0)
    }

    /// Wait for a token of `class`.
    pub async fn acquire(&self, class: RequestClass) {
        loop {
            let wait = {
                let mut s = self.state.lock();
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(s.last).as_secs_f64();
                s.tokens = (s.tokens + elapsed * s.rate).min(Self::burst(s.rate));
                s.last = now;
                match s.paused_until {
                    Some(until) if until > now => until - now,
                    _ => {
                        s.paused_until = None;
                        let reserve = match class {
                            RequestClass::Read => self.cfg.order_budget_frac * Self::burst(s.rate),
                            RequestClass::Write => 0.0,
                        };
                        if s.tokens - 1.0 >= reserve - 1e-9 {
                            s.tokens -= 1.0;
                            return;
                        }
                        Duration::from_secs_f64(((reserve + 1.0 - s.tokens) / s.rate).max(0.001))
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// A request succeeded: additive increase.
    pub fn on_success(&self) {
        let mut s = self.state.lock();
        s.rate = (s.rate + self.cfg.increase_per_success).min(self.cfg.max_rps);
    }

    /// A `429`: halve the rate, empty the bucket and pause for `Retry-After`.
    pub fn on_rate_limited(&self, retry_after: Option<Duration>) {
        let mut s = self.state.lock();
        s.rate = (s.rate / 2.0).max(self.cfg.min_rps);
        s.tokens = 0.0;
        s.rate_limited += 1;
        let until = Instant::now() + retry_after.unwrap_or(self.cfg.default_backoff);
        s.paused_until = Some(s.paused_until.map_or(until, |p| p.max(until)));
        tracing::warn!(rate = s.rate, ?retry_after, "rate limited: backing off");
    }

    /// Current allowed rate (requests/second).
    pub fn rate(&self) -> f64 {
        self.state.lock().rate
    }

    /// 429s seen since start.
    pub fn rate_limited_count(&self) -> u64 {
        self.state.lock().rate_limited
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LimiterConfig {
        LimiterConfig {
            initial_rps: 4.0,
            min_rps: 0.5,
            max_rps: 20.0,
            order_budget_frac: 0.5,
            increase_per_success: 0.5,
            default_backoff: Duration::from_secs(1),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn aimd_bounds() {
        let l = RateLimiter::new(cfg());
        for _ in 0..100 {
            l.on_success();
        }
        assert_eq!(l.rate(), 20.0);
        for _ in 0..10 {
            l.on_rate_limited(None);
        }
        assert_eq!(l.rate(), 0.5);
        assert_eq!(l.rate_limited_count(), 10);
    }

    #[tokio::test(start_paused = true)]
    async fn honours_retry_after() {
        let l = RateLimiter::new(cfg());
        let start = Instant::now();
        l.on_rate_limited(Some(Duration::from_secs(7)));
        l.acquire(RequestClass::Write).await;
        assert!(start.elapsed() >= Duration::from_secs(7));
    }

    #[tokio::test(start_paused = true)]
    async fn reads_leave_the_write_reserve() {
        let l = RateLimiter::new(cfg()); // 4 rps, burst 4, reserve 2
        let start = Instant::now();
        l.acquire(RequestClass::Read).await;
        l.acquire(RequestClass::Read).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        // The remaining 2 tokens are writes-only: another read has to wait for refill...
        let t = Instant::now();
        l.acquire(RequestClass::Read).await;
        assert!(t.elapsed() >= Duration::from_millis(240));
        // ...while a write goes through on the reserve immediately.
        let t = Instant::now();
        l.acquire(RequestClass::Write).await;
        assert_eq!(t.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_rate_is_respected() {
        let l = RateLimiter::new(LimiterConfig {
            increase_per_success: 0.0,
            ..cfg()
        });
        let start = Instant::now();
        for _ in 0..24 {
            l.acquire(RequestClass::Write).await;
        }
        // 4 burst + 20 more at 4 rps = at least 5 s.
        assert!(start.elapsed() >= Duration::from_secs(5), "{:?}", start.elapsed());
    }
}
