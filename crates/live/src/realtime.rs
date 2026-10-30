//! The realtime feed as the engine sees it: tournament-scope channels plus a capped, prioritised
//! set of per-market channels, spread over one or more connections.
//!
//! Market updates arrive on `tournament:{tid}:market:{market_id}` and Supabase caps the channels
//! one connection may join (commonly 100), so each connection carries at most `cap` market
//! channels and the feed opens `connections` of them: the needle's 71 races alone span 145
//! markets. We join the markets that matter most ([`prioritised_markets`]): the first connection
//! takes the first `cap` (and the tournament-scope channels), the next connection the next `cap`,
//! and the rest stay on REST polling, exactly as when realtime is down. The client cannot change
//! its subscriptions on a live socket, so when a connection's market set changes it is rebuilt,
//! at most once per [`REBUILD_EVERY_NS`] per connection; the new session starts with the usual
//! `Connected` + `Resync`, so nothing is trusted across the swap. At most one connection is
//! (re)built per [`RealtimeFeed::ensure`] call, so the joins of several connections never land in
//! one burst.
//!
//! `Connected` is passed on only once every open connection has connected, and `Disconnected`
//! whenever any one drops, so the engine's realtime health is the worst of them;
//! [`RealtimeFeed::live_markets`] lists just the markets on connected sessions.

use std::collections::BTreeSet;

use susq_core::{MarketId, TournamentId};
use susq_platform::realtime::{RealtimeEvent, market_channel};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Shortest time between two rebuilds of one connection for a changed market set.
pub const REBUILD_EVERY_NS: i64 = 600 * 1_000_000_000;

/// Starts a realtime client on the given channels; the token stops it.
pub type Spawner =
    Box<dyn Fn(Vec<String>) -> (mpsc::Receiver<RealtimeEvent>, CancellationToken) + Send + Sync>;

/// One live client session.
struct Session {
    rx: mpsc::Receiver<RealtimeEvent>,
    cancel: Option<CancellationToken>,
    /// Markets with a channel on this session, sorted.
    markets: Vec<MarketId>,
    spawned_ns: i64,
    connected: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
    }
}

/// The engine's realtime feed.
pub struct RealtimeFeed {
    tid: TournamentId,
    /// Channels joined whatever the market set (violations, account, leaderboard), on the first
    /// connection only.
    base: Vec<String>,
    /// Market channels per connection.
    cap: usize,
    /// `None` for a fixed receiver (tests): the market set is never managed.
    spawn: Option<Spawner>,
    /// One slot per connection; `None` until it is first needed.
    sessions: Vec<Option<Session>>,
    /// Markets of every session, sorted.
    markets: Vec<MarketId>,
    /// Session `try_recv` reads first, so no connection starves the others.
    next: usize,
}

impl RealtimeFeed {
    /// A managed feed over `connections` (at least one) connections of up to `cap` market
    /// channels each: nothing is connected until the first [`RealtimeFeed::ensure`].
    pub fn new(tid: TournamentId, base: Vec<String>, cap: usize, connections: usize, spawn: Spawner) -> Self {
        Self {
            tid,
            base,
            cap,
            spawn: Some(spawn),
            sessions: (0..connections.max(1)).map(|_| None).collect(),
            markets: Vec::new(),
            next: 0,
        }
    }

    /// A feed over an existing receiver whose subscriptions are fixed (tests).
    pub fn fixed(rx: mpsc::Receiver<RealtimeEvent>) -> Self {
        Self {
            tid: TournamentId::default(),
            base: Vec::new(),
            cap: 0,
            spawn: None,
            sessions: vec![Some(Session {
                rx,
                cancel: None,
                markets: Vec::new(),
                spawned_ns: 0,
                connected: false,
            })],
            markets: Vec::new(),
            next: 0,
        }
    }

    /// Next buffered event, if any, from any connection.
    pub fn try_recv(&mut self) -> Option<RealtimeEvent> {
        let n = self.sessions.len();
        for k in 0..n {
            let i = (self.next + k) % n;
            while let Some(ev) = self.sessions[i].as_mut().and_then(|s| s.rx.try_recv().ok()) {
                if let Some(ev) = self.observe(i, ev) {
                    self.next = (i + 1) % n;
                    return Some(ev);
                }
            }
        }
        None
    }

    /// Track connection state; swallow a `Connected` while another open session is still down.
    fn observe(&mut self, i: usize, ev: RealtimeEvent) -> Option<RealtimeEvent> {
        match ev {
            RealtimeEvent::Connected => {
                if let Some(s) = self.sessions[i].as_mut() {
                    s.connected = true;
                }
                self.sessions
                    .iter()
                    .flatten()
                    .all(|s| s.connected)
                    .then_some(RealtimeEvent::Connected)
            }
            RealtimeEvent::Disconnected { .. } => {
                if let Some(s) = self.sessions[i].as_mut() {
                    s.connected = false;
                }
                Some(ev)
            }
            other => Some(other),
        }
    }

    /// Markets with a per-market channel on any session (sorted).
    pub fn markets(&self) -> &[MarketId] {
        &self.markets
    }

    /// Markets with a per-market channel on a session that is connected right now.
    pub fn live_markets(&self) -> impl Iterator<Item = &MarketId> {
        self.sessions
            .iter()
            .flatten()
            .filter(|s| s.connected)
            .flat_map(|s| s.markets.iter())
    }

    /// Per-market channel slots over all connections.
    pub fn cap(&self) -> usize {
        self.cap * self.sessions.len()
    }

    /// Make the sessions cover `wanted` (candidates in priority order; see
    /// [`prioritised_markets`]): connection `k` carries the `k`-th run of `cap` markets. Builds
    /// at most one connection per call: a missing one at once, a changed one no sooner than
    /// [`REBUILD_EVERY_NS`] after its last build; a connection left with no markets is closed.
    /// Returns the channel count when a session was (re)built or closed (0).
    pub fn ensure(&mut self, wanted: impl IntoIterator<Item = MarketId>, now_ns: i64) -> Option<usize> {
        let spawn = self.spawn.as_ref()?;
        let all = prioritised_markets(self.cap(), wanted);
        let mut built = None;
        for k in 0..self.sessions.len() {
            let mut want: Vec<MarketId> = all.iter().skip(k * self.cap).take(self.cap).cloned().collect();
            want.sort();
            let due = match &self.sessions[k] {
                None => k == 0 || !want.is_empty(),
                Some(s) => s.markets != want && now_ns.saturating_sub(s.spawned_ns) >= REBUILD_EVERY_NS,
            };
            if !due {
                continue;
            }
            if k > 0 && want.is_empty() {
                self.sessions[k] = None;
                built = Some(0);
                break;
            }
            let base = if k == 0 { self.base.as_slice() } else { &[] };
            let channels: Vec<String> = base
                .iter()
                .cloned()
                .chain(want.iter().map(|m| market_channel(&self.tid, m)))
                .collect();
            let n = channels.len();
            let (rx, cancel) = spawn(channels);
            // Replacing the slot drops (and so cancels) the old session.
            self.sessions[k] = Some(Session {
                rx,
                cancel: Some(cancel),
                markets: want,
                spawned_ns: now_ns,
                connected: false,
            });
            built = Some(n);
            break;
        }
        if built.is_some() {
            let mut markets: Vec<MarketId> = self
                .sessions
                .iter()
                .flatten()
                .flat_map(|s| s.markets.iter().cloned())
                .collect();
            markets.sort();
            self.markets = markets;
        }
        built
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
            1,
            spawn,
        )
    }

    /// A two-connection feed whose sessions' senders are kept, so tests can play events.
    fn two_connections(
        cap: usize,
        seen: Arc<parking_lot::Mutex<Vec<Vec<String>>>>,
        senders: Arc<parking_lot::Mutex<Vec<mpsc::Sender<RealtimeEvent>>>>,
    ) -> RealtimeFeed {
        let spawn: Spawner = Box::new(move |channels| {
            seen.lock().push(channels);
            let (tx, rx) = mpsc::channel(8);
            senders.lock().push(tx);
            (rx, CancellationToken::new())
        });
        RealtimeFeed::new(
            TournamentId::new("t1"),
            vec!["leaderboard:all".into()],
            cap,
            2,
            spawn,
        )
    }

    #[test]
    fn a_second_connection_takes_the_next_markets_one_build_per_call() {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut f = two_connections(2, seen.clone(), Arc::default());
        assert_eq!(f.cap(), 4);
        let wanted = || [m("e"), m("a"), m("d"), m("b"), m("c")];
        assert_eq!(
            f.ensure(wanted(), 0),
            Some(3),
            "the first connection carries the base channels"
        );
        assert_eq!(f.markets(), &[m("a"), m("e")]);
        assert_eq!(
            f.ensure(wanted(), 1),
            Some(2),
            "the second connection follows on the next call"
        );
        assert_eq!(f.markets(), &[m("a"), m("b"), m("d"), m("e")]);
        assert_eq!(
            seen.lock()[1],
            vec![
                "tournament:t1:market:b".to_owned(),
                "tournament:t1:market:d".to_owned()
            ],
            "only the first connection joins the tournament-scope channels"
        );
        assert_eq!(f.ensure(wanted(), 2), None);
        // Fewer markets than one connection holds: the first is rebuilt, then the second closed.
        assert_eq!(f.ensure([m("a")], REBUILD_EVERY_NS), Some(2));
        assert_eq!(f.ensure([m("a")], REBUILD_EVERY_NS + 1), Some(0));
        assert_eq!(f.markets(), &[m("a")]);
        assert_eq!(f.ensure([m("a")], REBUILD_EVERY_NS + 2), None);
        assert_eq!(seen.lock().len(), 3);
    }

    #[test]
    fn health_is_the_worst_connection_and_only_connected_markets_are_live() {
        let senders = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut f = two_connections(1, Arc::default(), senders.clone());
        f.ensure([m("a"), m("b")], 0);
        f.ensure([m("a"), m("b")], 1);
        let send = |i: usize, ev: RealtimeEvent| senders.lock()[i].try_send(ev).expect("send");
        let live = |f: &RealtimeFeed| f.live_markets().cloned().collect::<Vec<_>>();
        assert!(live(&f).is_empty());

        send(0, RealtimeEvent::Connected);
        assert_eq!(f.try_recv(), None, "one connection up is not yet `Connected`");
        assert_eq!(live(&f), vec![m("a")]);

        send(1, RealtimeEvent::Connected);
        assert_eq!(f.try_recv(), Some(RealtimeEvent::Connected));
        assert_eq!(live(&f), vec![m("a"), m("b")]);

        let lost = RealtimeEvent::Disconnected { reason: "eof".into() };
        send(1, lost.clone());
        assert_eq!(f.try_recv(), Some(lost), "any connection dropping is reported");
        assert_eq!(live(&f), vec![m("a")]);
        assert_eq!(f.try_recv(), None);
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
        let mut f = RealtimeFeed::new(TournamentId::new("t1"), vec![], 10, 1, spawn);
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
