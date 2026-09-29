//! The rust-live NATS client: stamped publishing and decoded subscriptions.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_nats::connection::State;
use async_nats::{Client, ConnectOptions, Event};
use futures::StreamExt;
use futures::stream::BoxStream;
use susq_contracts::{SCHEMA_VERSION, encode, v1};
use susq_core::{Clock, SystemClock};

use crate::message::{NeedleMessage, decode_kill};
use crate::subjects;

/// Keepalive ping period; a dead server is noticed within a couple of pings.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// Upper bound for one publish + flush (or a bare flush). Publishing never blocks the caller
/// for longer than this, even while NATS is down.
pub const OP_TIMEOUT: Duration = Duration::from_secs(2);

/// Largest pause between reconnect attempts (local server, so retry quickly).
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Errors from the bus client.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    /// The NATS options or address were rejected.
    #[error("connect to {url}: {source}")]
    Connect {
        url: String,
        #[source]
        source: async_nats::ConnectError,
    },
    /// Subscribing failed.
    #[error("subscribe {subject}: {source}")]
    Subscribe {
        subject: &'static str,
        #[source]
        source: async_nats::SubscribeError,
    },
    /// Publishing failed.
    #[error("publish {subject}: {source}")]
    Publish {
        subject: &'static str,
        #[source]
        source: async_nats::PublishError,
    },
    /// Flushing failed.
    #[error("flush: {0}")]
    Flush(#[from] async_nats::client::FlushError),
    /// The operation did not finish in time (typically NATS is down and reconnecting).
    #[error("{op} timed out after {timeout:?}")]
    Timeout { op: &'static str, timeout: Duration },
}

/// Connection to NATS for one producer.
///
/// The connection is established in the background and retried forever, so rust-live starts and
/// keeps running while NATS is down; needle data simply stops arriving and the watchdog reacts.
#[derive(Debug)]
pub struct Bus {
    client: Client,
    producer: v1::Producer,
    clock: Arc<dyn Clock>,
    county_seq: AtomicU64,
    kill_seq: AtomicU64,
}

impl Bus {
    /// Connect using the system clock.
    pub async fn connect(url: &str, producer_name: &str, git_sha: &str) -> Result<Self, BusError> {
        Self::connect_with_clock(url, producer_name, git_sha, Arc::new(SystemClock)).await
    }

    /// Connect with an explicit clock for header timestamps.
    pub async fn connect_with_clock(
        url: &str,
        producer_name: &str,
        git_sha: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BusError> {
        let options = ConnectOptions::new()
            .name(format!("{producer_name}@{git_sha}"))
            .retry_on_initial_connect()
            .max_reconnects(None)
            .ping_interval(PING_INTERVAL)
            .connection_timeout(OP_TIMEOUT)
            .reconnect_delay_callback(reconnect_delay)
            .event_callback(|event| async move { log_event(&event) });
        let client = async_nats::connect_with_options(url, options)
            .await
            .map_err(|source| BusError::Connect {
                url: url.to_owned(),
                source,
            })?;
        Ok(Self {
            client,
            producer: v1::Producer {
                name: producer_name.to_owned(),
                git_sha: git_sha.to_owned(),
            },
            clock,
            county_seq: AtomicU64::new(0),
            kill_seq: AtomicU64::new(0),
        })
    }

    /// True while the TCP connection to NATS is up.
    pub fn is_connected(&self) -> bool {
        self.client.connection_state() == State::Connected
    }

    /// Round-trip to the server: everything sent before (publishes, subscriptions) was processed.
    pub async fn flush(&self) -> Result<(), BusError> {
        with_timeout("flush", async { Ok(self.client.flush().await?) }).await
    }

    /// Publish a `CountyResultsBatch` on `enr.county_results` and flush. Returns the header `seq`
    /// used (1, 2, 3, ... per process; a seq is consumed even when publishing fails).
    pub async fn publish_county_results(&self, results: Vec<v1::CountyResult>) -> Result<u64, BusError> {
        let header = self.header(&self.county_seq);
        let seq = header.seq;
        let msg = v1::CountyResultsBatch {
            header: Some(header),
            results,
        };
        self.publish(subjects::ENR_COUNTY_RESULTS, encode(&msg)).await?;
        Ok(seq)
    }

    /// Publish a `Kill` on `ops.kill` and flush. Every subscriber, this process included, gets it.
    pub async fn publish_kill(&self, reason: &str, issuer: &str) -> Result<u64, BusError> {
        let header = self.header(&self.kill_seq);
        let seq = header.seq;
        let msg = v1::Kill {
            header: Some(header),
            reason: reason.to_owned(),
            issuer: issuer.to_owned(),
        };
        self.publish(subjects::OPS_KILL, encode(&msg)).await?;
        Ok(seq)
    }

    /// Subscribe to all `needle.*` streams. Undecodable payloads arrive as
    /// [`NeedleMessage::Malformed`] rather than being hidden. Subscriptions survive reconnects.
    pub async fn subscribe_needle(&self) -> Result<BoxStream<'static, NeedleMessage>, BusError> {
        let mut subs = Vec::with_capacity(subjects::NEEDLE.len());
        for subject in subjects::NEEDLE {
            subs.push(self.subscribe(subject).await?);
        }
        Ok(futures::stream::select_all(subs)
            .map(|m| NeedleMessage::decode(m.subject.as_str(), &m.payload))
            .boxed())
    }

    /// Subscribe to `ops.kill`. Every payload yields a kill, even a malformed one
    /// (see [`decode_kill`]).
    pub async fn subscribe_kill(&self) -> Result<BoxStream<'static, v1::Kill>, BusError> {
        let sub = self.subscribe(subjects::OPS_KILL).await?;
        Ok(sub.map(|m| decode_kill(&m.payload)).boxed())
    }

    async fn subscribe(&self, subject: &'static str) -> Result<async_nats::Subscriber, BusError> {
        self.client
            .subscribe(subject)
            .await
            .map_err(|source| BusError::Subscribe { subject, source })
    }

    async fn publish(&self, subject: &'static str, payload: Vec<u8>) -> Result<(), BusError> {
        with_timeout("publish", async {
            self.client
                .publish(subject, payload.into())
                .await
                .map_err(|source| BusError::Publish { subject, source })?;
            Ok(self.client.flush().await?)
        })
        .await
    }

    fn header(&self, seq: &AtomicU64) -> v1::Header {
        v1::Header {
            schema_version: SCHEMA_VERSION,
            produced_at_ns: self.clock.now_ns(),
            producer: Some(self.producer.clone()),
            seq: seq.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }
}

async fn with_timeout(
    op: &'static str,
    fut: impl Future<Output = Result<(), BusError>>,
) -> Result<(), BusError> {
    tokio::time::timeout(OP_TIMEOUT, fut)
        .await
        .map_err(|_| BusError::Timeout {
            op,
            timeout: OP_TIMEOUT,
        })?
}

/// Backoff between reconnect attempts: immediate, then 50 ms doubling, capped at
/// [`MAX_RECONNECT_DELAY`].
pub fn reconnect_delay(attempts: usize) -> Duration {
    if attempts <= 1 {
        return Duration::ZERO;
    }
    let exp = u32::try_from(attempts - 2).unwrap_or(u32::MAX).min(16);
    Duration::from_millis(50u64 << exp).min(MAX_RECONNECT_DELAY)
}

fn log_event(event: &Event) {
    match event {
        Event::Connected => tracing::info!("nats connected"),
        Event::Disconnected => tracing::warn!("nats disconnected; reconnecting"),
        // Repeats on every reconnect attempt while NATS is down; the Disconnected warning above
        // already records the outage, so keep the log quiet.
        Event::ClientError(_) => tracing::debug!(%event, "nats event"),
        Event::ServerError(_) | Event::SlowConsumer(_) | Event::LameDuckMode => {
            tracing::warn!(%event, "nats event")
        }
        Event::Draining | Event::Closed => tracing::info!(%event, "nats event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_delay_backs_off_to_the_cap() {
        assert_eq!(reconnect_delay(0), Duration::ZERO);
        assert_eq!(reconnect_delay(1), Duration::ZERO);
        assert_eq!(reconnect_delay(2), Duration::from_millis(50));
        assert_eq!(reconnect_delay(3), Duration::from_millis(100));
        assert_eq!(reconnect_delay(6), Duration::from_millis(800));
        assert_eq!(reconnect_delay(7), MAX_RECONNECT_DELAY);
        assert_eq!(reconnect_delay(usize::MAX), MAX_RECONNECT_DELAY);
    }
}
