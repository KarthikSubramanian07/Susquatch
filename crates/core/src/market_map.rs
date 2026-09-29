//! Lookup index over `state/market_map.json` (`susq.v1.MarketMap`).
//!
//! Strategies only trade rows with `verified = true`; risk reads each exchange's clusters
//! (`national`, `region:northeast`, `state:ME`, `house`) to enforce cluster caps.

use std::collections::{BTreeSet, HashMap};

use susq_contracts::v1::{MarketMap, MarketRule};

use crate::ids::{ExchangeId, MarketId};

/// Index of market rules by exchange, race and market.
#[derive(Debug, Clone, Default)]
pub struct MarketIndex {
    by_exchange: HashMap<ExchangeId, MarketRule>,
    by_race: HashMap<String, Vec<ExchangeId>>,
    by_market: HashMap<MarketId, Vec<ExchangeId>>,
}

impl MarketIndex {
    /// Build from a parsed market map. Later duplicates of an exchange id replace earlier ones.
    pub fn new(map: &MarketMap) -> Self {
        let mut idx = Self::default();
        for rule in &map.markets {
            if rule.exchange_id.is_empty() {
                continue;
            }
            idx.by_exchange
                .insert(ExchangeId::new(rule.exchange_id.clone()), rule.clone());
        }
        let mut ids: Vec<&ExchangeId> = idx.by_exchange.keys().collect();
        ids.sort();
        for id in ids {
            let rule = &idx.by_exchange[id];
            if !rule.race_id.is_empty() {
                idx.by_race
                    .entry(rule.race_id.clone())
                    .or_default()
                    .push(id.clone());
            }
            if !rule.market_id.is_empty() {
                idx.by_market
                    .entry(MarketId::new(rule.market_id.clone()))
                    .or_default()
                    .push(id.clone());
            }
        }
        idx
    }

    /// The rule for an exchange.
    pub fn rule(&self, exchange: &ExchangeId) -> Option<&MarketRule> {
        self.by_exchange.get(exchange)
    }

    /// True iff the exchange is mapped and human-verified.
    pub fn is_verified(&self, exchange: &ExchangeId) -> bool {
        self.rule(exchange).is_some_and(|r| r.verified)
    }

    /// Exchanges resolving on a race (sorted).
    pub fn exchanges_for_race(&self, race_id: &str) -> &[ExchangeId] {
        self.by_race.get(race_id).map(Vec::as_slice).unwrap_or_default()
    }

    /// Exchanges of a market (sorted).
    pub fn exchanges_for_market(&self, market: &MarketId) -> &[ExchangeId] {
        self.by_market.get(market).map(Vec::as_slice).unwrap_or_default()
    }

    /// Market of an exchange, if mapped.
    pub fn market_of(&self, exchange: &ExchangeId) -> Option<MarketId> {
        self.rule(exchange)
            .filter(|r| !r.market_id.is_empty())
            .map(|r| MarketId::new(r.market_id.clone()))
    }

    /// Clusters of an exchange (empty if unmapped).
    pub fn clusters(&self, exchange: &ExchangeId) -> &[String] {
        self.rule(exchange)
            .map(|r| r.clusters.as_slice())
            .unwrap_or_default()
    }

    /// Every race id in the map (for validating needle posteriors).
    pub fn race_ids(&self) -> BTreeSet<String> {
        self.by_race.keys().cloned().collect()
    }

    /// Every mapped exchange.
    pub fn exchanges(&self) -> impl Iterator<Item = &ExchangeId> {
        self.by_exchange.keys()
    }

    /// Number of mapped exchanges.
    pub fn len(&self) -> usize {
        self.by_exchange.len()
    }

    /// True when nothing is mapped.
    pub fn is_empty(&self) -> bool {
        self.by_exchange.is_empty()
    }
}

/// The cap key for a cluster label: `region:northeast` → `region`, `state:ME` → `state`,
/// `national` → `national`.
pub fn cluster_kind(cluster: &str) -> &str {
    cluster.split(':').next().unwrap_or(cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(ex: &str, market: &str, race: &str, verified: bool, clusters: &[&str]) -> MarketRule {
        MarketRule {
            market_id: market.into(),
            exchange_id: ex.into(),
            race_id: race.into(),
            verified,
            clusters: clusters.iter().map(|c| (*c).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn indexes_by_exchange_race_and_market() {
        let map = MarketMap {
            header: None,
            markets: vec![
                rule("11", "1", "2026-SEN-ME", true, &["national", "state:ME"]),
                rule("12", "1", "2026-SEN-ME", false, &["national", "state:ME"]),
                rule("21", "2", "", true, &["house"]),
                rule("", "3", "x", true, &[]),
            ],
        };
        let idx = MarketIndex::new(&map);
        assert_eq!(idx.len(), 3);
        assert!(idx.is_verified(&ExchangeId::new("11")));
        assert!(!idx.is_verified(&ExchangeId::new("12")));
        assert!(!idx.is_verified(&ExchangeId::new("99")));
        assert_eq!(
            idx.exchanges_for_race("2026-SEN-ME"),
            &[ExchangeId::new("11"), ExchangeId::new("12")]
        );
        assert_eq!(
            idx.exchanges_for_market(&MarketId::new("2")),
            &[ExchangeId::new("21")]
        );
        assert_eq!(idx.market_of(&ExchangeId::new("21")), Some(MarketId::new("2")));
        assert_eq!(idx.clusters(&ExchangeId::new("11")).len(), 2);
        assert_eq!(idx.race_ids().len(), 1);
    }

    #[test]
    fn cluster_kinds() {
        assert_eq!(cluster_kind("region:northeast"), "region");
        assert_eq!(cluster_kind("state:ME"), "state");
        assert_eq!(cluster_kind("national"), "national");
    }
}
