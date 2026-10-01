//! Supabase Realtime client for the platform's private broadcast channels.
//!
//! Speaks the Phoenix channel protocol (JSON serializer `vsn=1.0.0`) over a websocket, joins every
//! configured channel as a private channel authorized with the token from `POST /realtime/token`,
//! and turns broadcasts into typed [`RealtimeEvent`]s. Designed to run unattended for weeks:
//!
//! * heartbeats every [`RealtimeConfig::heartbeat`]; an unanswered heartbeat means the socket is
//!   dead and the client reconnects;
//! * reconnects with exponential backoff and jitter, capped at [`RealtimeConfig::max_backoff`] and
//!   reset after [`RealtimeConfig::stable_after`] of healthy connection;
//! * rejoins a channel the server errored or closed, with its own backoff;
//! * refreshes the token [`RealtimeConfig::refresh_before`] its expiry (3 h tokens) and pushes it
//!   to every joined channel; if minting fails it keeps the socket and retries, and only
//!   reconnects once the token has actually expired;
//! * tracks `delivery.revision` per channel.
//!
//! Realtime is a best-effort hint, never the source of truth. Whenever the docs require an
//! authoritative REST resync (initial subscription, reconnect, token refresh, channel error,
//! revision gap) the client emits [`RealtimeEvent::Resync`] *before* any further data. The
//! consumer should refetch from REST on every `Resync`.

mod backoff;
mod client;
mod lenient;
mod outbox;
mod payload;
mod protocol;
mod revision;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use susq_core::{MarketId, TournamentId};

pub use client::RealtimeClient;
pub use payload::{
    AccountBatch, BookDirty, Delivery, LeaderboardTick, MarketBatch, MarketSettled, OrderUpdate, RtBook,
    RtFill, RtLevel, RtRefund, RtSettlement, RtTrade,
};
pub use protocol::{PROTOCOL_VSN, TOPIC_PREFIX, topic, websocket_url};

use crate::{
    adapter::PlatformAdapter,
    error::PlatformResult,
    models::{ConstraintEval, RealtimeToken},
};

/// Realtime client settings. Durations are wall time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeConfig {
    /// Channel names without the `realtime:` prefix, e.g. `tournament:{tid}`, `user:{profile}`.
    pub channels: Vec<String>,
    /// Heartbeat period. A heartbeat still unanswered when the next is due kills the socket.
    pub heartbeat: Duration,
    /// Refresh the token this long before it expires (at the latest at half its remaining life).
    pub refresh_before: Duration,
    /// Shortest reconnect / rejoin / token-retry delay (the first retry waits `min/2..=min`).
    pub min_backoff: Duration,
    /// Longest reconnect / rejoin / token-retry delay.
    pub max_backoff: Duration,
    /// Timeout of each network step: token mint, websocket handshake, channel join, frame write.
    pub join_timeout: Duration,
    /// A connection that stayed up this long resets the reconnect backoff when it drops.
    pub stable_after: Duration,
    /// Capacity of the event channel. When the consumer falls this far behind, events are
    /// dropped (never buffered without bound) and a [`ResyncReason::ConsumerLagged`] is delivered
    /// as soon as there is room again.
    pub buffer: usize,
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            heartbeat: Duration::from_secs(25),
            refresh_before: Duration::from_secs(15 * 60),
            min_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            join_timeout: Duration::from_secs(10),
            stable_after: Duration::from_secs(60),
            buffer: 4_096,
        }
    }
}

impl RealtimeConfig {
    /// Defaults with the given channels.
    pub fn new(channels: Vec<String>) -> Self {
        Self {
            channels,
            ..Self::default()
        }
    }
}

/// The tournament-scope channels Susquatch consumes: tournament-scope violations, our account
/// batches and leaderboard ticks. Market updates are per market (see [`market_channel`]); since
/// 2026-10-01 the tournament-wide `tournament:{tid}` topic carries none, so it is not joined.
///
/// `user_channel` is `channels.user` from the token response; a bare profile id is accepted
/// and prefixed with `user:`.
pub fn tournament_channels(tid: &TournamentId, user_channel: &str) -> Vec<String> {
    let user = if user_channel.starts_with("user:") {
        user_channel.to_owned()
    } else {
        format!("user:{user_channel}")
    };
    vec![
        format!("relationships:violations:{tid}"),
        user,
        "leaderboard:all".to_owned(),
    ]
}

/// The per-market channel carrying `market_batch`, `book_dirty` and `market_settled` for every
/// exchange of `market`: `tournament:{tid}:market:{market_id}`.
pub fn market_channel(tid: &TournamentId, market: &MarketId) -> String {
    format!("tournament:{tid}:market:{market}")
}

/// The market a per-market channel name refers to (`None` for any other channel).
pub fn market_of_channel(channel: &str) -> Option<MarketId> {
    let rest = channel.strip_prefix("tournament:")?;
    let (_, market) = rest.split_once(":market:")?;
    (!market.is_empty() && !market.contains(':')).then(|| MarketId::new(market))
}

/// Where realtime tokens come from (normally `POST /realtime/token`).
#[async_trait]
pub trait TokenSource: Send + Sync + 'static {
    /// Mint a fresh token.
    async fn token(&self) -> PlatformResult<RealtimeToken>;
}

#[async_trait]
impl<P: PlatformAdapter + ?Sized> TokenSource for Arc<P> {
    async fn token(&self) -> PlatformResult<RealtimeToken> {
        self.realtime_token().await
    }
}

/// Why the consumer must resync from REST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResyncReason {
    /// First successful subscription of this client.
    InitialSubscribe,
    /// Subscribed again after the socket was lost.
    Reconnect,
    /// The token was refreshed on the live socket.
    TokenRefresh,
    /// The server errored or closed `channel` (or its join failed); it has now been rejoined.
    SocketError { channel: String },
    /// `previousRevision` on `channel` was `got`, but the last accepted revision was `expected`.
    RevisionGap {
        channel: String,
        expected: u64,
        got: Option<u64>,
    },
    /// The consumer fell behind and events were dropped.
    ConsumerLagged,
}

/// What the realtime client reports.
#[derive(Debug, Clone, PartialEq)]
pub enum RealtimeEvent {
    /// Every channel has answered its first join on a new socket. Always followed by a `Resync`.
    Connected,
    /// Refetch authoritative state from REST before acting on anything older.
    Resync { reason: ResyncReason },
    /// `market_batch` on a per-market `tournament:{tid}:market:{market_id}` channel.
    MarketBatch { channel: String, batch: MarketBatch },
    /// `account_batch` on the `user:*` channel.
    AccountBatch { channel: String, batch: AccountBatch },
    /// `violation` on a `relationships:violations*` channel.
    Violation {
        channel: String,
        eval: Box<ConstraintEval>,
    },
    /// `tick` on `leaderboard:all`.
    LeaderboardTick { channel: String, tick: LeaderboardTick },
    /// The socket was lost; the client is reconnecting.
    Disconnected { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tournament_channels_accepts_bare_or_prefixed_user() {
        let tid = TournamentId::new("t1");
        let expected = vec![
            "relationships:violations:t1".to_owned(),
            "user:p1".to_owned(),
            "leaderboard:all".to_owned(),
        ];
        assert_eq!(tournament_channels(&tid, "p1"), expected);
        assert_eq!(tournament_channels(&tid, "user:p1"), expected);
        assert!(
            !expected.iter().any(|c| c == "tournament:t1"),
            "the tournament-wide topic no longer carries market updates"
        );
    }

    #[test]
    fn market_channels_round_trip() {
        let tid = TournamentId::new("t1");
        let ch = market_channel(&tid, &MarketId::new("m-9"));
        assert_eq!(ch, "tournament:t1:market:m-9");
        assert_eq!(market_of_channel(&ch), Some(MarketId::new("m-9")));
        assert_eq!(market_of_channel("tournament:t1"), None);
        assert_eq!(market_of_channel("relationships:violations:t1"), None);
        assert_eq!(market_of_channel("tournament:t1:market:"), None);
    }
}
