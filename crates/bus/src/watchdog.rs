//! Needle health watchdog: a pure state machine (no I/O; time is always passed in as `now_ns`).
//!
//! # Rules
//!
//! * Starts **unhealthy** ([`UnhealthyReason::NoData`]).
//! * Every message on a needle stream is judged. It is *unhealthy* if its header is missing or has
//!   the wrong schema, comes from a producer other than the configured one, is stale
//!   (`now - produced_at > timeout`), is dated too far in the future, or its `seq` is not exactly
//!   `last + 1` for that stream (gap, duplicate, or out-of-order). The caller may also report
//!   content-level failures via [`Watchdog::on_rejected`].
//! * After judging, the stream's `seq` baseline is reset to the received `seq`, so a restarted
//!   needle (seq back to 1) costs one unhealthy event and then recovers normally.
//! * **Liveness rule:** any healthy message on *any* needle stream (posteriors, national shift, or
//!   heartbeat) keeps the needle alive; heartbeats are not required specifically. This matches the
//!   brief ("no heartbeat or posterior for the interval"): heartbeats alone keep an idle needle
//!   alive, and a posterior stream that keeps flowing is proof of life by itself.
//!   If no healthy message arrives for longer than the timeout, the needle is unhealthy
//!   ([`UnhealthyReason::Silent`]), detected by [`Watchdog::on_tick`] or by the next message.
//! * Any unhealthy event resets the consecutive-healthy counter to 0, and so does a silence longer
//!   than the timeout (a run of healthy messages must be unbroken in time as well as in content).
//!   The needle becomes healthy again only after `resume_after_healthy` consecutive healthy
//!   messages.
//! * Every healthy -> unhealthy transition bumps [`Watchdog::epoch`] and queues a
//!   [`Transition`], so the executor cancels needle-derived orders exactly once per outage.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use susq_contracts::{SCHEMA_VERSION, v1};
use susq_core::clock::duration_ns;
use susq_core::config::NeedleCfg;

use crate::subjects;

/// Producer name the needle stamps into its headers.
pub const NEEDLE_PRODUCER: &str = "needle";

/// How far in the future a `produced_at_ns` may be before the message is refused (same-host clock).
pub const DEFAULT_MAX_FUTURE_SKEW: Duration = Duration::from_secs(2);

/// One of the needle's outbound streams; each has its own `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeedleStream {
    /// `needle.race_posteriors`
    Posteriors,
    /// `needle.national_shift`
    Shift,
    /// `needle.heartbeat`
    Heartbeat,
}

impl NeedleStream {
    /// Every needle stream.
    pub const ALL: [Self; 3] = [Self::Posteriors, Self::Shift, Self::Heartbeat];

    /// NATS subject carrying this stream.
    pub fn subject(self) -> &'static str {
        match self {
            Self::Posteriors => subjects::NEEDLE_RACE_POSTERIORS,
            Self::Shift => subjects::NEEDLE_NATIONAL_SHIFT,
            Self::Heartbeat => subjects::NEEDLE_HEARTBEAT,
        }
    }

    /// The stream carried on `subject`, if it is a needle subject.
    pub fn from_subject(subject: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.subject() == subject)
    }

    fn index(self) -> usize {
        match self {
            Self::Posteriors => 0,
            Self::Shift => 1,
            Self::Heartbeat => 2,
        }
    }
}

impl fmt::Display for NeedleStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.subject())
    }
}

/// Why the needle is not trusted right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnhealthyReason {
    /// No healthy message has ever been seen.
    NoData,
    /// No healthy message on any stream for `quiet_ns` (> timeout).
    Silent { quiet_ns: i64 },
    /// `seq` skipped ahead: at least one message was lost.
    SeqGap {
        stream: NeedleStream,
        expected: u64,
        got: u64,
    },
    /// `seq` repeated or went backwards (duplicate, reorder, or needle restart).
    OutOfOrder {
        stream: NeedleStream,
        last: u64,
        got: u64,
    },
    /// Header missing or schema version mismatch.
    BadHeader { stream: NeedleStream, detail: String },
    /// Header names a producer other than the expected needle.
    WrongProducer { stream: NeedleStream, got: String },
    /// Message older than the timeout when it arrived.
    Stale { stream: NeedleStream, age_ns: i64 },
    /// Message dated further in the future than the allowed skew.
    FromFuture { stream: NeedleStream, ahead_ns: i64 },
    /// Payload failed to decode or failed content validation.
    Invalid { stream: NeedleStream, detail: String },
    /// The needle's own heartbeat reported a non-"ok" status.
    Degraded { status: String },
    /// The monitor stopped; nothing downstream may act on needle numbers.
    Stopped,
}

impl fmt::Display for UnhealthyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoData => f.write_str("no needle data yet"),
            Self::Silent { quiet_ns } => write!(f, "needle silent for {} ms", quiet_ns / 1_000_000),
            Self::SeqGap {
                stream,
                expected,
                got,
            } => {
                write!(f, "{stream}: seq gap (expected {expected}, got {got})")
            }
            Self::OutOfOrder { stream, last, got } => {
                write!(f, "{stream}: seq out of order (last {last}, got {got})")
            }
            Self::BadHeader { stream, detail } => write!(f, "{stream}: bad header: {detail}"),
            Self::WrongProducer { stream, got } => {
                write!(f, "{stream}: unexpected producer {got:?}")
            }
            Self::Stale { stream, age_ns } => {
                write!(f, "{stream}: stale message ({} ms old)", age_ns / 1_000_000)
            }
            Self::FromFuture { stream, ahead_ns } => {
                write!(f, "{stream}: message {} ms in the future", ahead_ns / 1_000_000)
            }
            Self::Invalid { stream, detail } => write!(f, "{stream}: invalid message: {detail}"),
            Self::Degraded { status } => write!(f, "needle heartbeat reports {status:?}"),
            Self::Stopped => f.write_str("needle monitor stopped"),
        }
    }
}

/// A change of trust the executor must react to exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Stop acting on needle numbers and cancel orders derived from them.
    BecameUnhealthy { reason: UnhealthyReason, epoch: u64 },
    /// Needle numbers may be used again.
    BecameHealthy,
}

/// Watchdog tuning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogCfg {
    /// Silence / staleness limit (`needle.heartbeat_timeout_s`).
    pub timeout: Duration,
    /// Consecutive healthy messages required to resume (`needle.resume_after_healthy`, min 1).
    pub resume_after_healthy: u32,
    /// Largest accepted `produced_at_ns - now`.
    pub max_future_skew: Duration,
    /// Required `header.producer.name`.
    pub producer: String,
}

impl WatchdogCfg {
    /// Config with the default skew and producer name.
    pub fn new(timeout: Duration, resume_after_healthy: u32) -> Self {
        Self {
            timeout,
            resume_after_healthy: resume_after_healthy.max(1),
            max_future_skew: DEFAULT_MAX_FUTURE_SKEW,
            producer: NEEDLE_PRODUCER.to_owned(),
        }
    }

    /// From the `[needle]` config section.
    pub fn from_config(cfg: &NeedleCfg) -> Self {
        Self::new(
            Duration::from_secs(cfg.heartbeat_timeout_s),
            cfg.resume_after_healthy,
        )
    }
}

/// The needle health state machine. See the module docs for the rules.
#[derive(Debug, Clone)]
pub struct Watchdog {
    timeout_ns: i64,
    skew_ns: i64,
    needed: u32,
    producer: String,
    last_seq: [Option<u64>; 3],
    streak: u32,
    reason: Option<UnhealthyReason>,
    last_healthy_msg_ns: Option<i64>,
    last_ok_ns: i64,
    epoch: u64,
    transitions: VecDeque<Transition>,
}

impl Watchdog {
    /// A watchdog that has seen nothing yet (unhealthy).
    pub fn new(cfg: WatchdogCfg) -> Self {
        Self {
            timeout_ns: duration_ns(cfg.timeout),
            skew_ns: duration_ns(cfg.max_future_skew),
            needed: cfg.resume_after_healthy.max(1),
            producer: cfg.producer,
            last_seq: [None; 3],
            streak: 0,
            reason: Some(UnhealthyReason::NoData),
            last_healthy_msg_ns: None,
            last_ok_ns: 0,
            epoch: 0,
            transitions: VecDeque::new(),
        }
    }

    /// Judge a message whose content already passed validation. Returns why it was unhealthy, if
    /// it was; the content of an unhealthy message must not be used.
    pub fn on_message(
        &mut self,
        stream: NeedleStream,
        header: Option<&v1::Header>,
        now_ns: i64,
    ) -> Result<(), UnhealthyReason> {
        self.check_silence(now_ns);
        let verdict = self.judge(stream, header, now_ns);
        self.rebaseline(stream, header);
        match &verdict {
            Ok(()) => self.record_healthy(now_ns),
            Err(reason) => self.record_unhealthy(reason.clone()),
        }
        verdict
    }

    /// Record a needle message that is unhealthy for a reason the watchdog cannot see itself
    /// (undecodable payload, failed content validation, degraded heartbeat). Its `seq`, when
    /// readable, still becomes the stream's baseline.
    pub fn on_rejected(
        &mut self,
        stream: NeedleStream,
        header: Option<&v1::Header>,
        reason: UnhealthyReason,
        now_ns: i64,
    ) {
        self.check_silence(now_ns);
        self.rebaseline(stream, header);
        self.record_unhealthy(reason);
    }

    /// Periodic check: the needle is unhealthy once no healthy message arrived for > timeout.
    pub fn on_tick(&mut self, now_ns: i64) {
        self.check_silence(now_ns);
    }

    /// Force the unhealthy state (e.g. the monitor is shutting down).
    pub fn mark_unhealthy(&mut self, reason: UnhealthyReason) {
        self.record_unhealthy(reason);
    }

    /// True when needle numbers may be acted on (as of the last event or tick).
    pub fn is_healthy(&self) -> bool {
        self.reason.is_none()
    }

    /// Why the needle is unhealthy (`None` when healthy).
    pub fn reason(&self) -> Option<&UnhealthyReason> {
        self.reason.as_ref()
    }

    /// Consecutive healthy messages in the current run.
    pub fn healthy_streak(&self) -> u32 {
        self.streak
    }

    /// Healthy messages required to resume.
    pub fn resume_after(&self) -> u32 {
        self.needed
    }

    /// Number of healthy -> unhealthy transitions so far.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Receive time of the last healthy message.
    pub fn last_healthy_msg_ns(&self) -> Option<i64> {
        self.last_healthy_msg_ns
    }

    /// Oldest transition not yet taken. Each transition is returned exactly once, in order.
    pub fn take_transition(&mut self) -> Option<Transition> {
        self.transitions.pop_front()
    }

    /// Dashboard/status entry named "needle".
    pub fn health_item(&self, now_ns: i64) -> v1::HealthItem {
        let (state, detail) = match &self.reason {
            None => {
                let ago_ms = self
                    .last_healthy_msg_ns
                    .map_or(0, |t| now_ns.saturating_sub(t) / 1_000_000);
                ("ok", format!("last healthy message {ago_ms} ms ago"))
            }
            Some(reason) => {
                let state = match reason {
                    UnhealthyReason::NoData | UnhealthyReason::Silent { .. } | UnhealthyReason::Stopped => {
                        "down"
                    }
                    _ => "degraded",
                };
                let detail = format!(
                    "{reason}; {}/{} healthy messages toward resume",
                    self.streak, self.needed
                );
                (state, detail)
            }
        };
        v1::HealthItem {
            name: "needle".to_owned(),
            state: state.to_owned(),
            detail,
            last_ok_ns: self.last_ok_ns,
        }
    }

    fn judge(
        &self,
        stream: NeedleStream,
        header: Option<&v1::Header>,
        now_ns: i64,
    ) -> Result<(), UnhealthyReason> {
        let Some(h) = header else {
            return Err(UnhealthyReason::BadHeader {
                stream,
                detail: "missing header".to_owned(),
            });
        };
        if h.schema_version != SCHEMA_VERSION {
            return Err(UnhealthyReason::BadHeader {
                stream,
                detail: format!("schema version {} (want {SCHEMA_VERSION})", h.schema_version),
            });
        }
        let producer = h.producer.as_ref().map_or("", |p| p.name.as_str());
        if producer != self.producer {
            return Err(UnhealthyReason::WrongProducer {
                stream,
                got: producer.to_owned(),
            });
        }
        let age_ns = now_ns.saturating_sub(h.produced_at_ns);
        if age_ns > self.timeout_ns {
            return Err(UnhealthyReason::Stale { stream, age_ns });
        }
        if age_ns < 0 && age_ns.saturating_neg() > self.skew_ns {
            return Err(UnhealthyReason::FromFuture {
                stream,
                ahead_ns: age_ns.saturating_neg(),
            });
        }
        if let Some(last) = self.last_seq[stream.index()] {
            match last.checked_add(1) {
                Some(expected) if h.seq == expected => {}
                Some(expected) if h.seq > expected => {
                    return Err(UnhealthyReason::SeqGap {
                        stream,
                        expected,
                        got: h.seq,
                    });
                }
                _ => {
                    return Err(UnhealthyReason::OutOfOrder {
                        stream,
                        last,
                        got: h.seq,
                    });
                }
            }
        }
        Ok(())
    }

    /// Only the expected producer's `seq` may move the baseline; a foreign or headerless message
    /// must not mask a real gap.
    fn rebaseline(&mut self, stream: NeedleStream, header: Option<&v1::Header>) {
        if let Some(h) = header
            && h.producer.as_ref().is_some_and(|p| p.name == self.producer)
        {
            self.last_seq[stream.index()] = Some(h.seq);
        }
    }

    fn check_silence(&mut self, now_ns: i64) {
        let Some(last) = self.last_healthy_msg_ns else {
            return;
        };
        let quiet_ns = now_ns.saturating_sub(last);
        if quiet_ns > self.timeout_ns && (self.streak > 0 || self.reason.is_none()) {
            self.record_unhealthy(UnhealthyReason::Silent { quiet_ns });
        }
    }

    fn record_healthy(&mut self, now_ns: i64) {
        self.streak = self.streak.saturating_add(1);
        self.last_healthy_msg_ns = Some(now_ns);
        if self.reason.is_some() && self.streak >= self.needed {
            self.reason = None;
            self.transitions.push_back(Transition::BecameHealthy);
        }
        if self.reason.is_none() {
            self.last_ok_ns = now_ns;
        }
    }

    fn record_unhealthy(&mut self, reason: UnhealthyReason) {
        self.streak = 0;
        if self.reason.is_none() {
            self.epoch += 1;
            self.transitions.push_back(Transition::BecameUnhealthy {
                reason: reason.clone(),
                epoch: self.epoch,
            });
        }
        self.reason = Some(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    const T0: i64 = 1_800_000_000_000_000_000;

    fn cfg() -> WatchdogCfg {
        WatchdogCfg::new(Duration::from_millis(1_000), 3)
    }

    fn header(seq: u64, produced_at_ns: i64) -> v1::Header {
        v1::Header {
            schema_version: SCHEMA_VERSION,
            produced_at_ns,
            producer: Some(v1::Producer {
                name: NEEDLE_PRODUCER.to_owned(),
                git_sha: "abc".to_owned(),
            }),
            seq,
        }
    }

    /// Feed `count` healthy heartbeats starting at `seq`, 10 ms apart from `t`. Returns the next
    /// (seq, t).
    fn feed(wd: &mut Watchdog, seq: u64, t: i64, count: u64) -> (u64, i64) {
        let mut t = t;
        for s in seq..seq + count {
            wd.on_message(NeedleStream::Heartbeat, Some(&header(s, t)), t)
                .expect("healthy message");
            t += 10 * MS;
        }
        (seq + count, t)
    }

    fn healthy_watchdog() -> (Watchdog, u64, i64) {
        let mut wd = Watchdog::new(cfg());
        let (seq, t) = feed(&mut wd, 1, T0, 3);
        assert!(wd.is_healthy());
        assert_eq!(wd.take_transition(), Some(Transition::BecameHealthy));
        (wd, seq, t)
    }

    #[test]
    fn starts_unhealthy_without_transition() {
        let mut wd = Watchdog::new(cfg());
        assert!(!wd.is_healthy());
        assert_eq!(wd.reason(), Some(&UnhealthyReason::NoData));
        assert_eq!(wd.take_transition(), None);
        wd.on_tick(T0 + 60_000 * MS);
        assert_eq!(wd.reason(), Some(&UnhealthyReason::NoData));
        assert_eq!(wd.epoch(), 0);
    }

    #[test]
    fn becomes_healthy_only_after_n_messages() {
        let mut wd = Watchdog::new(cfg());
        feed(&mut wd, 1, T0, 2);
        assert!(!wd.is_healthy());
        assert_eq!(wd.healthy_streak(), 2);
        assert_eq!(wd.take_transition(), None);
        feed(&mut wd, 3, T0 + 20 * MS, 1);
        assert!(wd.is_healthy());
        assert_eq!(wd.take_transition(), Some(Transition::BecameHealthy));
        assert_eq!(wd.take_transition(), None);
    }

    #[test]
    fn streams_count_together_but_track_seq_separately() {
        let mut wd = Watchdog::new(cfg());
        wd.on_message(NeedleStream::Heartbeat, Some(&header(10, T0)), T0)
            .expect("hb");
        wd.on_message(NeedleStream::Posteriors, Some(&header(1, T0)), T0)
            .expect("post");
        wd.on_message(NeedleStream::Shift, Some(&header(500, T0)), T0)
            .expect("shift");
        assert!(wd.is_healthy());
        wd.on_message(NeedleStream::Posteriors, Some(&header(2, T0)), T0)
            .expect("post 2");
        wd.on_message(NeedleStream::Heartbeat, Some(&header(11, T0)), T0)
            .expect("hb 11");
    }

    #[test]
    fn seq_gap_trips_and_rebaselines() {
        let (mut wd, seq, t) = healthy_watchdog();
        let err = wd
            .on_message(NeedleStream::Heartbeat, Some(&header(seq + 1, t)), t)
            .expect_err("gap");
        assert_eq!(
            err,
            UnhealthyReason::SeqGap {
                stream: NeedleStream::Heartbeat,
                expected: seq,
                got: seq + 1
            }
        );
        assert!(!wd.is_healthy());
        assert_eq!(
            wd.take_transition(),
            Some(Transition::BecameUnhealthy {
                reason: err,
                epoch: 1
            })
        );
        feed(&mut wd, seq + 2, t, 3);
        assert!(wd.is_healthy());
    }

    #[test]
    fn duplicate_and_reorder_are_unhealthy() {
        let (mut wd, seq, t) = healthy_watchdog();
        let dup = wd
            .on_message(NeedleStream::Heartbeat, Some(&header(seq - 1, t)), t)
            .expect_err("duplicate");
        assert!(matches!(dup, UnhealthyReason::OutOfOrder { got, .. } if got == seq - 1));
        let back = wd
            .on_message(NeedleStream::Heartbeat, Some(&header(seq - 3, t)), t)
            .expect_err("backwards");
        assert!(matches!(back, UnhealthyReason::OutOfOrder { .. }));
    }

    #[test]
    fn restarted_needle_recovers_after_n() {
        let (mut wd, _, t) = healthy_watchdog();
        wd.on_message(NeedleStream::Heartbeat, Some(&header(1, t)), t)
            .expect_err("restart looks out of order");
        assert!(!wd.is_healthy());
        feed(&mut wd, 2, t + 10 * MS, 2);
        assert!(!wd.is_healthy());
        feed(&mut wd, 4, t + 30 * MS, 1);
        assert!(wd.is_healthy());
    }

    #[test]
    fn header_problems_are_unhealthy() {
        let (mut wd, seq, t) = healthy_watchdog();
        let err = wd
            .on_message(NeedleStream::Posteriors, None, t)
            .expect_err("missing");
        assert!(matches!(err, UnhealthyReason::BadHeader { .. }));

        let mut h = header(seq, t);
        h.schema_version = 99;
        let err = wd
            .on_message(NeedleStream::Heartbeat, Some(&h), t)
            .expect_err("schema");
        assert!(matches!(err, UnhealthyReason::BadHeader { .. }));

        let mut h = header(seq + 1, t);
        h.producer = Some(v1::Producer {
            name: "impostor".to_owned(),
            git_sha: String::new(),
        });
        let err = wd
            .on_message(NeedleStream::Heartbeat, Some(&h), t)
            .expect_err("producer");
        assert!(matches!(err, UnhealthyReason::WrongProducer { .. }));
        // The impostor's seq did not move the baseline: seq + 1 is still in order.
        wd.on_message(NeedleStream::Heartbeat, Some(&header(seq + 1, t)), t)
            .expect("baseline untouched by impostor");
    }

    #[test]
    fn stale_message_is_refused_even_in_order() {
        let (mut wd, seq, t) = healthy_watchdog();
        let err = wd
            .on_message(NeedleStream::Posteriors, Some(&header(1, t - 1_001 * MS)), t)
            .expect_err("stale");
        assert!(matches!(err, UnhealthyReason::Stale { age_ns, .. } if age_ns == 1_001 * MS));
        // Exactly at the timeout is still acceptable.
        let mut wd2 = Watchdog::new(cfg());
        wd2.on_message(NeedleStream::Heartbeat, Some(&header(seq, t - 1_000 * MS)), t)
            .expect("boundary");
    }

    #[test]
    fn future_dated_message_is_refused() {
        let (mut wd, seq, t) = healthy_watchdog();
        wd.on_message(NeedleStream::Heartbeat, Some(&header(seq, t + 1_999 * MS)), t)
            .expect("within skew");
        let err = wd
            .on_message(NeedleStream::Heartbeat, Some(&header(seq + 1, t + 2_001 * MS)), t)
            .expect_err("too far ahead");
        assert!(matches!(err, UnhealthyReason::FromFuture { .. }));
    }

    #[test]
    fn silence_trips_on_tick_once() {
        let (mut wd, seq, t) = healthy_watchdog();
        let last = t - 10 * MS;
        wd.on_tick(last + 1_000 * MS);
        assert!(wd.is_healthy(), "exactly at timeout is not silent");
        wd.on_tick(last + 1_001 * MS);
        assert!(!wd.is_healthy());
        assert!(matches!(
            wd.take_transition(),
            Some(Transition::BecameUnhealthy {
                reason: UnhealthyReason::Silent { .. },
                epoch: 1
            })
        ));
        wd.on_tick(last + 5_000 * MS);
        assert_eq!(wd.take_transition(), None, "one transition per outage");
        assert_eq!(wd.epoch(), 1);
        let t2 = last + 6_000 * MS;
        feed(&mut wd, seq, t2, 3);
        assert!(wd.is_healthy());
    }

    #[test]
    fn silence_detected_by_next_message_without_ticks() {
        let (mut wd, seq, t) = healthy_watchdog();
        let late = t + 5_000 * MS;
        wd.on_message(NeedleStream::Heartbeat, Some(&header(seq, late)), late)
            .expect("message itself is fine");
        assert!(!wd.is_healthy());
        assert_eq!(wd.healthy_streak(), 1);
        assert!(matches!(
            wd.take_transition(),
            Some(Transition::BecameUnhealthy {
                reason: UnhealthyReason::Silent { .. },
                ..
            })
        ));
    }

    #[test]
    fn silence_during_recovery_restarts_the_count() {
        let mut wd = Watchdog::new(cfg());
        feed(&mut wd, 1, T0, 2);
        wd.on_tick(T0 + 3_000 * MS);
        assert_eq!(wd.healthy_streak(), 0);
        feed(&mut wd, 3, T0 + 3_000 * MS, 1);
        assert!(!wd.is_healthy());
    }

    #[test]
    fn rejected_message_resets_and_rebaselines() {
        let (mut wd, seq, t) = healthy_watchdog();
        wd.on_rejected(
            NeedleStream::Heartbeat,
            Some(&header(seq, t)),
            UnhealthyReason::Degraded {
                status: "degraded:feed".to_owned(),
            },
            t,
        );
        assert!(!wd.is_healthy());
        assert_eq!(wd.healthy_streak(), 0);
        feed(&mut wd, seq + 1, t, 3);
        assert!(wd.is_healthy());
    }

    #[test]
    fn transitions_are_queued_in_order() {
        let (mut wd, seq, t) = healthy_watchdog();
        wd.on_message(NeedleStream::Heartbeat, Some(&header(seq + 5, t)), t)
            .expect_err("gap");
        feed(&mut wd, seq + 6, t, 3);
        wd.mark_unhealthy(UnhealthyReason::Stopped);
        assert!(matches!(
            wd.take_transition(),
            Some(Transition::BecameUnhealthy { epoch: 1, .. })
        ));
        assert_eq!(wd.take_transition(), Some(Transition::BecameHealthy));
        assert_eq!(
            wd.take_transition(),
            Some(Transition::BecameUnhealthy {
                reason: UnhealthyReason::Stopped,
                epoch: 2
            })
        );
        assert_eq!(wd.take_transition(), None);
    }

    #[test]
    fn health_item_reports_state() {
        let mut wd = Watchdog::new(cfg());
        let item = wd.health_item(T0);
        assert_eq!(item.name, "needle");
        assert_eq!(item.state, "down");
        assert_eq!(item.last_ok_ns, 0);
        let (_, t) = feed(&mut wd, 1, T0, 3);
        let item = wd.health_item(t);
        assert_eq!(item.state, "ok");
        assert_eq!(item.last_ok_ns, t - 10 * MS);
        wd.on_message(NeedleStream::Heartbeat, Some(&header(99, t)), t)
            .expect_err("gap");
        let item = wd.health_item(t);
        assert_eq!(item.state, "degraded");
        assert!(item.detail.contains("0/3"), "{}", item.detail);
    }

    #[test]
    fn config_from_needle_section() {
        let c = WatchdogCfg::from_config(&NeedleCfg {
            heartbeat_timeout_s: 10,
            resume_after_healthy: 5,
        });
        assert_eq!(c.timeout, Duration::from_secs(10));
        assert_eq!(c.resume_after_healthy, 5);
        assert_eq!(c.producer, NEEDLE_PRODUCER);
        assert_eq!(
            WatchdogCfg::new(Duration::from_secs(1), 0).resume_after_healthy,
            1
        );
    }

    #[test]
    fn stream_subjects_round_trip() {
        for s in NeedleStream::ALL {
            assert_eq!(NeedleStream::from_subject(s.subject()), Some(s));
        }
        assert_eq!(NeedleStream::from_subject("ops.kill"), None);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        const TIMEOUT_MS: i64 = 100;
        const N: u32 = 4;

        #[derive(Debug, Clone)]
        enum Event {
            Msg {
                stream: usize,
                seq_delta: i64,
                delay_ms: i64,
                age_ms: i64,
            },
            Tick {
                delay_ms: i64,
            },
        }

        fn event() -> impl Strategy<Value = Event> {
            prop_oneof![
                4 => (
                    0usize..3,
                    prop::sample::select(vec![-1i64, 0, 1, 1, 1, 1, 1, 1, 2]),
                    0i64..2 * TIMEOUT_MS,
                    prop::sample::select(vec![0i64, 0, 0, 5, 50, TIMEOUT_MS + 1]),
                )
                    .prop_map(|(stream, seq_delta, delay_ms, age_ms)| Event::Msg {
                        stream,
                        seq_delta,
                        delay_ms: delay_ms / 3,
                        age_ms,
                    }),
                1 => (0i64..2 * TIMEOUT_MS).prop_map(|delay_ms| Event::Tick { delay_ms }),
            ]
        }

        proptest! {
            /// Healthy exactly when the trailing run of healthy messages, unbroken by any gap
            /// longer than the timeout (including up to now), has at least N members.
            #[test]
            fn healthy_iff_last_n_healthy_and_recent(events in prop::collection::vec(event(), 1..200)) {
                let mut wd = Watchdog::new(WatchdogCfg::new(
                    Duration::from_millis(u64::try_from(TIMEOUT_MS).expect("positive")),
                    N,
                ));
                let mut now = T0;
                let mut seqs = [100u64; 3];
                let mut run: Vec<i64> = Vec::new();
                let mut was_healthy = false;
                let mut epoch = 0u64;
                for ev in events {
                    let delay_ms = match &ev {
                        Event::Msg { delay_ms, .. } | Event::Tick { delay_ms } => *delay_ms,
                    };
                    now += delay_ms * MS;
                    if run.last().is_some_and(|&l| now - l > TIMEOUT_MS * MS) {
                        run.clear();
                    }
                    if let Event::Msg { stream, seq_delta, age_ms, .. } = ev {
                        let seq = seqs[stream].saturating_add_signed(seq_delta);
                        seqs[stream] = seq;
                        let verdict = wd.on_message(
                            NeedleStream::ALL[stream],
                            Some(&header(seq, now - age_ms * MS)),
                            now,
                        );
                        if verdict.is_ok() {
                            run.push(now);
                        } else {
                            run.clear();
                            prop_assert!(!wd.is_healthy());
                        }
                    } else {
                        wd.on_tick(now);
                    }
                    let model_healthy = run.len() >= N as usize;
                    prop_assert_eq!(wd.is_healthy(), model_healthy);
                    if wd.is_healthy() {
                        let last = *run.last().expect("non-empty run");
                        prop_assert!(now - last <= TIMEOUT_MS * MS);
                    }
                    while let Some(tr) = wd.take_transition() {
                        match tr {
                            Transition::BecameHealthy => {
                                prop_assert!(!was_healthy);
                                was_healthy = true;
                            }
                            Transition::BecameUnhealthy { epoch: e, .. } => {
                                prop_assert!(was_healthy);
                                was_healthy = false;
                                epoch += 1;
                                prop_assert_eq!(e, epoch);
                            }
                        }
                    }
                    prop_assert_eq!(was_healthy, wd.is_healthy());
                    prop_assert_eq!(wd.epoch(), epoch);
                }
            }
        }
    }
}
