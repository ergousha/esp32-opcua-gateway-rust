//! Telemetry batching.
//!
//! The spike published one MQTT message per tag per sample. At 250 tags on a
//! 1 s scan that is 250 publishes/s against an AWS IoT Core limit of 100/s per
//! connection — the connection would be throttled and eventually dropped.
//! Batching collapses that to roughly one publish per second.
//!
//! A batch is flushed on whichever comes first: item count, estimated encoded
//! size, or the age of the oldest buffered sample. Time is injected so the
//! policy is testable without sleeping.

use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};

use crate::settings::TelemetrySettings;
use crate::value::{is_good, TagValue};

/// One tag observation.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// Bare tag address; the key the cloud side joins on.
    pub address: String,
    /// Source timestamp from the server, ms since the Unix epoch.
    pub ts_ms: i64,
    /// The value, already reduced to a JSON-representable form.
    pub value: TagValue,
    /// OPC UA StatusCode. Omitted from the wire format when Good.
    pub status: u32,
}

impl Sample {
    /// Pessimistic encoded size, including the row's brackets and commas.
    pub fn estimated_bytes(&self) -> usize {
        // ["address",1753660799871,<value>] plus an optional status field.
        self.address.len() + 2 + 1 + 15 + 1 + self.value.estimated_bytes() + 2
            + if is_good(self.status) { 0 } else { 12 }
    }
}

/// Rows are positional arrays, not objects: at 250 tags the repeated
/// `"address":`/`"value":` keys would roughly double the payload.
impl Serialize for Sample {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let good = is_good(self.status);
        let mut seq = ser.serialize_seq(Some(if good { 3 } else { 4 }))?;
        seq.serialize_element(&self.address)?;
        seq.serialize_element(&self.ts_ms)?;
        seq.serialize_element(&self.value)?;
        if !good {
            seq.serialize_element(&self.status)?;
        }
        seq.end()
    }
}

/// A batch as published to the telemetry topic.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Batch {
    /// Device time the batch was assembled, ms since the Unix epoch.
    pub t: i64,
    /// Config version the samples were collected under.
    pub v: u32,
    /// The rows.
    pub d: Vec<Sample>,
}

/// Why a batch was flushed. Useful for diagnosing a mis-tuned configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushReason {
    /// `batch_max_items` reached.
    Count,
    /// `batch_max_bytes` would be exceeded by the next sample.
    Bytes,
    /// The oldest sample reached `batch_max_age_ms`.
    Age,
    /// The caller asked for everything buffered (shutdown, config change).
    Forced,
}

/// Accumulates samples and hands out batches according to the configured
/// count/size/age policy.
#[derive(Debug)]
pub struct Batcher {
    max_items: usize,
    max_bytes: usize,
    max_age_ms: i64,
    cfg_version: u32,
    buf: Vec<Sample>,
    bytes: usize,
    oldest_ms: Option<i64>,
}

/// Fixed overhead of the `{"t":...,"v":...,"d":[]}` envelope.
const ENVELOPE_BYTES: usize = 40;

impl Batcher {
    /// Creates a batcher from validated telemetry settings.
    pub fn new(settings: &TelemetrySettings, cfg_version: u32) -> Self {
        Self {
            max_items: settings.batch_max_items.max(1),
            max_bytes: settings.batch_max_bytes.max(ENVELOPE_BYTES + 64),
            max_age_ms: settings.batch_max_age_ms as i64,
            cfg_version,
            buf: Vec::new(),
            bytes: ENVELOPE_BYTES,
            oldest_ms: None,
        }
    }

    /// Number of buffered samples.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Adds a sample, returning a batch when the size or count limit is hit.
    ///
    /// A sample that would push the buffer past `batch_max_bytes` flushes the
    /// *previous* contents first and then starts the next batch, so no batch
    /// ever exceeds the budget — the alternative (flush after adding) can
    /// produce an oversized, unpublishable payload.
    pub fn push(&mut self, sample: Sample, now_ms: i64) -> Option<(Batch, FlushReason)> {
        let size = sample.estimated_bytes() + 1;
        let mut flushed = None;

        if !self.buf.is_empty() && self.bytes + size > self.max_bytes {
            flushed = self.take(now_ms).map(|b| (b, FlushReason::Bytes));
        }

        self.oldest_ms.get_or_insert(now_ms);
        self.bytes += size;
        self.buf.push(sample);

        if flushed.is_none() && self.buf.len() >= self.max_items {
            flushed = self.take(now_ms).map(|b| (b, FlushReason::Count));
        }

        flushed
    }

    /// Flushes when the oldest buffered sample has aged out. Call periodically.
    ///
    /// Returns `None` for an empty buffer: publishing empty batches would burn
    /// the publish-rate budget for nothing.
    pub fn poll(&mut self, now_ms: i64) -> Option<(Batch, FlushReason)> {
        let oldest = self.oldest_ms?;
        if now_ms.saturating_sub(oldest) >= self.max_age_ms {
            self.take(now_ms).map(|b| (b, FlushReason::Age))
        } else {
            None
        }
    }

    /// Flushes everything buffered regardless of policy.
    pub fn flush(&mut self, now_ms: i64) -> Option<(Batch, FlushReason)> {
        self.take(now_ms).map(|b| (b, FlushReason::Forced))
    }

    fn take(&mut self, now_ms: i64) -> Option<Batch> {
        if self.buf.is_empty() {
            return None;
        }
        self.bytes = ENVELOPE_BYTES;
        self.oldest_ms = None;
        Some(Batch {
            t: now_ms,
            v: self.cfg_version,
            d: std::mem::take(&mut self.buf),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(items: usize, bytes: usize, age: u64) -> TelemetrySettings {
        TelemetrySettings {
            topic: "dt/gw/opcua".into(),
            qos: 1,
            batch_max_items: items,
            batch_max_bytes: bytes,
            batch_max_age_ms: age,
        }
    }

    fn sample(addr: &str, status: u32) -> Sample {
        Sample {
            address: addr.into(),
            ts_ms: 1_753_660_799_871,
            value: TagValue::F64(23.5),
            status,
        }
    }

    #[test]
    fn wire_format_matches_the_specification() {
        let batch = Batch {
            t: 1_753_660_800_000,
            v: 7,
            d: vec![
                Sample { address: "slow".into(), ts_ms: 1_753_660_799_871, value: TagValue::F64(23.5), status: 0 },
                Sample { address: "fast".into(), ts_ms: 1_753_660_799_902, value: TagValue::Bool(true), status: 0 },
                Sample { address: "txt".into(), ts_ms: 1_753_660_799_902, value: TagValue::Str("RUN".into()), status: 0 },
                Sample { address: "bad".into(), ts_ms: 1_753_660_799_910, value: TagValue::Null, status: 2_153_775_104 },
            ],
        };
        assert_eq!(
            serde_json::to_string(&batch).unwrap(),
            r#"{"t":1753660800000,"v":7,"d":[["slow",1753660799871,23.5],["fast",1753660799902,true],["txt",1753660799902,"RUN"],["bad",1753660799910,null,2153775104]]}"#
        );
    }

    #[test]
    fn status_is_omitted_only_when_good() {
        let good = serde_json::to_string(&sample("a", 0)).unwrap();
        assert_eq!(good, r#"["a",1753660799871,23.5]"#);
        let bad = serde_json::to_string(&sample("a", 0x8034_0000)).unwrap();
        assert_eq!(bad, r#"["a",1753660799871,23.5,2150891520]"#);
    }

    #[test]
    fn flushes_on_item_count() {
        let mut b = Batcher::new(&settings(3, 64_000, 1_000), 7);
        assert!(b.push(sample("a", 0), 0).is_none());
        assert!(b.push(sample("b", 0), 0).is_none());
        let (batch, why) = b.push(sample("c", 0), 0).unwrap();
        assert_eq!(why, FlushReason::Count);
        assert_eq!(batch.d.len(), 3);
        assert!(b.is_empty());
    }

    #[test]
    fn flushes_on_byte_budget_without_exceeding_it() {
        let budget = 200;
        let mut b = Batcher::new(&settings(1_000, budget, 60_000), 7);
        let mut flushes = 0;
        for i in 0..40 {
            if let Some((batch, why)) = b.push(sample(&format!("tag{i:03}"), 0), 0) {
                assert_eq!(why, FlushReason::Bytes);
                flushes += 1;
                let encoded = serde_json::to_vec(&batch).unwrap().len();
                assert!(encoded <= budget, "batch encoded to {encoded} B > {budget} B");
            }
        }
        assert!(flushes > 0, "byte budget never triggered");
    }

    #[test]
    fn flushes_on_age() {
        let mut b = Batcher::new(&settings(1_000, 64_000, 1_000), 7);
        b.push(sample("a", 0), 10_000);
        assert!(b.poll(10_500).is_none());
        let (batch, why) = b.poll(11_000).unwrap();
        assert_eq!(why, FlushReason::Age);
        assert_eq!(batch.d.len(), 1);
        assert_eq!(batch.t, 11_000);
    }

    #[test]
    fn age_is_measured_from_the_oldest_sample_not_the_newest() {
        let mut b = Batcher::new(&settings(1_000, 64_000, 1_000), 7);
        b.push(sample("a", 0), 10_000);
        b.push(sample("b", 0), 10_900);
        assert!(b.poll(11_000).is_some(), "oldest sample had aged out");
    }

    #[test]
    fn empty_flushes_are_suppressed() {
        let mut b = Batcher::new(&settings(10, 64_000, 100), 7);
        assert!(b.poll(1_000_000).is_none());
        assert!(b.flush(1_000_000).is_none());
    }

    #[test]
    fn batch_carries_the_config_version() {
        let mut b = Batcher::new(&settings(1, 64_000, 1_000), 42);
        let (batch, _) = b.push(sample("a", 0), 0).unwrap();
        assert_eq!(batch.v, 42);
    }
}
