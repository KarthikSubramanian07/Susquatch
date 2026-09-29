//! Per-channel `delivery.revision` tracking.
//!
//! Rules from the endpoint reference: ignore a duplicate revision; if `previousRevision` does not
//! equal the last accepted revision a batch was missed, so the caller must resync from REST before
//! acting (the batch itself is still accepted and becomes the new baseline).

use std::collections::{HashMap, VecDeque};

use super::payload::Delivery;

/// How many recently accepted revisions are remembered for duplicate detection.
const SEEN_WINDOW: usize = 256;

/// Outcome of observing one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Deliver the batch.
    Accept,
    /// Already delivered: drop it.
    Duplicate,
    /// A batch was missed: resync, then deliver this batch.
    Gap { expected: u64, got: Option<u64> },
}

/// Revision state of one channel.
#[derive(Debug, Default)]
struct Tracker {
    last: Option<u64>,
    seen: VecDeque<u64>,
}

impl Tracker {
    fn observe(&mut self, delivery: &Delivery) -> Verdict {
        let Some(revision) = delivery.revision else {
            return Verdict::Accept;
        };
        if self.seen.contains(&revision) {
            return Verdict::Duplicate;
        }
        let verdict = match self.last {
            Some(last) if delivery.previous_revision != Some(last) => Verdict::Gap {
                expected: last,
                got: delivery.previous_revision,
            },
            _ => Verdict::Accept,
        };
        self.accept(revision);
        verdict
    }

    fn accept(&mut self, revision: u64) {
        self.last = Some(revision);
        if self.seen.len() == SEEN_WINDOW {
            self.seen.pop_front();
        }
        self.seen.push_back(revision);
    }
}

/// Revision trackers for every channel.
#[derive(Debug, Default)]
pub(crate) struct Revisions(HashMap<String, Tracker>);

impl Revisions {
    /// Observe a batch on `channel`. Batches without a revision are always accepted.
    pub fn observe(&mut self, channel: &str, delivery: &Delivery) -> Verdict {
        self.0.entry(channel.to_owned()).or_default().observe(delivery)
    }

    /// Forget one channel (it was rejoined and the consumer resyncs).
    pub fn reset(&mut self, channel: &str) {
        self.0.remove(channel);
    }

    /// Forget every channel (reconnect or token refresh; the consumer resyncs).
    pub fn reset_all(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(revision: u64, previous: Option<u64>) -> Delivery {
        Delivery {
            revision: Some(revision),
            previous_revision: previous,
            correlation_id: None,
        }
    }

    #[test]
    fn first_batch_is_the_baseline_and_chains_are_accepted() {
        let mut r = Revisions::default();
        assert_eq!(r.observe("c", &d(10, Some(4))), Verdict::Accept);
        assert_eq!(r.observe("c", &d(11, Some(10))), Verdict::Accept);
        assert_eq!(r.observe("c", &d(12, Some(11))), Verdict::Accept);
    }

    #[test]
    fn duplicates_are_dropped_including_older_ones() {
        let mut r = Revisions::default();
        r.observe("c", &d(1, None));
        r.observe("c", &d(2, Some(1)));
        assert_eq!(r.observe("c", &d(2, Some(1))), Verdict::Duplicate);
        assert_eq!(r.observe("c", &d(1, None)), Verdict::Duplicate);
        assert_eq!(r.observe("c", &d(3, Some(2))), Verdict::Accept);
    }

    #[test]
    fn gap_is_reported_and_the_batch_becomes_the_baseline() {
        let mut r = Revisions::default();
        r.observe("c", &d(1, None));
        assert_eq!(
            r.observe("c", &d(5, Some(4))),
            Verdict::Gap {
                expected: 1,
                got: Some(4)
            }
        );
        assert_eq!(r.observe("c", &d(6, Some(5))), Verdict::Accept);
        assert_eq!(
            r.observe("c", &d(7, None)),
            Verdict::Gap {
                expected: 6,
                got: None
            }
        );
    }

    #[test]
    fn channels_are_independent_and_resets_forget() {
        let mut r = Revisions::default();
        r.observe("a", &d(1, None));
        r.observe("b", &d(50, None));
        assert_eq!(r.observe("a", &d(2, Some(1))), Verdict::Accept);
        r.reset("a");
        assert_eq!(r.observe("a", &d(9, Some(8))), Verdict::Accept);
        r.reset_all();
        assert_eq!(r.observe("b", &d(50, None)), Verdict::Accept);
    }

    #[test]
    fn batches_without_revision_are_accepted_untracked() {
        let mut r = Revisions::default();
        r.observe("c", &d(1, None));
        assert_eq!(r.observe("c", &Delivery::default()), Verdict::Accept);
        assert_eq!(r.observe("c", &d(2, Some(1))), Verdict::Accept);
    }

    #[test]
    fn seen_window_is_bounded() {
        let mut r = Revisions::default();
        let total = u64::try_from(SEEN_WINDOW).expect("fits") + 10;
        for rev in 1..=total {
            r.observe("c", &d(rev, rev.checked_sub(1).filter(|p| *p > 0)));
        }
        assert_eq!(r.0["c"].seen.len(), SEEN_WINDOW);
    }
}
