//! Pure checks on a parsed `TargetFile`: content ranges (time-independent) and freshness (time-dependent).

use std::collections::HashSet;

use susq_contracts::v1::{ClusterLimit, FairValue, TargetFile, TargetPosition, TourneyEstimate};

use crate::error::{ContentError, RejectReason};

/// Time limits a file must satisfy at the moment it is used.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Maximum `now - produced_at` in nanoseconds.
    pub(crate) max_age_ns: i64,
    /// Maximum `produced_at - now` in nanoseconds tolerated as clock skew.
    pub(crate) future_skew_ns: i64,
}

/// Fails if `file` is produced too far in the future, expired, or older than the maximum age.
pub(crate) fn check_fresh(file: &TargetFile, now_ns: i64, limits: Limits) -> Result<(), RejectReason> {
    let produced_at_ns = file.header.as_ref().map_or(0, |h| h.produced_at_ns);
    if produced_at_ns.saturating_sub(now_ns) > limits.future_skew_ns {
        return Err(RejectReason::ProducedInFuture {
            produced_at_ns,
            now_ns,
        });
    }
    if file.valid_until_ns <= now_ns {
        return Err(RejectReason::Expired {
            valid_until_ns: file.valid_until_ns,
            now_ns,
        });
    }
    if now_ns.saturating_sub(produced_at_ns) > limits.max_age_ns {
        return Err(RejectReason::TooOld {
            produced_at_ns,
            now_ns,
            max_age_ns: limits.max_age_ns,
        });
    }
    Ok(())
}

/// Checks every value strategies will act on; see the crate docs for the exact rules.
pub(crate) fn validate_content(file: &TargetFile) -> Result<(), ContentError> {
    validate_fair(&file.fair)?;
    validate_positions(&file.positions)?;
    validate_clusters(&file.clusters)?;
    file.tourney.as_ref().map_or(Ok(()), validate_tourney)
}

fn is_probability(x: f64) -> bool {
    // `contains` is false for NaN; the bounds exclude infinities.
    (0.0..=1.0).contains(&x)
}

fn validate_fair(fair: &[FairValue]) -> Result<(), ContentError> {
    let mut seen = HashSet::with_capacity(fair.len());
    for (index, fv) in fair.iter().enumerate() {
        if fv.exchange_id.is_empty() {
            return Err(ContentError::EmptyFairId { index });
        }
        if !seen.insert(fv.exchange_id.as_str()) {
            return Err(ContentError::DuplicateFair {
                exchange_id: fv.exchange_id.clone(),
            });
        }
        validate_band(fv)?;
    }
    Ok(())
}

fn validate_band(fv: &FairValue) -> Result<(), ContentError> {
    for (field, value) in [("p_yes", fv.p_yes), ("p_lo", fv.p_lo), ("p_hi", fv.p_hi)] {
        if !is_probability(value) {
            return Err(ContentError::Probability {
                exchange_id: fv.exchange_id.clone(),
                field,
                value,
            });
        }
    }
    if fv.p_lo <= fv.p_yes && fv.p_yes <= fv.p_hi {
        Ok(())
    } else {
        Err(ContentError::BandOrder {
            exchange_id: fv.exchange_id.clone(),
            p_lo: fv.p_lo,
            p_yes: fv.p_yes,
            p_hi: fv.p_hi,
        })
    }
}

fn validate_positions(positions: &[TargetPosition]) -> Result<(), ContentError> {
    let mut seen = HashSet::with_capacity(positions.len());
    for (index, pos) in positions.iter().enumerate() {
        if pos.exchange_id.is_empty() {
            return Err(ContentError::EmptyPositionId { index });
        }
        if !seen.insert(pos.exchange_id.as_str()) {
            return Err(ContentError::DuplicatePosition {
                exchange_id: pos.exchange_id.clone(),
            });
        }
        validate_position(pos)?;
    }
    Ok(())
}

fn validate_position(pos: &TargetPosition) -> Result<(), ContentError> {
    let price = pos.max_entry_price;
    if !(price > 0.0 && price < 1.0) {
        return Err(ContentError::MaxEntryPrice {
            exchange_id: pos.exchange_id.clone(),
            value: price,
        });
    }
    if i32::try_from(pos.target_qty).is_err() {
        return Err(ContentError::TargetQty {
            exchange_id: pos.exchange_id.clone(),
            qty: pos.target_qty,
        });
    }
    Ok(())
}

fn validate_clusters(clusters: &[ClusterLimit]) -> Result<(), ContentError> {
    match clusters.iter().find(|c| !is_probability(c.max_gross_frac)) {
        Some(c) => Err(ContentError::ClusterFrac {
            cluster: c.cluster.clone(),
            value: c.max_gross_frac,
        }),
        None => Ok(()),
    }
}

fn validate_tourney(t: &TourneyEstimate) -> Result<(), ContentError> {
    for (field, value) in [("p_first", t.p_first), ("p_top3", t.p_top3)] {
        if !is_probability(value) {
            return Err(ContentError::TourneyProbability { field, value });
        }
    }
    for (field, value) in [
        ("threshold_t_p50", t.threshold_t_p50),
        ("threshold_t_p90", t.threshold_t_p90),
    ] {
        if !value.is_finite() {
            return Err(ContentError::TourneyNotFinite { field, value });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use susq_contracts::v1::Header;

    use super::*;

    fn fair(id: &str, p_lo: f64, p_yes: f64, p_hi: f64) -> FairValue {
        FairValue {
            exchange_id: id.into(),
            p_yes,
            p_lo,
            p_hi,
            ..Default::default()
        }
    }

    fn position(id: &str, qty: i64, price: f64) -> TargetPosition {
        TargetPosition {
            exchange_id: id.into(),
            target_qty: qty,
            max_entry_price: price,
            ..Default::default()
        }
    }

    fn valid() -> TargetFile {
        TargetFile {
            fair: vec![
                fair("1", 0.4, 0.5, 0.6),
                fair("2", 0.0, 0.0, 0.0),
                fair("3", 1.0, 1.0, 1.0),
            ],
            positions: vec![position("1", 10, 0.55), position("2", -5, 0.01)],
            clusters: vec![ClusterLimit {
                cluster: "national".into(),
                max_gross_frac: 1.0,
            }],
            tourney: Some(TourneyEstimate {
                p_first: 0.03,
                p_top3: 0.08,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn content_err(edit: impl FnOnce(&mut TargetFile)) -> ContentError {
        let mut f = valid();
        edit(&mut f);
        validate_content(&f).expect_err("edit must make the file invalid")
    }

    #[test]
    fn valid_file_passes_including_degenerate_bands() {
        validate_content(&valid()).expect("valid");
        validate_content(&TargetFile::default()).expect("an empty file is valid content");
    }

    #[test]
    fn probabilities_must_be_finite_and_in_unit_interval() {
        for bad in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
            let e = content_err(|f| f.fair[0].p_yes = bad);
            assert!(
                matches!(e, ContentError::Probability { field: "p_yes", .. }),
                "{bad}: {e}"
            );
        }
        let e = content_err(|f| f.fair[0].p_hi = f64::NEG_INFINITY);
        assert!(matches!(e, ContentError::Probability { field: "p_hi", .. }));
    }

    #[test]
    fn band_must_bracket_p_yes() {
        let e = content_err(|f| f.fair[0] = fair("1", 0.6, 0.5, 0.7));
        assert!(matches!(e, ContentError::BandOrder { .. }));
        // An omitted band (both zero) around a non-zero p_yes is refused, not treated as "no band".
        let e = content_err(|f| f.fair[0] = fair("1", 0.0, 0.5, 0.0));
        assert!(matches!(e, ContentError::BandOrder { .. }));
    }

    #[test]
    fn fair_ids_must_be_present_and_unique() {
        assert_eq!(
            content_err(|f| f.fair[1].exchange_id.clear()),
            ContentError::EmptyFairId { index: 1 }
        );
        assert_eq!(
            content_err(|f| f.fair[2].exchange_id = "1".into()),
            ContentError::DuplicateFair {
                exchange_id: "1".into()
            }
        );
    }

    #[test]
    fn position_ids_must_be_present_and_unique() {
        assert_eq!(
            content_err(|f| f.positions[0].exchange_id.clear()),
            ContentError::EmptyPositionId { index: 0 }
        );
        assert_eq!(
            content_err(|f| f.positions[1].exchange_id = "1".into()),
            ContentError::DuplicatePosition {
                exchange_id: "1".into()
            }
        );
    }

    #[test]
    fn max_entry_price_is_strictly_inside_unit_interval() {
        for bad in [0.0, 1.0, -0.5, f64::NAN, f64::INFINITY] {
            let e = content_err(|f| f.positions[0].max_entry_price = bad);
            assert!(matches!(e, ContentError::MaxEntryPrice { .. }), "{bad}: {e}");
        }
    }

    #[test]
    fn target_qty_fits_i32() {
        let mut f = valid();
        f.positions[0].target_qty = i64::from(i32::MIN);
        f.positions[1].target_qty = i64::from(i32::MAX);
        validate_content(&f).expect("i32 bounds are fine");
        let e = content_err(|f| f.positions[0].target_qty = i64::from(i32::MAX) + 1);
        assert!(matches!(e, ContentError::TargetQty { .. }));
        let e = content_err(|f| f.positions[0].target_qty = i64::from(i32::MIN) - 1);
        assert!(matches!(e, ContentError::TargetQty { .. }));
    }

    #[test]
    fn cluster_fraction_in_unit_interval() {
        for bad in [-0.1, 1.5, f64::NAN] {
            let e = content_err(|f| f.clusters[0].max_gross_frac = bad);
            assert!(matches!(e, ContentError::ClusterFrac { .. }), "{bad}: {e}");
        }
    }

    #[test]
    fn tourney_values_checked_when_present() {
        let e = content_err(|f| f.tourney.as_mut().expect("tourney").p_top3 = 1.2);
        assert!(matches!(
            e,
            ContentError::TourneyProbability { field: "p_top3", .. }
        ));
        let e = content_err(|f| f.tourney.as_mut().expect("tourney").threshold_t_p90 = f64::NAN);
        assert!(matches!(
            e,
            ContentError::TourneyNotFinite {
                field: "threshold_t_p90",
                ..
            }
        ));
        let mut f = valid();
        f.tourney = None;
        validate_content(&f).expect("tourney is optional");
    }

    const H: i64 = 3_600 * 1_000_000_000;
    const LIMITS: Limits = Limits {
        max_age_ns: 36 * H,
        future_skew_ns: H / 12,
    };

    fn timed(produced_at_ns: i64, valid_until_ns: i64) -> TargetFile {
        TargetFile {
            header: Some(Header {
                produced_at_ns,
                ..Default::default()
            }),
            valid_until_ns,
            ..Default::default()
        }
    }

    #[test]
    fn freshness_boundaries() {
        let now = 100 * H;
        check_fresh(&timed(now - 36 * H, now + 1), now, LIMITS).expect("exactly max age is fine");
        check_fresh(&timed(now + H / 12, now + H), now, LIMITS).expect("exactly the skew is fine");
        assert!(matches!(
            check_fresh(&timed(now - 36 * H - 1, now + H), now, LIMITS),
            Err(RejectReason::TooOld { .. })
        ));
        assert!(matches!(
            check_fresh(&timed(now - H, now), now, LIMITS),
            Err(RejectReason::Expired { .. })
        ));
        assert!(matches!(
            check_fresh(&timed(now + H / 12 + 1, now + H), now, LIMITS),
            Err(RejectReason::ProducedInFuture { .. })
        ));
    }
}
