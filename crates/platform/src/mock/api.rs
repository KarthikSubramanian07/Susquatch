//! Endpoint semantics on top of the engine: validation, idempotency, pagination and response
//! shapes, one method per [`crate::PlatformAdapter`] call.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use susq_core::{BookSide, ExchangeId, OrderId, Qty, Side, TournamentId, ids::MAX_ORDER_QTY};

use super::{
    CALLER,
    engine::{FillRecord, Holding, MarketEntry, State, Status, Submission, iso, side_price},
};
use crate::{
    error::{ApiError, PlatformError, PlatformResult},
    models::*,
};

/// Deepest book the orderbook endpoint returns.
const MAX_DEPTH: u32 = 200;

impl State {
    /// `GET /tournaments/{slug}`
    pub(super) fn tournament(&self, slug: &str) -> PlatformResult<Tournament> {
        self.check_slug(slug)?;
        Ok(Tournament {
            id: self.cfg.tournament_id.clone(),
            slug: self.cfg.slug.clone(),
            name: "Mock tournament".to_owned(),
            status: "active".to_owned(),
            start_date: None,
            end_date: None,
            initial_balance: self.cfg.starting_balance,
            currency_name: "SUSQie".to_owned(),
            member_count: self.cfg.member_count,
            market_count: u32::try_from(self.markets.len()).unwrap_or(u32::MAX),
            my_balance: self.cash_of(CALLER),
            is_pending_enrolment: false,
        })
    }

    /// `GET /tournaments/{slug}/markets`
    pub(super) fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>> {
        self.check_slug(slug)?;
        let rows = self
            .markets
            .iter()
            .enumerate()
            .filter(|(_, m)| status == MarketStatusFilter::Any || m.status == status.as_str())
            .map(|(i, m)| (i as i64, self.market_row(m)));
        paginate(rows, cursor, self.page_size())
    }

    /// `GET /exchanges/prices`
    pub(super) fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices> {
        if ids.len() > MAX_PRICE_IDS {
            return Err(PlatformError::Invalid(format!(
                "{} ids requested, at most {MAX_PRICE_IDS} allowed",
                ids.len()
            )));
        }
        self.check_tournament(tid)?;
        let mut out = BulkPrices::default();
        for id in ids {
            match self.quote(id) {
                Some(quote) => out.data.push(quote),
                None => out.missing_ids.push(id.clone()),
            }
        }
        Ok(out)
    }

    /// `GET /exchanges/{id}/orderbook`
    pub(super) fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook> {
        self.check_tournament(tid)?;
        let info = self
            .exchanges
            .get(exchange)
            .ok_or_else(|| not_found(&format!("exchange {exchange}")))?;
        let depth = depth.clamp(1, MAX_DEPTH);
        let best_bid = self.best(exchange, BookSide::Bid).map(|p| p.as_f64());
        let best_ask = self.best(exchange, BookSide::Ask).map(|p| p.as_f64());
        Ok(OrderBook {
            exchange_id: exchange.clone(),
            market_id: Some(info.market_id.clone()),
            depth,
            bids: self.levels(exchange, BookSide::Bid, depth as usize),
            asks: self.levels(exchange, BookSide::Ask, depth as usize),
            best_bid,
            best_ask,
            spread: best_bid.zip(best_ask).map(|(b, a)| a - b),
        })
    }

    /// `POST /orders`
    pub(super) fn place(&mut self, req: &PlaceOrderRequest, now: DateTime<Utc>) -> PlatformResult<OrderAck> {
        let fp = fingerprint(&req.order)?;
        if let Some(ack) = self.placed.replay(&req.idempotency_key, &fp)? {
            return Ok(ack);
        }
        self.validate(&req.order, now)?;
        let ack = self.execute(&req.order, now)?;
        self.placed.remember(&req.idempotency_key, fp, ack.clone());
        Ok(ack)
    }

    /// `POST /orders/batch`: validated upfront, then each item executes independently.
    pub(super) fn batch(&mut self, req: &BatchRequest, now: DateTime<Utc>) -> PlatformResult<BatchResponse> {
        if req.orders.is_empty() || req.orders.len() > MAX_BATCH {
            return Err(validation(&format!("a batch holds 1..={MAX_BATCH} orders")));
        }
        let fp = fingerprint(&req.orders)?;
        if let Some(stored) = self.batches.replay(&req.idempotency_key, &fp)? {
            return Ok(stored);
        }
        for order in &req.orders {
            self.validate(order, now)?;
        }
        let results: Vec<BatchItem> = req
            .orders
            .iter()
            .enumerate()
            .map(|(index, order)| batch_item(index, self.execute(order, now)))
            .collect();
        let succeeded = results.iter().filter(|r| r.ok).count();
        let http_status = match succeeded {
            0 => 422,
            n if n == results.len() => 200,
            _ => 207,
        };
        let response = BatchResponse { http_status, results };
        self.batches.remember(&req.idempotency_key, fp, response.clone());
        Ok(response)
    }

    /// `POST /orders/multi-leg`: all legs execute on a scratch copy that is committed only if
    /// every leg succeeds.
    pub(super) fn multi_leg(
        &mut self,
        req: &MultiLegRequest,
        now: DateTime<Utc>,
    ) -> PlatformResult<MultiLegResponse> {
        if req.legs.is_empty() || req.legs.len() > MAX_LEGS {
            return Err(validation(&format!(
                "a multi-leg order holds 1..={MAX_LEGS} legs"
            )));
        }
        if has_duplicate_exchange(&req.legs) {
            return Err(validation("duplicate legs for the same exchange"));
        }
        let fp = fingerprint(&(&req.legs, &req.relationship_constraint))?;
        if let Some(stored) = self.multi_legs.replay(&req.idempotency_key, &fp)? {
            return Ok(stored);
        }
        for (index, leg) in req.legs.iter().enumerate() {
            self.validate(leg, now).map_err(|e| at_leg(e, index))?;
        }
        let mut scratch = self.clone();
        let mut results = Vec::with_capacity(req.legs.len());
        for (index, leg) in req.legs.iter().enumerate() {
            let data = scratch.execute(leg, now).map_err(|e| at_leg(e, index))?;
            results.push(MultiLegItem { index, data });
        }
        *self = scratch;
        let response = MultiLegResponse { results };
        self.multi_legs
            .remember(&req.idempotency_key, fp, response.clone());
        Ok(response)
    }

    /// `DELETE /orders/{id}`
    pub(super) fn cancel_order(&mut self, id: OrderId) -> PlatformResult<CancelOutcome> {
        self.cancel(id)
    }

    /// `POST /orders/cancel-all`
    pub(super) fn cancel_all(
        &mut self,
        tid: &TournamentId,
        scope: &CancelScope,
    ) -> PlatformResult<CancelAllResponse> {
        self.check_tournament(tid)?;
        let exchanges: Option<Vec<ExchangeId>> = match (&scope.exchange_id, &scope.market_id) {
            (Some(_), Some(_)) => {
                return Err(validation("exchangeId and marketId are mutually exclusive"));
            }
            (Some(exchange), None) => Some(vec![exchange.clone()]),
            (None, Some(market)) => Some(
                self.markets
                    .iter()
                    .filter(|m| &m.id == market)
                    .flat_map(|m| m.exchanges.iter().cloned())
                    .collect(),
            ),
            (None, None) => None,
        };
        let cancelled = self.cancel_open(CALLER, |o| {
            exchanges.as_ref().is_none_or(|xs| xs.contains(&o.exchange))
        });
        Ok(CancelAllResponse {
            http_status: 200,
            cancelled,
            errors: Vec::new(),
        })
    }

    /// `GET /orders`
    pub(super) fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Order>> {
        self.check_tournament(tid)?;
        let rows = self
            .orders_of(CALLER)
            .filter(|o| status_matches(status, o.status))
            .filter(|o| exchange.is_none_or(|e| &o.exchange == e))
            .map(|o| (o.id, o.to_wire()));
        paginate(rows, cursor, self.page_size())
    }

    /// `GET /tournaments/{slug}/portfolio/positions`
    pub(super) fn positions(&self, slug: &str) -> PlatformResult<Positions> {
        self.check_slug(slug)?;
        let positions: Vec<Position> = self
            .holdings
            .iter()
            .filter(|((owner, _), h)| owner == CALLER && h.qty != 0)
            .map(|((_, exchange), h)| self.position_row(exchange, h))
            .collect();
        let summary = PositionsSummary {
            total_market_value: positions.iter().map(|p| p.market_value).sum(),
            total_cost_basis: positions.iter().map(|p| p.cost_basis).sum(),
            total_unrealized_pnl: positions.iter().map(|p| p.unrealized_pnl).sum(),
        };
        Ok(Positions { positions, summary })
    }

    /// `GET /tournaments/{slug}/portfolio/pnl`: `totalAccountValue = cash + holdings at mark`.
    pub(super) fn pnl(&self, slug: &str, period: PnlPeriod, now: DateTime<Utc>) -> PlatformResult<Pnl> {
        let summary = self.positions(slug)?.summary;
        let start = self.cfg.starting_balance;
        // The outstanding ALL advance is a liability (the Python mock's assumption A4).
        let total = self.cash_of(CALLER) + summary.total_market_value - self.liability(CALLER);
        Ok(Pnl {
            period: period.as_str().to_owned(),
            period_start: None,
            period_end: iso(now),
            period_pnl: Some(total - start),
            unrealized_pnl: summary.total_unrealized_pnl,
            total_account_value: total,
            total_holdings_value: summary.total_market_value,
            total_cost_basis: summary.total_cost_basis,
            roi: (start > 0.0).then(|| (total - start) / start),
            sharpe: None,
        })
    }

    /// `GET /tournaments/{slug}/portfolio/fills`
    pub(super) fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>> {
        self.check_slug(slug)?;
        let rows = self
            .fills
            .iter()
            .filter(|f| f.owner == CALLER)
            .map(|f| (f.id, self.fill_row(f)));
        paginate(rows, cursor, self.page_size())
    }

    /// `GET /tournaments/{slug}/leaderboard`: the seeded board, windowed by `offset`/`limit`.
    pub(super) fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard> {
        self.check_slug(slug)?;
        let mut board = self.leaderboard.clone();
        board.leaderboard = board
            .leaderboard
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        board.period = period.as_str().to_owned();
        board.limit = i64::from(limit);
        board.offset = i64::from(offset);
        Ok(board)
    }

    /// `GET /relationships`
    pub(super) fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>> {
        self.check_tournament(tid)?;
        let rows = self
            .relationships
            .iter()
            .enumerate()
            .map(|(i, r)| (i as i64, r.clone()));
        paginate(rows, cursor, self.page_size())
    }

    /// `GET /relationships/constraints`
    pub(super) fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
        now: DateTime<Utc>,
    ) -> PlatformResult<Constraints> {
        self.check_tournament(tid)?;
        let violated = |c: &ConstraintEval| c.evaluation_status == "violated";
        let data: Vec<ConstraintEval> = self
            .constraints
            .iter()
            .filter(|c| !violations_only || (violated(c) && c.violation_amount >= min_violation))
            .cloned()
            .collect();
        let violations_count = data.iter().filter(|c| violated(c)).count();
        Ok(Constraints {
            violations_count: i64::try_from(violations_count).unwrap_or(i64::MAX),
            data,
            computed_at: iso(now),
        })
    }

    /// `GET /portfolio/collateral`
    pub(super) fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral> {
        self.check_tournament(tid)?;
        Ok(self
            .collateral_override
            .clone()
            .unwrap_or_else(|| self.collateral_of(CALLER)))
    }

    /// Static checks every order passes before anything executes.
    fn validate(&self, order: &OrderRequest, now: DateTime<Utc>) -> PlatformResult<()> {
        self.check_tournament(&order.tournament_id)?;
        if Qty::order(order.quantity).is_none() {
            return Err(validation(&format!(
                "quantity {} is outside 1..={MAX_ORDER_QTY}",
                order.quantity
            )));
        }
        if order.expiration_date <= now {
            return Err(validation("expirationDate must be in the future"));
        }
        Ok(())
    }

    /// Execute one validated caller order.
    fn execute(&mut self, order: &OrderRequest, now: DateTime<Utc>) -> PlatformResult<OrderAck> {
        if !self.exchanges.contains_key(&order.exchange_id) {
            return Err(not_found(&format!("exchange {}", order.exchange_id)));
        }
        let sub = Submission {
            owner: CALLER.to_owned(),
            exchange: order.exchange_id.clone(),
            side: order.side,
            action: order.action,
            quantity: order.quantity,
            price: order.price,
            expiration: Some(order.expiration_date),
        };
        self.check_balance(&sub)?;
        Ok(self.submit(sub, now).1)
    }

    fn check_tournament(&self, tid: &TournamentId) -> PlatformResult<()> {
        if tid.as_str() == self.cfg.tournament_id {
            Ok(())
        } else {
            Err(ApiError::new(
                403,
                "FORBIDDEN",
                &format!("caller is not a member of tournament '{tid}'"),
            )
            .into())
        }
    }

    fn check_slug(&self, slug: &str) -> PlatformResult<()> {
        if slug == self.cfg.slug {
            Ok(())
        } else {
            Err(not_found(&format!("tournament '{slug}'")))
        }
    }

    fn page_size(&self) -> usize {
        self.cfg.page_size.max(1)
    }

    fn market_row(&self, market: &MarketEntry) -> Market {
        let exchanges = market
            .exchanges
            .iter()
            .map(|id| MarketExchange {
                id: id.clone(),
                option: self.exchanges.get(id).map(|i| i.option.clone()),
                latest_price: self.last_trade.get(id).map(|p| p.as_f64()),
                initial_price: None,
            })
            .collect::<Vec<_>>();
        Market {
            id: market.id.clone(),
            title: market.title.clone(),
            status: market.status.clone(),
            settlement_date: None,
            settled_with: None,
            is_composite: false,
            is_multi_outcome: exchanges.len() > 1,
            exchanges,
        }
    }

    fn quote(&self, id: &ExchangeId) -> Option<PriceQuote> {
        let info = self.exchanges.get(id)?;
        let best_bid = self.best(id, BookSide::Bid).map(|p| p.as_f64());
        let best_ask = self.best(id, BookSide::Ask).map(|p| p.as_f64());
        Some(PriceQuote {
            exchange_id: id.clone(),
            market_id: Some(info.market_id.clone()),
            option: Some(info.option.clone()),
            latest_price: self.last_trade.get(id).map(|p| p.as_f64()),
            best_bid,
            best_ask,
            spread: best_bid.zip(best_ask).map(|(b, a)| a - b),
        })
    }

    /// A caller position valued at the side-relative mark (average cost when there is no mark).
    fn position_row(&self, exchange: &ExchangeId, holding: &Holding) -> Position {
        let side = if holding.qty > 0 { Side::Yes } else { Side::No };
        let shares = holding.qty.abs() as f64;
        let avg_cost = holding.cost_basis / shares;
        let mark = self.mark_yes(exchange).map_or(avg_cost, |m| side_price(side, m));
        let market_value = shares * mark;
        let info = self.exchanges.get(exchange);
        Position {
            exchange_id: exchange.clone(),
            market_id: info.map(|i| i.market_id.clone()),
            market_title: info.map(|i| i.market_title.clone()).unwrap_or_default(),
            option: info.map(|i| i.option.clone()),
            settled: false,
            quantity: holding.qty as f64,
            avg_cost,
            current_price: Some(mark),
            market_value,
            cost_basis: holding.cost_basis,
            unrealized_pnl: market_value - holding.cost_basis,
            money_earned: holding.money_earned,
            lots: Vec::new(),
        }
    }

    /// Outcome-signed fill: `+q` YES for a bid fill, `-q` NO for an ask fill; YES price.
    fn fill_row(&self, fill: &FillRecord) -> Fill {
        Fill {
            id: fill.id,
            order_id: Some(fill.order_id),
            exchange_id: fill.exchange.clone(),
            market_id: self.exchanges.get(&fill.exchange).map(|i| i.market_id.clone()),
            price: Some(fill.yes_price.as_f64()),
            quantity: fill.yes_delta as f64,
            side: if fill.yes_delta > 0 { Side::Yes } else { Side::No },
            filled_at: iso(fill.at),
        }
    }
}

/// Does an order in state `s` belong in a listing with filter `filter`?
fn status_matches(filter: OrderStatusFilter, s: Status) -> bool {
    match filter {
        OrderStatusFilter::Open => s == Status::Open,
        OrderStatusFilter::Closed => s != Status::Open,
        OrderStatusFilter::Expired => s == Status::Expired,
        OrderStatusFilter::All => true,
    }
}

/// One page of `rows` after `cursor`. Each row carries a strictly increasing key; the cursor is the
/// key of the last row returned, so rows added later never shift a page.
fn paginate<T>(
    rows: impl Iterator<Item = (i64, T)>,
    cursor: Option<&str>,
    limit: usize,
) -> PlatformResult<Page<T>> {
    let after = cursor
        .map(|c| {
            c.parse::<i64>()
                .map_err(|_| validation(&format!("invalid cursor '{c}'")))
        })
        .transpose()?;
    let mut data = Vec::new();
    let mut last = None;
    let mut has_more = false;
    for (key, row) in rows.filter(|(key, _)| after.is_none_or(|a| *key > a)) {
        if data.len() == limit {
            has_more = true;
            break;
        }
        last = Some(key);
        data.push(row);
    }
    Ok(Page {
        data,
        pagination: Pagination {
            total: None,
            limit: i64::try_from(limit).ok(),
            has_more,
            next_cursor: last.filter(|_| has_more).map(|k| k.to_string()),
        },
    })
}

/// Canonical request identity for idempotency (the key itself excluded).
fn fingerprint<T: Serialize>(payload: &T) -> PlatformResult<String> {
    serde_json::to_string(payload).map_err(|e| PlatformError::Invalid(format!("unserializable request: {e}")))
}

fn has_duplicate_exchange(legs: &[OrderRequest]) -> bool {
    legs.iter()
        .enumerate()
        .any(|(i, leg)| legs[..i].iter().any(|prev| prev.exchange_id == leg.exchange_id))
}

fn batch_item(index: usize, result: PlatformResult<OrderAck>) -> BatchItem {
    match result {
        Ok(ack) => BatchItem {
            index,
            ok: true,
            status: 200,
            data: serde_json::to_value(&ack).unwrap_or_else(|e| error_body("INTERNAL_ERROR", &e.to_string())),
        },
        Err(PlatformError::Api(e)) => BatchItem {
            index,
            ok: false,
            status: e.status,
            data: error_body(&e.code, &e.message),
        },
        Err(other) => BatchItem {
            index,
            ok: false,
            status: 500,
            data: error_body(other.label(), &other.to_string()),
        },
    }
}

fn error_body(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message } })
}

/// Tag a multi-leg failure with the zero-based leg index, as the platform does.
fn at_leg(mut e: PlatformError, leg: usize) -> PlatformError {
    if let PlatformError::Api(api) = &mut e {
        api.details = json!({ "leg": leg });
    }
    e
}

fn validation(message: &str) -> PlatformError {
    ApiError::new(400, "VALIDATION_ERROR", message).into()
}

fn not_found(what: &str) -> PlatformError {
    ApiError::new(404, "NOT_FOUND", &format!("{what} not found")).into()
}
