//! Typed, tolerant broadcast payloads (`market_batch`, `account_batch`, `violation`, `tick`).
//!
//! Shapes follow `POST /realtime/token` in the official endpoint reference. Unknown fields are
//! ignored, missing fields default, and ids / numbers accept either JSON numbers or numeric
//! strings. Only the ids that identify an item are required: an item without one is useless, so
//! the whole batch fails to decode, is dropped, and the next batch reports a revision gap.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use susq_core::{ExchangeId, MarketId, TournamentId};

use super::lenient;
use crate::models::ConstraintEval;

/// Delivery metadata carried by every batch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Delivery {
    /// Topic-local revision of this batch.
    #[serde(deserialize_with = "lenient::opt_u64")]
    pub revision: Option<u64>,
    /// Revision of the batch published before this one on the same topic.
    #[serde(deserialize_with = "lenient::opt_u64")]
    pub previous_revision: Option<u64>,
    /// Support diagnostics id of the published batch.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub correlation_id: Option<String>,
}

/// One execution in a `market_batch`. `price` is YES-denominated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtTrade {
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub price: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub quantity: f64,
    #[serde(default)]
    pub executed_at: Option<String>,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
}

/// A book whose resting depth changed: refetch it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookDirty {
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
    #[serde(default)]
    pub at: Option<String>,
}

/// A market scope settled or refunded: refetch the market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketSettled {
    #[serde(deserialize_with = "lenient::id")]
    pub market_id: MarketId,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
    /// Winning outcome label, or `REFUND`.
    #[serde(default)]
    pub settled_with: Option<String>,
    #[serde(default)]
    pub at: Option<String>,
}

/// `market_batch`: one 250 ms window of public market activity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarketBatch {
    pub trades: Vec<RtTrade>,
    pub book_dirty: Vec<BookDirty>,
    pub market_settled: Vec<MarketSettled>,
    pub delivery: Delivery,
}

/// One of our executions in an `account_batch`. `price` is YES-denominated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtFill {
    #[serde(default, deserialize_with = "lenient::opt_i64")]
    pub order_id: Option<i64>,
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub price: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub quantity: f64,
    #[serde(default)]
    pub executed_at: Option<String>,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
}

/// An order's post-execution state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderUpdate {
    #[serde(default, deserialize_with = "lenient::opt_i64")]
    pub order_id: Option<i64>,
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub open: bool,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub quantity_traded: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub total_cost: f64,
    #[serde(default, deserialize_with = "lenient::opt_f64")]
    pub latest_trade_price: Option<f64>,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
    #[serde(default)]
    pub at: Option<String>,
}

/// Per (user, exchange) settlement, including losses with `payout: 0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtSettlement {
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub outcome_side: Option<String>,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub shares: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub cost_basis: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub payout: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub realized_pnl: f64,
    #[serde(default)]
    pub settlement_outcome: Option<String>,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
    #[serde(default)]
    pub at: Option<String>,
}

/// Per (user, exchange) refund when a market resolves to `REFUND`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtRefund {
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub shares: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub refund_amount: f64,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
    #[serde(default)]
    pub at: Option<String>,
}

/// `account_batch`: one 250 ms window of our own account activity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountBatch {
    pub fills: Vec<RtFill>,
    pub order_updates: Vec<OrderUpdate>,
    pub settlements: Vec<RtSettlement>,
    pub refunds: Vec<RtRefund>,
    /// `{ delta, outstandingAdvanceAfter, componentId, tournamentId }` items, kept raw.
    pub collateral_changes: Vec<Value>,
    pub delivery: Delivery,
}

/// `tick` on `leaderboard:all`: standings were recomputed, refetch the leaderboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LeaderboardTick {
    pub at: Option<String>,
    pub delivery: Delivery,
}

/// A decoded broadcast body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Broadcast {
    Market(MarketBatch),
    Account(AccountBatch),
    Violation(Box<ConstraintEval>),
    Tick(LeaderboardTick),
    /// An event this client does not consume (e.g. the legacy per-item feed).
    Ignored,
}

/// Decode the inner `payload` of a broadcast by its inner `event` name.
pub(crate) fn decode(event: &str, payload: Value) -> Result<Broadcast, serde_json::Error> {
    Ok(match event {
        "market_batch" => Broadcast::Market(serde_json::from_value(payload)?),
        "account_batch" => Broadcast::Account(serde_json::from_value(payload)?),
        "violation" => Broadcast::Violation(Box::new(serde_json::from_value(payload)?)),
        "tick" => Broadcast::Tick(serde_json::from_value(payload)?),
        _ => Broadcast::Ignored,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn market_batch_decodes_documented_shape() {
        let b = decode(
            "market_batch",
            json!({
                "trades": [{"exchangeId": 381, "marketId": "m1", "price": 0.42, "quantity": 5,
                            "executedAt": "2026-10-01T16:00:00Z", "tournamentId": "t1", "extra": 1}],
                "bookDirty": [{"exchangeId": "381", "marketId": "m1", "tournamentId": "t1", "at": "x"}],
                "marketSettled": [{"marketId": "m1", "tournamentId": "t1", "settledWith": "REFUND"}],
                "delivery": {"revision": "12", "previousRevision": 11, "correlationId": "c-1"}
            }),
        )
        .expect("decodes");
        let Broadcast::Market(b) = b else {
            panic!("wrong kind: {b:?}")
        };
        assert_eq!(b.trades[0].exchange_id, ExchangeId::new("381"));
        assert_eq!(b.trades[0].quantity, 5.0);
        assert_eq!(b.book_dirty[0].market_id, Some(MarketId::new("m1")));
        assert_eq!(b.market_settled[0].settled_with.as_deref(), Some("REFUND"));
        assert_eq!(
            b.delivery,
            Delivery {
                revision: Some(12),
                previous_revision: Some(11),
                correlation_id: Some("c-1".into())
            }
        );
    }

    #[test]
    fn account_batch_tolerates_missing_arrays() {
        let b = decode(
            "account_batch",
            json!({"orderUpdates": [{"orderId": "77", "exchangeId": 9, "open": false,
                                     "quantityTraded": 3, "totalCost": 1.2, "latestTradePrice": null}]}),
        )
        .expect("decodes");
        let Broadcast::Account(b) = b else {
            panic!("wrong kind: {b:?}")
        };
        assert!(b.fills.is_empty() && b.settlements.is_empty() && b.refunds.is_empty());
        assert_eq!(b.order_updates[0].order_id, Some(77));
        assert_eq!(b.order_updates[0].latest_trade_price, None);
        assert_eq!(b.delivery, Delivery::default());
    }

    #[test]
    fn violation_and_tick_decode() {
        let v = decode(
            "violation",
            json!({"relationshipId": "r1", "type": "mutually_exclusive", "violationAmount": 0.07,
                   "evaluationStatus": "violated", "delivery": {"model": "best-effort-authoritative-resync"}}),
        )
        .expect("violation");
        assert!(matches!(v, Broadcast::Violation(e) if e.relationship_id == "r1"));
        let t = decode("tick", json!({"at": "2026-10-01T16:00:00Z"})).expect("tick");
        assert!(matches!(t, Broadcast::Tick(t) if t.at.is_some()));
    }

    #[test]
    fn unknown_events_are_ignored_and_items_without_ids_fail() {
        assert_eq!(decode("trade", json!({})).expect("ignored"), Broadcast::Ignored);
        assert!(decode("market_batch", json!({"trades": [{"price": 0.5}]})).is_err());
    }
}
