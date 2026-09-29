//! Content validation for needle messages, plus the rolling error-rate counter risk watches.
//!
//! A message that fails validation is dropped whole (never partially applied), logged, and
//! counted in an [`ErrorRate`]. `ops.kill` is deliberately *not* validated here: a kill is honored
//! even when its header is missing or wrong (see [`crate::message::decode_kill`]).

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use susq_contracts::{check_header, v1};
use susq_core::clock::duration_ns;

/// Slack allowed on `p_dem + p_rep <= 1` for floating-point noise.
pub const PROB_SUM_TOLERANCE: f64 = 1e-6;

/// Why a message was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ValidationError {
    /// The payload did not decode as the subject's message type.
    #[error("undecodable payload: {0}")]
    Undecodable(String),
    /// Missing header or schema mismatch.
    #[error("header: {0}")]
    Header(String),
    /// A race entry has an empty `race_id`.
    #[error("race #{index}: empty race_id")]
    EmptyRaceId { index: usize },
    /// A race is not in the current market map.
    #[error("race {race_id}: not in the market map")]
    UnknownRace { race_id: String },
    /// The same race appears twice in one message.
    #[error("race {race_id}: listed more than once")]
    DuplicateRace { race_id: String },
    /// NaN or infinity.
    #[error("{context}: {field} is not finite ({value})")]
    NonFinite {
        context: String,
        field: &'static str,
        value: f64,
    },
    /// Value outside its allowed range.
    #[error("{context}: {field} = {value} outside [{min}, {max}]")]
    OutOfRange {
        context: String,
        field: &'static str,
        value: f64,
        min: f64,
        max: f64,
    },
    /// Win probabilities sum past 1.
    #[error("race {race_id}: p_dem + p_rep = {sum} > 1")]
    ProbSum { race_id: String, sum: f64 },
}

/// Validate a `needle.race_posteriors` message against the known race ids (from the market map).
pub fn race_posteriors(
    msg: &v1::RacePosteriors,
    known_races: &HashSet<String>,
) -> Result<(), ValidationError> {
    header(msg.header.as_ref())?;
    let mut seen = HashSet::with_capacity(msg.races.len());
    for (index, race) in msg.races.iter().enumerate() {
        if race.race_id.is_empty() {
            return Err(ValidationError::EmptyRaceId { index });
        }
        if !known_races.contains(&race.race_id) {
            return Err(ValidationError::UnknownRace {
                race_id: race.race_id.clone(),
            });
        }
        if !seen.insert(race.race_id.as_str()) {
            return Err(ValidationError::DuplicateRace {
                race_id: race.race_id.clone(),
            });
        }
        race_posterior(race)?;
    }
    Ok(())
}

/// Validate a `needle.national_shift` message.
pub fn national_shift(msg: &v1::NationalShift) -> Result<(), ValidationError> {
    header(msg.header.as_ref())?;
    let ctx = "national_shift";
    finite(ctx, "mean", msg.mean)?;
    in_range(ctx, "sd", msg.sd, 0.0, f64::INFINITY)
}

/// Validate a `needle.heartbeat` message.
pub fn heartbeat(msg: &v1::Heartbeat) -> Result<(), ValidationError> {
    header(msg.header.as_ref())
}

fn header(h: Option<&v1::Header>) -> Result<(), ValidationError> {
    check_header(h)
        .map(|_| ())
        .map_err(|e| ValidationError::Header(e.to_string()))
}

fn race_posterior(r: &v1::RacePosterior) -> Result<(), ValidationError> {
    let ctx = format!("race {}", r.race_id);
    finite(&ctx, "margin_mean", r.margin_mean)?;
    in_range(&ctx, "p_dem", r.p_dem, 0.0, 1.0)?;
    in_range(&ctx, "p_rep", r.p_rep, 0.0, 1.0)?;
    in_range(&ctx, "margin_sd", r.margin_sd, 0.0, f64::INFINITY)?;
    in_range(&ctx, "reported_frac", r.reported_frac, 0.0, 1.0)?;
    in_range(
        &ctx,
        "expected_remaining_votes",
        r.expected_remaining_votes,
        0.0,
        f64::INFINITY,
    )?;
    let sum = r.p_dem + r.p_rep;
    if sum > 1.0 + PROB_SUM_TOLERANCE {
        return Err(ValidationError::ProbSum {
            race_id: r.race_id.clone(),
            sum,
        });
    }
    Ok(())
}

fn finite(context: &str, field: &'static str, value: f64) -> Result<(), ValidationError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ValidationError::NonFinite {
            context: context.to_owned(),
            field,
            value,
        })
    }
}

/// Finite and within `[min, max]` (`max` may be infinite to mean "no upper bound").
fn in_range(
    context: &str,
    field: &'static str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<(), ValidationError> {
    finite(context, field, value)?;
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::OutOfRange {
            context: context.to_owned(),
            field,
            value,
            min,
            max,
        })
    }
}

/// Error timestamps over a rolling window (60 s by default).
///
/// Risk trips its breaker when [`ErrorRate::per_minute`] reaches `risk.bus_error_breaker_per_min`
/// (see [`ErrorRate::spiking`]).
#[derive(Debug, Clone)]
pub struct ErrorRate {
    window_ns: i64,
    events: VecDeque<i64>,
    total: u64,
}

impl Default for ErrorRate {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

impl ErrorRate {
    /// Counter over a custom window.
    pub fn new(window: Duration) -> Self {
        Self {
            window_ns: duration_ns(window),
            events: VecDeque::new(),
            total: 0,
        }
    }

    /// Record one error at `now_ns`.
    pub fn record(&mut self, now_ns: i64) {
        self.total += 1;
        self.events.push_back(now_ns);
        let cutoff = now_ns.saturating_sub(self.window_ns);
        while self.events.front().is_some_and(|&t| t <= cutoff) {
            self.events.pop_front();
        }
    }

    /// Errors within the window ending at `now_ns`.
    pub fn per_minute(&self, now_ns: i64) -> u32 {
        let cutoff = now_ns.saturating_sub(self.window_ns);
        let n = self.events.iter().filter(|&&t| t > cutoff).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// True when the windowed count is at or above `threshold`.
    pub fn spiking(&self, now_ns: i64, threshold: u32) -> bool {
        self.per_minute(now_ns) >= threshold
    }

    /// Errors ever recorded.
    pub fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_contracts::SCHEMA_VERSION;

    fn hdr() -> Option<v1::Header> {
        Some(v1::Header {
            schema_version: SCHEMA_VERSION,
            produced_at_ns: 1,
            producer: None,
            seq: 1,
        })
    }

    fn race(id: &str) -> v1::RacePosterior {
        v1::RacePosterior {
            race_id: id.to_owned(),
            p_dem: 0.6,
            p_rep: 0.4,
            margin_mean: 2.5,
            margin_sd: 3.0,
            expected_remaining_votes: 10_000.0,
            reported_frac: 0.4,
            n_batches: 3,
            n_counties_reporting: 2,
        }
    }

    fn known() -> HashSet<String> {
        ["2026-SEN-OH-S", "2026-GOV-GA"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn posteriors(races: Vec<v1::RacePosterior>) -> v1::RacePosteriors {
        v1::RacePosteriors { header: hdr(), races }
    }

    fn check_race(edit: impl FnOnce(&mut v1::RacePosterior)) -> Result<(), ValidationError> {
        let mut r = race("2026-SEN-OH-S");
        edit(&mut r);
        race_posteriors(&posteriors(vec![r]), &known())
    }

    #[test]
    fn good_posteriors_pass() {
        let msg = posteriors(vec![race("2026-SEN-OH-S"), race("2026-GOV-GA")]);
        assert_eq!(race_posteriors(&msg, &known()), Ok(()));
        assert_eq!(race_posteriors(&posteriors(vec![]), &known()), Ok(()));
    }

    #[test]
    fn boundaries_pass() {
        assert_eq!(check_race(|r| (r.p_dem, r.p_rep) = (1.0, 0.0)), Ok(()));
        assert_eq!(check_race(|r| (r.p_dem, r.p_rep) = (0.0, 0.0)), Ok(()));
        assert_eq!(check_race(|r| (r.p_dem, r.p_rep) = (0.5, 0.5 + 5e-7)), Ok(()));
        assert_eq!(check_race(|r| r.reported_frac = 1.0), Ok(()));
        assert_eq!(check_race(|r| r.margin_sd = 0.0), Ok(()));
        assert_eq!(check_race(|r| r.margin_mean = -40.0), Ok(()));
    }

    #[test]
    fn header_is_checked() {
        let mut msg = posteriors(vec![]);
        msg.header = None;
        assert!(matches!(
            race_posteriors(&msg, &known()),
            Err(ValidationError::Header(_))
        ));
        let bad = v1::Heartbeat {
            header: Some(v1::Header {
                schema_version: 7,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(heartbeat(&bad), Err(ValidationError::Header(_))));
        let ok = v1::Heartbeat {
            header: hdr(),
            ..Default::default()
        };
        assert_eq!(heartbeat(&ok), Ok(()));
    }

    #[test]
    fn race_ids_are_checked() {
        let msg = posteriors(vec![race("")]);
        assert_eq!(
            race_posteriors(&msg, &known()),
            Err(ValidationError::EmptyRaceId { index: 0 })
        );
        let msg = posteriors(vec![race("2026-SEN-OH-S"), race("2026-HOUSE-ZZ-99")]);
        assert!(matches!(
            race_posteriors(&msg, &known()),
            Err(ValidationError::UnknownRace { race_id }) if race_id == "2026-HOUSE-ZZ-99"
        ));
        let msg = posteriors(vec![race("2026-GOV-GA"), race("2026-GOV-GA")]);
        assert!(matches!(
            race_posteriors(&msg, &known()),
            Err(ValidationError::DuplicateRace { .. })
        ));
    }

    #[test]
    fn non_finite_values_fail() {
        let edits: [fn(&mut v1::RacePosterior); 6] = [
            |r| r.p_dem = f64::NAN,
            |r| r.p_rep = f64::INFINITY,
            |r| r.margin_mean = f64::NEG_INFINITY,
            |r| r.margin_sd = f64::NAN,
            |r| r.expected_remaining_votes = f64::INFINITY,
            |r| r.reported_frac = f64::NAN,
        ];
        for edit in edits {
            assert!(matches!(check_race(edit), Err(ValidationError::NonFinite { .. })));
        }
    }

    #[test]
    fn out_of_range_values_fail() {
        let edits: [fn(&mut v1::RacePosterior); 7] = [
            |r| r.p_dem = -0.01,
            |r| (r.p_dem, r.p_rep) = (1.01, 0.0),
            |r| r.p_rep = -1e-9,
            |r| r.margin_sd = -0.1,
            |r| r.reported_frac = 1.2,
            |r| r.reported_frac = -0.1,
            |r| r.expected_remaining_votes = -1.0,
        ];
        for edit in edits {
            assert!(matches!(
                check_race(edit),
                Err(ValidationError::OutOfRange { .. })
            ));
        }
    }

    #[test]
    fn probability_sum_fails() {
        assert!(matches!(
            check_race(|r| (r.p_dem, r.p_rep) = (0.7, 0.4)),
            Err(ValidationError::ProbSum { .. })
        ));
    }

    #[test]
    fn national_shift_checks() {
        let good = v1::NationalShift {
            header: hdr(),
            mean: -1.5,
            sd: 2.0,
            n_races_informing: 4,
        };
        assert_eq!(national_shift(&good), Ok(()));
        let mut bad = good.clone();
        bad.mean = f64::NAN;
        assert!(national_shift(&bad).is_err());
        let mut bad = good.clone();
        bad.sd = -1.0;
        assert!(national_shift(&bad).is_err());
        let mut bad = good;
        bad.sd = f64::INFINITY;
        assert!(national_shift(&bad).is_err());
    }

    #[test]
    fn error_messages_are_readable() {
        let err = check_race(|r| r.p_dem = 1.5).expect_err("range");
        assert_eq!(err.to_string(), "race 2026-SEN-OH-S: p_dem = 1.5 outside [0, 1]");
    }

    #[test]
    fn error_rate_rolls_over_sixty_seconds() {
        const S: i64 = 1_000_000_000;
        let mut rate = ErrorRate::default();
        assert_eq!(rate.per_minute(0), 0);
        for i in 0..5 {
            rate.record(i * S);
        }
        assert_eq!(rate.per_minute(4 * S), 5);
        assert!(rate.spiking(4 * S, 5));
        assert!(!rate.spiking(4 * S, 6));
        // At t = 60 s the event at t = 0 has left the window.
        assert_eq!(rate.per_minute(60 * S), 4);
        assert_eq!(rate.per_minute(64 * S), 0);
        rate.record(200 * S);
        assert_eq!(rate.per_minute(200 * S), 1);
        assert_eq!(rate.total(), 6);
    }
}
