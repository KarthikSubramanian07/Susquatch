//! Bounded, non-blocking delivery to the consumer.
//!
//! The client never waits on a slow consumer (that would stall heartbeats) and never buffers
//! without bound. When the channel is full the event is dropped and the outbox remembers it; the
//! next delivery attempt first sends [`ResyncReason::ConsumerLagged`], so the consumer knows to
//! refetch everything it may have missed.

use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::warn;

use super::{RealtimeEvent, ResyncReason};

/// The consumer dropped its receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConsumerGone;

/// Sender side with lag tracking.
#[derive(Debug)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<RealtimeEvent>,
    lagged: bool,
}

impl Outbox {
    pub fn new(tx: mpsc::Sender<RealtimeEvent>) -> Self {
        Self { tx, lagged: false }
    }

    /// Deliver `event`, or drop it (and mark the consumer as lagged) if the channel is full.
    pub fn emit(&mut self, event: RealtimeEvent) -> Result<(), ConsumerGone> {
        if !self.flush()? {
            return Ok(());
        }
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.mark_lagged();
                Ok(())
            }
            Err(TrySendError::Closed(_)) => Err(ConsumerGone),
        }
    }

    /// Deliver a pending `ConsumerLagged` resync if there is room. `Ok(true)` when nothing is
    /// pending any more.
    pub fn flush(&mut self) -> Result<bool, ConsumerGone> {
        if !self.lagged {
            return Ok(true);
        }
        let resync = RealtimeEvent::Resync {
            reason: ResyncReason::ConsumerLagged,
        };
        match self.tx.try_send(resync) {
            Ok(()) => {
                self.lagged = false;
                Ok(true)
            }
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Closed(_)) => Err(ConsumerGone),
        }
    }

    /// Resolves when the consumer drops its receiver.
    pub async fn closed(&self) {
        self.tx.closed().await;
    }

    fn mark_lagged(&mut self) {
        if !self.lagged {
            warn!("realtime consumer is lagging; dropping events until it catches up");
        }
        self.lagged = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick() -> RealtimeEvent {
        RealtimeEvent::Disconnected { reason: "x".into() }
    }

    #[test]
    fn full_channel_drops_then_reports_lag_before_the_next_event() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut out = Outbox::new(tx);
        out.emit(RealtimeEvent::Connected).expect("open");
        out.emit(tick()).expect("open");
        out.emit(tick()).expect("dropped, not an error");
        out.emit(tick()).expect("dropped, not an error");
        assert_eq!(rx.try_recv().expect("first"), RealtimeEvent::Connected);
        assert_eq!(rx.try_recv().expect("second"), tick());
        assert!(rx.try_recv().is_err(), "dropped events were not buffered");
        out.emit(RealtimeEvent::Connected).expect("open");
        assert_eq!(
            rx.try_recv().expect("lag"),
            RealtimeEvent::Resync {
                reason: ResyncReason::ConsumerLagged
            }
        );
        assert_eq!(rx.try_recv().expect("event"), RealtimeEvent::Connected);
    }

    #[test]
    fn flush_delivers_pending_lag_without_new_events() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut out = Outbox::new(tx);
        out.emit(tick()).expect("open");
        out.emit(tick()).expect("dropped");
        assert!(!out.flush().expect("open"), "still full");
        rx.try_recv().expect("drain");
        assert!(out.flush().expect("open"));
        assert!(matches!(rx.try_recv(), Ok(RealtimeEvent::Resync { .. })));
    }

    #[test]
    fn closed_receiver_is_reported() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut out = Outbox::new(tx);
        assert_eq!(out.emit(tick()), Err(ConsumerGone));
    }
}
