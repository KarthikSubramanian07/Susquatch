//! What the results pollers' health (`susq_enr::service::EnrHealth`) means for trading: which
//! races are disputed, which still have a live feed, and the `enr:*` health tiles.
//!
//! Fields used, per source (`SourceHealth`):
//! * `last_ok_ns`: the last successful poll (a 304 counts). Zero means never.
//! * `consecutive_failures`: failed polls in a row (the poller backs off exponentially meanwhile).
//! * `polls`, `ok_polls`, `failed_polls`, `last_new_data_ns`, `last_error`: shown in the tile.
//!
//! A source is **stale** when it has never succeeded, its last success is older than
//! `[strategies.en] feed_stale_s`, or it has failed [`STALE_AFTER_FAILURES`] polls in a row. A race's
//! feed is **live** while at least one enabled source mapped to it (`EnrConfig::race_sources`) is
//! not stale; the executor places no new buys on the others. `EnrHealth.disputed_races` (two
//! sources disagree beyond `[consistency]`) are refused outright.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use susq_contracts::v1::HealthItem;
use susq_enr::service::{EnrHealth, SourceHealth};
use tokio::sync::watch;

/// Failed polls in a row that make a source stale regardless of its last success.
pub const STALE_AFTER_FAILURES: u32 = 3;

/// race id → enabled sources that can report it.
pub type RaceSources = BTreeMap<String, BTreeSet<String>>;

/// The engine's handle on the pollers: their health channel and the race → source map.
#[derive(Debug, Clone)]
pub struct EnrFeed {
    pub health: watch::Receiver<EnrHealth>,
    pub race_sources: RaceSources,
}

/// One assessment of the pollers' health.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnrView {
    /// Races two sources disagree on: refuse them.
    pub disputed_races: HashSet<String>,
    /// Races with at least one non-stale source; every other race has a stale feed.
    pub live_feed_races: HashSet<String>,
    /// `enr` (summary) plus one `enr:<source>` tile per source.
    pub health: Vec<HealthItem>,
}

/// Whether one source is stale at `now_ns`.
pub fn source_stale(h: &SourceHealth, now_ns: i64, stale_after_ns: i64) -> bool {
    h.last_ok_ns <= 0
        || now_ns.saturating_sub(h.last_ok_ns) > stale_after_ns
        || h.consecutive_failures >= STALE_AFTER_FAILURES
}

fn age(now_ns: i64, at_ns: i64) -> String {
    if at_ns <= 0 {
        "never".to_owned()
    } else {
        format!("{}s ago", now_ns.saturating_sub(at_ns).max(0) / 1_000_000_000)
    }
}

/// Assess the pollers' health for trading.
pub fn assess(health: &EnrHealth, race_sources: &RaceSources, now_ns: i64, stale_after_ns: i64) -> EnrView {
    let stale: BTreeSet<&str> = health
        .sources
        .iter()
        .filter(|(_, h)| source_stale(h, now_ns, stale_after_ns))
        .map(|(id, _)| id.as_str())
        .collect();
    let live_feed_races = race_sources
        .iter()
        .filter(|(_, sources)| {
            sources
                .iter()
                .any(|s| health.sources.contains_key(s) && !stale.contains(s.as_str()))
        })
        .map(|(race, _)| race.clone())
        .collect();
    let mut items = Vec::with_capacity(health.sources.len() + 1);
    for (id, h) in &health.sources {
        let state = if stale.contains(id.as_str()) {
            "down"
        } else if h.consecutive_failures > 0 {
            "degraded"
        } else {
            "ok"
        };
        let mut detail = format!(
            "polls {} (ok {}, failed {}, {} in a row); last ok {}; last new data {}",
            h.polls,
            h.ok_polls,
            h.failed_polls,
            h.consecutive_failures,
            age(now_ns, h.last_ok_ns),
            age(now_ns, h.last_new_data_ns),
        );
        if let Some(e) = h.last_error.as_deref().filter(|_| h.consecutive_failures > 0) {
            detail.push_str("; ");
            detail.push_str(e);
        }
        items.push(HealthItem {
            name: format!("enr:{id}"),
            state: state.to_owned(),
            detail,
            last_ok_ns: h.last_ok_ns.max(0),
        });
    }
    let (state, detail) = if health.disputed_races.is_empty() {
        (
            "ok",
            format!(
                "{} sources ({} stale), seq {}, {} results accepted",
                health.sources.len(),
                stale.len(),
                health.last_seq,
                health.pipeline.accepted
            ),
        )
    } else {
        (
            "degraded",
            format!(
                "disputed: {}",
                health
                    .disputed_races
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };
    items.push(HealthItem {
        name: "enr".to_owned(),
        state: state.to_owned(),
        detail,
        last_ok_ns: health.updated_at_ns.max(0),
    });
    EnrView {
        disputed_races: health.disputed_races.iter().cloned().collect(),
        live_feed_races,
        health: items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;
    const NOW: i64 = 1_800_000_000 * S;
    const STALE_AFTER: i64 = 180 * S;

    fn src(last_ok_s_ago: i64, consecutive_failures: u32) -> SourceHealth {
        SourceHealth {
            polls: 10,
            ok_polls: 8,
            failed_polls: 2,
            consecutive_failures,
            last_ok_ns: if last_ok_s_ago < 0 {
                0
            } else {
                NOW - last_ok_s_ago * S
            },
            last_new_data_ns: NOW - 300 * S,
            last_error: Some("boom".into()),
            ..SourceHealth::default()
        }
    }

    fn mapping() -> RaceSources {
        [
            ("2026-GOV-CA", vec!["ca_sos", "ca_sos_api"]),
            ("2026-SEN-GA", vec!["ga_sos"]),
            ("2026-GOV-NV", vec!["nv_sos"]), // configured, but never built (disabled)
        ]
        .into_iter()
        .map(|(r, s)| (r.to_owned(), s.iter().map(|x| (*x).to_owned()).collect()))
        .collect()
    }

    #[test]
    fn a_source_is_stale_when_old_failing_or_never_polled() {
        assert!(!source_stale(&src(30, 0), NOW, STALE_AFTER));
        assert!(!source_stale(&src(30, 2), NOW, STALE_AFTER));
        assert!(source_stale(&src(30, 3), NOW, STALE_AFTER), "3 failures in a row");
        assert!(source_stale(&src(181, 0), NOW, STALE_AFTER), "last ok too old");
        assert!(
            !source_stale(&src(180, 0), NOW, STALE_AFTER),
            "boundary is inclusive"
        );
        assert!(source_stale(&src(-1, 0), NOW, STALE_AFTER), "never polled");
    }

    #[test]
    fn a_race_is_live_while_any_mapped_source_is_fresh() {
        let mut health = EnrHealth::default();
        health.sources.insert("ca_sos".into(), src(600, 0)); // stale
        health.sources.insert("ca_sos_api".into(), src(10, 1)); // fresh, one failure
        health.sources.insert("ga_sos".into(), src(-1, 0)); // never polled
        let v = assess(&health, &mapping(), NOW, STALE_AFTER);
        assert!(
            v.live_feed_races.contains("2026-GOV-CA"),
            "the API path still feeds CA"
        );
        assert!(!v.live_feed_races.contains("2026-SEN-GA"));
        assert!(
            !v.live_feed_races.contains("2026-GOV-NV"),
            "a source that was never built feeds nothing"
        );
        assert!(v.disputed_races.is_empty());
        let tile = |name: &str| v.health.iter().find(|h| h.name == name).expect(name).clone();
        assert_eq!(tile("enr:ca_sos").state, "down");
        assert_eq!(tile("enr:ca_sos_api").state, "degraded");
        assert!(tile("enr:ca_sos_api").detail.contains("boom"));
        assert_eq!(tile("enr:ga_sos").state, "down");
        assert!(tile("enr:ga_sos").detail.contains("last ok never"));
        assert_eq!(tile("enr").state, "ok");
        assert!(tile("enr").detail.contains("3 sources (2 stale)"));
        assert_eq!(tile("enr:ca_sos_api").last_ok_ns, NOW - 10 * S);
    }

    #[test]
    fn disputes_are_passed_through_and_shown() {
        let mut health = EnrHealth::default();
        health.sources.insert("ca_sos".into(), src(5, 0));
        health.sources.insert("ca_sos_api".into(), src(5, 0));
        health.disputed_races.insert("2026-GOV-CA".into());
        let v = assess(&health, &mapping(), NOW, STALE_AFTER);
        assert!(v.disputed_races.contains("2026-GOV-CA"));
        assert!(v.live_feed_races.contains("2026-GOV-CA"), "disputed, not stale");
        let summary = v.health.iter().find(|h| h.name == "enr").expect("enr");
        assert_eq!(summary.state, "degraded");
        assert!(summary.detail.contains("2026-GOV-CA"));
        assert_eq!(v.health.len(), 3);
    }
}
