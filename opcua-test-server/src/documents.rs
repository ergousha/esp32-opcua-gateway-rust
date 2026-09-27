//! The cloud half of the fixture: the two-plane configuration the gateway
//! expects, built from the catalogue.
//!
//! * control plane — `state.desired` of the `opcua` named shadow, carrying the
//!   small settings and, in `cfg`, a *pointer* to the tag list;
//! * data plane — the tag bundle itself, which the cloud publishes retained.
//!
//! Built with plain `serde_json` on purpose, not with `gateway-core`'s types.
//! These documents stand in for a producer the firmware does not control, so
//! they must be written independently of the parser they are fed to; sharing
//! the types would let a schema bug agree with itself and pass.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::catalogue::{Tag, NAMESPACE_URI};

/// A tag bundle exactly as it is published.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// Bundle version; `cfg.v` must match it.
    pub version: u32,
    /// The exact bytes the digest is computed over.
    pub payload: Vec<u8>,
    /// Lowercase hex SHA-256 of `payload`.
    pub sha256: String,
    /// Number of tags in the bundle.
    pub count: usize,
    /// Retained topic the bundle is published on.
    pub topic: String,
}

/// Builds the grouped wire form of the tag bundle and its digest.
///
/// Byte-deterministic: groups are ordered by scan rate and tags keep their
/// catalogue order, so republishing an unchanged tag set yields the same
/// SHA-256 — which is what makes the gateway's idempotency check meaningful.
pub fn bundle(thing: &str, version: u32, tags: &[Tag]) -> Bundle {
    // Structs rather than `json!`, which would sort the keys: the documented
    // wire form is `{"v":…,"g":[{"r":…,"a":[…]}]}`, in that order.
    #[derive(Serialize)]
    struct Wire<'a> {
        v: u32,
        g: Vec<Group<'a>>,
    }
    #[derive(Serialize)]
    struct Group<'a> {
        r: u32,
        a: Vec<&'a str>,
    }

    let mut by_rate: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
    for tag in tags {
        by_rate
            .entry(tag.scan_rate_ms)
            .or_default()
            .push(tag.address);
    }
    let wire = Wire {
        v: version,
        g: by_rate.into_iter().map(|(r, a)| Group { r, a }).collect(),
    };

    // `serde_json::to_vec` never emits incidental whitespace, and the digest
    // is over these exact bytes: the separators are part of the contract.
    let payload = serde_json::to_vec(&wire).expect("the bundle always serialises");
    let sha256 = Sha256::digest(&payload)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    Bundle {
        version,
        payload,
        sha256,
        count: tags.len(),
        topic: bundle_topic(thing, version),
    }
}

/// Retained topic the bundle for `version` is published on.
pub fn bundle_topic(thing: &str, version: u32) -> String {
    format!("cmd/{thing}/opcua/tags/v{version}")
}

/// Telemetry topic of `thing`.
pub fn telemetry_topic(thing: &str) -> String {
    format!("dt/{thing}/opcua")
}

/// Knobs for `state.desired`.
///
/// The overrides exist so negative scenarios can ship a document that is
/// well-formed but must be REFUSED — a non-`None` security policy, a digest
/// that does not match the bundle. Checking that the gateway rejects those
/// loudly matters more than checking that it accepts good ones.
#[derive(Debug, Clone)]
pub struct Desired {
    /// `opc.tcp://` endpoint the gateway should dial.
    pub endpoint: String,
    /// Fallback namespace index.
    pub ns: u16,
    /// Namespace URI; `None` leaves the field out.
    pub ns_uri: Option<String>,
    /// Master switch.
    pub enabled: bool,
    /// SecurityPolicy name.
    pub sec_policy: String,
    /// MessageSecurityMode name.
    pub sec_mode: String,
    /// Session timeout.
    pub session_timeout_ms: u32,
    /// Keep-alive interval.
    pub keepalive_ms: u32,
    /// Subscription publishing interval.
    pub publish_ms: u32,
    /// Batch count limit.
    pub batch_max_items: usize,
    /// Batch size limit.
    pub batch_max_bytes: usize,
    /// Batch age limit.
    pub batch_max_age_ms: u64,
    /// Replaces the bundle's real digest in `cfg.sha256`.
    pub sha256_override: Option<String>,
}

impl Desired {
    /// A document the gateway should accept, pointing at `endpoint`.
    pub fn new(endpoint: impl Into<String>, ns: u16) -> Self {
        Self {
            endpoint: endpoint.into(),
            ns,
            ns_uri: Some(NAMESPACE_URI.to_string()),
            enabled: true,
            sec_policy: "None".into(),
            sec_mode: "None".into(),
            session_timeout_ms: 60_000,
            keepalive_ms: 10_000,
            publish_ms: 1_000,
            batch_max_items: 100,
            batch_max_bytes: 16_384,
            batch_max_age_ms: 2_000,
            sha256_override: None,
        }
    }

    /// Renders `state.desired` for `thing`, pointing at `bundle`.
    pub fn to_json(&self, thing: &str, bundle: &Bundle) -> Value {
        let mut instance = json!({
            "endpoint": self.endpoint,
            "ns": self.ns,
            "id_type": "s",
            "sec_mode": self.sec_mode,
            "sec_policy": self.sec_policy,
            "session_timeout_ms": self.session_timeout_ms,
            "keepalive_ms": self.keepalive_ms,
            "publish_ms": self.publish_ms,
        });
        if let Some(uri) = &self.ns_uri {
            instance["ns_uri"] = json!(uri);
        }

        json!({
            "enabled": self.enabled,
            "instance": instance,
            "telemetry": {
                "topic": telemetry_topic(thing),
                "qos": 1,
                "batch_max_items": self.batch_max_items,
                "batch_max_bytes": self.batch_max_bytes,
                "batch_max_age_ms": self.batch_max_age_ms,
            },
            "cfg": {
                "v": bundle.version,
                "n": bundle.count,
                "sha256": self.sha256_override.as_deref().unwrap_or(&bundle.sha256),
                "topic": bundle.topic,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue;

    #[test]
    fn bundle_is_compact_grouped_and_deterministic() {
        let a = bundle("thing", 1, &catalogue::tags());
        let b = bundle("thing", 1, &catalogue::tags());
        assert_eq!(a.payload, b.payload);
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.sha256.len(), 64);

        let text = String::from_utf8(a.payload.clone()).unwrap();
        assert!(!text.contains(' '), "no incidental whitespace: {text}");
        assert!(
            text.starts_with(r#"{"v":1,"g":[{"r":1000,"a":["Line1.Temp","#),
            "documented field order: {text}"
        );

        let doc: Value = serde_json::from_slice(&a.payload).unwrap();
        let rates: Vec<u64> = doc["g"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["r"].as_u64().unwrap())
            .collect();
        assert_eq!(rates, vec![1_000, 5_000]);
        assert_eq!(a.count, catalogue::tags().len());
        assert_eq!(a.topic, "cmd/thing/opcua/tags/v1");
    }

    #[test]
    fn bundle_is_byte_identical_to_the_documented_contract() {
        // The v1 bundle captured from the real bring-up run
        // (docs/OPCUA_INTEGRATION_TEST.md §5.1). A different digest means the
        // wire form changed, and every device holding a cached bundle would
        // see it as a new configuration.
        let bundle = bundle("28848553144F", 1, &catalogue::tags());
        assert_eq!(bundle.payload.len(), 280);
        assert_eq!(
            bundle.sha256,
            "d037a0b510d550e108342d39fa080ff7b1588509169d19ced87d911a8ad50754"
        );
    }

    #[test]
    fn desired_points_at_the_bundle() {
        let bundle = bundle("thing", 3, &catalogue::present());
        let doc = Desired::new("opc.tcp://127.0.0.1:4855/x", 2).to_json("thing", &bundle);
        assert_eq!(doc["cfg"]["v"], 3);
        assert_eq!(doc["cfg"]["n"], catalogue::present().len());
        assert_eq!(doc["cfg"]["sha256"], bundle.sha256.as_str());
        assert_eq!(doc["telemetry"]["topic"], "dt/thing/opcua");
        assert_eq!(doc["instance"]["ns_uri"], NAMESPACE_URI);
    }
}
