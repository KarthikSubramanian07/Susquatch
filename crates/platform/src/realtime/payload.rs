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
use crate::models::{BookLevel, ConstraintEval, OrderBook};

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

/// One price rung of a realtime book (numbers or numeric strings).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtLevel {
    #[serde(default, deserialize_with = "lenient::f64")]
    pub price: f64,
    #[serde(default, deserialize_with = "lenient::f64")]
    pub quantity: f64,
}

/// The versioned order book of one exchange the batch names (`books` on a per-market topic).
/// Bids descending, asks ascending, YES space, like `GET /exchanges/{id}/orderbook`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtBook {
    #[serde(deserialize_with = "lenient::id")]
    pub exchange_id: ExchangeId,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub market_id: Option<MarketId>,
    #[serde(default, deserialize_with = "lenient::opt_id")]
    pub tournament_id: Option<TournamentId>,
    /// Monotonic per exchange; an older version than the one already applied is stale.
    /// The live feed (captured 2026-10-01) carries it as `asOf.sequence`; `version` is kept for
    /// the older documented shape.
    #[serde(default, deserialize_with = "lenient::opt_u64")]
    pub version: Option<u64>,
    #[serde(default)]
    pub as_of: Option<RtAsOf>,
    #[serde(default)]
    pub bids: Vec<RtLevel>,
    #[serde(default)]
    pub asks: Vec<RtLevel>,
    #[serde(default)]
    pub at: Option<String>,
    /// When the soonest resting order in this book expires (informational).
    #[serde(default)]
    pub next_expiry_at: Option<String>,
}

/// `books[].asOf`: the book version `{ at, sequence }`, the same `asOf` REST order books carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RtAsOf {
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default, deserialize_with = "lenient::opt_u64")]
    pub sequence: Option<u64>,
}

impl RtBook {
    /// The book version: `asOf.sequence` on the live feed, else the legacy `version` field.
    pub fn version(&self) -> Option<u64> {
        self.as_of.as_ref().and_then(|a| a.sequence).or(self.version)
    }

    /// The same book as the REST snapshot type (`depth` is the deeper side).
    pub fn to_order_book(&self) -> OrderBook {
        let level = |l: &RtLevel| BookLevel {
            price: l.price,
            quantity: l.quantity,
        };
        let bids: Vec<BookLevel> = self.bids.iter().map(level).collect();
        let asks: Vec<BookLevel> = self.asks.iter().map(level).collect();
        OrderBook {
            exchange_id: self.exchange_id.clone(),
            market_id: self.market_id.clone(),
            depth: u32::try_from(bids.len().max(asks.len())).unwrap_or(u32::MAX),
            best_bid: bids.first().map(|l| l.price),
            best_ask: asks.first().map(|l| l.price),
            spread: match (bids.first(), asks.first()) {
                (Some(b), Some(a)) => Some(a.price - b.price),
                _ => None,
            },
            bids,
            asks,
        }
    }
}

/// `market_batch`: one coalescing window (one second) of public activity on one market's topic
/// (`tournament:{tid}:market:{market_id}`). Every exchange of a multi-outcome market shares the
/// topic; items carry `exchangeId` (and `tournamentId`) to demux. `books` is optional: the
/// versioned book of every exchange the batch names, when the platform includes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarketBatch {
    pub trades: Vec<RtTrade>,
    pub book_dirty: Vec<BookDirty>,
    pub market_settled: Vec<MarketSettled>,
    pub books: Vec<RtBook>,
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

/// `account_batch`: one coalescing window of our own account activity (`user:{profile_id}`).
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
    fn live_books_carry_their_version_in_as_of() {
        // Shape captured from the live feed on 2026-10-01 (values made up): the version is
        // `asOf.sequence`, and `nextExpiryAt` rides along.
        let b = decode(
            "market_batch",
            json!({
                "trades": [{"exchangeId": "7", "executedAt": "2026-10-01T20:14:05.1052965+00:00", "id": "1",
                            "marketId": "3", "price": 0.91, "quantity": 500, "sequence": 1,
                            "tournamentId": "t1"}],
                "bookDirty": [{"at": "2026-10-01T20:14:05.1052965+00:00", "exchangeId": "7", "marketId": "3",
                               "tournamentId": "t1"}],
                "marketSettled": [],
                "books": [{"exchangeId": "7",
                           "asOf": {"at": "2026-10-01T20:14:12.6758522+00:00", "sequence": 3021237},
                           "asks": [{"price": 0.91, "quantity": 9361}, {"price": 0.915, "quantity": 95868}],
                           "bids": [{"price": 0.9, "quantity": 2160}],
                           "nextExpiryAt": "2026-10-01T20:20:00+00:00"}],
                "delivery": {"correlationId": "realtime:tournament:t1:market:3:2072",
                             "model": "best-effort-authoritative-resync", "previousRevision": 2071,
                             "revision": 2072, "sourceSequenceFrom": 3020497, "sourceSequenceThrough": 3020596}
            }),
        )
        .expect("decode");
        let Broadcast::Market(batch) = b else {
            panic!("market batch")
        };
        assert_eq!(batch.books.len(), 1);
        assert_eq!(batch.books[0].version(), Some(3_021_237));
        assert_eq!(batch.books[0].to_order_book().best_ask, Some(0.91));
        assert_eq!(batch.delivery.revision, Some(2072));
    }

    #[test]
    fn market_batch_decodes_the_per_market_topic_shape() {
        // One market's topic: every item names its tournament and (for multi-outcome markets)
        // its own exchange; `books` carries the versioned book of the exchanges the batch names.
        let b = decode(
            "market_batch",
            json!({
                "trades": [{"exchangeId": 381, "marketId": "m1", "price": 0.42, "quantity": 5,
                            "executedAt": "2026-10-01T16:00:00Z", "tournamentId": "t1", "extra": 1}],
                "bookDirty": [{"exchangeId": "381", "marketId": "m1", "tournamentId": "t1", "at": "x"},
                              {"exchangeId": "382", "marketId": "m1", "tournamentId": "t1", "at": "x"}],
                "marketSettled": [{"marketId": "m1", "tournamentId": "t1", "settledWith": "REFUND"}],
                "books": [{"exchangeId": "381", "marketId": "m1", "tournamentId": "t1", "version": "7",
                           "bids": [{"price": "0.41", "quantity": 10}, {"price": 0.40, "quantity": "3"}],
                           "asks": [{"price": 0.43, "quantity": 4}]}],
                "delivery": {"revision": "12", "previousRevision": 11, "correlationId": "c-1"}
            }),
        )
        .expect("decodes");
        let Broadcast::Market(b) = b else {
            panic!("wrong kind: {b:?}")
        };
        assert_eq!(b.trades[0].exchange_id, ExchangeId::new("381"));
        assert_eq!(b.trades[0].tournament_id, Some(TournamentId::new("t1")));
        assert_eq!(b.trades[0].quantity, 5.0);
        assert_eq!(b.book_dirty[1].exchange_id, ExchangeId::new("382"));
        assert_eq!(b.book_dirty[0].market_id, Some(MarketId::new("m1")));
        assert_eq!(b.market_settled[0].settled_with.as_deref(), Some("REFUND"));
        assert_eq!(b.books.len(), 1);
        let book = &b.books[0];
        assert_eq!(book.version, Some(7));
        assert_eq!(book.tournament_id, Some(TournamentId::new("t1")));
        let ob = book.to_order_book();
        assert_eq!(ob.exchange_id, ExchangeId::new("381"));
        assert_eq!(ob.bids.len(), 2);
        assert_eq!(ob.best_bid, Some(0.41));
        assert_eq!(ob.best_ask, Some(0.43));
        assert_eq!(ob.depth, 2);
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
    fn market_batch_books_are_optional_and_versionless_books_decode() {
        let b = decode(
            "market_batch",
            json!({"bookDirty": [{"exchangeId": 1, "tournamentId": "t1"}]}),
        )
        .expect("decodes");
        let Broadcast::Market(b) = b else {
            panic!("wrong kind: {b:?}")
        };
        assert!(b.books.is_empty());
        let b = decode("market_batch", json!({"books": [{"exchangeId": 1}]})).expect("decodes");
        let Broadcast::Market(b) = b else {
            panic!("wrong kind: {b:?}")
        };
        assert_eq!(b.books[0].version, None);
        let ob = b.books[0].to_order_book();
        assert!(ob.bids.is_empty() && ob.best_ask.is_none() && ob.spread.is_none());
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
