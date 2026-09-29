//! The connection supervisor: reconnect loop, per-socket session, joins, heartbeats, token refresh.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt};
use susq_core::{
    Clock,
    clock::{duration_ns, to_ns},
};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, Interval, MissedTickBehavior, interval_at, sleep, sleep_until, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Error as WsError, Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    RealtimeConfig, RealtimeEvent, ResyncReason, TokenSource,
    backoff::Backoff,
    outbox::{ConsumerGone, Outbox},
    payload::{self, Broadcast, Delivery},
    protocol::{Frame, Inbound, websocket_url},
    revision::{Revisions, Verdict},
};
use crate::{
    error::{PlatformError, PlatformResult},
    models::RealtimeToken,
};

/// Documented token lifetime, assumed when `expiresAt` does not parse.
const TOKEN_LIFETIME: Duration = Duration::from_secs(3 * 60 * 60);
/// Timer used when no channel is waiting on a deadline.
const IDLE: Duration = Duration::from_secs(60 * 60);
/// Grace period for a polite close frame on shutdown.
const CLOSE_GRACE: Duration = Duration::from_millis(250);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Mint = Pin<Box<dyn Future<Output = PlatformResult<RealtimeToken>> + Send>>;

/// Entry point: spawns the supervisor task. See the [module docs](super) for behaviour.
#[derive(Debug, Clone, Copy)]
pub struct RealtimeClient;

impl RealtimeClient {
    /// Start the client. Events arrive on the returned receiver (capacity
    /// [`RealtimeConfig::buffer`]); the task ends when `shutdown` is cancelled or the receiver is
    /// dropped, and never panics.
    pub fn spawn<T: TokenSource>(
        cfg: RealtimeConfig,
        tokens: T,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
    ) -> (mpsc::Receiver<RealtimeEvent>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(cfg.buffer.max(1));
        let worker = Worker::new(cfg, Arc::new(tokens), clock, shutdown, Outbox::new(tx));
        (rx, tokio::spawn(worker.run()))
    }
}

/// Why a session ended.
#[derive(Debug)]
enum Stop {
    Shutdown,
    ConsumerGone,
    Lost(String),
}

impl From<ConsumerGone> for Stop {
    fn from(_: ConsumerGone) -> Self {
        Stop::ConsumerGone
    }
}

type Step = Result<(), Stop>;

/// A token plus its expiry in wall-clock nanoseconds.
#[derive(Clone)]
struct HeldToken {
    token: RealtimeToken,
    expires_ns: i64,
}

impl HeldToken {
    fn new(token: RealtimeToken, now_ns: i64) -> Self {
        let expires_ns = token
            .expires_at()
            .map(to_ns)
            .unwrap_or_else(|| now_ns.saturating_add(duration_ns(TOKEN_LIFETIME)));
        Self { token, expires_ns }
    }

    fn remaining(&self, now_ns: i64) -> Duration {
        Duration::from_nanos(u64::try_from(self.expires_ns.saturating_sub(now_ns)).unwrap_or(0))
    }
}

/// When to refresh a token with `remaining` life: `refresh_before` ahead of expiry, but never
/// earlier than half-way, so short-lived tokens do not cause a refresh storm.
fn refresh_lead(remaining: Duration, refresh_before: Duration) -> Duration {
    remaining.saturating_sub(refresh_before).max(remaining / 2)
}

/// Text for a websocket error that can never contain the URL (which carries the anon key).
fn describe(e: &WsError) -> String {
    match e {
        WsError::Url(_) => "invalid websocket url".to_owned(),
        WsError::Http(resp) => format!("websocket upgrade rejected with HTTP {}", resp.status()),
        other => other.to_string(),
    }
}

/// Join state of one channel on the current socket.
#[derive(Debug, Clone)]
enum SlotState {
    Joining { reference: String, deadline: Instant },
    Joined { join_ref: String },
    Waiting { until: Instant },
}

#[derive(Debug)]
struct Slot {
    channel: String,
    state: SlotState,
    /// The first join attempt on this socket has been answered (or timed out).
    settled: bool,
    backoff: Backoff,
}

impl Slot {
    /// The ref the server uses for this channel instance, if a join is live or in flight.
    fn live_ref(&self) -> Option<&str> {
        match &self.state {
            SlotState::Joining { reference, .. } => Some(reference),
            SlotState::Joined { join_ref } => Some(join_ref),
            SlotState::Waiting { .. } => None,
        }
    }

    /// Schedule a rejoin after backoff.
    fn fail(&mut self) {
        let delay = self.backoff.next_delay();
        debug!(channel = %self.channel, ?delay, "realtime channel rejoin scheduled");
        self.state = SlotState::Waiting {
            until: Instant::now() + delay,
        };
        self.settled = true;
    }
}

/// Per-socket state.
struct Link {
    ws: Ws,
    slots: Vec<Slot>,
    heartbeat: Interval,
    pending_heartbeat: Option<String>,
    refresh: Mint,
    expiry: Instant,
    /// Set once `Connected` was emitted on this socket.
    connected_at: Option<Instant>,
}

impl Link {
    fn slot_index(&self, channel: &str) -> Option<usize> {
        self.slots.iter().position(|s| s.channel == channel)
    }

    fn next_channel_deadline(&self) -> Option<Instant> {
        self.slots
            .iter()
            .filter_map(|s| match s.state {
                SlotState::Joining { deadline, .. } => Some(deadline),
                SlotState::Waiting { until } => Some(until),
                SlotState::Joined { .. } => None,
            })
            .min()
    }
}

/// The supervisor task's state, shared across sockets.
struct Worker<T> {
    cfg: RealtimeConfig,
    tokens: Arc<T>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    out: Outbox,
    revisions: Revisions,
    held: Option<HeldToken>,
    ever_connected: bool,
    next_ref: u64,
    reconnect: Backoff,
    refresh_retry: Backoff,
}

impl<T: TokenSource> Worker<T> {
    fn new(
        cfg: RealtimeConfig,
        tokens: Arc<T>,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
        out: Outbox,
    ) -> Self {
        let seed = clock.now_ns() as u64;
        Self {
            reconnect: Backoff::new(cfg.min_backoff, cfg.max_backoff, seed),
            refresh_retry: Backoff::new(cfg.min_backoff, cfg.max_backoff, !seed),
            cfg,
            tokens,
            clock,
            shutdown,
            out,
            revisions: Revisions::default(),
            held: None,
            ever_connected: false,
            next_ref: 0,
        }
    }

    /// Reconnect loop.
    async fn run(mut self) {
        loop {
            let (stop, stable) = self.connect_and_serve().await;
            let reason = match stop {
                Stop::Shutdown => break,
                Stop::ConsumerGone => {
                    debug!("realtime consumer dropped its receiver");
                    break;
                }
                Stop::Lost(reason) => reason,
            };
            warn!(%reason, "realtime connection lost; reconnecting");
            if self.out.emit(RealtimeEvent::Disconnected { reason }).is_err() {
                break;
            }
            if stable {
                self.reconnect.reset();
            }
            let delay = self.reconnect.next_delay();
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = sleep(delay) => {}
            }
        }
        info!("realtime client stopped");
    }

    /// One socket's lifetime. Returns why it ended and whether it had been healthy long enough
    /// to reset the reconnect backoff.
    async fn connect_and_serve(&mut self) -> (Stop, bool) {
        let mut link = match self.open().await {
            Ok(link) => link,
            Err(stop) => return (stop, false),
        };
        let stop = self.serve(&mut link).await;
        if matches!(stop, Stop::Shutdown) {
            // Best effort: the process is going away either way.
            let _ = timeout(CLOSE_GRACE, link.ws.close(None)).await;
        }
        let stable = link
            .connected_at
            .is_some_and(|t| t.elapsed() >= self.cfg.stable_after);
        (stop, stable)
    }

    /// Mint (or reuse) a token, connect, and send the joins.
    async fn open(&mut self) -> Result<Link, Stop> {
        let held = self.ensure_token().await?;
        let url = websocket_url(&held.token.supabase_url, &held.token.anon_key)
            .map_err(|e| Stop::Lost(e.to_string()))?;
        let (ws, _) = self
            .guarded("websocket connect", connect_async(url.as_str()))
            .await?
            .map_err(|e| Stop::Lost(format!("websocket connect failed: {}", describe(&e))))?;
        info!(channels = self.cfg.channels.len(), "realtime socket open");

        let now = Instant::now();
        let seed = self.clock.now_ns() as u64;
        let slots = self
            .cfg
            .channels
            .iter()
            .zip(1u64..)
            .map(|(channel, i)| Slot {
                channel: channel.clone(),
                state: SlotState::Waiting { until: now },
                settled: false,
                backoff: Backoff::new(
                    self.cfg.min_backoff,
                    self.cfg.max_backoff,
                    seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                ),
            })
            .collect();
        let mut heartbeat = interval_at(now + self.cfg.heartbeat, self.cfg.heartbeat);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let (lead, expiry) = self.token_timers(&held);
        let mut link = Link {
            ws,
            slots,
            heartbeat,
            pending_heartbeat: None,
            refresh: self.mint_after(lead),
            expiry,
            connected_at: None,
        };
        self.service_channels(&mut link).await?;
        Ok(link)
    }

    /// The event loop of one socket. Timers come before the socket in the biased order so a
    /// flood of frames can never starve heartbeats or token refresh.
    async fn serve(&mut self, link: &mut Link) -> Stop {
        loop {
            let wake = link
                .next_channel_deadline()
                .unwrap_or_else(|| Instant::now() + IDLE);
            let step = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => Err(Stop::Shutdown),
                _ = link.heartbeat.tick() => self.on_heartbeat(link).await,
                _ = sleep_until(link.expiry) => Err(Stop::Lost("realtime token expired".into())),
                res = &mut link.refresh => self.on_refresh(link, res).await,
                _ = sleep_until(wake) => self.service_channels(link).await,
                _ = self.out.closed() => Err(Stop::ConsumerGone),
                msg = link.ws.next() => self.on_message(link, msg).await,
            };
            if let Err(stop) = step {
                return stop;
            }
        }
    }

    /// Reuse the held token while it has more than `refresh_before` left, else mint a new one.
    async fn ensure_token(&mut self) -> Result<HeldToken, Stop> {
        let now = self.clock.now_ns();
        if let Some(held) = self
            .held
            .as_ref()
            .filter(|h| h.remaining(now) > self.cfg.refresh_before)
        {
            return Ok(held.clone());
        }
        let token = self
            .guarded("realtime token mint", self.tokens.token())
            .await?
            .map_err(|e| Stop::Lost(format!("realtime token mint failed: {e}")))?;
        let held = HeldToken::new(token, self.clock.now_ns());
        self.held = Some(held.clone());
        Ok(held)
    }

    /// Refresh delay and expiry instant for a token.
    fn token_timers(&self, held: &HeldToken) -> (Duration, Instant) {
        let remaining = held.remaining(self.clock.now_ns());
        (
            refresh_lead(remaining, self.cfg.refresh_before),
            Instant::now() + remaining,
        )
    }

    /// A future that waits `delay` and then mints a token (bounded by `join_timeout`).
    fn mint_after(&self, delay: Duration) -> Mint {
        let tokens = Arc::clone(&self.tokens);
        let limit = self.cfg.join_timeout;
        Box::pin(async move {
            sleep(delay).await;
            timeout(limit, tokens.token()).await.unwrap_or_else(|_| {
                Err(PlatformError::Network {
                    message: "realtime token mint timed out".into(),
                    timeout: true,
                })
            })
        })
    }

    /// Run `fut` bounded by `join_timeout` and interruptible by shutdown.
    async fn guarded<F: Future>(&self, what: &str, fut: F) -> Result<F::Output, Stop> {
        tokio::select! {
            _ = self.shutdown.cancelled() => Err(Stop::Shutdown),
            res = timeout(self.cfg.join_timeout, fut) => {
                res.map_err(|_| Stop::Lost(format!("{what} timed out")))
            }
        }
    }

    fn next_ref(&mut self) -> String {
        self.next_ref = self.next_ref.wrapping_add(1);
        self.next_ref.to_string()
    }

    fn access_token(&self) -> &str {
        self.held.as_ref().map_or("", |h| h.token.token.as_str())
    }

    async fn send(&self, ws: &mut Ws, frame: &Frame) -> Step {
        let text = frame
            .encode()
            .map_err(|e| Stop::Lost(format!("frame encode failed: {e}")))?;
        self.guarded("socket write", ws.send(Message::text(text)))
            .await?
            .map_err(|e| Stop::Lost(format!("socket write failed: {}", describe(&e))))
    }

    async fn send_join(&mut self, link: &mut Link, i: usize) -> Step {
        let reference = self.next_ref();
        let frame = Frame::join(&link.slots[i].channel, self.access_token(), &reference);
        self.send(&mut link.ws, &frame).await?;
        link.slots[i].state = SlotState::Joining {
            reference,
            deadline: Instant::now() + self.cfg.join_timeout,
        };
        Ok(())
    }

    /// Expire overdue joins and send due (re)joins.
    async fn service_channels(&mut self, link: &mut Link) -> Step {
        let now = Instant::now();
        for i in 0..link.slots.len() {
            match link.slots[i].state {
                SlotState::Joining { deadline, .. } if deadline <= now => {
                    warn!(channel = %link.slots[i].channel, "realtime channel join timed out");
                    link.slots[i].fail();
                }
                SlotState::Waiting { until } if until <= now => self.send_join(link, i).await?,
                _ => {}
            }
        }
        self.maybe_announce(link)
    }

    /// Emit `Connected` + `Resync` once every channel's first join on this socket is answered.
    fn maybe_announce(&mut self, link: &mut Link) -> Step {
        if link.connected_at.is_some() || !link.slots.iter().all(|s| s.settled) {
            return Ok(());
        }
        link.connected_at = Some(Instant::now());
        self.revisions.reset_all();
        let reason = if self.ever_connected {
            ResyncReason::Reconnect
        } else {
            ResyncReason::InitialSubscribe
        };
        self.ever_connected = true;
        let joined = link
            .slots
            .iter()
            .filter(|s| matches!(s.state, SlotState::Joined { .. }))
            .count();
        info!(joined, total = link.slots.len(), "realtime subscribed");
        self.out.emit(RealtimeEvent::Connected)?;
        self.out.emit(RealtimeEvent::Resync { reason })?;
        Ok(())
    }

    async fn on_heartbeat(&mut self, link: &mut Link) -> Step {
        if link.pending_heartbeat.is_some() {
            return Err(Stop::Lost("heartbeat reply missing".into()));
        }
        self.out.flush()?;
        let reference = self.next_ref();
        self.send(&mut link.ws, &Frame::heartbeat(&reference)).await?;
        link.pending_heartbeat = Some(reference);
        Ok(())
    }

    async fn on_refresh(&mut self, link: &mut Link, res: PlatformResult<RealtimeToken>) -> Step {
        let token = match res {
            Ok(token) => token,
            Err(e) => {
                let delay = self.refresh_retry.next_delay();
                warn!(error = %e, ?delay, "realtime token refresh failed; keeping the socket");
                link.refresh = self.mint_after(delay);
                return Ok(());
            }
        };
        let held = HeldToken::new(token, self.clock.now_ns());
        let (lead, expiry) = self.token_timers(&held);
        link.refresh = self.mint_after(lead);
        link.expiry = expiry;
        self.held = Some(held);
        self.refresh_retry.reset();
        for i in 0..link.slots.len() {
            let SlotState::Joined { join_ref } = &link.slots[i].state else {
                continue;
            };
            let reference = self.next_ref();
            let frame =
                Frame::access_token(&link.slots[i].channel, self.access_token(), join_ref, &reference);
            self.send(&mut link.ws, &frame).await?;
        }
        info!("realtime token refreshed");
        self.revisions.reset_all();
        self.out.emit(RealtimeEvent::Resync {
            reason: ResyncReason::TokenRefresh,
        })?;
        Ok(())
    }

    async fn on_message(&mut self, link: &mut Link, msg: Option<Result<Message, WsError>>) -> Step {
        match msg {
            None => Err(Stop::Lost("socket closed".into())),
            Some(Err(e)) => Err(Stop::Lost(format!("socket error: {}", describe(&e)))),
            Some(Ok(Message::Close(frame))) => Err(Stop::Lost(match frame {
                Some(f) => format!(
                    "server closed the socket ({} {})",
                    u16::from(f.code),
                    f.reason.as_str()
                ),
                None => "server closed the socket".into(),
            })),
            Some(Ok(Message::Text(text))) => match Frame::decode(text.as_str()) {
                Ok(frame) => self.on_frame(link, frame),
                Err(e) => {
                    warn!(error = %e, "undecodable realtime frame ignored");
                    Ok(())
                }
            },
            Some(Ok(_)) => Ok(()),
        }
    }

    fn on_frame(&mut self, link: &mut Link, frame: Frame) -> Step {
        match frame.classify() {
            Inbound::HeartbeatReply { reference } => {
                if reference.is_some() && reference == link.pending_heartbeat {
                    link.pending_heartbeat = None;
                }
                Ok(())
            }
            Inbound::Reply {
                channel,
                reference,
                ok,
                reason,
            } => self.on_reply(link, &channel, reference.as_deref(), ok, reason.as_deref()),
            Inbound::ChannelDown {
                channel,
                reference,
                event,
            } => {
                on_channel_down(link, &channel, reference.as_deref(), &event);
                Ok(())
            }
            Inbound::Broadcast {
                channel,
                event,
                payload,
            } => self.on_broadcast(&channel, &event, payload),
            Inbound::System { channel, ok, message } => {
                if ok {
                    debug!(%channel, %message, "realtime system message");
                } else {
                    warn!(%channel, %message, "realtime system error");
                }
                Ok(())
            }
            Inbound::Other => Ok(()),
        }
    }

    fn on_reply(
        &mut self,
        link: &mut Link,
        channel: &str,
        reference: Option<&str>,
        ok: bool,
        reason: Option<&str>,
    ) -> Step {
        let Some(i) = link.slot_index(channel) else {
            return Ok(());
        };
        let slot = &mut link.slots[i];
        let SlotState::Joining {
            reference: expected, ..
        } = &slot.state
        else {
            return Ok(());
        };
        let expected = expected.clone();
        if reference != Some(expected.as_str()) {
            return Ok(());
        }
        if !ok {
            warn!(%channel, reason = reason.unwrap_or("unknown"), "realtime channel join rejected");
            slot.fail();
            return self.maybe_announce(link);
        }
        slot.state = SlotState::Joined { join_ref: expected };
        slot.settled = true;
        slot.backoff.reset();
        debug!(%channel, "realtime channel joined");
        if link.connected_at.is_some() {
            self.revisions.reset(channel);
            self.out.emit(RealtimeEvent::Resync {
                reason: ResyncReason::SocketError {
                    channel: channel.to_owned(),
                },
            })?;
        }
        self.maybe_announce(link)
    }

    fn on_broadcast(&mut self, channel: &str, event: &str, payload: serde_json::Value) -> Step {
        let decoded = match payload::decode(event, payload) {
            Ok(decoded) => decoded,
            Err(e) => {
                warn!(%channel, %event, error = %e, "undecodable realtime broadcast dropped");
                return Ok(());
            }
        };
        let channel = channel.to_owned();
        let event = match decoded {
            Broadcast::Market(batch) => {
                if !self.admit(&channel, &batch.delivery)? {
                    return Ok(());
                }
                RealtimeEvent::MarketBatch { channel, batch }
            }
            Broadcast::Account(batch) => {
                if !self.admit(&channel, &batch.delivery)? {
                    return Ok(());
                }
                RealtimeEvent::AccountBatch { channel, batch }
            }
            Broadcast::Violation(eval) => RealtimeEvent::Violation { channel, eval },
            Broadcast::Tick(tick) => RealtimeEvent::LeaderboardTick { channel, tick },
            Broadcast::Ignored => {
                debug!(%channel, %event, "realtime broadcast ignored");
                return Ok(());
            }
        };
        self.out.emit(event)?;
        Ok(())
    }

    /// Apply revision rules; emits the gap `Resync` first. `Ok(false)` drops a duplicate.
    fn admit(&mut self, channel: &str, delivery: &Delivery) -> Result<bool, Stop> {
        match self.revisions.observe(channel, delivery) {
            Verdict::Accept => Ok(true),
            Verdict::Duplicate => {
                debug!(%channel, revision = ?delivery.revision, "duplicate realtime batch dropped");
                Ok(false)
            }
            Verdict::Gap { expected, got } => {
                warn!(%channel, expected, ?got, "realtime revision gap");
                self.out.emit(RealtimeEvent::Resync {
                    reason: ResyncReason::RevisionGap {
                        channel: channel.to_owned(),
                        expected,
                        got,
                    },
                })?;
                Ok(true)
            }
        }
    }
}

/// The server errored or closed a channel: schedule a rejoin unless the notice is stale.
fn on_channel_down(link: &mut Link, channel: &str, reference: Option<&str>, event: &str) {
    let Some(i) = link.slot_index(channel) else {
        return;
    };
    let slot = &mut link.slots[i];
    let Some(live) = slot.live_ref() else {
        return;
    };
    if reference.is_some_and(|r| r != live) {
        debug!(%channel, %event, "stale realtime channel notice ignored");
        return;
    }
    warn!(%channel, %event, "realtime channel dropped by server; rejoining");
    slot.fail();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_lead_respects_refresh_before_and_half_life() {
        let before = Duration::from_secs(15 * 60);
        assert_eq!(
            refresh_lead(TOKEN_LIFETIME, before),
            Duration::from_secs(165 * 60)
        );
        assert_eq!(
            refresh_lead(Duration::from_secs(20 * 60), before),
            Duration::from_secs(10 * 60)
        );
        assert_eq!(refresh_lead(Duration::ZERO, before), Duration::ZERO);
    }

    #[test]
    fn held_token_uses_expires_at_or_documented_lifetime() {
        let token = RealtimeToken {
            expires_at: "2026-10-01T19:00:00Z".into(),
            ..Default::default()
        };
        let now = to_ns("2026-10-01T16:00:00Z".parse().expect("ts"));
        assert_eq!(HeldToken::new(token, now).remaining(now), TOKEN_LIFETIME);
        let bad = RealtimeToken {
            expires_at: "soon".into(),
            ..Default::default()
        };
        assert_eq!(HeldToken::new(bad, now).remaining(now), TOKEN_LIFETIME);
        let held = HeldToken::new(RealtimeToken::default(), now);
        assert_eq!(held.remaining(held.expires_ns + 1), Duration::ZERO);
    }
}
