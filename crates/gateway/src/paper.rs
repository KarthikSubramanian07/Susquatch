//! Paper broker: writes are simulated against LIVE books, reads of market data go to the platform.
//!
//! Paper state (our orders, positions, cash, fills) lives in an in-process copy of the mock
//! matching engine. Before each paper write, and on [`PaperBroker::refresh_resting`], the live
//! book of the exchange is fetched and installed as the "market" owner's liquidity, so a paper order
//! fills exactly as far as the live book would let it, and resting paper orders fill when the live
//! book later trades through them. Paper fills never consume live liquidity (it is re-read each
//! time), which is the usual optimism of paper trading; the journal marks every paper write.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use parking_lot::Mutex;
use susq_core::{Action, Clock, ExchangeId, OrderId, Pips, Side, TournamentId};
use susq_platform::{
    PlatformAdapter, PlatformResult,
    mock::{CALLER, MockConfig, MockPlatform},
    models::*,
};

/// Owner name of the live liquidity inside the paper engine.
const MARKET: &str = "market";
/// Book depth mirrored from the live platform.
const DEPTH: u32 = 50;

/// Simulated execution against live books.
pub struct PaperBroker {
    live: Arc<dyn PlatformAdapter>,
    engine: MockPlatform,
    tid: TournamentId,
    clock: Arc<dyn Clock>,
    /// Live liquidity our paper fills already took. A paper fill never touches the real book, so
    /// without this the same resting level would fill us again on every mirror, and a sniper would
    /// buy the same stale order every cycle (seen on the first paper run: 26 identical fills).
    consumed: Mutex<Vec<Consumed>>,
}

/// One paper taker fill, remembered for [`CONSUMED_TTL_NS`] so the mirrored book is thinned by it.
#[derive(Debug, Clone)]
struct Consumed {
    exchange: ExchangeId,
    /// `true` when we lifted YES asks (bought YES / sold NO), `false` when we hit YES bids.
    asks: bool,
    qty: i64,
    at_ns: i64,
}

/// How long a paper fill keeps thinning the mirrored live book (15 minutes). Real liquidity at that
/// level may well refill sooner; erring toward fewer paper fills is the conservative side.
const CONSUMED_TTL_NS: i64 = 15 * 60 * 1_000_000_000;

/// Remove `qty` shares from `levels`, walking from the best price outward.
fn thin(levels: &mut [(Pips, i64)], mut qty: i64) {
    for level in levels.iter_mut() {
        if qty <= 0 {
            break;
        }
        let take = level.1.min(qty);
        level.1 -= take;
        qty -= take;
    }
}

impl std::fmt::Debug for PaperBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaperBroker")
            .field("tid", &self.tid)
            .finish_non_exhaustive()
    }
}

impl PaperBroker {
    /// A paper account starting with `starting_balance` in tournament `tid` / `slug`.
    pub fn new(
        live: Arc<dyn PlatformAdapter>,
        clock: Arc<dyn Clock>,
        tid: TournamentId,
        slug: &str,
        starting_balance: f64,
    ) -> Self {
        let engine = MockPlatform::new(
            clock.clone(),
            MockConfig {
                tournament_id: tid.to_string(),
                slug: slug.to_owned(),
                starting_balance,
                member_count: 0,
                page_size: 200,
                collateral: true,
            },
        );
        Self {
            live,
            engine,
            tid,
            clock,
            consumed: Mutex::new(Vec::new()),
        }
    }

    /// Install the live book of `exchange` as the market's liquidity (may fill resting paper
    /// orders), minus whatever our own recent paper fills already took from it.
    pub async fn mirror(&self, exchange: &ExchangeId) -> PlatformResult<()> {
        let book = self.live.orderbook(&self.tid, exchange, DEPTH).await?;
        let levels = |v: &[BookLevel]| -> Vec<(Pips, i64)> {
            v.iter()
                .filter_map(|l| Some((Pips::from_f64(l.price).ok()?, l.quantity.max(0.0).floor() as i64)))
                .collect()
        };
        let (mut bids, mut asks) = (levels(&book.bids), levels(&book.asks));
        // Best first: the platform returns bids descending and asks ascending, but sort anyway.
        bids.sort_by_key(|l| std::cmp::Reverse(l.0));
        asks.sort_by_key(|l| l.0);
        let (taken_bids, taken_asks) = self.taken(exchange);
        thin(&mut bids, taken_bids);
        thin(&mut asks, taken_asks);
        self.engine.replace_liquidity(MARKET, exchange, &bids, &asks);
        Ok(())
    }

    /// Shares our unexpired paper fills took from `exchange`'s live (bids, asks).
    fn taken(&self, exchange: &ExchangeId) -> (i64, i64) {
        let now = self.clock.now_ns();
        let mut consumed = self.consumed.lock();
        consumed.retain(|c| now - c.at_ns < CONSUMED_TTL_NS);
        consumed
            .iter()
            .filter(|c| &c.exchange == exchange)
            .fold(
                (0, 0),
                |(b, a), c| if c.asks { (b, a + c.qty) } else { (b + c.qty, a) },
            )
    }

    /// Remember the live liquidity a taker ack consumed.
    fn record(&self, ack: &OrderAck) {
        let qty = ack.quantity_traded.floor() as i64;
        if qty <= 0 {
            return;
        }
        let asks = matches!(
            (ack.action, ack.side),
            (Some(Action::Buy), Some(Side::Yes)) | (Some(Action::Sell), Some(Side::No))
        );
        self.consumed.lock().push(Consumed {
            exchange: ack.exchange_id.clone(),
            asks,
            qty,
            at_ns: self.clock.now_ns(),
        });
    }

    async fn mirror_all(&self, exchanges: impl IntoIterator<Item = &ExchangeId>) -> PlatformResult<()> {
        let unique: BTreeSet<&ExchangeId> = exchanges.into_iter().collect();
        for ex in unique {
            self.mirror(ex).await?;
        }
        Ok(())
    }

    /// Re-mirror every exchange where we have resting paper orders, so they fill when the live book
    /// moves through them. Returns the number of exchanges refreshed.
    pub async fn refresh_resting(&self) -> PlatformResult<usize> {
        let exchanges: BTreeSet<ExchangeId> = self
            .engine
            .open_orders_of(CALLER)
            .into_iter()
            .map(|o| o.exchange_id)
            .collect();
        self.mirror_all(&exchanges).await?;
        Ok(exchanges.len())
    }

    /// Paper position (for tests and status).
    pub fn position(&self, exchange: &ExchangeId) -> i64 {
        self.engine.position_of(CALLER, exchange.clone())
    }
}

#[async_trait]
impl PlatformAdapter for PaperBroker {
    async fn tournament(&self, slug: &str) -> PlatformResult<Tournament> {
        let mut t = self.live.tournament(slug).await?;
        t.my_balance = self.engine.cash_of(CALLER);
        Ok(t)
    }

    async fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>> {
        self.live.markets_page(slug, status, cursor).await
    }

    async fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices> {
        self.live.prices(tid, ids).await
    }

    async fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook> {
        self.live.orderbook(tid, exchange, depth).await
    }

    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck> {
        self.mirror(&req.order.exchange_id).await?;
        let ack = self.engine.place_order(req).await?;
        self.record(&ack);
        Ok(ack)
    }

    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse> {
        self.mirror_all(req.orders.iter().map(|o| &o.exchange_id)).await?;
        let res = self.engine.place_batch(req).await?;
        for item in res.results.iter().filter(|i| i.ok) {
            if let Ok(ack) = serde_json::from_value::<OrderAck>(item.data.clone()) {
                self.record(&ack);
            }
        }
        Ok(res)
    }

    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse> {
        self.mirror_all(req.legs.iter().map(|o| &o.exchange_id)).await?;
        let res = self.engine.place_multi_leg(req).await?;
        for leg in &res.results {
            self.record(&leg.data);
        }
        Ok(res)
    }

    async fn cancel_order(&self, id: OrderId) -> PlatformResult<CancelOutcome> {
        self.engine.cancel_order(id).await
    }

    async fn cancel_all(&self, tid: &TournamentId, scope: &CancelScope) -> PlatformResult<CancelAllResponse> {
        self.engine.cancel_all(tid, scope).await
    }

    async fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Order>> {
        self.engine.orders_page(tid, status, exchange, cursor).await
    }

    async fn positions(&self, slug: &str) -> PlatformResult<Positions> {
        self.engine.positions(slug).await
    }

    async fn pnl(&self, slug: &str, period: PnlPeriod) -> PlatformResult<Pnl> {
        self.engine.pnl(slug, period).await
    }

    async fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>> {
        self.engine.fills_page(slug, cursor).await
    }

    async fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard> {
        self.live.leaderboard(slug, period, limit, offset).await
    }

    async fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>> {
        self.live.relationships_page(tid, cursor).await
    }

    async fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
    ) -> PlatformResult<Constraints> {
        self.live.constraints(tid, violations_only, min_violation).await
    }

    async fn realtime_token(&self) -> PlatformResult<RealtimeToken> {
        self.live.realtime_token().await
    }

    async fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral> {
        self.engine.collateral(tid).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use susq_core::{Action, ManualClock, Qty, Side};

    fn t0() -> DateTime<Utc> {
        "2026-10-02T12:00:00Z".parse().expect("ts")
    }

    fn setup() -> (ManualClock, Arc<MockPlatform>, PaperBroker) {
        let clock = ManualClock::at(t0());
        let live = Arc::new(MockPlatform::new(
            Arc::new(clock.clone()),
            MockConfig {
                tournament_id: "tid".into(),
                ..MockConfig::default()
            },
        ));
        live.add_market("m", "Race", &[("7", "Yes")]);
        live.seed_order(
            "maker",
            "7",
            Side::Yes,
            Action::Sell,
            5,
            Pips::new(5500).expect("valid"),
        );
        live.seed_order(
            "maker",
            "7",
            Side::Yes,
            Action::Buy,
            5,
            Pips::new(4500).expect("valid"),
        );
        let paper = PaperBroker::new(
            live.clone(),
            Arc::new(clock.clone()),
            TournamentId::new("tid"),
            "midterm-elections",
            1000.0,
        );
        (clock, live, paper)
    }

    fn buy_yes(key: &str, qty: i64, price: u32) -> PlaceOrderRequest {
        PlaceOrderRequest {
            order: OrderRequest::limit(
                ExchangeId::new("7"),
                Side::Yes,
                Action::Buy,
                Qty(qty),
                Pips::new(price).expect("valid"),
                t0() + chrono::Duration::minutes(5),
                TournamentId::new("tid"),
            ),
            idempotency_key: key.into(),
        }
    }

    #[tokio::test]
    async fn fills_against_the_live_book_without_touching_it() {
        let (_, live, paper) = setup();
        let ack = paper
            .place_order(&buy_yes("k1", 8, 5500))
            .await
            .expect("paper order");
        assert_eq!(ack.quantity_traded, 5.0, "only the live depth at <= 0.55 fills");
        assert!(ack.open, "the remainder rests on paper");
        assert_eq!(paper.position(&ExchangeId::new("7")), 5);
        assert_eq!(live.engine_orders_created(), 0, "nothing was sent live");
        assert_eq!(live.open_orders_of("maker").len(), 2, "live liquidity untouched");
        // Idempotent like the real thing.
        let again = paper.place_order(&buy_yes("k1", 8, 5500)).await.expect("replay");
        assert_eq!(again, ack);
    }

    #[tokio::test]
    async fn resting_paper_orders_fill_when_the_live_book_moves_through_them() {
        let (_, live, paper) = setup();
        paper.place_order(&buy_yes("k1", 3, 5000)).await.expect("rests");
        assert_eq!(paper.position(&ExchangeId::new("7")), 0);
        live.seed_order(
            "seller",
            "7",
            Side::Yes,
            Action::Sell,
            10,
            Pips::new(4900).expect("valid"),
        );
        assert_eq!(paper.refresh_resting().await.expect("refresh"), 1);
        assert_eq!(paper.position(&ExchangeId::new("7")), 3);
        let tid = TournamentId::new("tid");
        let open = paper
            .orders_page(&tid, OrderStatusFilter::Open, None, None)
            .await
            .expect("orders");
        assert!(open.data.is_empty());
    }

    #[tokio::test]
    async fn a_paper_fill_thins_the_mirrored_book_until_it_expires() {
        let (clock, _, paper) = setup();
        let first = paper.place_order(&buy_yes("k1", 5, 5500)).await.expect("first");
        assert_eq!(first.quantity_traded, 5.0);
        // The live book is unchanged (a paper fill never touches it), but we already took that level.
        let again = paper.place_order(&buy_yes("k2", 5, 5500)).await.expect("second");
        assert_eq!(
            again.quantity_traded, 0.0,
            "the same live level must not fill us twice"
        );
        assert!(again.open);
        assert_eq!(paper.position(&ExchangeId::new("7")), 5);
        clock.advance(std::time::Duration::from_secs(16 * 60));
        // After the TTL the level counts as refilled (the resting k2 order has expired by then).
        let mut third = buy_yes("k3", 5, 5500);
        third.order.expiration_date = t0() + chrono::Duration::minutes(30);
        let later = paper.place_order(&third).await.expect("third");
        assert_eq!(later.quantity_traded, 5.0);
        assert_eq!(paper.position(&ExchangeId::new("7")), 10);
    }

    #[tokio::test]
    async fn tournament_balance_is_the_paper_cash() {
        let (_, _, paper) = setup();
        paper.place_order(&buy_yes("k1", 2, 5500)).await.expect("fill");
        let t = paper.tournament("midterm-elections").await.expect("t");
        assert!((t.my_balance - (1000.0 - 1.1)).abs() < 1e-9, "{}", t.my_balance);
    }
}
