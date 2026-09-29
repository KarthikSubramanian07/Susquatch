//! Constraints racer: trade engine-published relationship violations before the 120-180 s pollers.
//!
//! For every `violated` constraint whose relationship we know and which is `active`, the racer builds
//! a buy-only bundle whose payoff is guaranteed by the relationship's logic, prices it by walking the
//! live books, and sends it as one atomic multi-leg order:
//!
//! | relationship | trigger | bundle | allowed outcomes |
//! |---|---|---|---|
//! | mutually exclusive | Σ YES bids > 1 | buy NO on every leg (top 10 bids) | at most one YES |
//! | mutually exclusive + `isExhaustive` | Σ YES asks < 1 | buy YES on every leg, only if every leg's rule is verified and no independent can win | exactly one YES |
//! | implication / monotonic (A ⇒ B) | bid(A) > ask(B) | buy NO on A, buy YES on B | (A,B) ∈ {(0,0),(0,1),(1,1)} |
//!
//! Nothing is sent unless the generic [`verify`] proves `min over allowed outcomes of payoff − cost ≥
//! min_edge` at the limit prices actually sent. Racer intents whose violation has closed are cancelled.

use std::collections::HashSet;

use susq_book::L2Book;
use susq_core::{Action, ExchangeId, OrderBundle, Pips, Side, config::RacerCfg, price::PIPS_PER_UNIT};
use susq_platform::{ConstraintEval, NodeRole, Relationship, RelationshipType};

use crate::{
    Strategy,
    draft::{Draft, shares_for_cost},
    snapshot::{Decision, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "racer";
/// Largest atomic bundle the platform accepts (`/orders/multi-leg`).
pub const MAX_LEGS: usize = 10;

/// Which joint YES/NO resolutions of a relationship's legs are logically possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structure {
    /// `n` legs of which at most one resolves YES.
    AtMostOne(usize),
    /// `n` legs of which exactly one resolves YES.
    ExactlyOne(usize),
    /// Two legs `[antecedent, consequent]`: antecedent YES implies consequent YES.
    Implication,
}

impl Structure {
    /// Number of legs the structure describes.
    pub fn legs(self) -> usize {
        match self {
            Structure::AtMostOne(n) | Structure::ExactlyOne(n) => n,
            Structure::Implication => 2,
        }
    }

    /// Whether a joint outcome (one bool per leg, `true` = YES) is possible.
    pub fn allows(self, outcome: &[bool]) -> bool {
        if outcome.len() != self.legs() {
            return false;
        }
        let yes = outcome.iter().filter(|&&o| o).count();
        match self {
            Structure::AtMostOne(_) => yes <= 1,
            Structure::ExactlyOne(_) => yes == 1,
            Structure::Implication => !outcome[0] || outcome[1],
        }
    }

    /// Every allowed joint outcome, by exhaustive enumeration (legs are capped at [`MAX_LEGS`]).
    pub fn allowed_outcomes(self) -> Vec<Vec<bool>> {
        let n = self.legs();
        if n == 0 || n > MAX_LEGS {
            return Vec::new();
        }
        (0u32..1 << n)
            .map(|mask| (0..n).map(|i| mask & (1 << i) != 0).collect::<Vec<bool>>())
            .filter(|o| self.allows(o))
            .collect()
    }
}

/// One buy leg of a bundle: buy `side` of leg `node` at limit `price` (side terms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricedLeg {
    pub node: usize,
    pub side: Side,
    pub price: Pips,
}

/// Guaranteed payoff minus cost per bundle share, in pips: `min over allowed outcomes of Σ leg payoff
/// − Σ limit price`. `None` when a leg references a missing node or no outcome is allowed.
pub fn guaranteed_edge_pips(structure: Structure, legs: &[PricedLeg]) -> Option<i64> {
    if legs.is_empty() || legs.iter().any(|l| l.node >= structure.legs()) {
        return None;
    }
    let unit = i64::from(PIPS_PER_UNIT);
    let payoff = structure
        .allowed_outcomes()
        .iter()
        .map(|o| legs.iter().filter(|l| o[l.node] == (l.side == Side::Yes)).count() as i64 * unit)
        .min()?;
    let cost: i64 = legs.iter().map(|l| i64::from(l.price.get())).sum();
    Some(payoff - cost)
}

/// Approve a bundle only if it earns at least `min_edge` (price units) per bundle share in every
/// allowed outcome. Returns the guaranteed edge in price units.
pub fn verify(structure: Structure, legs: &[PricedLeg], min_edge: f64) -> Option<f64> {
    let edge = guaranteed_edge_pips(structure, legs)? as f64 / f64::from(PIPS_PER_UNIT);
    (edge + 1e-9 >= min_edge).then_some(edge)
}

/// The constraints racer.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintsRacer {
    pub cfg: RacerCfg,
}

/// A bundle shape before sizing: which side of which exchange, under which structure.
struct Plan<'b> {
    structure: Structure,
    legs: Vec<(ExchangeId, Side, &'b L2Book)>,
    label: &'static str,
}

impl ConstraintsRacer {
    /// Racer with its config.
    pub fn new(cfg: RacerCfg) -> Self {
        Self { cfg }
    }

    fn plan<'b>(&self, snap: &Snapshot<'b>, rel: &Relationship) -> Result<Plan<'b>, String> {
        match rel.kind {
            RelationshipType::MutuallyExclusive => plan_exclusive(snap, rel),
            RelationshipType::Implication | RelationshipType::Monotonic => plan_implication(snap, rel),
            other => Err(format!("type {other:?} not traded")),
        }
    }

    /// Size a plan by walking depth: the largest quantity at which the bundle still verifies at the
    /// worst price touched on each leg, capped by the notional budget.
    fn size(&self, snap: &Snapshot<'_>, plan: &Plan<'_>) -> Result<(i64, Vec<PricedLeg>, f64), String> {
        let depth = plan
            .legs
            .iter()
            .map(|(_, side, book)| book.sweep(*side, Action::Buy, i64::MAX, Pips::MAX).qty)
            .min()
            .unwrap_or(0);
        if depth < 1 {
            return Err("no depth".into());
        }
        let mut candidates: Vec<i64> = plan
            .legs
            .iter()
            .flat_map(|(_, side, book)| cumulative_depth(book, *side))
            .map(|q| q.min(depth))
            .collect();
        candidates.push(depth);
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        candidates.dedup();

        let best = candidates.into_iter().find_map(|q| {
            let legs = price_legs(plan, q)?;
            verify(plan.structure, &legs, self.cfg.min_edge).map(|_| (q, legs))
        });
        let Some((depth_q, legs)) = best else {
            return Err("edge below min_edge at the touch".into());
        };
        let cost = legs.iter().map(|l| l.price.prob()).sum::<f64>();
        let cap = shares_for_cost(self.cfg.max_bundle_notional_frac * snap.equity, cost);
        let qty = depth_q.min(cap);
        if qty < 1 {
            return Err(format!("size < 1 (depth {depth_q}, budget cap {cap})"));
        }
        let legs = price_legs(plan, qty).ok_or("depth vanished while resizing")?;
        let edge =
            verify(plan.structure, &legs, self.cfg.min_edge).ok_or("edge below min_edge after resize")?;
        Ok((qty, legs, edge))
    }
}

impl Strategy for ConstraintsRacer {
    fn name(&self) -> &'static str {
        NAME
    }

    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        let open: HashSet<&str> = snap
            .violations
            .iter()
            .filter(|v| v.evaluation_status == "violated")
            .map(|v| v.relationship_id.as_str())
            .collect();

        for intent in snap.live_intents(NAME) {
            match racer_relationship(&intent.intent_id) {
                Some(rel) if !open.contains(rel) => d.cancel(&intent.intent_id, "violation closed"),
                _ => {}
            }
        }

        let mut seen = HashSet::new();
        for v in snap.violations {
            if v.evaluation_status != "violated" || !seen.insert(v.relationship_id.as_str()) {
                continue;
            }
            self.race(snap, v, &mut d);
        }
        d
    }
}

impl ConstraintsRacer {
    fn race(&self, snap: &Snapshot<'_>, v: &ConstraintEval, d: &mut Decision) {
        let rel_id = v.relationship_id.as_str();
        let Some(rel) = snap.relationships.get(rel_id) else {
            d.note(format!("racer {rel_id}: unknown relationship"));
            return;
        };
        if !rel.status.eq_ignore_ascii_case("active") {
            d.note(format!(
                "racer {rel_id}: relationship not active ({:?})",
                rel.status
            ));
            return;
        }
        if snap
            .live_intents(NAME)
            .any(|i| racer_relationship(&i.intent_id) == Some(rel_id))
        {
            return;
        }
        let plan = match self.plan(snap, rel) {
            Ok(p) => p,
            Err(why) => {
                d.note(format!("racer {rel_id}: skip, {why}"));
                return;
            }
        };
        let (qty, priced, edge) = match self.size(snap, &plan) {
            Ok(s) => s,
            Err(why) => {
                d.note(format!("racer {rel_id}: skip {}, {why}", plan.label));
                return;
            }
        };
        let legs: Vec<_> = plan
            .legs
            .iter()
            .zip(&priced)
            .filter_map(|((ex, side, _), leg)| {
                Draft {
                    exchange: ex.clone(),
                    side: *side,
                    action: Action::Buy,
                    qty,
                    price: leg.price,
                    fair_yes: None,
                }
                .into_spec(
                    snap,
                    NAME,
                    &format!("{rel_id}:{ex}"),
                    self.cfg.order_ttl_s,
                    false,
                )
            })
            .collect();
        if legs.len() != plan.legs.len() {
            d.note(format!("racer {rel_id}: skip, invalid leg quantity"));
            return;
        }
        d.note(format!(
            "racer {rel_id}: {} x{qty} over {} legs, guaranteed edge {edge:.4}/share",
            plan.label,
            legs.len()
        ));
        d.bundles.push(OrderBundle { legs, atomic: true });
    }
}

/// Relationship id of a racer intent id `racer:{rel}:{exchange}:{now_s}`.
pub fn racer_relationship(intent_id: &str) -> Option<&str> {
    let rest = intent_id.strip_prefix(NAME)?.strip_prefix(':')?;
    let mut parts = rest.rsplitn(3, ':');
    let (_now, _ex, rel) = (parts.next()?, parts.next()?, parts.next()?);
    (!rel.is_empty()).then_some(rel)
}

fn distinct_exchanges(rel: &Relationship) -> Result<Vec<ExchangeId>, String> {
    let ids: Vec<ExchangeId> = rel.nodes.iter().map(|n| n.exchange_id.clone()).collect();
    let unique: HashSet<&ExchangeId> = ids.iter().collect();
    if unique.len() != ids.len() {
        return Err("duplicate exchange in relationship".into());
    }
    Ok(ids)
}

fn fresh_books<'b>(snap: &Snapshot<'b>, ids: &[ExchangeId]) -> Result<Vec<&'b L2Book>, String> {
    ids.iter()
        .map(|ex| {
            snap.fresh_book(ex)
                .ok_or_else(|| format!("no fresh book for {ex}"))
        })
        .collect()
}

fn plan_exclusive<'b>(snap: &Snapshot<'b>, rel: &Relationship) -> Result<Plan<'b>, String> {
    let ids = distinct_exchanges(rel)?;
    if ids.len() < 2 {
        return Err("fewer than two legs".into());
    }
    let books = fresh_books(snap, &ids)?;
    let unit = i64::from(PIPS_PER_UNIT);

    // Σ YES bids > 1: buy NO on the legs with the highest bids (a subset keeps "at most one YES").
    let mut bid_legs: Vec<(u32, ExchangeId, &L2Book)> = ids
        .iter()
        .zip(&books)
        .filter_map(|(ex, b)| Some((b.best_bid()?.price.get(), ex.clone(), *b)))
        .collect();
    bid_legs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    bid_legs.truncate(MAX_LEGS);
    let bid_sum: i64 = bid_legs.iter().map(|l| i64::from(l.0)).sum();
    if bid_legs.len() >= 2 && bid_sum > unit {
        return Ok(Plan {
            structure: Structure::AtMostOne(bid_legs.len()),
            legs: bid_legs.into_iter().map(|(_, ex, b)| (ex, Side::No, b)).collect(),
            label: "buy NO on every leg",
        });
    }

    // Σ YES asks < 1 on an exhaustive set: buy YES on every leg, only with verified exactly-one rules.
    let ask_sum: Option<i64> = books
        .iter()
        .map(|b| b.best_ask().map(|l| i64::from(l.price.get())))
        .sum();
    if !rel.is_exhaustive || !ask_sum.is_some_and(|s| s < unit) {
        return Err("no priced violation at the touch".into());
    }
    if ids.len() > MAX_LEGS {
        return Err(format!("{} legs exceed the multi-leg limit", ids.len()));
    }
    for ex in &ids {
        match snap.markets.rule(ex) {
            Some(r) if r.verified && !r.independent_can_win => {}
            Some(r) if r.independent_can_win => {
                return Err(format!("exhaustive buy refused: independent can win on {ex}"));
            }
            _ => return Err(format!("exhaustive buy refused: {ex} rule missing or unverified")),
        }
    }
    Ok(Plan {
        structure: Structure::ExactlyOne(ids.len()),
        legs: ids
            .into_iter()
            .zip(books)
            .map(|(ex, b)| (ex, Side::Yes, b))
            .collect(),
        label: "buy YES on every leg",
    })
}

/// Antecedent and consequent of a two-node implication: node roles first, then the direction.
fn implication_ends(rel: &Relationship) -> Result<(ExchangeId, ExchangeId), String> {
    if rel.nodes.len() != 2 {
        return Err(format!("implication with {} nodes", rel.nodes.len()));
    }
    let by_role = |role| {
        rel.nodes
            .iter()
            .find(|n| n.role == role)
            .map(|n| n.exchange_id.clone())
    };
    if let (Some(a), Some(b)) = (by_role(NodeRole::Antecedent), by_role(NodeRole::Consequent)) {
        if a != b {
            return Ok((a, b));
        }
    }
    let dir = rel
        .direction
        .as_ref()
        .ok_or("implication without roles or direction")?;
    let is_node = |ex: &ExchangeId| rel.nodes.iter().any(|n| &n.exchange_id == ex);
    match (dir.from_exchange_ids.as_slice(), dir.to_exchange_ids.as_slice()) {
        ([a], [b]) if a != b && is_node(a) && is_node(b) => Ok((a.clone(), b.clone())),
        _ => Err("implication direction is ambiguous".into()),
    }
}

fn plan_implication<'b>(snap: &Snapshot<'b>, rel: &Relationship) -> Result<Plan<'b>, String> {
    let (a, b) = implication_ends(rel)?;
    let books = fresh_books(snap, &[a.clone(), b.clone()])?;
    let (book_a, book_b) = (books[0], books[1]);
    match (book_a.best_bid(), book_b.best_ask()) {
        (Some(bid_a), Some(ask_b)) if bid_a.price > ask_b.price => Ok(Plan {
            structure: Structure::Implication,
            legs: vec![(a, Side::No, book_a), (b, Side::Yes, book_b)],
            label: "buy NO antecedent + YES consequent",
        }),
        _ => Err("no priced violation at the touch".into()),
    }
}

/// Cumulative shares available at each successive price rung for a buy of `side`.
fn cumulative_depth(book: &L2Book, side: Side) -> Vec<i64> {
    let levels = match side {
        Side::Yes => &book.asks,
        Side::No => &book.bids,
    };
    levels
        .iter()
        .scan(0i64, |acc, l| {
            *acc = acc.saturating_add(l.qty);
            Some(*acc)
        })
        .collect()
}

/// Limit per leg for `qty` bundle shares: the worst price touched walking that leg's book.
fn price_legs(plan: &Plan<'_>, qty: i64) -> Option<Vec<PricedLeg>> {
    plan.legs
        .iter()
        .enumerate()
        .map(|(node, (_, side, book))| {
            let s = book.sweep(*side, Action::Buy, qty, Pips::MAX);
            (s.qty >= qty).then_some(PricedLeg {
                node,
                side: *side,
                price: s.worst?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::{
        prelude::{Just, any, prop_assert, prop_oneof, proptest},
        strategy::Strategy as _,
    };
    use susq_platform::Direction;

    use super::*;
    use crate::testkit::{World, intent, node, p, rel, violation};

    fn racer() -> ConstraintsRacer {
        ConstraintsRacer::new(RacerCfg::default())
    }

    fn me_world(exhaustive: bool) -> World {
        let mut w = World::new(100_000.0);
        for ex in ["1", "2", "3"] {
            w.rule(ex, "m1", "", "", true);
        }
        w.relationship(rel(
            "rel-9",
            RelationshipType::MutuallyExclusive,
            exhaustive,
            ["1", "2", "3"]
                .iter()
                .map(|e| node(e, NodeRole::Member))
                .collect(),
        ));
        w.violations
            .push(violation("rel-9", RelationshipType::MutuallyExclusive));
        w
    }

    #[test]
    fn prices_an_exclusive_sum_of_bids_above_one_as_an_atomic_no_bundle() {
        let mut w = me_world(false);
        // Σ best bids = 0.40 + 0.35 + 0.30 = 1.05. Buying NO costs 0.60 + 0.65 + 0.70 = 1.95, pays ≥ 2.
        w.book("1", &[(4000, 30), (3900, 100)], &[(4200, 10)]);
        w.book("2", &[(3500, 50)], &[(3700, 10)]);
        w.book("3", &[(3000, 20), (2900, 100)], &[(3200, 10)]);
        let d = racer().decide(&w.snap());
        assert_eq!(d.bundles.len(), 1, "{:?}", d.notes);
        let b = &d.bundles[0];
        assert!(b.atomic);
        assert_eq!(b.legs.len(), 3);
        // Leg 2 holds only 50 shares, so 50 is the most the bundle can do. At 50 shares leg 1 walks to
        // the 0.39 bid (NO 0.61) and leg 3 to 0.29 (NO 0.71): cost 1.97, guaranteed edge 0.03 ≥ 0.01.
        // The 1% budget (1000) allows 507 bundle shares, so depth binds.
        for l in &b.legs {
            assert_eq!((l.side, l.action), (Side::No, Action::Buy));
            assert_eq!(l.quantity.get(), 50);
            assert!(l.intent_id.starts_with("racer:rel-9:"));
            assert_eq!(l.ttl.as_secs(), 20);
            assert_eq!(l.market_id.as_ref().map(|m| m.as_str()), Some("m1"));
        }
        let px: Vec<u32> = b.legs.iter().map(|l| l.price.get()).collect();
        assert_eq!(px, vec![6100, 6500, 7100]);
        assert_eq!(
            b.legs[0].intent_id,
            format!("racer:rel-9:1:{}", w.now_ns / 1_000_000_000)
        );
    }

    #[test]
    fn notional_budget_caps_the_bundle() {
        let mut w = me_world(false);
        w.equity = 1_000.0; // 1% = 10 SUSQies; cost per bundle share 1.95 → 5 shares.
        w.book("1", &[(4000, 30)], &[]);
        w.book("2", &[(3500, 50)], &[]);
        w.book("3", &[(3000, 20)], &[]);
        let d = racer().decide(&w.snap());
        assert_eq!(d.bundles[0].legs[0].quantity.get(), 5);
    }

    #[test]
    fn exhaustive_yes_bundle_requires_verified_rules_without_independents() {
        let mut w = me_world(true);
        w.book("1", &[(3000, 10)], &[(3100, 10)]);
        w.book("2", &[(3000, 10)], &[(3200, 10)]);
        w.book("3", &[(3000, 10)], &[(3300, 10)]);
        let d = racer().decide(&w.snap());
        assert_eq!(d.bundles.len(), 1, "{:?}", d.notes);
        assert!(d.bundles[0].legs.iter().all(|l| l.side == Side::Yes));

        let mut unverified = me_world(true);
        unverified.rule("2", "m1", "", "", false);
        unverified.books = w.books.clone();
        let d = racer().decide(&unverified.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("unverified")), "{:?}", d.notes);

        let mut independent = me_world(true);
        independent.books = w.books.clone();
        independent.rule_with("3", |r| r.independent_can_win = true);
        let d = racer().decide(&independent.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("independent")), "{:?}", d.notes);

        let mut not_exhaustive = me_world(false);
        not_exhaustive.books = w.books.clone();
        assert!(racer().decide(&not_exhaustive.snap()).bundles.is_empty());
    }

    #[test]
    fn implication_bundle_buys_no_antecedent_and_yes_consequent() {
        let mut w = World::new(100_000.0);
        // "D wins ≥ 51 seats" (A) ⇒ "D wins ≥ 50 seats" (B), but A bids 0.45 while B offers 0.40.
        w.relationship(rel(
            "imp",
            RelationshipType::Implication,
            false,
            vec![node("A", NodeRole::Antecedent), node("B", NodeRole::Consequent)],
        ));
        w.violations.push(violation("imp", RelationshipType::Implication));
        w.book("A", &[(4500, 7)], &[(4700, 7)]);
        w.book("B", &[(3800, 9)], &[(4000, 9)]);
        let d = racer().decide(&w.snap());
        let b = &d.bundles[0];
        assert_eq!(b.legs[0].exchange_id.as_str(), "A");
        assert_eq!((b.legs[0].side, b.legs[0].price.get()), (Side::No, 5500));
        assert_eq!((b.legs[1].side, b.legs[1].price.get()), (Side::Yes, 4000));
        assert_eq!(b.legs[0].quantity.get(), 7);
    }

    #[test]
    fn implication_falls_back_to_direction() {
        let mut w = World::new(100_000.0);
        let mut r = rel(
            "mono",
            RelationshipType::Monotonic,
            false,
            vec![node("B", NodeRole::Member), node("A", NodeRole::Member)],
        );
        r.direction = Some(Direction {
            kind: "implies".into(),
            from_exchange_ids: vec![ExchangeId::new("A")],
            to_exchange_ids: vec![ExchangeId::new("B")],
            description: String::new(),
        });
        w.relationship(r);
        w.violations.push(violation("mono", RelationshipType::Monotonic));
        w.book("A", &[(6000, 5)], &[(6200, 5)]);
        w.book("B", &[(5000, 5)], &[(5200, 5)]);
        let d = racer().decide(&w.snap());
        assert_eq!(d.bundles[0].legs[0].exchange_id.as_str(), "A");
        assert_eq!(d.bundles[0].legs[0].side, Side::No);
    }

    #[test]
    fn skips_other_types_unknown_and_inactive_relationships() {
        let mut w = World::new(100_000.0);
        w.relationship(rel(
            "c",
            RelationshipType::Complementary,
            false,
            vec![node("1", NodeRole::Member)],
        ));
        let mut inactive = rel("i", RelationshipType::MutuallyExclusive, false, vec![]);
        inactive.status = "retired".into();
        w.relationship(inactive);
        for id in ["c", "i", "ghost"] {
            w.violations.push(violation(id, RelationshipType::Complementary));
        }
        let d = racer().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.notes.len(), 3, "{:?}", d.notes);
    }

    #[test]
    fn no_fresh_book_means_no_trade() {
        let mut w = me_world(false);
        w.book("1", &[(4000, 30)], &[]);
        w.book("2", &[(3500, 50)], &[]);
        w.stale_book("3", &[(3000, 20)], &[]);
        let d = racer().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("no fresh book")));
    }

    #[test]
    fn skips_when_a_racer_intent_for_the_relationship_is_live() {
        let mut w = me_world(false);
        w.book("1", &[(4000, 30)], &[]);
        w.book("2", &[(3500, 50)], &[]);
        w.book("3", &[(3000, 20)], &[]);
        w.intents.push(intent(
            "racer:rel-9:1:100",
            NAME,
            "1",
            Side::No,
            Action::Buy,
            6000,
        ));
        let d = racer().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.cancels.is_empty());
    }

    #[test]
    fn cancels_intents_whose_violation_closed() {
        let mut w = me_world(false);
        w.violations[0].evaluation_status = "satisfied".into();
        w.intents.push(intent(
            "racer:rel-9:1:100",
            NAME,
            "1",
            Side::No,
            Action::Buy,
            6000,
        ));
        w.intents.push(intent(
            "racer:rel-9:2:100",
            NAME,
            "2",
            Side::No,
            Action::Buy,
            6500,
        ));
        w.intents.push(intent(
            "sniper:1:yes:100",
            "sniper",
            "1",
            Side::Yes,
            Action::Buy,
            4000,
        ));
        let d = racer().decide(&w.snap());
        let ids: Vec<&str> = d.cancels.iter().map(|c| c.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["racer:rel-9:1:100", "racer:rel-9:2:100"]);
        assert!(d.cancels.iter().all(|c| c.reason == "violation closed"));
    }

    #[test]
    fn parses_relationship_ids_with_colons() {
        assert_eq!(racer_relationship("racer:rel-9:381:1791475200"), Some("rel-9"));
        assert_eq!(racer_relationship("racer:a:b:381:1"), Some("a:b"));
        assert_eq!(racer_relationship("sniper:rel:1:2"), None);
        assert_eq!(racer_relationship("racer:1:2"), None);
    }

    #[test]
    fn verifier_rejects_a_bundle_that_can_lose() {
        // Buying YES on two of three exhaustive legs loses when the third wins.
        let legs = [
            PricedLeg {
                node: 0,
                side: Side::Yes,
                price: p(3000),
            },
            PricedLeg {
                node: 1,
                side: Side::Yes,
                price: p(3000),
            },
        ];
        assert_eq!(guaranteed_edge_pips(Structure::ExactlyOne(3), &legs), Some(-6000));
        assert!(verify(Structure::ExactlyOne(3), &legs, 0.0).is_none());
        assert_eq!(Structure::ExactlyOne(3).allowed_outcomes().len(), 3);
        assert_eq!(Structure::AtMostOne(3).allowed_outcomes().len(), 4);
        assert_eq!(Structure::Implication.allowed_outcomes().len(), 3);
    }

    fn arb_structure() -> impl proptest::strategy::Strategy<Value = Structure> {
        prop_oneof![
            (1usize..=MAX_LEGS).prop_map(Structure::AtMostOne),
            (1usize..=MAX_LEGS).prop_map(Structure::ExactlyOne),
            Just(Structure::Implication),
        ]
    }

    proptest! {
        /// The verifier never approves a bundle that can lose (by more than min_edge) in any
        /// outcome the structure allows; checked against an independent brute force.
        #[test]
        fn verifier_never_approves_a_losing_bundle(
            structure in arb_structure(),
            raw in proptest::collection::vec((0usize..MAX_LEGS, any::<bool>(), 1u32..=199), 1..12),
            min_edge in 0.0f64..0.05,
        ) {
            let n = structure.legs();
            let legs: Vec<PricedLeg> = raw
                .into_iter()
                .map(|(node, yes, t)| PricedLeg {
                    node: node % n,
                    side: if yes { Side::Yes } else { Side::No },
                    price: p(t * 50),
                })
                .collect();
            if verify(structure, &legs, min_edge).is_some() {
                for mask in 0u32..1 << n {
                    let outcome: Vec<bool> = (0..n).map(|i| mask & (1 << i) != 0).collect();
                    let yes = outcome.iter().filter(|&&o| o).count();
                    let possible = match structure {
                        Structure::AtMostOne(_) => yes <= 1,
                        Structure::ExactlyOne(_) => yes == 1,
                        Structure::Implication => !(outcome[0] && !outcome[1]),
                    };
                    if !possible { continue; }
                    let pnl: f64 = legs.iter().map(|l| {
                        let wins = outcome[l.node] == (l.side == Side::Yes);
                        f64::from(u8::from(wins)) - l.price.prob()
                    }).sum();
                    prop_assert!(pnl + 1e-9 >= min_edge, "loses {pnl} in {outcome:?}");
                }
            }
        }

        /// End to end: whatever the racer sends on random exclusive books can never lose.
        #[test]
        fn racer_bundles_never_lose(
            books in proptest::collection::vec(crate::testkit::arb_book(), 3),
            exhaustive in any::<bool>(),
        ) {
            let mut w = me_world(exhaustive);
            for (i, (bids, asks)) in books.iter().enumerate() {
                w.book(&(i + 1).to_string(), bids, asks);
            }
            let d = racer().decide(&w.snap());
            for b in &d.bundles {
                prop_assert!(b.atomic);
                let structure = if b.legs.iter().all(|l| l.side == Side::Yes) {
                    Structure::ExactlyOne(b.legs.len())
                } else {
                    Structure::AtMostOne(b.legs.len())
                };
                let legs: Vec<PricedLeg> = b.legs.iter().enumerate()
                    .map(|(node, l)| PricedLeg { node, side: l.side, price: l.price })
                    .collect();
                let edge = guaranteed_edge_pips(structure, &legs).expect("edge");
                prop_assert!(edge >= 100, "edge {edge} pips below min_edge");
            }
        }
    }
}
