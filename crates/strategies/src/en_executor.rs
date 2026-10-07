//! Election-night executor: trade race markets on needle posteriors, and roll appreciated legs.
//!
//! Acts only when enabled **and** the needle watchdog reports healthy. With the needle missing or
//! unhealthy it cancels every live needle-derived intent (from any strategy) and sends nothing.
//!
//! The results pollers' health gates races individually (`NeedleView::disputed_races`,
//! `NeedleView::stale_feed_races`, filled by rust-live from `EnrHealth` with `--election`): a
//! **disputed** race (sources disagree beyond tolerance) is refused outright and this strategy's
//! resting orders on it are cancelled; a race whose **feed is stale** gets no new buys, though a
//! held leg is still rolled (that reduces exposure on a posterior the needle still stands by).
//!
//! For each posterior, every verified market rule on that race maps to a win probability `q` by party
//! (`DEM` → `p_dem`, `REP` → `p_rep`; other rules are skipped). Then, per exchange, at most one action:
//!
//! * **Roll**: holding a side whose sell touch is within `roll_within_pts` of that side's `q`, sell the
//!   held side (a covered `Action::Sell`, qty ≤ holding) at no less than `q − roll`. Locked legs are
//!   never rolled.
//! * **Buy**: buy YES when `q − ask ≥ threshold`, NO when `(1 − q) − no_ask ≥ threshold`, where
//!   `threshold = max(min_edge_pts, se_mult × SE)`. With `require_corroboration`, a buy also needs
//!   `n_batches ≥ 2` or a same-direction move of the fresh real-money fair
//!   (`sign(p_yes − prev_p_yes) = sign(q − mid)`). Limit = `floor(side q − threshold)`; size =
//!   `floor(max_order_frac × equity / limit)`, capped by depth within the limit. A buy that would
//!   reduce a locked holding (buying its opposite side) is skipped.
//!
//! Model SE. The posterior carries the margin it rests on (`margin_mean` ± `margin_sd`, points of
//! two-party vote) and `q` is a normal tail of it, so the standard error of `q` follows by the delta
//! method ([`margin_se`]): `SE(q) = phi(margin_mean / margin_sd) × se_margin_err_pts / margin_sd`,
//! the move in `q` for a model error of `se_margin_err_pts` points in the margin. A race whose
//! margin sits within a point or two of zero at a tight sd is fragile (a small error flips it, SE is
//! large) while a race called by several sd is not, whatever `n_batches` says. When the posterior
//! carries no `margin_sd` (0, as from an older needle) or `se_margin_err_pts = 0`, the binomial-style
//! proxy [`model_se`] `= sqrt(q (1 − q) / (1 + n_batches))` stands in: widest at q = 0.5 with no
//! reported batches, shrinking as batches accumulate.
//!
//! Count-order gate (EXP-006, `config/count_order.toml`). The needle's `margin_mean` is already a
//! projection of the **final** margin: `cpp/needle` adds the expected remaining votes of every
//! county and vote type at the prior's vote-type offsets (`NeedlePrior.vote_types[].margin_offset`,
//! with `typical_report_order` from `py/susq/replay/conventions.py` filling untyped tallies), shifted
//! by what the county's reported votes taught it. Subtracting the registered drift from it would
//! count the remaining-vote lean twice. So the table is used only as a **direction check and band**:
//! at `now` the race's registered remaining drift (interpolated by hour after poll close) either has a
//! direction (`|drift| > band`: the count still to come favours D when the drift is negative) or not.
//! A buy whose lean (YES on a DEM rule or NO on a REP rule is a D lean) agrees with that direction
//! needs the threshold above; a buy against it, or in a race with no registered direction, needs the
//! threshold plus [`band_prob`], the band converted to probability through the posterior's margin and
//! sd: `|Phi(m / sd) − Phi((m ∓ band) / sd)|`, the margin moved by the band against the buy. The band
//! is what the needle cannot know: within-type early-versus-late lean (late Clark mail, Maricopa late
//! earlies, the CA tail) that its vote-type offsets average away. With no usable `margin_sd` a gated
//! buy is refused. Races whose state is not in the table, and every race before its state's poll
//! close, trade on the threshold alone. Rolls are never gated.
//!
//! Lock-time rule. The contest ranks on the **final** count, not the lock. From `lock_window_start`,
//! a race whose posterior counts below `lock_max_reported_frac`, whose registered lock drift is at least
//! `lock_min_drift_pts` with a band smaller than the drift, may be bought on the side the remaining
//! count favours at the normal threshold, on the needle's last posterior for it even when the needle
//! has been quiet on that race for longer than `[needle] posterior_max_age_s` (counties post once a
//! day there), as long as the needle as a whole is healthy (an outage clears every posterior) and that
//! posterior is no older than `lock_posterior_max_age_s`. Disputed and stale-feed races are skipped;
//! quiet posteriors are never rolled on. Sizing is the normal per-order size under the night budget.
//!
//! Leader/contender switch: with `contender_hedge_multiple > 0`, once equity / 100,000 reaches it the
//! executor opens no new positions and only rolls. Off by default (the field maximum on the night is
//! not observed).
//!
//! Every order is `needle_derived` with `fair_yes = q`, and every order the executor sends is also
//! journalled as an [`ORDER_EVENT`] carrying the registered count order behind it.

use std::sync::Arc;

use serde_json::json;
use susq_book::L2Book;
use susq_contracts::v1::RacePosterior;
use susq_core::{
    Action, ExchangeId, OrderBundle, Pips, Side,
    config::EnCfg,
    count_order::{CountOrder, Lean, Registered},
};

use crate::{
    Strategy,
    draft::{Draft, shares_for_budget},
    snapshot::{Decision, NS_PER_S, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "en";

/// Journal event for every order the executor sends (what it saw and what the playbook said).
pub const ORDER_EVENT: &str = "en_order";

/// Everyone starts the contest at this balance (the contender switch's unit).
const STARTING_BALANCE: f64 = 100_000.0;

/// Standard-normal CDF (Abramowitz & Stegun 7.1.26 for erfc; absolute error below 1.5e-7).
fn norm_cdf(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erfc = poly * (-x * x).exp();
    if z >= 0.0 { 1.0 - 0.5 * erfc } else { 0.5 * erfc }
}

/// The lean of buying `side` on a rule whose YES pays when `party_dem`'s party wins.
fn buy_lean(dem_rule: bool, side: Side) -> Lean {
    match (dem_rule, side) {
        (true, Side::Yes) | (false, Side::No) => Lean::Dem,
        _ => Lean::Rep,
    }
}

/// The count-order band in probability for a buy leaning `lean`: how much that side's win
/// probability falls if the margin moves `band_pts` against it,
/// `|Phi(m / sd) − Phi((m ∓ band) / sd)|`. `None` without a usable margin and sd.
pub fn band_prob(margin_mean: f64, margin_sd: f64, band_pts: f64, lean: Lean) -> Option<f64> {
    if !(margin_sd.is_finite() && margin_sd > 0.0 && margin_mean.is_finite() && band_pts.is_finite()) {
        return None;
    }
    let against = match lean {
        Lean::Dem => -band_pts.max(0.0),
        Lean::Rep => band_pts.max(0.0),
    };
    Some((norm_cdf(margin_mean / margin_sd) - norm_cdf((margin_mean + against) / margin_sd)).abs())
}

/// What the playbook registers for one race at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct CountOrderView {
    /// USPS state code.
    pub state: String,
    /// The state code, or the race-level override pattern that applies.
    pub profile: String,
    pub registered: Registered,
}

impl CountOrderView {
    fn json(&self) -> serde_json::Value {
        json!({
            "state": self.state,
            "profile": self.profile,
            "hours_after_close": self.registered.hours_after_close,
            "drift_pts": self.registered.drift_pts,
            "band_pts": self.registered.band_pts,
            "favours": self.registered.favours().map(Lean::as_str),
        })
    }
}

/// How a buy is being considered.
#[derive(Debug, Clone, Copy)]
struct Mode {
    /// The race's results feed is stale: no new buys (rolls still happen).
    feed_stale: bool,
    /// Contender switch engaged: no new positions.
    no_new_positions: bool,
    /// Lock-time rule on a quiet posterior: only this lean, no rolls, no band.
    lock: Option<LockBuy>,
}

#[derive(Debug, Clone, Copy)]
struct LockBuy {
    lean: Lean,
    posterior_age_s: f64,
}

/// Proxy standard error of a posterior win probability: `sqrt(q (1 − q) / (1 + n_batches))`.
/// The fallback when the posterior carries no margin uncertainty.
pub fn model_se(q: f64, n_batches: u32) -> f64 {
    let q = q.clamp(0.0, 1.0);
    (q * (1.0 - q) / (1.0 + f64::from(n_batches))).sqrt()
}

/// Standard-normal density.
fn phi(z: f64) -> f64 {
    (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Standard error of the win probability from the posterior's margin, by the delta method:
/// `phi(margin_mean / margin_sd) × err_pts / margin_sd`, the change in `q = Phi(margin / sd)` for
/// a model error of `err_pts` points in the margin. The same for either party (the density is
/// symmetric). `None` when the posterior carries no usable `margin_sd` or `err_pts` is 0, so the
/// caller falls back to [`model_se`].
pub fn margin_se(margin_mean: f64, margin_sd: f64, err_pts: f64) -> Option<f64> {
    if !(margin_sd.is_finite() && margin_sd > 0.0 && margin_mean.is_finite()) {
        return None;
    }
    if !(err_pts.is_finite() && err_pts > 0.0) {
        return None;
    }
    Some(phi(margin_mean / margin_sd) * err_pts / margin_sd)
}

/// The election-night executor.
#[derive(Debug, Clone, PartialEq)]
pub struct EnExecutor {
    pub cfg: EnCfg,
    /// The pre-registered count-order table (rust-live loads `count_order_path` at start). Without
    /// it the gate and the lock-time rule are inert.
    count_order: Option<Arc<CountOrder>>,
}

/// Probability that `side` of a YES-probability-`q` contract pays.
fn side_prob(side: Side, q: f64) -> f64 {
    match side {
        Side::Yes => q,
        Side::No => 1.0 - q,
    }
}

impl EnExecutor {
    /// Election-night executor with its config.
    pub fn new(cfg: EnCfg) -> Self {
        Self {
            cfg,
            count_order: None,
        }
    }

    /// Attach the count-order table (the gate and the lock-time rule read it).
    pub fn with_count_order(mut self, table: Arc<CountOrder>) -> Self {
        self.count_order = Some(table);
        self
    }

    /// The registered count order for a race at `now_ns`: `None` with the gate's table absent, the
    /// race's state unregistered, or before its poll close.
    pub fn count_order_at(&self, race_id: &str, now_ns: i64) -> Option<CountOrderView> {
        let profile = self.count_order.as_ref()?.profile(race_id)?;
        Some(CountOrderView {
            state: profile.state.to_owned(),
            profile: profile.key.to_owned(),
            registered: profile.at_ns(now_ns)?,
        })
    }

    /// Whether the lock-time rule is on and its window has opened.
    fn lock_window_open(&self, now_ns: i64) -> bool {
        self.cfg.lock_rule_enabled
            && self.count_order.is_some()
            && self
                .cfg
                .lock_window_start
                .timestamp_nanos_opt()
                .is_some_and(|start| now_ns >= start)
    }

    /// The lean the lock-time rule may buy in a race, or why it may not.
    pub fn lock_rule_lean(&self, post: &RacePosterior) -> Result<Lean, String> {
        let profile = self
            .count_order
            .as_ref()
            .and_then(|t| t.profile(&post.race_id))
            .ok_or_else(|| "state not in the count-order table".to_owned())?;
        let lock = profile.lock();
        if lock.drift_pts.abs() < self.cfg.lock_min_drift_pts || lock.band_pts >= lock.drift_pts.abs() {
            return Err(format!(
                "registered lock drift {:+.2} ± {:.2} ({}) below the lock rule's bar",
                lock.drift_pts, lock.band_pts, profile.key
            ));
        }
        if !(post.reported_frac > 0.0 && post.reported_frac < self.cfg.lock_max_reported_frac) {
            return Err(format!(
                "reported {:.0}% is not below {:.0}%",
                100.0 * post.reported_frac,
                100.0 * self.cfg.lock_max_reported_frac
            ));
        }
        lock.favours()
            .ok_or_else(|| "registered lock drift has no direction".to_owned())
    }

    /// Model standard error of `q` for a posterior: from its margin when it carries one
    /// ([`margin_se`]), else the batch-count proxy ([`model_se`]).
    pub fn posterior_se(&self, post: &RacePosterior, q: f64) -> f64 {
        margin_se(post.margin_mean, post.margin_sd, self.cfg.se_margin_err_pts)
            .unwrap_or_else(|| model_se(q, post.n_batches))
    }

    /// Edge a buy must clear: `max(min_edge_pts / 100, se_mult × SE)`.
    pub fn threshold(&self, post: &RacePosterior, q: f64) -> f64 {
        (self.cfg.min_edge_pts / 100.0).max(self.cfg.se_mult * self.posterior_se(post, q))
    }

    fn act(&self, snap: &Snapshot<'_>, post: &RacePosterior, ex: &ExchangeId, mode: Mode, d: &mut Decision) {
        let Some(rule) = snap.markets.rule(ex).filter(|r| r.verified) else {
            return;
        };
        let dem_rule = rule.party.eq_ignore_ascii_case("DEM");
        let q = if dem_rule {
            post.p_dem
        } else if rule.party.eq_ignore_ascii_case("REP") {
            post.p_rep
        } else {
            return;
        };
        if !(q.is_finite() && (0.0..=1.0).contains(&q)) {
            d.note(format!("en {ex}: posterior q {q} out of range"));
            return;
        }
        if snap.has_live_intent(NAME, ex) {
            return;
        }
        let Some(book) = snap.fresh_book(ex) else {
            d.note(format!("en {ex}: no fresh book"));
            return;
        };
        let held = snap.position(ex);
        let held_side = match held.signum() {
            1 => Some(Side::Yes),
            -1 => Some(Side::No),
            _ => None,
        };
        let locked = snap.is_locked(ex);
        let count_order = self.count_order_at(&post.race_id, snap.now_ns);
        // Rolls only reduce exposure and are never gated by the playbook; a quiet posterior (lock
        // rule) is never rolled on.
        if let Some(side) = held_side.filter(|_| mode.lock.is_none())
            && self.roll(
                snap,
                post,
                ex,
                book,
                side,
                held.abs(),
                q,
                locked,
                count_order.as_ref(),
                d,
            )
        {
            return;
        }
        if mode.feed_stale {
            d.note(format!("en {ex}: results feed stale, no new buys"));
            return;
        }
        if mode.no_new_positions {
            return;
        }
        let ctx = BuyCtx {
            post,
            ex,
            book,
            q,
            dem_rule,
            locked_side: held_side.filter(|_| locked),
            count_order: count_order.as_ref(),
            lock: mode.lock,
        };
        self.buy(snap, &ctx, d);
    }

    /// Sell a held side whose bid is within `roll_within_pts` of its probability. True if acted on.
    #[allow(clippy::too_many_arguments)]
    fn roll(
        &self,
        snap: &Snapshot<'_>,
        post: &RacePosterior,
        ex: &ExchangeId,
        book: &L2Book,
        side: Side,
        held: i64,
        q: f64,
        locked: bool,
        count_order: Option<&CountOrderView>,
        d: &mut Decision,
    ) -> bool {
        let Some(floor) = Pips::ceil_from_prob(side_prob(side, q) - self.cfg.roll_within_pts / 100.0) else {
            return false;
        };
        if !book.touch(side, Action::Sell).is_some_and(|bid| bid >= floor) {
            return false;
        }
        if locked {
            d.note(format!("en {ex}: roll due but the leg is locked, not rolled"));
            return false;
        }
        let qty = book.sweep(side, Action::Sell, held, floor).qty;
        let draft = Draft {
            exchange: ex.clone(),
            side,
            action: Action::Sell,
            qty,
            price: floor,
            fair_yes: Some(q),
        };
        let key = format!("{ex}:roll");
        match draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, true) {
            Some(spec) => {
                d.note(format!("en {ex}: roll {side} x{qty} @ ≥ {floor} (q {q:.3})"));
                d.event(
                    ORDER_EVENT,
                    json!({
                        "intent_id": spec.intent_id,
                        "race_id": post.race_id,
                        "exchange_id": ex.as_str(),
                        "kind": "roll",
                        "side": side.to_string(),
                        "qty": spec.quantity.get(),
                        "limit": floor.prob(),
                        "q": q,
                        "reported_frac": post.reported_frac,
                        "margin_mean": post.margin_mean,
                        "margin_sd": post.margin_sd,
                        "count_order": count_order.map(CountOrderView::json),
                        "gate": "rolls are never gated",
                    }),
                );
                d.bundles.push(OrderBundle::single(spec));
                true
            }
            None => false,
        }
    }

    /// The extra edge the count-order gate asks of a buy leaning `lean`: 0 when it agrees with the
    /// registered direction, the band in probability otherwise; `Err` when the band cannot be
    /// converted (no usable margin sd), which refuses the buy.
    fn gate_extra(&self, ctx: &BuyCtx<'_>, lean: Lean) -> Result<f64, String> {
        if !self.cfg.count_order_enabled {
            return Ok(0.0);
        }
        if let Some(lock) = ctx.lock {
            // The lock rule only ever buys the favoured lean, at the registered threshold.
            debug_assert_eq!(lock.lean, lean);
            return Ok(0.0);
        }
        let Some(view) = ctx.count_order else {
            return Ok(0.0);
        };
        if view.registered.favours() == Some(lean) {
            return Ok(0.0);
        }
        band_prob(
            ctx.post.margin_mean,
            ctx.post.margin_sd,
            view.registered.band_pts,
            lean,
        )
        .ok_or_else(|| "posterior has no margin sd to price the count-order band".to_owned())
    }

    fn buy(&self, snap: &Snapshot<'_>, ctx: &BuyCtx<'_>, d: &mut Decision) {
        let BuyCtx {
            post,
            ex,
            book,
            q,
            locked_side,
            ..
        } = *ctx;
        let threshold = self.threshold(post, q);
        for side in [Side::Yes, Side::No] {
            let p = side_prob(side, q);
            let Some(ask) = book.touch(side, Action::Buy) else {
                continue;
            };
            if p - ask.prob() + 1e-9 < threshold {
                continue;
            }
            let lean = buy_lean(ctx.dem_rule, side);
            if ctx.lock.is_some_and(|l| l.lean != lean) {
                continue;
            }
            let extra = match self.gate_extra(ctx, lean) {
                Ok(extra) => extra,
                Err(why) => {
                    d.note(format!("en {ex}: {side} ({}) refused: {why}", lean.as_str()));
                    continue;
                }
            };
            let required = threshold + extra;
            if p - ask.prob() + 1e-9 < required {
                d.note(format!(
                    "en {ex}: {side} ({}) edge {:.3} clears {threshold:.3} but not the count-order band (needs {required:.3}; {})",
                    lean.as_str(),
                    p - ask.prob(),
                    describe(ctx.count_order),
                ));
                continue;
            }
            if locked_side == Some(side.opposite()) {
                d.note(format!(
                    "en {ex}: buying {side} would reduce a locked leg, skipped"
                ));
                return;
            }
            if self.cfg.require_corroboration && !corroborated(snap, post, ex, book, q) {
                d.note(format!(
                    "en {ex}: {side} edge uncorroborated (n_batches {})",
                    post.n_batches
                ));
                return;
            }
            let Some(limit) = Pips::floor_from_prob(p - required) else {
                return;
            };
            let cap = shares_for_budget(self.cfg.max_order_frac * snap.equity, limit);
            let draft = Draft {
                exchange: ex.clone(),
                side,
                action: Action::Buy,
                qty: book.sweep(side, Action::Buy, cap, limit).qty,
                price: limit,
                fair_yes: Some(q),
            };
            let key = format!("{ex}:{side}");
            if let Some(spec) = draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, true) {
                let path = if ctx.lock.is_some() { "lock_rule" } else { "needle" };
                d.note(format!(
                    "en {ex}: buy {side} x{} @ {limit} (q {q:.3}, threshold {threshold:.3}, required {required:.3}, {path}; {})",
                    spec.quantity,
                    describe(ctx.count_order),
                ));
                d.event(
                    ORDER_EVENT,
                    json!({
                        "intent_id": spec.intent_id,
                        "race_id": post.race_id,
                        "exchange_id": ex.as_str(),
                        "kind": "buy",
                        "path": path,
                        "side": side.to_string(),
                        "lean": lean.as_str(),
                        "qty": spec.quantity.get(),
                        "limit": limit.prob(),
                        "ask": ask.prob(),
                        "q": q,
                        "threshold": threshold,
                        "required": required,
                        "band_prob": extra,
                        "agrees": ctx.count_order.map(|v| v.registered.favours() == Some(lean)),
                        "reported_frac": post.reported_frac,
                        "margin_mean": post.margin_mean,
                        "margin_sd": post.margin_sd,
                        "n_batches": post.n_batches,
                        "posterior_age_s": ctx.lock.map(|l| l.posterior_age_s),
                        "count_order": ctx.count_order.map(CountOrderView::json),
                    }),
                );
                d.bundles.push(OrderBundle::single(spec));
            }
            return;
        }
    }

    /// The lock-time rule over the races the needle has gone quiet on.
    fn lock_rule(&self, snap: &Snapshot<'_>, no_new_positions: bool, d: &mut Decision) {
        let Some(needle) = snap.needle else { return };
        if !self.lock_window_open(snap.now_ns) || no_new_positions {
            return;
        }
        let mut quiet: Vec<_> = needle
            .quiet_posteriors
            .iter()
            .filter(|(race, _)| !needle.posteriors.contains_key(*race))
            .collect();
        quiet.sort_by(|a, b| a.0.cmp(b.0));
        let max_age_s = self.cfg.lock_posterior_max_age_s as f64;
        for (race, qp) in quiet {
            let age_s = snap.now_ns.saturating_sub(qp.produced_at_ns) as f64 / NS_PER_S as f64;
            if age_s > max_age_s {
                d.note(format!(
                    "en {race}: lock rule: last posterior {age_s:.0} s old, over {max_age_s:.0} s"
                ));
                continue;
            }
            if needle.disputed_races.contains(race) {
                let exchanges = snap.markets.exchanges_for_race(race);
                for i in snap
                    .live_intents(NAME)
                    .filter(|i| exchanges.contains(&i.exchange_id))
                {
                    d.cancel(&i.intent_id, "race disputed");
                }
                d.note(format!("en {race}: lock rule: race disputed, not acting"));
                continue;
            }
            if needle.stale_feed_races.contains(race) {
                d.note(format!("en {race}: lock rule: results feed stale, no new buys"));
                continue;
            }
            let lean = match self.lock_rule_lean(&qp.posterior) {
                Ok(lean) => lean,
                Err(why) => {
                    d.note(format!("en {race}: lock rule not eligible: {why}"));
                    continue;
                }
            };
            let mode = Mode {
                feed_stale: false,
                no_new_positions: false,
                lock: Some(LockBuy {
                    lean,
                    posterior_age_s: age_s,
                }),
            };
            for ex in snap.markets.exchanges_for_race(race) {
                self.act(snap, &qp.posterior, ex, mode, d);
            }
        }
    }
}

/// Everything one buy decision reads.
struct BuyCtx<'a> {
    post: &'a RacePosterior,
    ex: &'a ExchangeId,
    book: &'a L2Book,
    q: f64,
    /// YES pays when the Democrat wins (else the Republican).
    dem_rule: bool,
    locked_side: Option<Side>,
    count_order: Option<&'a CountOrderView>,
    lock: Option<LockBuy>,
}

/// One-line count-order context for notes.
fn describe(view: Option<&CountOrderView>) -> String {
    match view {
        None => "no registered count order".to_owned(),
        Some(v) => format!(
            "count order {} +{:.1} h: drift {:+.2} ± {:.2}, favours {}",
            v.profile,
            v.registered.hours_after_close,
            v.registered.drift_pts,
            v.registered.band_pts,
            v.registered.favours().map_or("neither", Lean::as_str)
        ),
    }
}

/// At least two county batches, or the fresh real-money fair moved the same way the needle points.
fn corroborated(snap: &Snapshot<'_>, post: &RacePosterior, ex: &ExchangeId, book: &L2Book, q: f64) -> bool {
    if post.n_batches >= 2 {
        return true;
    }
    let (Some(mid), Some(moved)) = (book.mid(), snap.fresh_fair(ex).and_then(|f| f.moved())) else {
        return false;
    };
    let dir = q - mid;
    dir != 0.0 && moved != 0.0 && dir.signum() == moved.signum()
}

impl Strategy for EnExecutor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        let needle = match snap.needle {
            Some(n) if n.healthy => n,
            _ => {
                for i in snap.intents.iter().filter(|i| i.needle_derived) {
                    d.cancel(&i.intent_id, "needle unhealthy");
                }
                d.note("en: needle missing or unhealthy, standing down");
                return d;
            }
        };
        let no_new_positions = self.cfg.contender_hedge_multiple > 0.0
            && snap.equity / STARTING_BALANCE >= self.cfg.contender_hedge_multiple;
        if no_new_positions {
            d.note(format!(
                "en: equity multiple {:.2} ≥ contender_hedge_multiple {:.2}: rolls only, no new positions",
                snap.equity / STARTING_BALANCE,
                self.cfg.contender_hedge_multiple
            ));
        }
        let mut races: Vec<&RacePosterior> = needle.posteriors.values().collect();
        races.sort_by(|a, b| a.race_id.cmp(&b.race_id));
        for post in races {
            let exchanges = snap.markets.exchanges_for_race(&post.race_id);
            if needle.disputed_races.contains(&post.race_id) {
                for i in snap
                    .live_intents(NAME)
                    .filter(|i| exchanges.contains(&i.exchange_id))
                {
                    d.cancel(&i.intent_id, "race disputed");
                }
                d.note(format!(
                    "en {}: results sources disagree, race disputed, not acting",
                    post.race_id
                ));
                continue;
            }
            let mode = Mode {
                feed_stale: needle.stale_feed_races.contains(&post.race_id),
                no_new_positions,
                lock: None,
            };
            for ex in exchanges {
                self.act(snap, post, ex, mode, &mut d);
            }
        }
        self.lock_rule(snap, no_new_positions, &mut d);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{World, intent};

    const RACE: &str = "2026-SEN-OH";

    fn cfg() -> EnCfg {
        EnCfg {
            enabled: true,
            ..EnCfg::default()
        }
    }

    /// Exchange 1 = DEM wins, exchange 2 = REP wins, both on [`RACE`].
    fn world(p_dem: f64, n_batches: u32) -> World {
        let mut w = World::new(100_000.0);
        w.rule("1", "m1", RACE, "DEM", true);
        w.rule("2", "m1", RACE, "REP", true);
        w.needle(true, &[(RACE, p_dem, 1.0 - p_dem, n_batches)]);
        w
    }

    fn legs(d: &Decision) -> Vec<(String, Side, Action, u32, i64)> {
        d.bundles
            .iter()
            .flat_map(|b| &b.legs)
            .map(|l| {
                (
                    l.exchange_id.to_string(),
                    l.side,
                    l.action,
                    l.price.get(),
                    l.quantity.get(),
                )
            })
            .collect()
    }

    #[test]
    fn disabled_does_nothing() {
        let mut w = world(0.9, 50);
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        assert_eq!(
            EnExecutor::new(EnCfg::default()).decide(&w.snap()),
            Decision::default()
        );
    }

    #[test]
    fn unhealthy_needle_cancels_needle_derived_intents_only() {
        let mut w = world(0.9, 50);
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        let mut en = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5000);
        en.needle_derived = true;
        let mut other = intent("scenario:9:1", "scenario", "9", Side::Yes, Action::Buy, 5000);
        other.needle_derived = true;
        w.intents.extend([
            en,
            other,
            intent("sniper:1:yes:1", "sniper", "1", Side::Yes, Action::Buy, 4000),
        ]);
        w.needle.as_mut().expect("needle").healthy = false;
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        let ids: Vec<&str> = d.cancels.iter().map(|c| c.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["en:1:yes:1", "scenario:9:1"]);
        assert!(d.cancels.iter().all(|c| c.reason == "needle unhealthy"));

        w.needle = None;
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 2);
    }

    #[test]
    fn model_se_heuristic() {
        assert!((model_se(0.5, 0) - 0.5).abs() < 1e-12);
        assert!((model_se(0.7, 99) - (0.21f64 / 100.0).sqrt()).abs() < 1e-12);
        assert_eq!(model_se(1.0, 3), 0.0);
    }

    #[test]
    fn buy_threshold_is_the_larger_of_min_edge_and_three_se() {
        // q = 0.70 with 99 batches: SE 0.0458, 3×SE = 0.137 > 0.08.
        let en = EnExecutor::new(cfg());
        let no_margin = RacePosterior {
            n_batches: 99,
            ..Default::default()
        };
        assert!((en.threshold(&no_margin, 0.70) - 3.0 * (0.0021f64).sqrt()).abs() < 1e-12);
        let mut w = world(0.70, 99);
        w.book("1", &[(5000, 100)], &[(5700, 100)]); // edge 0.13: above 8 pts, below 3 SE
        assert!(legs(&en.decide(&w.snap())).is_empty());
        w.book("1", &[(5000, 100)], &[(5500, 100)]); // edge 0.15 ≥ 0.137
        // Limit floor(0.70 − 0.1375) = 0.56; the 0.55 ask has 100 shares.
        assert_eq!(
            legs(&en.decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Buy, 5600, 100)]
        );

        // Plenty of batches: SE is small, the 8-point floor binds.
        let mut w = world(0.70, 9_999);
        w.book("1", &[(5000, 100)], &[(6150, 100)]); // edge 0.085 ≥ 0.08
        let d = en.decide(&w.snap());
        let o = &d.bundles[0].legs[0];
        assert!(o.needle_derived);
        assert_eq!(o.fair_yes, Some(0.70));
        assert_eq!((o.price.get(), o.quantity.get()), (6200, 100));
        assert!(o.intent_id.starts_with("en:1:yes:"));
    }

    #[test]
    fn buys_no_on_the_other_party_and_sizes_by_order_fraction() {
        let mut w = world(0.90, 9_999); // REP wins with 0.10 → NO on exchange 2 is worth 0.90.
        w.equity = 10_000.0; // 2% = 200 SUSQies at the 0.82 NO limit (0.90 − 8 pts) → 243 shares.
        // YES bid 0.25 → NO ask 0.75; edge 0.15.
        w.book("2", &[(2500, 1_000)], &[(3000, 1_000)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("2".into(), Side::No, Action::Buy, 8200, 243)]
        );
    }

    #[test]
    fn corroboration_needs_two_batches_or_a_same_direction_fair_move() {
        let en = EnExecutor::new(EnCfg {
            se_mult: 0.0,
            ..cfg()
        });
        let mut w = world(0.80, 1);
        w.book("1", &[(6000, 100)], &[(6500, 100)]); // mid 0.625, q above it
        let d = en.decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("uncorroborated")));

        w.fair("1", 0.66, Some(0.62)); // real money moved up too
        assert_eq!(en.decide(&w.snap()).bundles.len(), 1);

        w.fair("1", 0.60, Some(0.62)); // real money moved the other way
        assert!(en.decide(&w.snap()).bundles.is_empty());

        let mut w = world(0.80, 2);
        w.book("1", &[(6000, 100)], &[(6500, 100)]);
        assert_eq!(en.decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn rolls_an_appreciated_leg_near_q() {
        let mut w = world(0.70, 9_999);
        w.position("1", 40);
        // Bid 0.69 ≥ 0.70 − 0.02 → sell the 40 YES at no less than 0.68.
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Sell, 6800, 40)]
        );
        // Holding NO on exchange 2 (REP, q 0.30 → NO worth 0.70): NO bid = 1 − 0.31 = 0.69.
        let mut w = world(0.70, 9_999);
        w.position("2", -15);
        w.book("2", &[(2900, 100)], &[(3100, 100)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("2".into(), Side::No, Action::Sell, 6800, 15)]
        );
    }

    #[test]
    fn locked_legs_are_not_rolled_or_reduced() {
        let mut w = world(0.70, 9_999);
        w.position("1", 40);
        w.target("1", 40, 0.9, true);
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("locked")));

        // Bid far above q would be a NO buy, which would reduce the locked YES holding.
        let mut w = world(0.50, 9_999);
        w.position("1", 40);
        w.target("1", 40, 0.9, true);
        w.book("1", &[(6000, 100)], &[(6100, 100)]);
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
    }

    /// Gap 8 of the readiness audit: "the election-night executor must refuse to act on disputed
    /// races" (docs/ELECTION_NIGHT.md §4), including what it already has resting on them.
    #[test]
    fn disputed_races_are_refused_and_their_orders_cancelled() {
        let mut w = world(0.90, 9_999);
        w.book("1", &[(5000, 100)], &[(5500, 100)]); // a 35 pt edge
        w.rule("9", "m9", "2026-GOV-OH", "DEM", true);
        let mut on_race = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5000);
        on_race.needle_derived = true;
        let mut elsewhere = intent("en:9:yes:1", NAME, "9", Side::Yes, Action::Buy, 5000);
        elsewhere.needle_derived = true;
        w.intents.extend([on_race, elsewhere]);
        w.dispute(RACE);
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
        let ids: Vec<&str> = d.cancels.iter().map(|c| c.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["en:1:yes:1"], "only the disputed race's orders go");
        assert_eq!(d.cancels[0].reason, "race disputed");
        assert!(d.notes.iter().any(|n| n.contains("disputed")));
        // The dispute clears: trading resumes (the worked exchange still has its live intent).
        w.needle.as_mut().expect("needle").disputed_races.clear();
        w.intents.clear();
        assert_eq!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn stale_feed_skips_buys_but_still_rolls() {
        let mut w = world(0.70, 9_999);
        w.stale_feed(RACE);
        w.book("1", &[(5000, 100)], &[(6150, 100)]); // edge 0.085 ≥ 8 pts: would buy
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
        assert!(d.cancels.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("feed stale")));
        // A held leg whose bid is near q is still rolled: that only reduces exposure.
        w.position("1", 40);
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Sell, 6800, 40)]
        );
        // Once the feed is back, the buy goes through.
        w.position("1", 0);
        w.book("1", &[(5000, 100)], &[(6150, 100)]);
        w.needle.as_mut().expect("needle").stale_feed_races.clear();
        assert_eq!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn skips_worked_exchanges_and_other_parties() {
        let mut w = world(0.90, 9_999);
        w.rule("3", "m1", RACE, "IND", true);
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        w.book("3", &[(5000, 100)], &[(5500, 100)]);
        let mut live = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5000);
        live.needle_derived = true;
        w.intents.push(live);
        assert!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.is_empty());
    }

    /// Gap 11 of the readiness audit: SE from the posterior's margin, by the delta method.
    #[test]
    fn margin_se_follows_the_normal_density() {
        // Toss-up at sd 2: phi(0) × 1 / 2 = 0.1995.
        let toss = margin_se(0.0, 2.0, 1.0).expect("se");
        assert!((toss - 0.398_942_280_4 / 2.0).abs() < 1e-9);
        // Same margin error, called by 3 sd: tiny SE.
        let called = margin_se(6.0, 2.0, 1.0).expect("se");
        assert!(called < 0.01 && called > 0.0);
        // Symmetric in the sign of the margin (either party's side has the same SE).
        assert_eq!(margin_se(-6.0, 2.0, 1.0), Some(called));
        // Twice the assumed model error, twice the SE.
        assert!(
            (margin_se(1.0, 3.0, 2.0).expect("se") - 2.0 * margin_se(1.0, 3.0, 1.0).expect("se")).abs()
                < 1e-12
        );
        // No usable sd or a zero error: fall back.
        assert_eq!(margin_se(1.0, 0.0, 1.0), None);
        assert_eq!(margin_se(1.0, f64::NAN, 1.0), None);
        assert_eq!(margin_se(f64::INFINITY, 2.0, 1.0), None);
        assert_eq!(margin_se(1.0, 2.0, 0.0), None);
    }

    #[test]
    fn threshold_uses_the_margin_when_the_posterior_carries_one() {
        let en = EnExecutor::new(cfg());
        // q = 0.69 from margin 1 at sd 2: a fragile lead (SE 0.176, 3×SE = 0.53) though 99 batches
        // would make the proxy small. No buy can clear that.
        let fragile = RacePosterior {
            margin_mean: 1.0,
            margin_sd: 2.0,
            n_batches: 99,
            ..Default::default()
        };
        let se = en.posterior_se(&fragile, 0.69);
        assert!((se - margin_se(1.0, 2.0, 1.0).expect("se")).abs() < 1e-12);
        assert!(en.threshold(&fragile, 0.69) > 0.5);
        // Margin 8 at sd 2 (q ≈ 1): SE is negligible and the 8-point floor binds.
        let safe = RacePosterior {
            margin_mean: 8.0,
            margin_sd: 2.0,
            n_batches: 3,
            ..Default::default()
        };
        assert!((en.threshold(&safe, 0.99997) - 0.08).abs() < 1e-12);
        // se_margin_err_pts = 0 keeps the batch-count proxy even when the margin is there.
        let proxy = EnExecutor::new(EnCfg {
            se_margin_err_pts: 0.0,
            ..cfg()
        });
        assert!((proxy.posterior_se(&fragile, 0.69) - model_se(0.69, 99)).abs() < 1e-12);
    }

    // ── EXP-006: count-order gate, lock-time rule, contender switch ──────────────────────────────

    fn table() -> Arc<CountOrder> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/count_order.toml");
        Arc::new(CountOrder::load(&path).expect("shipped count-order table"))
    }

    fn gated(cfg: EnCfg) -> EnExecutor {
        EnExecutor::new(cfg).with_count_order(table())
    }

    /// Exchange 1 = DEM, exchange 2 = REP on `race`, one posterior with a margin, clock at `utc`.
    fn race_world(race: &str, utc: &str, margin_mean: f64, margin_sd: f64, reported_frac: f64) -> World {
        let mut w = World::new(100_000.0);
        w.at_utc(utc);
        w.rule("1", "m1", race, "DEM", true);
        w.rule("2", "m1", race, "REP", true);
        w.needle(true, &[]);
        w.posterior(posterior(race, margin_mean, margin_sd, reported_frac));
        w
    }

    fn posterior(race: &str, margin_mean: f64, margin_sd: f64, reported_frac: f64) -> RacePosterior {
        let p_dem = if margin_sd > 0.0 {
            norm_cdf(margin_mean / margin_sd)
        } else {
            0.97
        };
        RacePosterior {
            race_id: race.into(),
            p_dem,
            p_rep: 1.0 - p_dem,
            margin_mean,
            margin_sd,
            reported_frac,
            n_batches: 40,
            ..Default::default()
        }
    }

    fn order_events(d: &Decision) -> Vec<&serde_json::Value> {
        d.events
            .iter()
            .filter(|e| e.kind == ORDER_EVENT)
            .map(|e| &e.body)
            .collect()
    }

    #[test]
    fn band_prob_moves_the_margin_against_the_buy() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((norm_cdf(1.96) - 0.975_002_1).abs() < 1e-6);
        assert!((norm_cdf(-1.0) - 0.158_655_25).abs() < 1e-6);
        // m = 2 ± 2, band 1: a D buy loses Phi(1) − Phi(0.5); an R buy loses Phi(1.5) − Phi(1).
        let dem = band_prob(2.0, 2.0, 1.0, Lean::Dem).expect("dem");
        assert!((dem - (0.841_344_7 - 0.691_462_5)).abs() < 1e-6);
        let rep = band_prob(2.0, 2.0, 1.0, Lean::Rep).expect("rep");
        assert!((rep - (0.933_192_8 - 0.841_344_7)).abs() < 1e-6);
        assert_eq!(band_prob(2.0, 2.0, 0.0, Lean::Dem), Some(0.0));
        assert_eq!(band_prob(2.0, 0.0, 1.0, Lean::Dem), None);
        assert_eq!(band_prob(f64::NAN, 2.0, 1.0, Lean::Dem), None);
        assert_eq!(buy_lean(true, Side::Yes), Lean::Dem);
        assert_eq!(buy_lean(true, Side::No), Lean::Rep);
        assert_eq!(buy_lean(false, Side::Yes), Lean::Rep);
        assert_eq!(buy_lean(false, Side::No), Lean::Dem);
    }

    /// North Carolina, half an hour after close: the first-hour D mirage (+19 ± 5) means the count
    /// still to come favours R. An R buy agrees and needs the threshold; a D buy against it needs
    /// the threshold plus the 5-point band in probability.
    #[test]
    fn gate_charges_the_band_only_against_the_registered_direction() {
        const NC: &str = "2026-SEN-NC";
        const AT: &str = "2026-11-04T01:00:00Z";
        let en = gated(cfg());
        // D +4 ± 2 (q_dem 0.977); the Cup asks 0.70 for the Democrat: 27.7 pts of edge, threshold
        // 3 SE = 0.081, but the band costs Phi(2) − Phi(−0.5) = 0.67.
        let mut w = race_world(NC, AT, 4.0, 2.0, 0.3);
        w.book("1", &[(6500, 100)], &[(7000, 100)]);
        let d = en.decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
        assert!(
            d.notes.iter().any(|n| n.contains("count-order band")),
            "{:?}",
            d.notes
        );
        let view = en.count_order_at(NC, w.now_ns).expect("registered");
        assert_eq!(view.registered.favours(), Some(Lean::Rep));
        assert_eq!(view.profile, "NC");
        // The same buy with the gate switched off, or in a state with no registered count order,
        // or before the state's polls close, trades on the threshold alone.
        let off = gated(EnCfg {
            count_order_enabled: false,
            ..cfg()
        });
        assert_eq!(off.decide(&w.snap()).bundles.len(), 1);
        assert_eq!(
            EnExecutor::new(cfg()).decide(&w.snap()).bundles.len(),
            1,
            "no table attached"
        );
        let mut fl = race_world("2026-SEN-FL-S", AT, 4.0, 2.0, 0.3);
        fl.book("1", &[(6500, 100)], &[(7000, 100)]);
        assert_eq!(en.decide(&fl.snap()).bundles.len(), 1, "FL is not registered");
        let mut early = race_world(NC, "2026-11-04T00:00:00Z", 4.0, 2.0, 0.3);
        early.book("1", &[(6500, 100)], &[(7000, 100)]);
        assert!(en.count_order_at(NC, early.now_ns).is_none());
        assert_eq!(en.decide(&early.snap()).bundles.len(), 1, "before poll close");

        // R +4 ± 2: buying the Republican (YES on exchange 2) agrees with the registered direction.
        let mut w = race_world(NC, AT, -4.0, 2.0, 0.3);
        w.book("2", &[(6500, 100)], &[(7000, 100)]);
        let d = en.decide(&w.snap());
        assert_eq!(legs(&d).len(), 1, "{:?}", d.notes);
        assert_eq!((legs(&d)[0].1, legs(&d)[0].2), (Side::Yes, Action::Buy));
        let ev = order_events(&d);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["kind"], "buy");
        assert_eq!(ev[0]["path"], "needle");
        assert_eq!(ev[0]["lean"], "REP");
        assert_eq!(ev[0]["agrees"], true);
        assert_eq!(ev[0]["band_prob"], 0.0);
        assert_eq!(ev[0]["count_order"]["favours"], "REP");
        assert_eq!(ev[0]["count_order"]["profile"], "NC");
        assert_eq!(ev[0]["count_order"]["drift_pts"], 19.0);
        assert_eq!(ev[0]["count_order"]["band_pts"], 5.0);
    }

    /// A buy against the registered direction that clears the band too goes through, priced at the
    /// higher bar; one with no margin sd to price the band is refused.
    #[test]
    fn against_the_direction_buys_only_past_the_band() {
        const NV: &str = "2026-GOV-NV";
        // NV at +6 h: drift −2.65 ± 0.9, the count to come favours D. R +8 ± 2 (q_rep 0.99997):
        // band_prob = Phi(−3.55) − Phi(−4) ≈ 0.0002, so the REP buy at 0.55 clears it.
        let en = gated(cfg());
        let mut w = race_world(NV, "2026-11-04T09:00:00Z", -8.0, 2.0, 0.6);
        w.book("2", &[(5000, 100)], &[(5500, 100)]);
        let d = en.decide(&w.snap());
        assert_eq!(legs(&d).len(), 1, "{:?}", d.notes);
        let ev = order_events(&d);
        assert_eq!(ev[0]["agrees"], false);
        let extra = ev[0]["band_prob"].as_f64().expect("band_prob");
        assert!(extra > 0.0 && extra < 0.001, "{extra}");
        assert!(
            (ev[0]["required"].as_f64().expect("required") - ev[0]["threshold"].as_f64().expect("t") - extra)
                .abs()
                < 1e-12
        );
        // No margin sd: the agreeing D buy still goes (no band to price), the gated R buy does not.
        let mut w = race_world(NV, "2026-11-04T09:00:00Z", 0.0, 0.0, 0.6);
        w.posterior(RacePosterior {
            race_id: NV.into(),
            p_dem: 0.03,
            p_rep: 0.97,
            n_batches: 9_999,
            reported_frac: 0.6,
            ..Default::default()
        });
        w.book("2", &[(5000, 100)], &[(5500, 100)]);
        let d = en.decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(
            d.notes.iter().any(|n| n.contains("no margin sd")),
            "{:?}",
            d.notes
        );
        w.posterior(RacePosterior {
            race_id: NV.into(),
            p_dem: 0.97,
            p_rep: 0.03,
            n_batches: 9_999,
            reported_frac: 0.6,
            ..Default::default()
        });
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        // YES on the DEM leg and NO on the REP leg: both lean D, both agree, neither needs a band.
        let d = en.decide(&w.snap());
        assert_eq!(d.bundles.len(), 2, "{:?}", d.notes);
        assert!(
            order_events(&d)
                .iter()
                .all(|e| e["lean"] == "DEM" && e["agrees"] == true)
        );
    }

    /// WA-03 is registered with no direction (0 ± 5): every buy pays the band, either way.
    #[test]
    fn no_registered_direction_charges_the_band_both_ways() {
        const WA3: &str = "2026-HOUSE-WA-03";
        let en = gated(cfg());
        for (margin, ex) in [(4.0, "1"), (-4.0, "2")] {
            let mut w = race_world(WA3, "2026-11-04T10:00:00Z", margin, 2.0, 0.6);
            w.book(ex, &[(6500, 100)], &[(7000, 100)]);
            let d = en.decide(&w.snap());
            assert!(d.bundles.is_empty(), "margin {margin}: {:?}", legs(&d));
            assert!(
                d.notes.iter().any(|n| n.contains("favours neither")),
                "{:?}",
                d.notes
            );
        }
    }

    /// Rolls reduce exposure and are never gated, whatever the playbook says.
    #[test]
    fn rolls_are_never_gated() {
        // NC first hour favours R; we hold the Democrat and its bid is at q.
        let mut w = race_world("2026-SEN-NC", "2026-11-04T01:00:00Z", 4.0, 2.0, 0.3);
        w.position("1", 40);
        w.book("1", &[(9700, 100)], &[(9800, 100)]);
        let d = gated(cfg()).decide(&w.snap());
        let l = legs(&d);
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].1, l[0].2, l[0].4), (Side::Yes, Action::Sell, 40));
        let ev = order_events(&d);
        assert_eq!(ev[0]["kind"], "roll");
        assert_eq!(ev[0]["count_order"]["favours"], "REP");
    }

    /// Nevada near the lock: 60% counted, the Democrat trailing on the print but projected D +3 ± 1.2
    /// (q 0.994) by the needle, which then went quiet for 90 minutes. The Cup asks 0.40.
    fn nevada_lock_world(age_s: i64, reported_frac: f64) -> World {
        let mut w = race_world("2026-GOV-NV", "2026-11-04T15:00:00Z", 3.0, 1.2, reported_frac);
        let post = posterior("2026-GOV-NV", 3.0, 1.2, reported_frac);
        w.quiet(post, age_s);
        w.book("1", &[(3500, 500)], &[(4000, 500)]);
        w
    }

    #[test]
    fn lock_rule_buys_the_favoured_side_on_a_quiet_posterior() {
        let en = gated(cfg());
        let w = nevada_lock_world(5_400, 0.6);
        assert!(
            w.needle.as_ref().expect("needle").posteriors.is_empty(),
            "nothing fresh"
        );
        let d = en.decide(&w.snap());
        let l = legs(&d);
        assert_eq!(l.len(), 1, "{:?}", d.notes);
        assert_eq!((l[0].0.as_str(), l[0].1, l[0].2), ("1", Side::Yes, Action::Buy));
        let ev = order_events(&d);
        assert_eq!(ev[0]["path"], "lock_rule");
        assert_eq!(ev[0]["lean"], "DEM");
        assert_eq!(ev[0]["posterior_age_s"], 5_400.0);
        assert_eq!(ev[0]["band_prob"], 0.0);
        assert_eq!(ev[0]["count_order"]["profile"], "NV");
        assert!(d.bundles[0].legs[0].needle_derived);
        // Sized like any other buy: 2% of equity at the limit, within the depth.
        let limit = f64::from(l[0].3) / 10_000.0;
        assert_eq!(
            l[0].4,
            shares_for_budget(2_000.0, Pips::from_f64(limit).expect("pips")).min(500)
        );

        // Switched off: the quiet posterior is not traded.
        let off = gated(EnCfg {
            lock_rule_enabled: false,
            ..cfg()
        });
        assert!(off.decide(&w.snap()).bundles.is_empty());
        // Without the table the rule is inert too.
        assert!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.is_empty());
    }

    #[test]
    fn lock_rule_respects_its_window_age_count_and_state_bars() {
        let en = gated(cfg());
        // Before the window opens (14:00Z).
        let mut w = nevada_lock_world(5_400, 0.6);
        w.at_utc("2026-11-04T13:30:00Z");
        w.quiet(posterior("2026-GOV-NV", 3.0, 1.2, 0.6), 5_400);
        assert!(en.decide(&w.snap()).bundles.is_empty());
        // Posterior older than lock_posterior_max_age_s (3 h).
        let d = en.decide(&nevada_lock_world(11_000, 0.6).snap());
        assert!(d.bundles.is_empty());
        assert!(
            d.notes.iter().any(|n| n.contains("over 10800 s")),
            "{:?}",
            d.notes
        );
        // Counted past lock_max_reported_frac.
        let d = en.decide(&nevada_lock_world(5_400, 0.9).snap());
        assert!(d.bundles.is_empty());
        assert!(
            d.notes.iter().any(|n| n.contains("not below 85%")),
            "{:?}",
            d.notes
        );
        // Low-drift states (TX lock 0 ± 0.3), no-direction overrides (WA-03 0 ± 5) and states
        // whose band is not smaller than the drift are not eligible.
        for race in [
            "2026-GOV-TX",
            "2026-HOUSE-WA-03",
            "2026-GOV-AZ",
            "2026-HOUSE-ME-02",
        ] {
            let mut w = race_world(race, "2026-11-04T15:00:00Z", 3.0, 1.2, 0.6);
            w.quiet(posterior(race, 3.0, 1.2, 0.6), 5_400);
            w.book("1", &[(3500, 500)], &[(4000, 500)]);
            let d = en.decide(&w.snap());
            assert!(d.bundles.is_empty(), "{race}: {:?}", legs(&d));
            assert!(
                d.notes.iter().any(|n| n.contains("lock rule not eligible")),
                "{race}"
            );
        }
        // CA House seats and PA are eligible (−3 ± 1.5, −1.5 ± 1.0).
        for race in ["2026-HOUSE-CA-22", "2026-GOV-PA"] {
            let mut w = race_world(race, "2026-11-04T15:00:00Z", 3.0, 1.2, 0.6);
            w.quiet(posterior(race, 3.0, 1.2, 0.6), 5_400);
            w.book("1", &[(3500, 500)], &[(4000, 500)]);
            assert_eq!(en.decide(&w.snap()).bundles.len(), 1, "{race}");
        }
        // Only the favoured side: an R projection with the Republican cheap is not a lock-rule buy.
        let mut w = race_world("2026-GOV-NV", "2026-11-04T15:00:00Z", -3.0, 1.2, 0.6);
        w.quiet(posterior("2026-GOV-NV", -3.0, 1.2, 0.6), 5_400);
        w.book("2", &[(3500, 500)], &[(4000, 500)]);
        assert!(en.decide(&w.snap()).bundles.is_empty());
    }

    #[test]
    fn lock_rule_skips_disputed_stale_and_unhealthy_and_never_rolls() {
        let en = gated(cfg());
        let mut w = nevada_lock_world(5_400, 0.6);
        let mut live = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 4000);
        live.needle_derived = true;
        w.intents.push(live);
        w.dispute("2026-GOV-NV");
        let d = en.decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 1, "a disputed race's resting order goes");
        w.intents.clear();
        w.needle.as_mut().expect("needle").disputed_races.clear();
        w.stale_feed("2026-GOV-NV");
        assert!(en.decide(&w.snap()).bundles.is_empty());
        w.needle.as_mut().expect("needle").stale_feed_races.clear();
        w.needle.as_mut().expect("needle").healthy = false;
        assert!(en.decide(&w.snap()).bundles.is_empty());
        // A held leg whose bid sits at q is not rolled on a quiet posterior.
        let mut w = nevada_lock_world(5_400, 0.6);
        w.position("1", 40);
        w.book("1", &[(9900, 500)], &[(9950, 500)]);
        let d = en.decide(&w.snap());
        assert!(legs(&d).iter().all(|l| l.2 != Action::Sell), "{:?}", legs(&d));
    }

    #[test]
    fn contender_switch_stops_new_positions_but_keeps_rolling() {
        let switched = |multiple: f64| {
            gated(EnCfg {
                contender_hedge_multiple: multiple,
                ..cfg()
            })
        };
        let mut w = world(0.90, 9_999);
        w.book("1", &[(5000, 100)], &[(5500, 100)]); // 35 pts of edge
        w.equity = 250_000.0; // 2.5x
        let d = switched(2.0).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("rolls only")));
        assert_eq!(
            switched(3.0).decide(&w.snap()).bundles.len(),
            1,
            "below the multiple"
        );
        assert_eq!(switched(0.0).decide(&w.snap()).bundles.len(), 1, "0 = off");
        // Rolls still happen above the multiple.
        let mut w = world(0.70, 9_999);
        w.equity = 250_000.0;
        w.position("1", 40);
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        assert_eq!(
            legs(&switched(2.0).decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Sell, 6800, 40)]
        );
        // And the lock rule opens nothing either.
        let mut w = nevada_lock_world(5_400, 0.6);
        w.equity = 250_000.0;
        assert!(switched(2.0).decide(&w.snap()).bundles.is_empty());
    }
}
