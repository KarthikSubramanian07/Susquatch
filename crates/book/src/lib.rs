//! L2 books in YES space with staleness stamps.
//!
//! The platform keeps one YES-normalized book per exchange. [`L2Book`] stores it with integer
//! [`Pips`] prices and whole-share sizes, stamped with when we observed it. [`BookStore`] holds every
//! book, marks books dirty when realtime says their depth changed (`bookDirty`), and only hands out
//! books that are both fresh and clean: decisions are never made on a book we know has moved.
//!
//! [`L2Book::sweep`] walks depth for a target size in the order's own side terms (buying NO walks
//! YES bids at `1 - price`), which is what sizing and the paper broker need.

use std::collections::{BTreeSet, HashMap};

use susq_core::{Action, BookSide, ExchangeId, Pips, Side, side::yes_space};
use susq_platform::{BookLevel, OrderBook};

/// One aggregated price rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price: Pips,
    pub qty: i64,
}

/// A YES-space order book snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct L2Book {
    pub exchange_id: ExchangeId,
    /// Descending by price.
    pub bids: Vec<Level>,
    /// Ascending by price.
    pub asks: Vec<Level>,
    /// When we fetched this snapshot (UTC ns).
    pub observed_at_ns: i64,
    /// Rungs dropped while parsing (off-tick price or non-positive size).
    pub dropped_levels: u32,
}

/// Result of walking the book for a target size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sweep {
    /// Shares available within the limit (≤ requested).
    pub qty: i64,
    /// Total cost in the order's side terms (SUSQies).
    pub notional: f64,
    /// Worst price touched, in the order's side terms.
    pub worst: Option<Pips>,
}

impl Sweep {
    /// Average price paid per share, in the order's side terms.
    pub fn avg(&self) -> Option<f64> {
        (self.qty > 0).then(|| self.notional / self.qty as f64)
    }
}

fn aggregate(levels: &[BookLevel], descending: bool, dropped: &mut u32) -> Vec<Level> {
    let mut by_price: std::collections::BTreeMap<Pips, i64> = std::collections::BTreeMap::new();
    for l in levels {
        let qty = if l.quantity.is_finite() {
            l.quantity.floor() as i64
        } else {
            0
        };
        match Pips::from_f64(l.price) {
            Ok(p) if qty > 0 => *by_price.entry(p).or_default() += qty,
            _ => *dropped += 1,
        }
    }
    let mut out: Vec<Level> = by_price
        .into_iter()
        .map(|(price, qty)| Level { price, qty })
        .collect();
    if descending {
        out.reverse();
    }
    out
}

impl L2Book {
    /// Build from an API snapshot, normalizing order and dropping malformed rungs.
    pub fn from_api(book: &OrderBook, observed_at_ns: i64) -> Self {
        let mut dropped = 0;
        let bids = aggregate(&book.bids, true, &mut dropped);
        let asks = aggregate(&book.asks, false, &mut dropped);
        Self {
            exchange_id: book.exchange_id.clone(),
            bids,
            asks,
            observed_at_ns,
            dropped_levels: dropped,
        }
    }

    /// Build directly from levels (tests, paper broker). Levels are re-sorted.
    pub fn from_levels(
        exchange_id: ExchangeId,
        mut bids: Vec<Level>,
        mut asks: Vec<Level>,
        observed_at_ns: i64,
    ) -> Self {
        bids.retain(|l| l.qty > 0);
        asks.retain(|l| l.qty > 0);
        bids.sort_by_key(|l| std::cmp::Reverse(l.price));
        asks.sort_by_key(|l| l.price);
        Self {
            exchange_id,
            bids,
            asks,
            observed_at_ns,
            dropped_levels: 0,
        }
    }

    /// Highest bid.
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.first().copied()
    }

    /// Lowest ask.
    pub fn best_ask(&self) -> Option<Level> {
        self.asks.first().copied()
    }

    /// Mid price as a probability, when both sides exist.
    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()?.price.prob() + self.best_ask()?.price.prob()) / 2.0)
    }

    /// Spread in ticks, when both sides exist.
    pub fn spread_ticks(&self) -> Option<i64> {
        Some(i64::from(self.best_ask()?.price.ticks()) - i64::from(self.best_bid()?.price.ticks()))
    }

    /// Age of the snapshot at `now_ns`.
    pub fn age_ns(&self, now_ns: i64) -> i64 {
        now_ns.saturating_sub(self.observed_at_ns)
    }

    /// Crossed or locked book (should not happen on the platform; treat as bad data).
    pub fn is_crossed(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some(b), Some(a)) if b.price >= a.price)
    }

    /// Best price at which an order `(side, action)` could trade immediately, in that side's terms:
    /// buy YES → best ask; buy NO → 1 − best bid; sell YES → best bid; sell NO → 1 − best ask.
    pub fn touch(&self, side: Side, action: Action) -> Option<Pips> {
        let (book_side, _) = yes_space(side, action, Pips::MIN);
        let level = match book_side {
            BookSide::Bid => self.best_ask()?,
            BookSide::Ask => self.best_bid()?,
        };
        Some(to_side_terms(side, level.price))
    }

    /// Walk the opposite side for up to `max_qty` shares of `(side, action)` at prices no worse than
    /// `limit` (all in the order's side terms).
    pub fn sweep(&self, side: Side, action: Action, max_qty: i64, limit: Pips) -> Sweep {
        let (book_side, yes_limit) = yes_space(side, action, limit);
        let (levels, crosses): (&[Level], CrossTest) = match book_side {
            BookSide::Bid => (&self.asks, |level, lim| level <= lim),
            BookSide::Ask => (&self.bids, |level, lim| level >= lim),
        };
        let mut out = Sweep {
            qty: 0,
            notional: 0.0,
            worst: None,
        };
        for level in levels {
            if out.qty >= max_qty || !crosses(level.price, yes_limit) {
                break;
            }
            let take = level.qty.min(max_qty - out.qty);
            let px = to_side_terms(side, level.price);
            out.qty += take;
            out.notional += px.prob() * take as f64;
            out.worst = Some(px);
        }
        out
    }

    /// Shares resting at prices strictly better than `price` on `book_side` (queue ahead of a new order).
    pub fn qty_better_than(&self, book_side: BookSide, price: Pips) -> i64 {
        match book_side {
            BookSide::Bid => self.bids.iter().filter(|l| l.price > price).map(|l| l.qty).sum(),
            BookSide::Ask => self.asks.iter().filter(|l| l.price < price).map(|l| l.qty).sum(),
        }
    }
}

/// Whether a resting YES price is marketable against a YES-space limit.
type CrossTest = fn(Pips, Pips) -> bool;

fn to_side_terms(side: Side, yes_price: Pips) -> Pips {
    match side {
        Side::Yes => yes_price,
        Side::No => yes_price.complement(),
    }
}

/// Every book we track, with dirty marks from realtime.
#[derive(Debug, Default, Clone)]
pub struct BookStore {
    books: HashMap<ExchangeId, L2Book>,
    /// exchange → time the realtime feed said its depth changed.
    dirty: HashMap<ExchangeId, i64>,
}

impl BookStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a snapshot. It clears a dirty mark only if it was observed at or after the mark
    /// (a snapshot fetched before the change must not hide it).
    pub fn apply_snapshot(&mut self, book: L2Book) {
        if self
            .dirty
            .get(&book.exchange_id)
            .is_some_and(|&at| book.observed_at_ns >= at)
        {
            self.dirty.remove(&book.exchange_id);
        }
        self.books.insert(book.exchange_id.clone(), book);
    }

    /// Realtime `bookDirty` for `exchange` at `at_ns`.
    pub fn mark_dirty(&mut self, exchange: &ExchangeId, at_ns: i64) {
        let e = self.dirty.entry(exchange.clone()).or_insert(at_ns);
        *e = (*e).min(at_ns);
    }

    /// After a realtime gap/reconnect every book is suspect.
    pub fn mark_all_dirty(&mut self, at_ns: i64) {
        let ids: Vec<ExchangeId> = self.books.keys().cloned().collect();
        for id in ids {
            self.mark_dirty(&id, at_ns);
        }
    }

    /// Books that need a refetch, in stable order.
    pub fn dirty_ids(&self) -> Vec<ExchangeId> {
        self.dirty
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Whether `exchange` is marked dirty.
    pub fn is_dirty(&self, exchange: &ExchangeId) -> bool {
        self.dirty.contains_key(exchange)
    }

    /// The latest snapshot regardless of freshness (for display/marks only).
    pub fn get(&self, exchange: &ExchangeId) -> Option<&L2Book> {
        self.books.get(exchange)
    }

    /// A book safe to decide on: present, clean, not crossed, and no older than `max_age_ns`.
    pub fn fresh(&self, exchange: &ExchangeId, now_ns: i64, max_age_ns: i64) -> Option<&L2Book> {
        let book = self.books.get(exchange)?;
        let ok = !self.is_dirty(exchange) && book.age_ns(now_ns) <= max_age_ns && !book.is_crossed();
        ok.then_some(book)
    }

    /// Ids of books older than `max_age_ns` (to refresh proactively).
    pub fn stale_ids(&self, now_ns: i64, max_age_ns: i64) -> Vec<ExchangeId> {
        let mut ids: Vec<ExchangeId> = self
            .books
            .values()
            .filter(|b| b.age_ns(now_ns) > max_age_ns)
            .map(|b| b.exchange_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Every book held, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &L2Book> {
        self.books.values()
    }

    /// Number of books held.
    pub fn len(&self) -> usize {
        self.books.len()
    }

    /// True when no books are held.
    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(raw: u32) -> Pips {
        Pips::new(raw).expect("valid")
    }

    fn lvl(price: u32, qty: i64) -> Level {
        Level { price: p(price), qty }
    }

    fn book() -> L2Book {
        L2Book::from_levels(
            ExchangeId::new("7"),
            vec![lvl(4000, 10), lvl(4500, 5)],
            vec![lvl(5500, 3), lvl(5000, 4), lvl(6000, 100)],
            1_000,
        )
    }

    #[test]
    fn from_api_normalizes_and_drops_garbage() {
        let api = OrderBook {
            exchange_id: ExchangeId::new("7"),
            market_id: None,
            depth: 20,
            bids: vec![
                BookLevel {
                    price: 0.4,
                    quantity: 3.0,
                },
                BookLevel {
                    price: 0.45,
                    quantity: 1.0,
                },
                BookLevel {
                    price: 0.4,
                    quantity: 2.0,
                },
                BookLevel {
                    price: 0.4123,
                    quantity: 9.0,
                },
                BookLevel {
                    price: 0.3,
                    quantity: 0.0,
                },
            ],
            asks: vec![
                BookLevel {
                    price: 0.6,
                    quantity: 2.5,
                },
                BookLevel {
                    price: f64::NAN,
                    quantity: 1.0,
                },
            ],
            best_bid: Some(0.45),
            best_ask: Some(0.6),
            spread: Some(0.15),
        };
        let b = L2Book::from_api(&api, 5);
        assert_eq!(b.bids, vec![lvl(4500, 1), lvl(4000, 5)]);
        assert_eq!(b.asks, vec![lvl(6000, 2)]);
        assert_eq!(b.dropped_levels, 3);
    }

    #[test]
    fn touch_in_side_terms() {
        let b = book();
        assert_eq!(b.touch(Side::Yes, Action::Buy), Some(p(5000)));
        assert_eq!(b.touch(Side::No, Action::Buy), Some(p(5500)));
        assert_eq!(b.touch(Side::Yes, Action::Sell), Some(p(4500)));
        assert_eq!(b.touch(Side::No, Action::Sell), Some(p(5000)));
        assert!((b.mid().expect("mid") - 0.475).abs() < 1e-12);
        assert_eq!(b.spread_ticks(), Some(10));
    }

    #[test]
    fn sweep_walks_depth_within_limit() {
        let b = book();
        let s = b.sweep(Side::Yes, Action::Buy, 6, p(5500));
        assert_eq!(s.qty, 6);
        assert_eq!(s.worst, Some(p(5500)));
        assert!((s.notional - (4.0 * 0.5 + 2.0 * 0.55)).abs() < 1e-9);
        // Buying NO at ≤ 0.58 walks YES bids ≥ 0.42: only the 0.45 rung (NO price 0.55).
        let n = b.sweep(Side::No, Action::Buy, 100, p(5800));
        assert_eq!((n.qty, n.worst), (5, Some(p(5500))));
        assert_eq!(b.sweep(Side::Yes, Action::Buy, 10, p(4900)).qty, 0);
    }

    #[test]
    fn store_dirty_and_freshness() {
        let mut s = BookStore::new();
        let id = ExchangeId::new("7");
        s.apply_snapshot(book());
        assert!(s.fresh(&id, 2_000, 5_000).is_some());
        assert!(s.fresh(&id, 10_000, 5_000).is_none(), "too old");
        s.mark_dirty(&id, 1_500);
        assert!(s.fresh(&id, 2_000, 5_000).is_none(), "dirty");
        let mut older = book();
        older.observed_at_ns = 1_400;
        s.apply_snapshot(older);
        assert!(
            s.is_dirty(&id),
            "a snapshot from before the change must not clear it"
        );
        let mut newer = book();
        newer.observed_at_ns = 1_600;
        s.apply_snapshot(newer);
        assert!(!s.is_dirty(&id));
        s.mark_all_dirty(1_700);
        assert_eq!(s.dirty_ids(), vec![id]);
    }

    #[test]
    fn crossed_books_are_not_fresh() {
        let mut s = BookStore::new();
        let crossed = L2Book::from_levels(ExchangeId::new("1"), vec![lvl(5000, 1)], vec![lvl(5000, 1)], 0);
        assert!(crossed.is_crossed());
        s.apply_snapshot(crossed);
        assert!(s.fresh(&ExchangeId::new("1"), 0, 10).is_none());
    }

    fn arb_levels() -> impl Strategy<Value = Vec<Level>> {
        proptest::collection::vec((1u32..=199, 1i64..50), 0..12)
            .prop_map(|v| v.into_iter().map(|(t, q)| lvl(t * 50, q)).collect())
    }

    proptest! {
        #[test]
        fn sweep_respects_size_and_limit(
            bids in arb_levels(), asks in arb_levels(), qty in 0i64..200, limit in 1u32..=199,
            side in prop_oneof![Just(Side::Yes), Just(Side::No)],
            action in prop_oneof![Just(Action::Buy), Just(Action::Sell)],
        ) {
            let b = L2Book::from_levels(ExchangeId::new("x"), bids, asks, 0);
            let limit = p(limit * 50);
            let s = b.sweep(side, action, qty, limit);
            prop_assert!(s.qty <= qty && s.qty >= 0);
            if let Some(w) = s.worst {
                match action {
                    Action::Buy => prop_assert!(w <= limit),
                    Action::Sell => prop_assert!(w >= limit),
                }
            }
            if let Some(avg) = s.avg() {
                prop_assert!((0.0..=1.0).contains(&avg));
            }
        }
    }
}
