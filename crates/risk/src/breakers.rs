//! Circuit breakers: loss per hour, reject rate, bus error spikes.
//!
//! A trip freezes new orders for a cooldown and is reported once through [`Breakers::take_trip`] so
//! the live loop cancels everything exactly once. Nobody is paged (by design): the freeze thaws on
//! its own after the cooldown and the trip is recorded for the daily summary.

use std::collections::VecDeque;

use susq_core::config::{EnCfg, RiskCfg};

const MINUTE_NS: i64 = 60 * 1_000_000_000;
const HOUR_NS: i64 = 60 * MINUTE_NS;

/// Which breaker fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerKind {
    LossPerHour,
    RejectRate,
    BusErrors,
}

/// A trip: freeze until `until_ns`, cancel all now.
#[derive(Debug, Clone, PartialEq)]
pub struct Trip {
    pub kind: BreakerKind,
    pub reason: String,
    pub until_ns: i64,
}

/// Breaker thresholds and cooldowns.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerConfig {
    pub loss_per_hour_frac: f64,
    pub reject_per_min: u32,
    pub bus_errors_per_min: u32,
    pub loss_cooldown_ns: i64,
    pub reject_cooldown_ns: i64,
    pub bus_cooldown_ns: i64,
}

impl BreakerConfig {
    /// From `[risk]` with the default cooldowns (1 h after a loss trip, 5 min otherwise).
    pub fn from_cfg(cfg: &RiskCfg) -> Self {
        Self {
            loss_per_hour_frac: cfg.loss_per_hour_frac,
            reject_per_min: cfg.reject_breaker_per_min,
            bus_errors_per_min: cfg.bus_error_breaker_per_min,
            loss_cooldown_ns: HOUR_NS,
            reject_cooldown_ns: 5 * MINUTE_NS,
            bus_cooldown_ns: 5 * MINUTE_NS,
        }
    }

    /// EXP-014: with `[strategies.en] contender_mode` on, the loss-per-hour breaker trips at
    /// `contender_loss_per_hour_frac` instead, so an adverse hour on a concentrated needle position
    /// does not cancel everything and freeze the account for an hour mid-chain. The breaker sees
    /// only account equity, so the raise is account-wide while the mode is on. Off: unchanged.
    pub fn with_contender(mut self, en: &EnCfg) -> Self {
        if en.contender_mode {
            self.loss_per_hour_frac = en.contender_loss_per_hour_frac;
        }
        self
    }
}

/// Breaker state.
#[derive(Debug, Clone)]
pub struct Breakers {
    cfg: BreakerConfig,
    equity: VecDeque<(i64, f64)>,
    rejects: VecDeque<i64>,
    frozen: Option<Trip>,
    pending_trip: Option<Trip>,
    history: Vec<Trip>,
}

impl Breakers {
    /// Fresh, untripped breakers.
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            cfg,
            equity: VecDeque::new(),
            rejects: VecDeque::new(),
            frozen: None,
            pending_trip: None,
            history: vec![],
        }
    }

    fn trip(&mut self, kind: BreakerKind, reason: String, until_ns: i64) {
        // Extending an active freeze does not re-trigger cancel-all, but a new kind does.
        if self
            .frozen
            .as_ref()
            .is_some_and(|f| f.kind == kind && f.until_ns >= until_ns)
        {
            return;
        }
        let trip = Trip {
            kind,
            reason,
            until_ns,
        };
        tracing::warn!(?kind, reason = %trip.reason, "risk breaker tripped");
        self.frozen = Some(trip.clone());
        self.pending_trip = Some(trip.clone());
        self.history.push(trip);
    }

    /// Mark-to-market equity sample. Trips when equity is `loss_per_hour_frac` below the hour's peak.
    pub fn record_equity(&mut self, now_ns: i64, equity: f64) {
        if !equity.is_finite() {
            return;
        }
        self.equity.push_back((now_ns, equity));
        while self.equity.front().is_some_and(|(t, _)| now_ns - t > HOUR_NS) {
            self.equity.pop_front();
        }
        let peak = self.equity.iter().map(|(_, e)| *e).fold(f64::MIN, f64::max);
        if peak > 0.0 && (peak - equity) / peak >= self.cfg.loss_per_hour_frac {
            let reason = format!(
                "equity {equity:.0} is {:.1}% below the hour's peak {peak:.0}",
                100.0 * (peak - equity) / peak
            );
            self.trip(
                BreakerKind::LossPerHour,
                reason,
                now_ns + self.cfg.loss_cooldown_ns,
            );
            // Start a fresh window so the same drawdown does not re-trip after the cooldown.
            self.equity.clear();
        }
    }

    /// A terminal order rejection.
    pub fn record_reject(&mut self, now_ns: i64) {
        self.rejects.push_back(now_ns);
        while self.rejects.front().is_some_and(|t| now_ns - t > MINUTE_NS) {
            self.rejects.pop_front();
        }
        let n = self.rejects.len();
        if n > self.cfg.reject_per_min as usize {
            self.trip(
                BreakerKind::RejectRate,
                format!("{n} rejects in the last minute"),
                now_ns + self.cfg.reject_cooldown_ns,
            );
            self.rejects.clear();
        }
    }

    /// Bus validation errors in the last minute (from the needle monitor).
    pub fn record_bus_errors(&mut self, now_ns: i64, per_min: u32) {
        if per_min >= self.cfg.bus_errors_per_min && self.cfg.bus_errors_per_min > 0 {
            self.trip(
                BreakerKind::BusErrors,
                format!("{per_min} malformed bus messages in the last minute"),
                now_ns + self.cfg.bus_cooldown_ns,
            );
        }
    }

    /// The active freeze, if any (expired freezes thaw here).
    pub fn frozen(&mut self, now_ns: i64) -> Option<&Trip> {
        if self.frozen.as_ref().is_some_and(|f| now_ns >= f.until_ns) {
            tracing::info!("risk freeze expired");
            self.frozen = None;
        }
        self.frozen.as_ref()
    }

    /// A trip not yet acted on (cancel-all). Returned once.
    pub fn take_trip(&mut self) -> Option<Trip> {
        self.pending_trip.take()
    }

    /// Every trip since start (for the status snapshot).
    pub fn history(&self) -> &[Trip] {
        &self.history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BreakerConfig {
        BreakerConfig {
            loss_per_hour_frac: 0.03,
            reject_per_min: 10,
            bus_errors_per_min: 20,
            loss_cooldown_ns: HOUR_NS,
            reject_cooldown_ns: 5 * MINUTE_NS,
            bus_cooldown_ns: 5 * MINUTE_NS,
        }
    }

    #[test]
    fn loss_per_hour_trips_once_and_thaws() {
        let mut b = Breakers::new(cfg());
        b.record_equity(0, 100_000.0);
        b.record_equity(MINUTE_NS, 98_000.0);
        assert!(b.frozen(MINUTE_NS).is_none());
        b.record_equity(2 * MINUTE_NS, 96_900.0);
        assert_eq!(
            b.frozen(2 * MINUTE_NS).map(|t| t.kind),
            Some(BreakerKind::LossPerHour)
        );
        assert!(b.take_trip().is_some());
        assert!(b.take_trip().is_none(), "reported once");
        assert!(
            b.frozen(2 * MINUTE_NS + HOUR_NS).is_none(),
            "thaws after the cooldown"
        );
    }

    /// EXP-014: contender mode raises the loss-per-hour threshold; off, the 3% breaker stands.
    #[test]
    fn contender_mode_raises_the_loss_per_hour_threshold_only_when_on() {
        let off = cfg().with_contender(&EnCfg::default());
        assert_eq!(off, cfg(), "contender_mode = false leaves the breaker as it was");
        let on = cfg().with_contender(&EnCfg {
            contender_mode: true,
            ..EnCfg::default()
        });
        assert_eq!(on.loss_per_hour_frac, 0.50);
        // A 20% adverse hour on a concentrated position: trips the 3% breaker, not the raised one.
        for (c, trips) in [(off, true), (on.clone(), false)] {
            let mut b = Breakers::new(c);
            b.record_equity(0, 100_000.0);
            b.record_equity(MINUTE_NS, 80_000.0);
            assert_eq!(b.take_trip().is_some(), trips);
        }
        let mut b = Breakers::new(on);
        b.record_equity(0, 100_000.0);
        b.record_equity(MINUTE_NS, 50_000.0);
        assert_eq!(b.take_trip().map(|t| t.kind), Some(BreakerKind::LossPerHour));
    }

    #[test]
    fn old_peaks_leave_the_window() {
        let mut b = Breakers::new(cfg());
        b.record_equity(0, 100_000.0);
        b.record_equity(HOUR_NS + MINUTE_NS, 96_000.0);
        assert!(b.frozen(HOUR_NS + MINUTE_NS).is_none());
    }

    #[test]
    fn reject_rate_trips_above_threshold() {
        let mut b = Breakers::new(cfg());
        for i in 0..10 {
            b.record_reject(i * 1_000_000_000);
        }
        assert!(b.frozen(10_000_000_000).is_none());
        b.record_reject(10_000_000_000);
        assert_eq!(b.take_trip().map(|t| t.kind), Some(BreakerKind::RejectRate));
        // Spread out rejects never trip.
        let mut b = Breakers::new(cfg());
        for i in 0..100 {
            b.record_reject(i * 7 * 1_000_000_000);
        }
        assert!(b.take_trip().is_none());
    }

    #[test]
    fn bus_error_spike_trips() {
        let mut b = Breakers::new(cfg());
        b.record_bus_errors(0, 19);
        assert!(b.take_trip().is_none());
        b.record_bus_errors(0, 20);
        assert_eq!(b.take_trip().map(|t| t.kind), Some(BreakerKind::BusErrors));
        assert_eq!(b.history().len(), 1);
    }
}
