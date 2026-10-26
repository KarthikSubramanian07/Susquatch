//! True cash (EXP-015): account value minus holdings value from the portfolio P&L read.
//!
//! This is the figure the bridge's risk sizes against (`gateway.portfolio`). The tournament's
//! `myBalance` lagged it by 6 to 10k in October, so it is only a fallback: for the world's cash
//! before the first portfolio read, and for the paper seed when that read fails.

use susq_platform::{Pnl, Tournament};

use crate::world::World;

/// Cash from a P&L read, if the read is usable: a positive account value and a holdings value
/// that fits inside it.
pub fn from_pnl(p: &Pnl) -> Option<f64> {
    let (value, holdings) = (p.total_account_value, p.total_holdings_value);
    (value.is_finite() && holdings.is_finite() && value > 0.0 && (0.0..=value).contains(&holdings))
        .then_some(value - holdings)
}

/// Starting cash for a paper run: the live account's cash, else `myBalance`, else the initial
/// balance.
pub fn paper_seed(t: &Tournament, pnl: Option<&Pnl>) -> f64 {
    if let Some(cash) = pnl.and_then(from_pnl) {
        return cash;
    }
    if t.my_balance.is_finite() && t.my_balance > 0.0 {
        t.my_balance
    } else {
        t.initial_balance
    }
}

/// Take the world's cash from a portfolio read. Returns whether it did.
pub fn observe_pnl(world: &mut World, pnl: &Pnl) -> bool {
    match from_pnl(pnl) {
        Some(cash) => {
            world.cash = cash;
            world.cash_from_portfolio = true;
            true
        }
        None => false,
    }
}

/// Fall back to `myBalance`, only while no portfolio read has set the cash (a failed read later
/// keeps the last true figure rather than reverting to the stale balance).
pub fn observe_balance(world: &mut World, my_balance: f64) {
    if !world.cash_from_portfolio {
        world.cash = my_balance;
    }
    if world.equity <= 0.0 {
        world.equity = my_balance;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pnl(value: f64, holdings: f64) -> Pnl {
        Pnl {
            total_account_value: value,
            total_holdings_value: holdings,
            ..Pnl::default()
        }
    }

    fn tournament(my_balance: f64) -> Tournament {
        Tournament {
            my_balance,
            initial_balance: 100_000.0,
            ..Tournament::default()
        }
    }

    #[test]
    fn cash_is_account_value_minus_holdings() {
        let cash = from_pnl(&pnl(98_442.18, 81_137.19)).expect("cash");
        assert!((cash - 17_304.99).abs() < 1e-6, "{cash}");
        assert_eq!(from_pnl(&pnl(0.0, 0.0)), None);
        assert_eq!(from_pnl(&pnl(100.0, 120.0)), None);
        assert_eq!(from_pnl(&pnl(f64::NAN, 1.0)), None);
    }

    #[test]
    fn paper_seed_prefers_the_portfolio_read() {
        let t = tournament(27_254.41);
        let seed = paper_seed(&t, Some(&pnl(98_442.18, 81_137.19)));
        assert!((seed - 17_304.99).abs() < 1e-6, "{seed}");
        assert_eq!(paper_seed(&t, None), 27_254.41);
        assert_eq!(paper_seed(&t, Some(&pnl(0.0, 0.0))), 27_254.41);
        assert_eq!(paper_seed(&tournament(0.0), None), 100_000.0);
    }

    #[test]
    fn world_cash_falls_back_to_my_balance_only_before_the_first_read() {
        let mut w = World::default();
        observe_balance(&mut w, 27_254.41);
        assert_eq!((w.cash, w.equity), (27_254.41, 27_254.41));
        assert!(observe_pnl(&mut w, &pnl(98_442.18, 81_137.19)));
        assert!((w.cash - 17_304.99).abs() < 1e-6);
        // A later unusable read or a balance read does not revert to the stale figure.
        assert!(!observe_pnl(&mut w, &pnl(0.0, 0.0)));
        observe_balance(&mut w, 27_254.41);
        assert!((w.cash - 17_304.99).abs() < 1e-6);
        assert!(observe_pnl(&mut w, &pnl(98_000.0, 77_000.0)));
        assert!((w.cash - 21_000.0).abs() < 1e-6);
    }
}
