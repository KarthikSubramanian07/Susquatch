//! The [`PlatformAdapter`] trait: exactly the endpoints in `docs/API.md`, one attempt per call.
//!
//! Retrying, rate limiting, kill/lease/mode enforcement and journaling of decisions live in the
//! gateway. Implementations: [`crate::SuperMarket`] (REST), [`crate::mock::MockPlatform`]
//! (in-memory matching engine), and the gateway's rate-limited and paper decorators.

use async_trait::async_trait;
use susq_core::{ExchangeId, OrderId, TournamentId};

use crate::{
    error::{PlatformError, PlatformResult},
    models::*,
};

/// Upper bound on pages walked by the pagination helpers (a repeating cursor can't loop forever).
pub const MAX_PAGES: usize = 1_000;

/// Every platform call Susquatch makes. All methods are a single attempt.
#[async_trait]
pub trait PlatformAdapter: Send + Sync + 'static {
    /// `GET /tournaments/{slug}`
    async fn tournament(&self, slug: &str) -> PlatformResult<Tournament>;

    /// `GET /tournaments/{slug}/markets?status=&limit=100&cursor=`
    async fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>>;

    /// `GET /exchanges/prices?ids=(≤100)&tournamentId=`. More than 100 ids is [`PlatformError::Invalid`];
    /// use [`bulk_prices`] to chunk.
    async fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices>;

    /// `GET /exchanges/{id}/orderbook?depth=&tournamentId=`
    async fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook>;

    /// `POST /orders`
    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck>;

    /// `POST /orders/batch` (≤ 50). 200/207/422 are `Ok` with `http_status` set.
    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse>;

    /// `POST /orders/multi-leg` (≤ 10, atomic).
    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse>;

    /// `DELETE /orders/{id}`. 409 (already closed) is `Ok(AlreadyClosed)`.
    async fn cancel_order(&self, id: OrderId) -> PlatformResult<CancelOutcome>;

    /// `POST /orders/cancel-all {tournamentId, exchangeId|marketId}`. 200/207/422 are `Ok`.
    async fn cancel_all(&self, tid: &TournamentId, scope: &CancelScope) -> PlatformResult<CancelAllResponse>;

    /// `GET /orders?status=&tournamentId=&exchangeId=&limit=200&cursor=`
    async fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Order>>;

    /// `GET /tournaments/{slug}/portfolio/positions`
    async fn positions(&self, slug: &str) -> PlatformResult<Positions>;

    /// `GET /tournaments/{slug}/portfolio/pnl?period=`
    async fn pnl(&self, slug: &str, period: PnlPeriod) -> PlatformResult<Pnl>;

    /// `GET /tournaments/{slug}/portfolio/fills?limit=200&cursor=`
    async fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>>;

    /// `GET /tournaments/{slug}/leaderboard?period=&limit=&offset=`
    async fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard>;

    /// `GET /relationships?tournamentId=&limit=200&cursor=`
    async fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>>;

    /// `GET /relationships/constraints?violationsOnly=&minViolation=&tournamentId=`
    async fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
    ) -> PlatformResult<Constraints>;

    /// `POST /realtime/token`
    async fn realtime_token(&self) -> PlatformResult<RealtimeToken>;

    /// `GET /portfolio/collateral?tournamentId=`
    async fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral>;
}

/// Walk a cursor-paginated endpoint to the end (bounded by [`MAX_PAGES`]).
async fn walk<T, F, Fut>(mut fetch: F) -> PlatformResult<Vec<T>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = PlatformResult<Page<T>>>,
{
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = fetch(cursor.clone()).await?;
        out.extend(page.data);
        match page.pagination.next_cursor {
            Some(next) if page.pagination.has_more && Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => return Ok(out),
        }
    }
    Err(PlatformError::Invalid(format!(
        "pagination exceeded {MAX_PAGES} pages"
    )))
}

/// Every market in the tournament with the given status.
pub async fn all_markets(
    p: &dyn PlatformAdapter,
    slug: &str,
    status: MarketStatusFilter,
) -> PlatformResult<Vec<Market>> {
    walk(|c| async move { p.markets_page(slug, status, c.as_deref()).await }).await
}

/// Every order with `status` (optionally on one exchange).
pub async fn all_orders(
    p: &dyn PlatformAdapter,
    tid: &TournamentId,
    status: OrderStatusFilter,
    exchange: Option<&ExchangeId>,
) -> PlatformResult<Vec<Order>> {
    walk(|c| async move { p.orders_page(tid, status, exchange, c.as_deref()).await }).await
}

/// Every canonical relationship.
pub async fn all_relationships(
    p: &dyn PlatformAdapter,
    tid: &TournamentId,
) -> PlatformResult<Vec<Relationship>> {
    walk(|c| async move { p.relationships_page(tid, c.as_deref()).await }).await
}

/// Top of book for any number of exchanges, in ≤100-id requests, preserving request order.
pub async fn bulk_prices(
    p: &dyn PlatformAdapter,
    tid: &TournamentId,
    ids: &[ExchangeId],
) -> PlatformResult<BulkPrices> {
    let mut out = BulkPrices::default();
    for chunk in ids.chunks(MAX_PRICE_IDS) {
        let page = p.prices(tid, chunk).await?;
        out.data.extend(page.data);
        out.missing_ids.extend(page.missing_ids);
    }
    Ok(out)
}
