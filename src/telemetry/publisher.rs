//! Turning queued samples into MQTT publishes.
//!
//! The spike published one message per tag per sample. At 250 tags on a one
//! second scan that is 250 publishes/s against an AWS IoT Core limit of 100/s
//! per connection: the connection gets throttled, then closed. This publisher
//! batches (see `gateway_core::batcher`) and rate-limits, so the same workload
//! costs roughly one publish per second.

use std::collections::VecDeque;

use esp_idf_svc::mqtt::client::QoS;

use gateway_core::batcher::{Batch, Batcher, FlushReason};
use gateway_core::settings::TelemetrySettings;

use crate::mqtt_util::MqttTransport;

/// Minimum spacing between telemetry publishes.
///
/// Well inside the 100/s AWS limit, and leaves headroom for the shadow, Jobs
/// and OTA traffic that share the same connection.
const MIN_PUBLISH_INTERVAL_MS: i64 = 20;

/// How many encoded batches may wait for the link. Each one is up to
/// `batch_max_bytes`, so this is a hard memory commitment, not a soft queue.
const MAX_PENDING_BATCHES: usize = 2;

/// Batches samples and publishes them under a rate limit.
pub struct Publisher {
    topic: String,
    qos: QoS,
    max_bytes: usize,
    batcher: Batcher,
    pending: VecDeque<Vec<u8>>,
    last_publish_ms: i64,
    dropped_batches: u64,
}

impl Publisher {
    /// Creates a publisher for the given validated telemetry settings.
    ///
    /// Batches take their config version from the samples themselves, so a
    /// publisher does not need to know which version it is publishing for.
    pub fn new(settings: &TelemetrySettings) -> Self {
        Self {
            topic: settings.topic.clone(),
            qos: match settings.qos {
                0 => QoS::AtMostOnce,
                _ => QoS::AtLeastOnce,
            },
            max_bytes: settings.batch_max_bytes,
            batcher: Batcher::new(settings),
            pending: VecDeque::new(),
            last_publish_ms: 0,
            dropped_batches: 0,
        }
    }

    /// Batches that had to be discarded because the link could not keep up.
    pub fn dropped_batches(&self) -> u64 {
        self.dropped_batches
    }

    /// Feeds samples in, flushing whenever the count or size policy fires.
    pub fn ingest(&mut self, samples: Vec<gateway_core::batcher::Sample>, now_ms: i64) {
        for sample in samples {
            if let Some((batch, reason)) = self.batcher.push(sample, now_ms) {
                self.enqueue(batch, reason);
            }
        }
    }

    /// Drives age-based flushing and the send queue. Call on every loop turn.
    pub fn tick(&mut self, client: &mut impl MqttTransport, now_ms: i64) {
        if let Some((batch, reason)) = self.batcher.poll(now_ms) {
            self.enqueue(batch, reason);
        }

        while !self.pending.is_empty() {
            if now_ms.saturating_sub(self.last_publish_ms) < MIN_PUBLISH_INTERVAL_MS {
                break;
            }
            let Some(payload) = self.pending.pop_front() else {
                break;
            };
            match client.publish(&self.topic, self.qos, false, &payload) {
                Ok(()) => {
                    self.last_publish_ms = now_ms;
                    log::debug!("published {} B to {}", payload.len(), self.topic);
                }
                Err(e) => {
                    // Put it back and stop: the link is down, and hammering it
                    // only burns heap. The bounded queue drops the oldest if
                    // this persists.
                    log::warn!("telemetry publish failed: {e:#}");
                    self.pending.push_front(payload);
                    break;
                }
            }
        }
    }

    /// Flushes whatever is buffered, e.g. before a configuration change.
    pub fn flush(&mut self, client: &mut impl MqttTransport, now_ms: i64) {
        if let Some((batch, reason)) = self.batcher.flush(now_ms) {
            self.enqueue(batch, reason);
        }
        self.last_publish_ms = 0;
        self.tick(client, now_ms);
    }

    fn enqueue(&mut self, batch: Batch, reason: FlushReason) {
        let count = batch.d.len();
        let payload = match serde_json::to_vec(&batch) {
            Ok(p) => p,
            Err(e) => {
                // Unreachable in practice: every `TagValue` is serialisable.
                log::error!("could not encode telemetry batch: {e}");
                self.dropped_batches += 1;
                return;
            }
        };

        // The batcher works from a pessimistic size estimate; this is the
        // authoritative check, and it protects the broker's payload limit.
        if payload.len() > self.max_bytes {
            log::error!(
                "dropping {count}-sample batch: encoded to {} B, budget is {} B",
                payload.len(),
                self.max_bytes
            );
            self.dropped_batches += 1;
            return;
        }

        if self.pending.len() >= MAX_PENDING_BATCHES {
            self.pending.pop_front();
            self.dropped_batches += 1;
            log::warn!("telemetry backlog full; dropped the oldest batch");
        }
        log::debug!(
            "queued {count}-sample batch ({reason:?}), {} B",
            payload.len()
        );
        self.pending.push_back(payload);
    }
}
