//! The realtime feed as the engine sees it: tournament-scope channels plus a capped, prioritised
//! set of per-market channels.
//!
//! Market updates arrive on `tournament:{tid}:market:{market_id}` and Supabase caps the channels
//! one connection may join (commonly 100), so with 237 markets we join the markets that matter
//! most ([`prioritised_markets`]) and leave the rest to REST polling, exactly as when realtime is
//! down. The client cannot change its subscriptions on a live socket, so when the wanted set
//! changes the session is rebuilt, at most once per [`REBUILD_EVERY_NS`]; the new session starts
//! with the usual `Connected` + `Resync`, so nothing is trusted across the swap.

use std::collections::BTreeSet;

use susq_core::{MarketId, TournamentId};
use susq_platform::realtime::{RealtimeEvent, market_channel};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Shortest time between two session rebuilds for a changed market set.
pub const REBUILD_EVERY_NS: i64 = 600 * 1_000_000_000;

/// Starts a realtime client on the given channels; the token stops it.
pub type Spawner =
    Box<dyn Fn(Vec<String>) -> (mpsc::Receiver<RealtimeEvent>, CancellationToken) + Send + Sync>;

/// One live client session.
struct Session {
    rx: mpsc::Receiver<RealtimeEvent>,
    cancel: Option<CancellationToken>,
}

/// The engine's realtime feed.
pub struct RealtimeFeed {
    tid: TournamentId,
    /// Channels joined whatever the market set (violations, account, leaderboard).
    base: Vec<String>,
    cap: usize,
    /// `None` for a fixed receiver (tests): the market set is never managed.
    spawn: Option<Spawner>,
    session: Option<Session>,
    /// Markets of the current session, sorted.
    markets: Vec<MarketId>,
    last_spawn_ns: Option<i64>,
}

impl RealtimeFeed {
    /// A managed feed: nothing is connected until the first [`RealtimeFeed::ensure`].
    pub fn new(tid: TournamentId, base: Vec<String>, cap: usize, spawn: Spawner) -> Self {
        Self {
            tid,
            base,
            cap,
            spawn: Some(spawn),
            session: None,
            markets: Vec::new(),
            last_spawn_ns: None,
        }
    }

    /// A feed over an existing receiver whose subscriptions are fixed (tests).
    pub fn fixed(rx: mpsc::Receiver<RealtimeEvent>) -> Self {
        Self {
            tid: TournamentId::default(),
            base: Vec::new(),
            cap: 0,
            spawn: None,
            session: Some(Session { rx, cancel: None }),
            markets: Vec::new(),
            last_spawn_ns: None,
        }
    }

    /// Next buffered event, if any.
    pub fn try_recv(&mut self) -> Option<RealtimeEvent> {
        self.session.as_mut()?.rx.try_recv().ok()
    }

    /// Markets with a per-market channel in the current session (sorted).
    pub fn markets(&self) -> &[MarketId] {
        &self.markets
    }

    /// Per-market channel cap.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Whether a rebuild for a changed set is allowed at `now_ns`.
    fn may_rebuild(&self, now_ns: i64) -> bool {
        self.last_spawn_ns
            .is_none_or(|t| now_ns.saturating_sub(t) >= REBUILD_EVERY_NS)
    }

    /// Make the session cover `wanted` (candidates in priority order; see
    /// [`prioritised_markets`]). Spawns the first session at once and rebuilds a changed one at
    /// most every [`REBUILD_EVERY_NS`]. Returns the channel count when a session was (re)built.
    pub fn ensure(&mut self, wanted: impl IntoIterator<Item = MarketId>, now_ns: i64) -> Option<usize> {
        let spawn = self.spawn.as_ref()?;
        let mut markets = prioritised_markets(self.cap, wanted);
        markets.sort();
        if self.session.is_some() && (markets == self.markets || !self.may_rebuild(now_ns)) {
            return None;
        }
        let channels: Vec<String> = self
            .base
            .iter()
            .cloned()
            .chain(markets.iter().map(|m| market_channel(&self.tid, m)))
            .collect();
        let n = channels.len();
        let (rx, cancel) = spawn(channels);
        if let Some(old) = self.session.replace(Session {
            rx,
            cancel: Some(cancel),
        }) {
            if let Some(c) = old.cancel {
                c.cancel();
            }
        }
        self.markets = markets;
        self.last_spawn_ns = Some(now_ns);
        Some(n)
    }
}

impl Drop for RealtimeFeed {
    fn drop(&mut self) {
        if let Some(c) = self.session.take().and_then(|s| s.cancel) {
            c.cancel();
        }
    }
}

/// The first `cap` distinct markets of `candidates`, which the caller lists most important
/// first: markets named in the current targets file, then markets we hold, are violated on or
/// have fair values for, then the ones whose books moved most recently.
pub fn prioritised_markets(cap: usize, candidates: impl IntoIterator<Item = MarketId>) -> Vec<MarketId> {
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|m| !m.as_str().is_empty() && seen.insert(m.clone()))
        .take(cap)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    fn m(s: &str) -> MarketId {
        MarketId::new(s)
    }

    fn feed(
        cap: usize,
        spawns: Arc<AtomicUsize>,
        seen: Arc<parking_lot::Mutex<Vec<Vec<String>>>>,
    ) -> RealtimeFeed {
        let spawn: Spawner = Box::new(move |channels| {
            spawns.fetch_add(1, Ordering::SeqCst);
            seen.lock().push(channels);
            let (_tx, rx) = mpsc::channel(1);
            (rx, CancellationToken::new())
        });
        RealtimeFeed::new(
            TournamentId::new("t1"),
            vec!["leaderboard:all".into()],
            cap,
            spawn,
        )
    }

    #[test]
    fn prioritised_markets_dedups_in_order_and_caps() {
        let got = prioritised_markets(3, [m("b"), m("a"), m("b"), m(""), m("c"), m("d")]);
        assert_eq!(got, vec![m("b"), m("a"), m("c")]);
        assert!(prioritised_markets(0, [m("a")]).is_empty());
    }

    #[test]
    fn first_session_is_immediate_and_rebuilds_are_rate_limited() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut f = feed(2, spawns.clone(), seen.clone());
        assert_eq!(f.ensure([m("x"), m("y"), m("z")], 0), Some(3));
        assert_eq!(f.markets(), &[m("x"), m("y")]);
        assert_eq!(
            seen.lock()[0],
            vec![
                "leaderboard:all".to_owned(),
                "tournament:t1:market:x".to_owned(),
                "tournament:t1:market:y".to_owned()
            ]
        );
        // Same set (any order): nothing happens.
        assert_eq!(f.ensure([m("y"), m("x")], 1), None);
        // Changed set, too soon: wait.
        assert_eq!(f.ensure([m("a")], REBUILD_EVERY_NS - 1), None);
        assert_eq!(f.markets(), &[m("x"), m("y")]);
        // Changed set, window elapsed: rebuilt.
        assert_eq!(f.ensure([m("a")], REBUILD_EVERY_NS), Some(2));
        assert_eq!(f.markets(), &[m("a")]);
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_rebuild_cancels_the_previous_session() {
        let token = CancellationToken::new();
        let first = token.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let spawn: Spawner = Box::new(move |_| {
            let (_tx, rx) = mpsc::channel(1);
            let n = c.fetch_add(1, Ordering::SeqCst);
            let t = if n == 0 {
                first.clone()
            } else {
                CancellationToken::new()
            };
            (rx, t)
        });
        let mut f = RealtimeFeed::new(TournamentId::new("t1"), vec![], 10, spawn);
        f.ensure([m("a")], 0);
        assert!(!token.is_cancelled());
        f.ensure([m("b")], REBUILD_EVERY_NS);
        assert!(token.is_cancelled(), "the old session is stopped");
    }

    #[test]
    fn a_fixed_feed_never_manages_subscriptions() {
        let (tx, rx) = mpsc::channel(4);
        let mut f = RealtimeFeed::fixed(rx);
        assert_eq!(f.ensure([m("a")], 0), None);
        tx.try_send(RealtimeEvent::Connected).expect("send");
        assert_eq!(f.try_recv(), Some(RealtimeEvent::Connected));
        assert_eq!(f.try_recv(), None);
    }
}
