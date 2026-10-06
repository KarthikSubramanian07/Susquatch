//! `NeedleMonitor`: validation + watchdog over the needle stream, published as a
//! [`NeedleSnapshot`] on a `tokio::sync::watch` channel.
//!
//! The logic lives in the pure [`NeedleState`]; [`NeedleMonitor`] only drives it from a message
//! stream, a periodic tick and a cancellation token.
//!
//! Two clocks govern what the executor may act on. **Liveness** is the watchdog's
//! `heartbeat_timeout` (`[needle] heartbeat_timeout_s`): no healthy message for that long trips
//! the needle, bumps the epoch and clears every stored number. **Posterior age** is the separate
//! `posterior_ttl` (`[needle] posterior_max_age_s`): the needle publishes race posteriors only
//! after an accepted county batch, minutes apart between SoS refreshes, so while heartbeats keep
//! it healthy a posterior stays actionable until it is that old. A silent needle never keeps a
//! posterior alive: the watchdog clears it first.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, StreamExt};
use susq_contracts::v1;
use susq_core::Clock;
use susq_core::clock::duration_ns;
use susq_core::config::NeedleCfg;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::message::NeedleMessage;
use crate::validate::{self, ErrorRate, ValidationError};
use crate::watchdog::{Transition, UnhealthyReason, Watchdog, WatchdogCfg};

/// A value with the time it was produced (needle clock) and received (our clock).
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped<T> {
    /// The message content.
    pub value: T,
    /// `header.produced_at_ns` of the message that carried it.
    pub produced_at_ns: i64,
    /// When the monitor accepted it.
    pub received_at_ns: i64,
}

/// What the executor sees. Needle numbers are only reachable through the `actionable_*`
/// accessors, which return `None` whenever they must not be traded on.
#[derive(Debug, Clone)]
pub struct NeedleSnapshot {
    /// Watchdog verdict when the snapshot was taken.
    pub healthy: bool,
    /// Why the needle is unhealthy.
    pub reason: Option<String>,
    /// Consecutive healthy messages (progress toward resuming).
    pub healthy_streak: u32,
    /// Malformed or invalid needle messages in the last 60 s.
    pub errors_per_min: u32,
    /// Malformed or invalid needle messages since start.
    pub errors_total: u64,
    /// Increments on every healthy -> unhealthy transition. When it changes, cancel every
    /// resting order derived from needle numbers.
    pub epoch: u64,
    /// Dashboard entry.
    pub health: v1::HealthItem,
    /// When this snapshot was taken.
    pub taken_at_ns: i64,
    timeout_ns: i64,
    posterior_ttl_ns: i64,
    posteriors: HashMap<String, Stamped<v1::RacePosterior>>,
    shift: Option<Stamped<v1::NationalShift>>,
}

impl NeedleSnapshot {
    /// All race posteriors that may be acted on at `now_ns`, or `None` when the needle is not
    /// trusted. Individual races older than the posterior TTL are left out.
    pub fn actionable_posteriors(&self, now_ns: i64) -> Option<HashMap<&str, &Stamped<v1::RacePosterior>>> {
        self.trusted(now_ns).then(|| {
            self.posteriors
                .iter()
                .filter(|(_, p)| self.posterior_fresh(p.produced_at_ns, now_ns))
                .map(|(race, p)| (race.as_str(), p))
                .collect()
        })
    }

    /// One race's posterior if it may be acted on at `now_ns`.
    pub fn actionable_posterior(&self, race_id: &str, now_ns: i64) -> Option<&Stamped<v1::RacePosterior>> {
        if !self.trusted(now_ns) {
            return None;
        }
        self.posteriors
            .get(race_id)
            .filter(|p| self.posterior_fresh(p.produced_at_ns, now_ns))
    }

    /// The national shift if it may be acted on at `now_ns`.
    pub fn actionable_shift(&self, now_ns: i64) -> Option<&Stamped<v1::NationalShift>> {
        if !self.trusted(now_ns) {
            return None;
        }
        self.shift
            .as_ref()
            .filter(|s| self.posterior_fresh(s.produced_at_ns, now_ns))
    }

    /// How long a posterior stays actionable after it was produced, while the needle is healthy.
    pub fn posterior_ttl_ns(&self) -> i64 {
        self.posterior_ttl_ns
    }

    /// Healthy, and the snapshot itself is recent (a stalled or dead monitor task cannot leave a
    /// stale "healthy" behind).
    fn trusted(&self, now_ns: i64) -> bool {
        self.healthy && self.fresh(self.taken_at_ns, now_ns)
    }

    /// Liveness freshness (the heartbeat timeout).
    fn fresh(&self, at_ns: i64, now_ns: i64) -> bool {
        now_ns.saturating_sub(at_ns) <= self.timeout_ns
    }

    /// Number freshness (the posterior TTL, never shorter than the heartbeat timeout).
    fn posterior_fresh(&self, at_ns: i64, now_ns: i64) -> bool {
        now_ns.saturating_sub(at_ns) <= self.posterior_ttl_ns
    }
}

/// Pure monitor logic: validation, error counting, watchdog, and the latest accepted numbers.
#[derive(Debug, Clone)]
pub struct NeedleState {
    watchdog: Watchdog,
    errors: ErrorRate,
    known_races: HashSet<String>,
    timeout_ns: i64,
    posterior_ttl_ns: i64,
    posteriors: HashMap<String, Stamped<v1::RacePosterior>>,
    shift: Option<Stamped<v1::NationalShift>>,
}

impl NeedleState {
    /// Nothing received yet (unhealthy). Posteriors age out with the heartbeat timeout until
    /// [`NeedleState::with_posterior_ttl`] says otherwise.
    pub fn new(cfg: WatchdogCfg, known_races: HashSet<String>) -> Self {
        Self {
            timeout_ns: duration_ns(cfg.timeout),
            posterior_ttl_ns: duration_ns(cfg.timeout),
            watchdog: Watchdog::new(cfg),
            errors: ErrorRate::default(),
            known_races,
            posteriors: HashMap::new(),
            shift: None,
        }
    }

    /// Let posteriors stay actionable for `ttl` after they were produced while the needle keeps
    /// heartbeating (clamped to at least the heartbeat timeout, so it can only ever extend).
    pub fn with_posterior_ttl(mut self, ttl: Duration) -> Self {
        self.posterior_ttl_ns = duration_ns(ttl).max(self.timeout_ns);
        self
    }

    /// Replace the known race ids; stored posteriors for races no longer known are dropped.
    pub fn set_known_races(&mut self, known_races: HashSet<String>) {
        self.posteriors.retain(|race, _| known_races.contains(race));
        self.known_races = known_races;
    }

    /// Validate, judge and (if healthy) store one message.
    pub fn on_message(&mut self, msg: NeedleMessage, now_ns: i64) {
        let epoch = self.watchdog.epoch();
        self.apply(msg, now_ns);
        self.clear_if_tripped(epoch);
    }

    /// Periodic silence check.
    pub fn on_tick(&mut self, now_ns: i64) {
        let epoch = self.watchdog.epoch();
        self.watchdog.on_tick(now_ns);
        self.clear_if_tripped(epoch);
    }

    /// Force unhealthy (shutdown).
    pub fn stop(&mut self) {
        let epoch = self.watchdog.epoch();
        self.watchdog.mark_unhealthy(UnhealthyReason::Stopped);
        self.clear_if_tripped(epoch);
    }

    /// Oldest untaken watchdog transition.
    pub fn take_transition(&mut self) -> Option<Transition> {
        self.watchdog.take_transition()
    }

    /// Current view for consumers.
    pub fn snapshot(&self, now_ns: i64) -> NeedleSnapshot {
        NeedleSnapshot {
            healthy: self.watchdog.is_healthy(),
            reason: self.watchdog.reason().map(ToString::to_string),
            healthy_streak: self.watchdog.healthy_streak(),
            errors_per_min: self.errors.per_minute(now_ns),
            errors_total: self.errors.total(),
            epoch: self.watchdog.epoch(),
            health: self.watchdog.health_item(now_ns),
            taken_at_ns: now_ns,
            timeout_ns: self.timeout_ns,
            posterior_ttl_ns: self.posterior_ttl_ns,
            posteriors: self.posteriors.clone(),
            shift: self.shift.clone(),
        }
    }

    fn apply(&mut self, msg: NeedleMessage, now_ns: i64) {
        let Some(stream) = msg.stream() else {
            if let NeedleMessage::Malformed { subject, error } = &msg {
                self.errors.record(now_ns);
                tracing::warn!(%subject, %error, "dropped message on unexpected subject");
            }
            return;
        };
        let header = msg.header().cloned();
        let checked = match &msg {
            NeedleMessage::Posteriors(m) => validate::race_posteriors(m, &self.known_races),
            NeedleMessage::Shift(m) => validate::national_shift(m),
            NeedleMessage::Heartbeat(m) => validate::heartbeat(m),
            NeedleMessage::Malformed { error, .. } => Err(ValidationError::Undecodable(error.clone())),
        }
        .map_err(|e| e.to_string());
        if let Err(detail) = checked {
            self.errors.record(now_ns);
            tracing::warn!(%stream, error = %detail, "dropped invalid needle message");
            let reason = UnhealthyReason::Invalid { stream, detail };
            self.watchdog.on_rejected(stream, header.as_ref(), reason, now_ns);
            return;
        }
        if let NeedleMessage::Heartbeat(hb) = &msg
            && hb.status != "ok"
        {
            let reason = UnhealthyReason::Degraded {
                status: hb.status.clone(),
            };
            self.watchdog.on_rejected(stream, header.as_ref(), reason, now_ns);
            return;
        }
        if let Err(reason) = self.watchdog.on_message(stream, header.as_ref(), now_ns) {
            tracing::debug!(%reason, "needle message not used");
            return;
        }
        let produced_at_ns = header.map_or(0, |h| h.produced_at_ns);
        let stamp = |value| Stamped {
            value,
            produced_at_ns,
            received_at_ns: now_ns,
        };
        match msg {
            NeedleMessage::Posteriors(m) => {
                for race in m.races {
                    self.posteriors.insert(race.race_id.clone(), stamp(race));
                }
            }
            NeedleMessage::Shift(m) => {
                self.shift = Some(Stamped {
                    value: m,
                    produced_at_ns,
                    received_at_ns: now_ns,
                });
            }
            NeedleMessage::Heartbeat(_) | NeedleMessage::Malformed { .. } => {}
        }
    }

    /// Never carry numbers across an outage: a new epoch starts empty.
    fn clear_if_tripped(&mut self, epoch_before: u64) {
        if self.watchdog.epoch() != epoch_before {
            self.posteriors.clear();
            self.shift = None;
        }
    }
}

/// Monitor task settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorCfg {
    /// Watchdog rules.
    pub watchdog: WatchdogCfg,
    /// Silence-check period.
    pub tick: Duration,
    /// How long a posterior stays actionable while the needle is healthy (`[needle]
    /// posterior_max_age_s`); never shorter than the heartbeat timeout.
    pub posterior_ttl: Duration,
}

impl MonitorCfg {
    /// Tick at a tenth of the timeout, clamped to [10 ms, 1 s]; posteriors age out with the
    /// heartbeat timeout.
    pub fn new(watchdog: WatchdogCfg) -> Self {
        let tick = (watchdog.timeout / 10).clamp(Duration::from_millis(10), Duration::from_secs(1));
        let posterior_ttl = watchdog.timeout;
        Self {
            watchdog,
            tick,
            posterior_ttl,
        }
    }

    /// From the `[needle]` config section (watchdog rules plus the posterior TTL).
    pub fn from_config(cfg: &NeedleCfg) -> Self {
        Self {
            posterior_ttl: Duration::from_secs(cfg.posterior_max_age_s),
            ..Self::new(WatchdogCfg::from_config(cfg))
        }
    }
}

/// Handle to the running monitor task.
#[derive(Debug)]
pub struct NeedleMonitor {
    snapshots: watch::Receiver<NeedleSnapshot>,
    known_races: watch::Sender<HashSet<String>>,
    task: JoinHandle<()>,
}

impl NeedleMonitor {
    /// Spawn the monitor on the current tokio runtime. It runs until `cancel` fires; if the
    /// message stream ends it keeps ticking, so the needle goes unhealthy by silence.
    pub fn spawn<S>(
        messages: S,
        known_races: HashSet<String>,
        cfg: MonitorCfg,
        clock: Arc<dyn Clock>,
        cancel: CancellationToken,
    ) -> Self
    where
        S: Stream<Item = NeedleMessage> + Send + Unpin + 'static,
    {
        let state = NeedleState::new(cfg.watchdog, known_races.clone()).with_posterior_ttl(cfg.posterior_ttl);
        let (snap_tx, snapshots) = watch::channel(state.snapshot(clock.now_ns()));
        let (known_tx, known_rx) = watch::channel(known_races);
        let task = tokio::spawn(run(state, messages, known_rx, snap_tx, cfg.tick, clock, cancel));
        Self {
            snapshots,
            known_races: known_tx,
            task,
        }
    }

    /// A receiver for snapshots (changes after every message and tick).
    pub fn subscribe(&self) -> watch::Receiver<NeedleSnapshot> {
        self.snapshots.clone()
    }

    /// The latest snapshot.
    pub fn snapshot(&self) -> NeedleSnapshot {
        self.snapshots.borrow().clone()
    }

    /// Replace the known race ids (e.g. after the market map reloads).
    pub fn set_known_races(&self, races: HashSet<String>) {
        self.known_races.send_replace(races);
    }

    /// Wait for the task to finish after cancellation.
    pub async fn join(self) {
        if let Err(e) = self.task.await {
            tracing::error!(error = %e, "needle monitor task failed");
        }
    }
}

async fn run<S>(
    mut state: NeedleState,
    mut messages: S,
    mut known_rx: watch::Receiver<HashSet<String>>,
    snap_tx: watch::Sender<NeedleSnapshot>,
    tick: Duration,
    clock: Arc<dyn Clock>,
    cancel: CancellationToken,
) where
    S: Stream<Item = NeedleMessage> + Unpin,
{
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stream_open = true;
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            next = messages.next(), if stream_open => match next {
                Some(msg) => state.on_message(msg, clock.now_ns()),
                None => {
                    tracing::warn!("needle message stream ended");
                    stream_open = false;
                }
            },
            Ok(()) = known_rx.changed() => {
                state.set_known_races(known_rx.borrow_and_update().clone());
            }
            _ = ticker.tick() => state.on_tick(clock.now_ns()),
        }
        log_transitions(&mut state);
        snap_tx.send_replace(state.snapshot(clock.now_ns()));
    }
    state.stop();
    log_transitions(&mut state);
    snap_tx.send_replace(state.snapshot(clock.now_ns()));
}

fn log_transitions(state: &mut NeedleState) {
    while let Some(t) = state.take_transition() {
        match t {
            Transition::BecameUnhealthy { reason, epoch } => {
                tracing::warn!(%reason, epoch, "needle UNHEALTHY: stop acting on needle numbers");
            }
            Transition::BecameHealthy => tracing::info!("needle healthy again"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchdog::NEEDLE_PRODUCER;
    use susq_contracts::SCHEMA_VERSION;

    const MS: i64 = 1_000_000;
    const T0: i64 = 1_800_000_000_000_000_000;
    const OH: &str = "2026-SEN-OH-S";
    const GA: &str = "2026-GOV-GA";

    fn races() -> HashSet<String> {
        [OH, GA].into_iter().map(String::from).collect()
    }

    fn state() -> NeedleState {
        NeedleState::new(WatchdogCfg::new(Duration::from_millis(1_000), 2), races())
    }

    fn header(seq: u64, t: i64) -> Option<v1::Header> {
        Some(v1::Header {
            schema_version: SCHEMA_VERSION,
            produced_at_ns: t,
            producer: Some(v1::Producer {
                name: NEEDLE_PRODUCER.to_owned(),
                git_sha: "abc".to_owned(),
            }),
            seq,
        })
    }

    fn hb(seq: u64, t: i64) -> NeedleMessage {
        NeedleMessage::Heartbeat(v1::Heartbeat {
            header: header(seq, t),
            status: "ok".to_owned(),
            last_input_seq: 0,
        })
    }

    fn race(id: &str, p_dem: f64) -> v1::RacePosterior {
        v1::RacePosterior {
            race_id: id.to_owned(),
            p_dem,
            p_rep: 1.0 - p_dem,
            margin_sd: 1.0,
            ..Default::default()
        }
    }

    fn post(seq: u64, t: i64, races: Vec<v1::RacePosterior>) -> NeedleMessage {
        NeedleMessage::Posteriors(v1::RacePosteriors {
            header: header(seq, t),
            races,
        })
    }

    fn healthy_state() -> NeedleState {
        let mut s = state();
        s.on_message(hb(1, T0), T0);
        s.on_message(post(1, T0, vec![race(OH, 0.7)]), T0);
        assert!(s.snapshot(T0).healthy);
        s
    }

    #[test]
    fn nothing_actionable_until_healthy() {
        let mut s = state();
        s.on_message(post(1, T0, vec![race(OH, 0.7)]), T0);
        let snap = s.snapshot(T0);
        assert!(!snap.healthy);
        assert_eq!(snap.healthy_streak, 1);
        assert!(snap.actionable_posteriors(T0).is_none());
        assert!(snap.actionable_posterior(OH, T0).is_none());
        s.on_message(hb(1, T0), T0);
        let snap = s.snapshot(T0);
        assert!(snap.healthy);
        let p = snap.actionable_posterior(OH, T0).expect("actionable");
        assert!((p.value.p_dem - 0.7).abs() < 1e-12);
        assert_eq!(p.received_at_ns, T0);
    }

    #[test]
    fn posteriors_merge_per_race() {
        let mut s = healthy_state();
        s.on_message(post(2, T0, vec![race(GA, 0.4)]), T0);
        let snap = s.snapshot(T0);
        let all = snap.actionable_posteriors(T0).expect("healthy");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn invalid_message_is_dropped_counted_and_trips() {
        let mut s = healthy_state();
        s.on_message(post(2, T0, vec![race(GA, 0.4), race("2026-XX", 0.5)]), T0);
        let snap = s.snapshot(T0);
        assert!(!snap.healthy);
        assert_eq!(snap.errors_per_min, 1);
        assert_eq!(snap.epoch, 1);
        assert!(snap.reason.as_deref().is_some_and(|r| r.contains("2026-XX")));
        // Seq 2 was consumed by the rejected message; seq 3 is in order and recovery proceeds.
        s.on_message(post(3, T0, vec![race(GA, 0.4)]), T0);
        s.on_message(hb(2, T0), T0);
        let snap = s.snapshot(T0);
        assert!(snap.healthy);
        let all = snap.actionable_posteriors(T0).expect("healthy");
        assert_eq!(all.len(), 1, "pre-outage OH posterior was cleared");
        assert!(all.contains_key(GA));
    }

    #[test]
    fn malformed_payload_counts_and_trips() {
        let mut s = healthy_state();
        s.on_message(
            NeedleMessage::Malformed {
                subject: "needle.heartbeat".to_owned(),
                error: "bad varint".to_owned(),
            },
            T0,
        );
        let snap = s.snapshot(T0);
        assert!(!snap.healthy);
        assert_eq!(snap.errors_total, 1);
        s.on_message(
            NeedleMessage::Malformed {
                subject: "needle.bogus".to_owned(),
                error: "not a needle subject".to_owned(),
            },
            T0,
        );
        assert_eq!(s.snapshot(T0).errors_total, 2);
    }

    #[test]
    fn degraded_heartbeat_trips_without_counting_an_error() {
        let mut s = healthy_state();
        s.on_message(
            NeedleMessage::Heartbeat(v1::Heartbeat {
                header: header(2, T0),
                status: "degraded:no county data".to_owned(),
                last_input_seq: 0,
            }),
            T0,
        );
        let snap = s.snapshot(T0);
        assert!(!snap.healthy);
        assert_eq!(snap.errors_total, 0);
        assert!(snap.actionable_shift(T0).is_none());
    }

    #[test]
    fn silence_clears_numbers_and_bumps_epoch() {
        let mut s = healthy_state();
        s.on_tick(T0 + 1_001 * MS);
        let snap = s.snapshot(T0 + 1_001 * MS);
        assert!(!snap.healthy);
        assert_eq!(snap.epoch, 1);
        assert_eq!(snap.health.state, "down");
        assert!(matches!(s.take_transition(), Some(Transition::BecameHealthy)));
        assert!(matches!(
            s.take_transition(),
            Some(Transition::BecameUnhealthy { epoch: 1, .. })
        ));
    }

    #[test]
    fn stale_entries_and_stale_snapshots_are_not_actionable() {
        let mut s = healthy_state();
        // Heartbeats keep the needle healthy but the OH posterior itself ages out.
        let t = T0 + 900 * MS;
        s.on_message(hb(2, t), t);
        let t2 = T0 + 1_500 * MS;
        s.on_message(hb(3, t2), t2);
        let snap = s.snapshot(t2);
        assert!(snap.healthy);
        assert!(snap.actionable_posterior(OH, t2).is_none());
        assert_eq!(snap.actionable_posteriors(t2).map(|m| m.len()), Some(0));
        // A snapshot nobody refreshed stops being actionable after the timeout.
        let snap = s.snapshot(T0);
        assert!(snap.actionable_posteriors(T0 + 1_001 * MS).is_none());
    }

    /// Gap 5 of the readiness audit: the needle publishes posteriors only per county batch, so a
    /// posterior must outlive the 10 s heartbeat timeout while heartbeats keep the needle healthy.
    #[test]
    fn posteriors_outlive_the_heartbeat_timeout_while_heartbeats_continue() {
        let mut s = healthy_state().with_posterior_ttl(Duration::from_secs(60));
        s.on_message(
            NeedleMessage::Shift(v1::NationalShift {
                header: header(1, T0),
                mean: 0.5,
                sd: 0.5,
                n_races_informing: 1,
            }),
            T0,
        );
        assert_eq!(s.snapshot(T0).posterior_ttl_ns(), 60 * 1_000 * MS);
        // Heartbeats every 900 ms (the 1 s watchdog never trips): the posterior from T0 stays
        // actionable for the whole 60 s TTL and not a tick longer, with the needle healthy throughout.
        let mut seq = 2;
        let mut t = T0;
        let mut aged_out_at = None;
        while t < T0 + 65_000 * MS {
            t += 900 * MS;
            s.on_message(hb(seq, t), t);
            seq += 1;
            let snap = s.snapshot(t);
            assert!(snap.healthy, "unhealthy after {} ms", (t - T0) / MS);
            let live = snap.actionable_posterior(OH, t).is_some();
            assert_eq!(live, snap.actionable_shift(t).is_some());
            assert_eq!(
                live,
                t - T0 <= 60_000 * MS,
                "posterior liveness wrong after {} ms",
                (t - T0) / MS
            );
            if !live && aged_out_at.is_none() {
                aged_out_at = Some(t);
                assert_eq!(snap.actionable_posteriors(t).map(|m| m.len()), Some(0));
            }
        }
        assert!(aged_out_at.is_some(), "the posterior must age out past the TTL");
        // A fresh batch (seq 2 on the posteriors stream) makes the race actionable again.
        s.on_message(post(2, t, vec![race(OH, 0.6)]), t);
        assert!(s.snapshot(t).actionable_posterior(OH, t).is_some());
    }

    #[test]
    fn a_silent_needle_clears_posteriors_before_the_ttl() {
        let mut s = healthy_state().with_posterior_ttl(Duration::from_secs(60));
        // 1.001 s of silence trips the 1 s watchdog; the 60 s TTL does not keep the number alive.
        let t = T0 + 1_001 * MS;
        s.on_tick(t);
        let snap = s.snapshot(t);
        assert!(!snap.healthy);
        assert_eq!(snap.epoch, 1);
        assert!(snap.actionable_posteriors(t).is_none());
        // Recovery (2 healthy messages) starts a new epoch with no carried-over numbers.
        s.on_message(hb(2, t), t);
        s.on_message(hb(3, t), t);
        let snap = s.snapshot(t);
        assert!(snap.healthy);
        assert_eq!(snap.actionable_posteriors(t).map(|m| m.len()), Some(0));
    }

    #[test]
    fn posterior_ttl_never_undercuts_the_heartbeat_timeout() {
        let s = state().with_posterior_ttl(Duration::from_millis(10));
        assert_eq!(s.snapshot(T0).posterior_ttl_ns(), 1_000 * MS);
        let c = MonitorCfg::from_config(&NeedleCfg {
            heartbeat_timeout_s: 10,
            resume_after_healthy: 5,
            posterior_max_age_s: 600,
        });
        assert_eq!(c.watchdog.timeout, Duration::from_secs(10));
        assert_eq!(c.posterior_ttl, Duration::from_secs(600));
        assert_eq!(c.tick, Duration::from_secs(1));
        assert_eq!(
            MonitorCfg::new(WatchdogCfg::new(Duration::from_secs(10), 5)).posterior_ttl,
            Duration::from_secs(10),
            "without a config the old behaviour holds"
        );
    }

    #[test]
    fn shift_is_stored_when_healthy() {
        let mut s = healthy_state();
        s.on_message(
            NeedleMessage::Shift(v1::NationalShift {
                header: header(1, T0),
                mean: 1.25,
                sd: 0.5,
                n_races_informing: 3,
            }),
            T0,
        );
        let snap = s.snapshot(T0);
        let shift = snap.actionable_shift(T0).expect("shift");
        assert!((shift.value.mean - 1.25).abs() < 1e-12);
    }

    #[test]
    fn known_races_update_prunes_and_gates() {
        let mut s = healthy_state();
        s.set_known_races([GA.to_owned()].into_iter().collect());
        assert!(s.snapshot(T0).actionable_posterior(OH, T0).is_none());
        s.on_message(post(2, T0, vec![race(OH, 0.5)]), T0);
        assert!(!s.snapshot(T0).healthy, "OH is no longer in the market map");
    }

    #[test]
    fn stop_makes_everything_unactionable() {
        let mut s = healthy_state();
        s.stop();
        let snap = s.snapshot(T0);
        assert!(!snap.healthy);
        assert_eq!(snap.reason.as_deref(), Some("needle monitor stopped"));
        assert!(snap.actionable_posteriors(T0).is_none());
    }

    #[test]
    fn monitor_cfg_tick_is_clamped() {
        let c = MonitorCfg::new(WatchdogCfg::new(Duration::from_secs(10), 5));
        assert_eq!(c.tick, Duration::from_secs(1));
        let c = MonitorCfg::new(WatchdogCfg::new(Duration::from_millis(50), 5));
        assert_eq!(c.tick, Duration::from_millis(10));
    }

    #[tokio::test]
    async fn task_runs_and_stops_on_cancel() {
        let clock = Arc::new(susq_core::ManualClock::at_ns(T0));
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let cancel = CancellationToken::new();
        let cfg = MonitorCfg::new(WatchdogCfg::new(Duration::from_millis(1_000), 2));
        let monitor = NeedleMonitor::spawn(rx, races(), cfg, clock.clone(), cancel.clone());
        let mut snaps = monitor.subscribe();
        tx.unbounded_send(hb(1, T0)).expect("send");
        tx.unbounded_send(hb(2, T0)).expect("send");
        snaps.wait_for(|s| s.healthy).await.expect("monitor alive");
        monitor.set_known_races(HashSet::new());
        cancel.cancel();
        let final_rx = monitor.subscribe();
        monitor.join().await;
        assert!(!final_rx.borrow().healthy);
    }
}
