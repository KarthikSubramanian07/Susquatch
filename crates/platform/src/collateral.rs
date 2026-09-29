//! ALL collateral model: guaranteed payout floors and relationship-backed advances.
//!
//! A port of `py/susq/platform/collateral.py`, which is the reference and documents what the
//! official API states versus what this model **assumes** (A1-A7). In short:
//!
//! * Active relationships sharing an exchange form a *component*; its id is the sorted covered
//!   exchange ids joined with `-` (the documented example is `"150-160"`).
//! * The **guaranteed payout floor** is the minimum payout of the component's positions over every
//!   joint outcome the relationships allow (exact enumeration, components of at most
//!   [`MAX_COMPONENT_EXCHANGES`] exchanges; larger ones have no floor and never back an advance).
//! * A fill's opening cost may be advanced up to `floor_after - outstanding` (A1); a fill that
//!   leaves `outstanding > floor_after` repays the difference at once (A2).
//! * `mutually_exclusive` = at most one YES (exactly one if exhaustive), `complementary` = exactly
//!   one of two, `implication`/`monotonic` with one antecedent and one consequent = antecedent YES
//!   implies consequent YES. Other types add no constraint, so the floor stays a lower bound (A5).

use std::collections::HashMap;

use susq_core::ExchangeId;

use crate::models::{NodeRole, Relationship, RelationshipType};

/// Largest component the model enumerates (2^16 joint outcomes, once per graph build).
pub const MAX_COMPONENT_EXCHANGES: usize = 16;
const EPS: f64 = 1e-9;

/// One active relationship, reduced to what the floor needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub id: String,
    pub kind: RelationshipType,
    pub members: Vec<ExchangeId>,
    pub antecedent: Option<ExchangeId>,
    pub consequent: Option<ExchangeId>,
    pub exhaustive: bool,
}

impl Relation {
    /// From a `GET /relationships` row: node roles first, then `direction` for implications.
    pub fn from_model(rel: &Relationship) -> Self {
        let role = |r: NodeRole| -> Vec<ExchangeId> {
            rel.nodes
                .iter()
                .filter(|n| n.role == r)
                .map(|n| n.exchange_id.clone())
                .collect()
        };
        let mut ante = role(NodeRole::Antecedent);
        let mut cons = role(NodeRole::Consequent);
        if matches!(
            rel.kind,
            RelationshipType::Implication | RelationshipType::Monotonic
        ) && let Some(d) = &rel.direction
        {
            if ante.is_empty() {
                ante.clone_from(&d.from_exchange_ids);
            }
            if cons.is_empty() {
                cons.clone_from(&d.to_exchange_ids);
            }
        }
        let mut members = role(NodeRole::Member);
        if members.is_empty() {
            members = rel
                .nodes
                .iter()
                .map(|n| n.exchange_id.clone())
                .filter(|e| !ante.contains(e) && !cons.contains(e))
                .collect();
        }
        let single = ante.len() == 1 && cons.len() == 1;
        Self {
            id: rel.id.clone(),
            kind: rel.kind,
            members,
            antecedent: single.then(|| ante[0].clone()),
            consequent: single.then(|| cons[0].clone()),
            exhaustive: rel.is_exhaustive,
        }
    }

    /// Every exchange the relationship covers, members first, without duplicates.
    pub fn exchanges(&self) -> Vec<ExchangeId> {
        let mut out: Vec<ExchangeId> = Vec::new();
        for e in self
            .members
            .iter()
            .chain(self.antecedent.iter())
            .chain(self.consequent.iter())
        {
            if !out.contains(e) {
                out.push(e.clone());
            }
        }
        out
    }

    /// False for relationship types the model does not understand (no constraint, A5).
    pub fn constrains(&self) -> bool {
        match self.kind {
            RelationshipType::MutuallyExclusive => self.members.len() >= 2,
            RelationshipType::Complementary => self.members.len() == 2,
            RelationshipType::Implication | RelationshipType::Monotonic => {
                self.antecedent.is_some() && self.consequent.is_some()
            }
            _ => false,
        }
    }

    fn allows(&self, yes: &dyn Fn(&ExchangeId) -> bool) -> bool {
        if !self.constrains() {
            return true;
        }
        let count = || self.members.iter().filter(|m| yes(m)).count();
        match self.kind {
            RelationshipType::MutuallyExclusive if self.exhaustive => count() == 1,
            RelationshipType::MutuallyExclusive => count() <= 1,
            RelationshipType::Complementary => count() == 1,
            _ => match (&self.antecedent, &self.consequent) {
                (Some(a), Some(c)) => !yes(a) || yes(c),
                _ => true,
            },
        }
    }
}

/// A connected set of exchanges linked by relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    pub id: String,
    pub exchange_ids: Vec<ExchangeId>,
    pub relationship_ids: Vec<String>,
    pub diagnostic: Option<String>,
    /// Allowed joint outcomes (`true` = YES, one per exchange); `None` when too large to enumerate.
    outcomes: Option<Vec<Vec<bool>>>,
}

impl Component {
    /// Guaranteed payout floor (in payout units = shares) of signed positions (+YES / -NO).
    pub fn floor(&self, position: &dyn Fn(&ExchangeId) -> i64) -> Option<f64> {
        let outcomes = self.outcomes.as_ref()?;
        let qty: Vec<i64> = self.exchange_ids.iter().map(position).collect();
        outcomes
            .iter()
            .map(|o| {
                qty.iter()
                    .zip(o)
                    .map(|(&q, &yes)| match (q > 0, yes) {
                        (true, true) => q,
                        (false, false) => -q,
                        _ => 0,
                    })
                    .sum::<i64>() as f64
            })
            .min_by(f64::total_cmp)
    }
}

/// What one fill does to cash and to its component's advance.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillEconomics {
    /// Nominal cost of the shares the fill opens.
    pub full_cost: f64,
    /// Cash released by the shares the fill closes.
    pub close_proceeds: f64,
    pub savings: f64,
    pub repayment: f64,
    /// The component the exchange belongs to, if any.
    pub component_id: Option<String>,
    pub floor_after: Option<f64>,
    pub outstanding_after: f64,
}

impl FillEconomics {
    /// Cash actually debited for the opening shares.
    pub fn effective_cost(&self) -> f64 {
        self.full_cost - self.savings
    }

    /// Total change in cash: proceeds − effective cost − repayment.
    pub fn cash_delta(&self) -> f64 {
        self.close_proceeds - self.effective_cost() - self.repayment
    }
}

/// Components of a relationship set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollateralGraph {
    components: Vec<Component>,
    by_exchange: HashMap<ExchangeId, usize>,
}

fn numeric_key(e: &ExchangeId) -> (u64, String) {
    (e.as_str().parse().unwrap_or(u64::MAX), e.as_str().to_owned())
}

impl CollateralGraph {
    /// From `GET /relationships` rows; only `active` ones (or ones without a status) count.
    pub fn new(relationships: &[Relationship]) -> Self {
        Self::from_relations(
            relationships
                .iter()
                .filter(|r| r.status.is_empty() || r.status.eq_ignore_ascii_case("active"))
                .map(Relation::from_model)
                .collect(),
        )
    }

    /// From already-reduced relations.
    pub fn from_relations(relations: Vec<Relation>) -> Self {
        let relations: Vec<Relation> = relations
            .into_iter()
            .filter(|r| !r.exchanges().is_empty())
            .collect();
        // Union-find over exchanges.
        let mut parent: HashMap<ExchangeId, ExchangeId> = HashMap::new();
        fn find(parent: &mut HashMap<ExchangeId, ExchangeId>, x: &ExchangeId) -> ExchangeId {
            let mut cur = x.clone();
            loop {
                let next = parent.entry(cur.clone()).or_insert_with(|| cur.clone()).clone();
                if next == cur {
                    return cur;
                }
                cur = next;
            }
        }
        for rel in &relations {
            let ex = rel.exchanges();
            let root = find(&mut parent, &ex[0]);
            for e in &ex[1..] {
                let r = find(&mut parent, e);
                if r != root {
                    parent.insert(r, root.clone());
                }
            }
        }
        let mut groups: Vec<(ExchangeId, Vec<Relation>)> = Vec::new();
        for rel in relations {
            let root = find(&mut parent, &rel.exchanges()[0]);
            match groups.iter_mut().find(|(r, _)| *r == root) {
                Some((_, rels)) => rels.push(rel),
                None => groups.push((root, vec![rel])),
            }
        }
        let mut graph = Self::default();
        for (_, rels) in groups {
            let mut exchanges: Vec<ExchangeId> = rels.iter().flat_map(Relation::exchanges).collect();
            exchanges.sort_by_key(numeric_key);
            exchanges.dedup();
            let mut ignored: Vec<String> = rels
                .iter()
                .filter(|r| !r.constrains())
                .map(|r| format!("{:?}", r.kind))
                .collect();
            ignored.sort();
            ignored.dedup();
            let mut notes = Vec::new();
            let outcomes = if exchanges.len() > MAX_COMPONENT_EXCHANGES {
                notes.push(format!(
                    "component spans {} exchanges (> {MAX_COMPONENT_EXCHANGES}); not evaluated",
                    exchanges.len()
                ));
                None
            } else {
                Some(enumerate(&exchanges, &rels))
            };
            if !ignored.is_empty() {
                notes.push(format!("no constraint modeled for: {}", ignored.join(", ")));
            }
            let mut relationship_ids: Vec<String> = rels.iter().map(|r| r.id.clone()).collect();
            relationship_ids.sort();
            let idx = graph.components.len();
            for e in &exchanges {
                graph.by_exchange.insert(e.clone(), idx);
            }
            graph.components.push(Component {
                id: exchanges
                    .iter()
                    .map(ExchangeId::as_str)
                    .collect::<Vec<_>>()
                    .join("-"),
                exchange_ids: exchanges,
                relationship_ids,
                diagnostic: (!notes.is_empty()).then(|| notes.join("; ")),
                outcomes,
            });
        }
        graph
    }

    /// Every component.
    pub fn components(&self) -> &[Component] {
        &self.components
    }

    /// The component an exchange belongs to.
    pub fn component_of(&self, exchange: &ExchangeId) -> Option<&Component> {
        self.by_exchange.get(exchange).map(|&i| &self.components[i])
    }

    /// Economics of one fill of `yes_delta` YES shares (+ buys YES / − buys NO) at YES price
    /// `yes_price`, against signed `positions` and per-component `outstanding` advances.
    pub fn fill(
        &self,
        positions: &dyn Fn(&ExchangeId) -> i64,
        outstanding: &HashMap<String, f64>,
        exchange: &ExchangeId,
        yes_delta: i64,
        yes_price: f64,
    ) -> FillEconomics {
        let pos = positions(exchange);
        let closed = if pos * yes_delta < 0 {
            yes_delta.abs().min(pos.abs())
        } else {
            0
        };
        let opened = yes_delta.abs() - closed;
        let (open_px, close_px) = if yes_delta > 0 {
            (yes_price, 1.0 - yes_price)
        } else {
            (1.0 - yes_price, yes_price)
        };
        let mut econ = FillEconomics {
            full_cost: opened as f64 * open_px,
            close_proceeds: closed as f64 * close_px,
            ..FillEconomics::default()
        };
        let Some(comp) = self.component_of(exchange) else {
            return econ;
        };
        let after = |e: &ExchangeId| positions(e) + if e == exchange { yes_delta } else { 0 };
        let floor = comp.floor(&after);
        let held = outstanding.get(&comp.id).copied().unwrap_or(0.0);
        let backing = floor.unwrap_or(0.0);
        if held > backing + EPS {
            econ.repayment = held - backing;
            econ.outstanding_after = backing;
        } else {
            econ.savings = econ.full_cost.min(backing - held).max(0.0);
            econ.outstanding_after = held + econ.savings;
        }
        econ.component_id = Some(comp.id.clone());
        econ.floor_after = floor;
        econ
    }

    /// Cash a sequence of fills `(exchange, yes_delta, yes_price)` would consume: effective entry
    /// cost plus any repayment the closing proceeds don't cover.
    ///
    /// The outstanding advance is not known to rust-live, so it is assumed to be the whole current
    /// floor of every component (already fully advanced): only floor *added* by these fills can back
    /// them. That can only over-state the cost.
    pub fn effective_cost(
        &self,
        positions: &HashMap<ExchangeId, i64>,
        fills: &[(ExchangeId, i64, f64)],
    ) -> f64 {
        let mut held = positions.clone();
        let mut outstanding: HashMap<String, f64> = HashMap::new();
        for (ex, _, _) in fills {
            if let Some(comp) = self.component_of(ex) {
                let now = |e: &ExchangeId| positions.get(e).copied().unwrap_or(0);
                outstanding
                    .entry(comp.id.clone())
                    .or_insert_with(|| comp.floor(&now).unwrap_or(0.0));
            }
        }
        let mut total = 0.0;
        for (ex, yes_delta, yes_price) in fills {
            if *yes_delta == 0 {
                continue;
            }
            let pos = |e: &ExchangeId| held.get(e).copied().unwrap_or(0);
            let econ = self.fill(&pos, &outstanding, ex, *yes_delta, *yes_price);
            total += econ.effective_cost() + (econ.repayment - econ.close_proceeds).max(0.0);
            *held.entry(ex.clone()).or_default() += yes_delta;
            if let Some(id) = econ.component_id {
                outstanding.insert(id, econ.outstanding_after);
            }
        }
        total
    }
}

/// Allowed joint outcomes by exhaustive enumeration (`exchanges.len() <= MAX_COMPONENT_EXCHANGES`).
fn enumerate(exchanges: &[ExchangeId], rels: &[Relation]) -> Vec<Vec<bool>> {
    let n = exchanges.len();
    let index: HashMap<&ExchangeId, usize> = exchanges.iter().enumerate().map(|(i, e)| (e, i)).collect();
    (0u32..1 << n)
        .map(|mask| (0..n).map(|i| mask & (1 << i) != 0).collect::<Vec<bool>>())
        .filter(|o| {
            let yes = |e: &ExchangeId| index.get(e).is_some_and(|&i| o[i]);
            rels.iter().all(|r| r.allows(&yes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ex(s: &str) -> ExchangeId {
        ExchangeId::new(s)
    }

    fn me(id: &str, members: &[&str], exhaustive: bool) -> Relation {
        Relation {
            id: id.into(),
            kind: RelationshipType::MutuallyExclusive,
            members: members.iter().map(|m| ex(m)).collect(),
            antecedent: None,
            consequent: None,
            exhaustive,
        }
    }

    fn implication(id: &str, a: &str, c: &str) -> Relation {
        Relation {
            id: id.into(),
            kind: RelationshipType::Implication,
            members: Vec::new(),
            antecedent: Some(ex(a)),
            consequent: Some(ex(c)),
            exhaustive: false,
        }
    }

    fn at<'a>(positions: &'a [(&'a str, i64)]) -> impl Fn(&ExchangeId) -> i64 + 'a {
        move |e| {
            positions
                .iter()
                .find(|(k, _)| *k == e.as_str())
                .map_or(0, |(_, q)| *q)
        }
    }

    #[test]
    fn floors_by_relationship_type() {
        let g = CollateralGraph::from_relations(vec![me("r", &["1", "2"], true)]);
        let c = g.component_of(&ex("1")).expect("linked");
        assert_eq!(c.id, "1-2");
        assert_eq!(c.floor(&at(&[("1", -10), ("2", -10)])), Some(10.0));
        assert_eq!(c.floor(&at(&[("1", 10), ("2", 10)])), Some(10.0));
        assert_eq!(c.floor(&at(&[("1", 10)])), Some(0.0));
        let open = CollateralGraph::from_relations(vec![me("r", &["1", "2"], false)]);
        let c = open.component_of(&ex("1")).expect("linked");
        assert_eq!(c.floor(&at(&[("1", 10), ("2", 10)])), Some(0.0));
        let imp = CollateralGraph::from_relations(vec![implication("i", "3", "1")]);
        let c = imp.component_of(&ex("3")).expect("linked");
        assert_eq!(c.floor(&at(&[("3", -5), ("1", 5)])), Some(5.0));
    }

    #[test]
    fn components_merge_and_sort_numerically() {
        let g = CollateralGraph::from_relations(vec![
            me("a", &["160", "150"], true),
            implication("b", "9", "150"),
        ]);
        let c = g.component_of(&ex("9")).expect("linked");
        assert_eq!(c.id, "9-150-160");
        assert_eq!(c.relationship_ids, vec!["a".to_owned(), "b".to_owned()]);
        assert!(g.component_of(&ex("7")).is_none());
    }

    #[test]
    fn second_no_leg_is_advanced_and_selling_repays() {
        let g = CollateralGraph::from_relations(vec![me("r", &["1", "2"], true)]);
        let mut out = HashMap::new();
        let e = g.fill(&at(&[("1", -10)]), &out, &ex("2"), -10, 0.55);
        assert!((e.full_cost - 4.5).abs() < 1e-9);
        assert!((e.savings - 4.5).abs() < 1e-9);
        assert_eq!(e.floor_after, Some(10.0));
        out.insert("1-2".to_owned(), e.outstanding_after);
        let sell = g.fill(&at(&[("1", -10), ("2", -10)]), &out, &ex("2"), 10, 0.5);
        assert!((sell.repayment - 4.5).abs() < 1e-9);
        assert!(sell.outstanding_after.abs() < 1e-9);
    }

    #[test]
    fn effective_cost_counts_only_what_new_floor_cannot_back() {
        let g = CollateralGraph::from_relations(vec![me("r", &["1", "2"], true)]);
        let fills = [(ex("1"), -10, 0.40), (ex("2"), -10, 0.55)];
        assert!((g.effective_cost(&HashMap::new(), &fills) - 6.0).abs() < 1e-9);
        // Without relationships: full notional.
        let none = CollateralGraph::default();
        assert!((none.effective_cost(&HashMap::new(), &fills) - 10.5).abs() < 1e-9);
    }

    #[test]
    fn oversized_components_have_no_floor() {
        let members: Vec<String> = (1..=20).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = members.iter().map(String::as_str).collect();
        let g = CollateralGraph::from_relations(vec![me("big", &refs, true)]);
        let c = g.component_of(&ex("1")).expect("linked");
        assert!(
            c.diagnostic
                .as_deref()
                .is_some_and(|d| d.contains("not evaluated"))
        );
        assert_eq!(c.floor(&at(&[("1", -1)])), None);
        let e = g.fill(&at(&[("1", -1)]), &HashMap::new(), &ex("2"), -1, 0.5);
        assert_eq!(e.savings, 0.0);
    }

    proptest! {
        #[test]
        fn floor_is_the_minimum_payout_over_allowed_outcomes(
            q in proptest::collection::vec(-20i64..20, 3),
            exhaustive in any::<bool>(),
        ) {
            let g = CollateralGraph::from_relations(vec![
                me("r", &["1", "2"], exhaustive),
                implication("i", "3", "1"),
            ]);
            let c = g.component_of(&ex("1")).expect("linked");
            let pos = [("1", q[0]), ("2", q[1]), ("3", q[2])];
            let floor = c.floor(&at(&pos)).expect("small");
            let mut min = f64::INFINITY;
            for mask in 0u8..8 {
                let (y1, y2, y3) = (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
                let yes_count = u8::from(y1) + u8::from(y2);
                let me_ok = if exhaustive { yes_count == 1 } else { yes_count <= 1 };
                if !me_ok || (y3 && !y1) {
                    continue;
                }
                let pay = |qty: i64, yes: bool| if qty > 0 { if yes { qty } else { 0 } } else if yes { 0 } else { -qty };
                let p = (pay(q[0], y1) + pay(q[1], y2) + pay(q[2], y3)) as f64;
                prop_assert!(floor <= p + 1e-9);
                min = min.min(p);
            }
            prop_assert!((floor - min).abs() < 1e-9);
        }
    }
}
