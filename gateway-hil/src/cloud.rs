//! The cloud half of the harness: the two-plane configuration the firmware
//! expects, plus the observability needed to assert on what the device did.
//!
//! Two planes, because a 250-tag list does not fit in AWS IoT's 8 KB shadow
//! (`docs/OPCUA_CLIENT_REQUIREMENTS.md` §3):
//!
//! * control plane — the `opcua` NAMED shadow carries the small stuff and, in
//!   `cfg`, a *pointer* to the tag list: version, count, SHA-256, topic;
//! * data plane — the tag bundle itself, published RETAINED on that topic so a
//!   device that reboots gets it without any request/response dance.
//!
//! Everything here goes through IAM-authorised HTTPS — `UpdateThingShadow`,
//! `GetThingShadow`, `Publish` with `retain`, `GetRetainedMessage`, and
//! CloudWatch `FilterLogEvents` — so the harness needs no device certificate
//! of its own. Telemetry is observed through the `dt/+/opcua` IoT rule rather
//! than by subscribing, which is what keeps that true.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_sdk_iotdataplane::primitives::Blob;
use gateway_core::shadow::SHADOW_NAME;
use opcua_test_server::documents::Bundle;
use serde_json::{json, Value};

/// CloudWatch log group fed by the `dt/+/opcua` IoT rule (iot-platform-infra).
pub const TELEMETRY_LOG_GROUP: &str = "/esp32-ztp/opcua-telemetry";

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Thin wrapper over the two AWS APIs the harness needs.
pub struct Cloud {
    thing: String,
    data: aws_sdk_iotdataplane::Client,
    logs: aws_sdk_cloudwatchlogs::Client,
    /// Reject a `reported` block AWS last wrote before this instant.
    fresh_since_ms: Option<i64>,
    /// When AWS last wrote any `reported` field, as of the latest poll.
    last_reported_ts_ms: Option<i64>,
}

impl Cloud {
    /// Credentials come from the usual chain (`source aws-env.sh`, a profile,
    /// SSO…). `iot_endpoint` is the account's ATS data endpoint — the same
    /// host the firmware's MQTT connection uses.
    pub async fn new(thing: &str, region: &str, iot_endpoint: &str) -> Result<Self> {
        let shared = aws_config::from_env()
            .region(aws_config::Region::new(region.to_string()))
            .load()
            .await;
        let data_config = aws_sdk_iotdataplane::config::Builder::from(&shared)
            .endpoint_url(format!("https://{iot_endpoint}"))
            .build();
        Ok(Self {
            thing: thing.to_string(),
            data: aws_sdk_iotdataplane::Client::from_conf(data_config),
            logs: aws_sdk_cloudwatchlogs::Client::new(&shared),
            fresh_since_ms: None,
            last_reported_ts_ms: None,
        })
    }

    /// Treats any `reported` block older than `t0_ms` as no report at all.
    ///
    /// The shadow persists across boots, so a device that is dead, unflashed
    /// or off the network still answers `GetThingShadow` — with whatever it
    /// last managed to report, possibly hours ago. Evaluated naively, that
    /// document makes a device that never booted look like one stuck in
    /// `connecting`, and every downstream assertion then describes firmware
    /// that is not running.
    pub fn require_fresh_since(&mut self, t0_ms: i64) {
        self.fresh_since_ms = Some(t0_ms);
    }

    // -- data plane --------------------------------------------------------

    /// Publishes the tag bundle RETAINED: the device subscribes only after it
    /// has read the shadow, long after the publish.
    pub async fn publish_bundle(&self, bundle: &Bundle) -> Result<()> {
        self.publish_retained(&bundle.topic, bundle.payload.clone())
            .await
    }

    /// Clears a retained message (an empty retained payload deletes it).
    pub async fn clear_retained(&self, topic: &str) -> Result<()> {
        self.publish_retained(topic, Vec::new()).await
    }

    /// The message retained on `topic`, if there is one.
    pub async fn get_retained(&self, topic: &str) -> Result<Option<Vec<u8>>> {
        match self.data.get_retained_message().topic(topic).send().await {
            Ok(out) => Ok(out
                .payload()
                .map(|b| b.as_ref().to_vec())
                .filter(|p| !p.is_empty())),
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_resource_not_found_exception()) =>
            {
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("GetRetainedMessage {topic}")),
        }
    }

    pub async fn publish_retained(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        self.data
            .publish()
            .topic(topic)
            .qos(1)
            .retain(true)
            .payload(Blob::new(payload))
            .send()
            .await
            .with_context(|| format!("publishing retained to {topic}"))?;
        Ok(())
    }

    // -- control plane -----------------------------------------------------

    pub async fn update_desired(&self, desired: &Value) -> Result<()> {
        let payload = serde_json::to_vec(&json!({ "state": { "desired": desired } }))?;
        self.data
            .update_thing_shadow()
            .thing_name(&self.thing)
            .shadow_name(SHADOW_NAME)
            .payload(Blob::new(payload))
            .send()
            .await
            .context("UpdateThingShadow")?;
        Ok(())
    }

    pub async fn get_shadow(&self) -> Result<Option<Value>> {
        match self
            .data
            .get_thing_shadow()
            .thing_name(&self.thing)
            .shadow_name(SHADOW_NAME)
            .send()
            .await
        {
            Ok(out) => {
                let bytes = out
                    .payload()
                    .map(|b| b.as_ref().to_vec())
                    .unwrap_or_default();
                Ok(Some(
                    serde_json::from_slice(&bytes).context("shadow is not JSON")?,
                ))
            }
            Err(e)
                if e.as_service_error()
                    .is_some_and(|se| se.is_resource_not_found_exception()) =>
            {
                Ok(None)
            }
            Err(e) => Err(e).context("GetThingShadow"),
        }
    }

    /// The `reported` block and when AWS last wrote any of it.
    pub async fn reported_with_ts(&self) -> Result<(Value, Option<i64>)> {
        let doc = self.get_shadow().await?.unwrap_or(Value::Null);
        let reported = doc["state"]["reported"].clone();
        let reported = if reported.is_object() {
            reported
        } else {
            json!({})
        };
        Ok((reported, newest_timestamp_ms(&doc["metadata"]["reported"])))
    }

    /// Why the last poll was rejected as stale, if it was.
    pub fn stale_note(&self) -> Option<String> {
        let fresh_since = self.fresh_since_ms?;
        let Some(last) = self.last_reported_ts_ms else {
            return Some("thing has never reported to the opcua shadow".into());
        };
        if last >= fresh_since {
            return None;
        }
        Some(format!(
            "device has not reported since the run started (shadow last written {} s \
             before it) — the device is not running this firmware, not merely slow",
            (fresh_since - last) / 1000
        ))
    }

    /// Polls `reported` until `predicate` holds or time runs out.
    ///
    /// A document older than the run is never handed to `predicate`:
    /// substituting an empty one would be worse than useless, since any
    /// predicate phrased as an absence would pass against a device that is
    /// not even powered on.
    pub async fn wait_for_reported(
        &mut self,
        predicate: impl Fn(&Value) -> bool,
        timeout: Duration,
        on_poll: impl Fn(&Value, bool),
    ) -> (bool, Value) {
        let deadline = Instant::now() + timeout;
        let mut last = json!({});
        while Instant::now() < deadline {
            match self.reported_with_ts().await {
                Ok((reported, ts)) => {
                    last = reported;
                    self.last_reported_ts_ms = ts;
                    let fresh = self.stale_note().is_none();
                    on_poll(&last, fresh);
                    if fresh && predicate(&last) {
                        return (true, last);
                    }
                }
                Err(e) => log::warn!("shadow poll failed: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        (false, last)
    }

    // -- telemetry observation --------------------------------------------

    /// Batches the `dt/+/opcua` IoT rule wrote to CloudWatch Logs, oldest
    /// first.
    pub async fn telemetry_since(&self, start_ms: i64) -> Vec<Value> {
        const LIMIT: usize = 2_000;
        let mut batches = Vec::new();
        let mut pages = self
            .logs
            .filter_log_events()
            .log_group_name(TELEMETRY_LOG_GROUP)
            .start_time(start_ms)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = match page {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("FilterLogEvents failed: {e}");
                    break;
                }
            };
            for event in page.events() {
                if let Some(Ok(batch)) = event.message().map(serde_json::from_str::<Value>) {
                    batches.push(batch);
                }
            }
            if batches.len() >= LIMIT {
                break;
            }
        }
        batches
    }

    /// Waits until at least `minimum` batches have landed; the IoT rule →
    /// CloudWatch hop lags a few seconds behind the publish.
    pub async fn wait_for_telemetry(
        &self,
        start_ms: i64,
        minimum: usize,
        timeout: Duration,
    ) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        let mut batches = Vec::new();
        while Instant::now() < deadline {
            batches = self.telemetry_since(start_ms).await;
            if batches.len() >= minimum {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        batches
    }
}

/// Highest config version a shadow document knows of, desired or reported.
pub fn cfg_version(doc: &Value) -> u32 {
    let desired = doc["state"]["desired"]["cfg"]["v"].as_u64().unwrap_or(0);
    let reported = doc["state"]["reported"]["cfg_v"].as_u64().unwrap_or(0);
    desired.max(reported) as u32
}

/// Newest `timestamp` anywhere under a shadow `metadata` subtree, in ms.
///
/// AWS stamps every reported leaf, in unix *seconds*; the newest stamp is the
/// only evidence that a document came from this boot rather than an old one.
pub fn newest_timestamp_ms(metadata: &Value) -> Option<i64> {
    match metadata {
        Value::Object(map) => {
            let own = map.get("timestamp").and_then(Value::as_i64);
            map.values()
                .filter_map(newest_timestamp_ms)
                .chain(own.map(|s| s * 1000))
                .max()
        }
        Value::Array(items) => items.iter().filter_map(newest_timestamp_ms).max(),
        _ => None,
    }
}

/// Flattens telemetry batches into `{address: [row, …]}`.
///
/// A row is `[address, source_ts_ms, value]`, with an optional 4th element
/// carrying the StatusCode when it is not Good (§4.3).
pub fn rows_by_address(batches: &[Value]) -> BTreeMap<String, Vec<Vec<Value>>> {
    let mut out: BTreeMap<String, Vec<Vec<Value>>> = BTreeMap::new();
    for batch in batches {
        for row in batch["d"].as_array().into_iter().flatten() {
            let Some(row) = row.as_array() else { continue };
            if let Some(address) = row.first().and_then(Value::as_str) {
                out.entry(address.to_string())
                    .or_default()
                    .push(row.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_timestamp_walks_the_whole_metadata_tree() {
        let metadata = json!({
            "state": { "timestamp": 100 },
            "failed_sample": [ { "a": { "timestamp": 250 } } ],
            "cfg_v": { "timestamp": 200 },
        });
        assert_eq!(newest_timestamp_ms(&metadata), Some(250_000));
        assert_eq!(newest_timestamp_ms(&json!({})), None);
        assert_eq!(newest_timestamp_ms(&Value::Null), None);
    }

    #[test]
    fn rows_are_grouped_by_address_in_order() {
        let batches = vec![
            json!({"t": 1, "v": 1, "d": [["a", 1, 1.0], ["b", 1, true]]}),
            json!({"t": 2, "v": 1, "d": [["a", 2, 2.0, 2150891520u32]]}),
            json!({"t": 3, "v": 1}),
        ];
        let rows = rows_by_address(&batches);
        assert_eq!(rows["a"].len(), 2);
        assert_eq!(rows["a"][1].len(), 4);
        assert_eq!(rows["b"][0][2], true);
    }
}
