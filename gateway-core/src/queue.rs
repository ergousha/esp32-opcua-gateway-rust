//! The bounded hand-off between the OPC UA task and the MQTT publisher.
//!
//! The two run at independent rates: OPC UA notifications arrive whenever the
//! server feels like it, while MQTT publishing is gated by TLS, link quality
//! and the AWS IoT publish-rate limit. An unbounded channel between them is a
//! guaranteed out-of-memory on a device with ~120 KB of free heap, so the queue
//! is bounded and has an explicit, observable overflow policy.
//!
//! Overflow policy, in order:
//!
//! 1. If a sample for the same address is already queued, replace it — the
//!    newer value supersedes the older one, and no tag disappears.
//! 2. Otherwise drop the oldest sample outright.
//!
//! Both cases bump a counter that is surfaced in the shadow's `reported`, so a
//! chronically undersized queue is visible from the cloud rather than silent.

use std::collections::VecDeque;

use crate::batcher::Sample;

/// Bounded, coalescing sample queue.
#[derive(Debug)]
pub struct SampleQueue {
    capacity: usize,
    items: VecDeque<Sample>,
    coalesced: u64,
    dropped: u64,
}

/// What [`SampleQueue::push`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The sample was queued.
    Queued,
    /// The queue was full and a stale sample for the same tag was replaced.
    Coalesced,
    /// The queue was full and the oldest sample of another tag was discarded.
    DroppedOldest,
}

impl SampleQueue {
    /// Creates a queue holding at most `capacity` samples.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            items: VecDeque::with_capacity(capacity.min(512)),
            coalesced: 0,
            dropped: 0,
        }
    }

    /// Number of queued samples.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Samples replaced by a newer value for the same tag because of overflow.
    pub fn coalesced(&self) -> u64 {
        self.coalesced
    }

    /// Samples discarded entirely because of overflow.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Enqueues a sample, applying the overflow policy when full.
    pub fn push(&mut self, sample: Sample) -> PushOutcome {
        if self.items.len() < self.capacity {
            self.items.push_back(sample);
            return PushOutcome::Queued;
        }

        // Full: prefer superseding a stale value for the same tag over losing
        // another tag's only observation.
        if let Some(slot) = self.items.iter_mut().find(|s| s.address == sample.address) {
            *slot = sample;
            self.coalesced += 1;
            return PushOutcome::Coalesced;
        }

        self.items.pop_front();
        self.dropped += 1;
        self.items.push_back(sample);
        PushOutcome::DroppedOldest
    }

    /// Removes and returns the oldest sample.
    pub fn pop(&mut self) -> Option<Sample> {
        self.items.pop_front()
    }

    /// Removes up to `n` samples, oldest first.
    pub fn drain(&mut self, n: usize) -> Vec<Sample> {
        let n = n.min(self.items.len());
        self.items.drain(..n).collect()
    }

    /// Discards everything queued; used when the configuration version changes
    /// and in-flight samples would be attributed to the wrong `cfg.v`.
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::TagValue;

    fn s(addr: &str, ts: i64) -> Sample {
        Sample {
            address: addr.into(),
            ts_ms: ts,
            value: TagValue::F64(ts as f64),
            status: 0,
            cfg_v: 1,
        }
    }

    #[test]
    fn queues_in_order_until_full() {
        let mut q = SampleQueue::new(3);
        assert_eq!(q.push(s("a", 1)), PushOutcome::Queued);
        assert_eq!(q.push(s("b", 2)), PushOutcome::Queued);
        assert_eq!(q.push(s("c", 3)), PushOutcome::Queued);
        assert_eq!(q.len(), 3);
        assert_eq!(q.pop().unwrap().address, "a");
    }

    #[test]
    fn overflow_supersedes_the_same_tag_first() {
        let mut q = SampleQueue::new(2);
        q.push(s("a", 1));
        q.push(s("b", 2));
        assert_eq!(q.push(s("a", 3)), PushOutcome::Coalesced);

        assert_eq!(q.len(), 2);
        assert_eq!(q.coalesced(), 1);
        assert_eq!(q.dropped(), 0);
        // "a" keeps its slot but carries the newest value; "b" survives.
        let all: Vec<_> = q.drain(2);
        assert_eq!(all[0].address, "a");
        assert_eq!(all[0].ts_ms, 3);
        assert_eq!(all[1].address, "b");
    }

    #[test]
    fn overflow_drops_the_oldest_when_no_tag_matches() {
        let mut q = SampleQueue::new(2);
        q.push(s("a", 1));
        q.push(s("b", 2));
        assert_eq!(q.push(s("c", 3)), PushOutcome::DroppedOldest);

        assert_eq!(q.dropped(), 1);
        assert_eq!(q.coalesced(), 0);
        let all: Vec<_> = q.drain(2);
        assert_eq!(all[0].address, "b");
        assert_eq!(all[1].address, "c");
    }

    #[test]
    fn no_tag_starves_under_sustained_overflow() {
        let mut q = SampleQueue::new(4);
        for round in 0..100 {
            for tag in ["a", "b", "c", "d"] {
                q.push(s(tag, round));
            }
        }
        let mut addrs: Vec<_> = q.drain(4).into_iter().map(|s| s.address).collect();
        addrs.sort();
        assert_eq!(addrs, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn drain_is_clamped_to_available_items() {
        let mut q = SampleQueue::new(8);
        q.push(s("a", 1));
        assert_eq!(q.drain(100).len(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn clear_discards_everything() {
        let mut q = SampleQueue::new(8);
        q.push(s("a", 1));
        q.clear();
        assert!(q.is_empty());
    }
}
