//! [`RateLimited`]: a [`PlatformAdapter`] decorator that spends limiter tokens on every call and
//! feeds `429`s back into the AIMD limiter.
//!
//! Reads are idempotent, so they are retried here (retryable and unknown-5xx classes, a few times
//! with backoff). Writes are single-attempt: the gateway owns write retries because they must go
//! through the OMS and the kill/lease checks between attempts.

use std::{future::Future, sync::Arc, time::Duration};

use async_trait::async_trait;
use susq_core::{ExchangeId, OrderId, TournamentId};
use susq_platform::{PlatformAdapter, PlatformResult, RetryClass, models::*};

use crate::limiter::{RateLimiter, RequestClass};

/// Read attempts including the first.
const READ_ATTEMPTS: u32 = 3;

/// Rate-limited platform.
#[derive(Debug)]
pub struct RateLimited<P: ?Sized> {
    inner: Arc<P>,
    limiter: Arc<RateLimiter>,
}

impl<P: PlatformAdapter + ?Sized> RateLimited<P> {
    /// Wrap `inner`.
    pub fn new(inner: Arc<P>, limiter: Arc<RateLimiter>) -> Self {
        Self { inner, limiter }
    }

    fn feedback<T>(&self, r: &PlatformResult<T>) {
        match r {
            Ok(_) => self.limiter.on_success(),
            Err(e) if e.is_rate_limited() => self.limiter.on_rate_limited(e.retry_after()),
            Err(_) => {}
        }
    }

    async fn write<T, Fut>(&self, call: impl FnOnce() -> Fut) -> PlatformResult<T>
    where
        Fut: Future<Output = PlatformResult<T>>,
    {
        self.limiter.acquire(RequestClass::Write).await;
        let r = call().await;
        self.feedback(&r);
        r
    }

    async fn read<T, Fut>(&self, call: impl Fn() -> Fut) -> PlatformResult<T>
    where
        Fut: Future<Output = PlatformResult<T>>,
    {
        let mut attempt = 1;
        loop {
            self.limiter.acquire(RequestClass::Read).await;
            let r = call().await;
            self.feedback(&r);
            let retry = match &r {
                Err(e) if attempt < READ_ATTEMPTS => match e.retry_class() {
                    RetryClass::Retry { min_wait } => Some(min_wait.unwrap_or_default()),
                    RetryClass::Unknown => Some(Duration::ZERO),
                    RetryClass::Terminal => None,
                },
                _ => None,
            };
            match retry {
                // The limiter already paused for Retry-After on 429s; add a small backoff otherwise.
                Some(min_wait) => {
                    tokio::time::sleep(min_wait.max(Duration::from_millis(200 * u64::from(attempt)))).await;
                    attempt += 1;
                }
                None => return r,
            }
        }
    }
}

#[async_trait]
impl<P: PlatformAdapter + ?Sized> PlatformAdapter for RateLimited<P> {
    async fn tournament(&self, slug: &str) -> PlatformResult<Tournament> {
        self.read(|| self.inner.tournament(slug)).await
    }

    async fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>> {
        self.read(|| self.inner.markets_page(slug, status, cursor)).await
    }

    async fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices> {
        self.read(|| self.inner.prices(tid, ids)).await
    }

    async fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook> {
        self.read(|| self.inner.orderbook(tid, exchange, depth)).await
    }

    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck> {
        self.write(|| self.inner.place_order(req)).await
    }

    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse> {
        self.write(|| self.inner.place_batch(req)).await
    }

    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse> {
        self.write(|| self.inner.place_multi_leg(req)).await
    }

    async fn cancel_order(&self, id: OrderId) -> PlatformResult<CancelOutcome> {
        self.write(|| self.inner.cancel_order(id)).await
    }

    async fn cancel_all(&self, tid: &TournamentId, scope: &CancelScope) -> PlatformResult<CancelAllResponse> {
        self.write(|| self.inner.cancel_all(tid, scope)).await
    }

    async fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Order>> {
        self.read(|| self.inner.orders_page(tid, status, exchange, cursor))
            .await
    }

    async fn positions(&self, slug: &str) -> PlatformResult<Positions> {
        self.read(|| self.inner.positions(slug)).await
    }

    async fn pnl(&self, slug: &str, period: PnlPeriod) -> PlatformResult<Pnl> {
        self.read(|| self.inner.pnl(slug, period)).await
    }

    async fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>> {
        self.read(|| self.inner.fills_page(slug, cursor)).await
    }

    async fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard> {
        self.read(|| self.inner.leaderboard(slug, period, limit, offset))
            .await
    }

    async fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>> {
        self.read(|| self.inner.relationships_page(tid, cursor)).await
    }

    async fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
    ) -> PlatformResult<Constraints> {
        self.read(|| self.inner.constraints(tid, violations_only, min_violation))
            .await
    }

    async fn realtime_token(&self) -> PlatformResult<RealtimeToken> {
        // Minting is a POST but idempotent in effect; treat it as a read for budgeting.
        self.read(|| self.inner.realtime_token()).await
    }

    async fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral> {
        self.read(|| self.inner.collateral(tid)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limiter::LimiterConfig;
    use susq_core::ManualClock;
    use susq_platform::mock::{Endpoint, Fault, MockConfig, MockPlatform};

    fn limiter() -> Arc<RateLimiter> {
        Arc::new(RateLimiter::new(LimiterConfig {
            initial_rps: 10.0,
            min_rps: 0.5,
            max_rps: 20.0,
            order_budget_frac: 0.3,
            increase_per_success: 0.1,
            default_backoff: Duration::from_secs(1),
        }))
    }

    #[tokio::test(start_paused = true)]
    async fn reads_retry_and_429_halves_the_rate() {
        let clock = ManualClock::at_ns(0);
        let mock = Arc::new(MockPlatform::new(Arc::new(clock), MockConfig::default()));
        mock.inject(
            Endpoint::Tournament,
            Fault::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            },
        );
        mock.inject(Endpoint::Tournament, Fault::Unavailable);
        let l = limiter();
        let p = RateLimited::new(mock.clone(), l.clone());
        let t = p
            .tournament("midterm-elections")
            .await
            .expect("third attempt succeeds");
        assert_eq!(t.slug, "midterm-elections");
        assert_eq!(mock.calls(Endpoint::Tournament), 3);
        assert_eq!(l.rate_limited_count(), 1);
        assert!(l.rate() < 10.0);
    }

    #[tokio::test(start_paused = true)]
    async fn writes_are_single_attempt() {
        let clock = ManualClock::at_ns(0);
        let mock = Arc::new(MockPlatform::new(Arc::new(clock), MockConfig::default()));
        mock.inject(Endpoint::Cancel, Fault::Unavailable);
        let p = RateLimited::new(mock.clone(), limiter());
        assert!(p.cancel_order(OrderId(1)).await.is_err());
        assert_eq!(mock.calls(Endpoint::Cancel), 1);
    }
}
