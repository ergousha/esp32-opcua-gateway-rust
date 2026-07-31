//! The `opcua` named shadow document: what the cloud asks for, and what the
//! device reports back.
//!
//! Only the *pointer* to the tag list lives here (`cfg`); the list itself is
//! far too big for the 8 KB AWS IoT shadow limit and travels on a retained MQTT
//! topic instead (see [`crate::bundle`]).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::node::IdType;
use crate::MAX_TAGS;

/// The only security policy this firmware accepts in phase 1.
pub const SECURITY_NONE: &str = "None";

/// Largest telemetry payload we will ever build.
///
/// AWS IoT Core allows 128 KB per publish, but the binding constraint here is
/// the device: a batch is held as a contiguous `Vec<u8>` on a heap with barely
/// 100 KB free once TLS and MQTT have taken their share. 32 KB is roughly
/// three full sweeps of 250 tags and still leaves room for a queued second
/// batch.
pub const MAX_BATCH_BYTES: usize = 32 * 1024;

/// `state.desired` of the `opcua` named shadow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesiredSettings {
    /// Master switch. When false the driver disconnects and idles.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Which server to talk to and how.
    pub instance: InstanceSettings,
    /// How samples are batched and where they are published.
    #[serde(default)]
    pub telemetry: TelemetrySettings,
    /// Pointer to the out-of-band tag bundle.
    pub cfg: CfgRef,
}

/// OPC UA server connection parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSettings {
    /// `opc.tcp://host:port` endpoint URL.
    pub endpoint: String,
    /// Default namespace index for tag addresses.
    #[serde(default)]
    pub ns: u16,
    /// Optional namespace URI; when the server publishes it, it wins over `ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ns_uri: Option<String>,
    /// Default NodeId identifier flavour for tag addresses.
    #[serde(default)]
    pub id_type: IdType,
    /// MessageSecurityMode name. Phase 1 accepts only `"None"`.
    #[serde(default = "default_security")]
    pub sec_mode: String,
    /// SecurityPolicy name. Phase 1 accepts only `"None"`.
    #[serde(default = "default_security")]
    pub sec_policy: String,
    /// Requested session timeout.
    #[serde(default = "default_session_timeout_ms")]
    pub session_timeout_ms: u32,
    /// Secure-channel keep-alive interval.
    #[serde(default = "default_keepalive_ms")]
    pub keepalive_ms: u32,
    /// Requested subscription publishing interval.
    #[serde(default = "default_publish_ms")]
    pub publish_ms: u32,
}

/// Telemetry batching and destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySettings {
    /// MQTT topic batches are published to.
    #[serde(default)]
    pub topic: String,
    /// MQTT QoS, 0 or 1. AWS IoT Core does not support QoS 2.
    #[serde(default = "default_qos")]
    pub qos: u8,
    /// Flush when this many samples have accumulated.
    #[serde(default = "default_batch_max_items")]
    pub batch_max_items: usize,
    /// Flush before the encoded payload would exceed this many bytes.
    #[serde(default = "default_batch_max_bytes")]
    pub batch_max_bytes: usize,
    /// Flush when the oldest buffered sample reaches this age.
    #[serde(default = "default_batch_max_age_ms")]
    pub batch_max_age_ms: u64,
}

/// Pointer to the tag bundle carried on a retained MQTT topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CfgRef {
    /// Bundle version. Monotonic; also embedded in the bundle itself.
    pub v: u32,
    /// Expected tag count, used to reject a truncated bundle early.
    pub n: usize,
    /// Lowercase hex SHA-256 of the exact bundle payload bytes.
    pub sha256: String,
    /// Retained topic the bundle is published on.
    pub topic: String,
}

fn default_enabled() -> bool {
    true
}
fn default_security() -> String {
    SECURITY_NONE.to_string()
}
fn default_session_timeout_ms() -> u32 {
    60_000
}
fn default_keepalive_ms() -> u32 {
    10_000
}
fn default_publish_ms() -> u32 {
    1_000
}
fn default_qos() -> u8 {
    1
}
fn default_batch_max_items() -> usize {
    200
}
fn default_batch_max_bytes() -> usize {
    16 * 1024
}
fn default_batch_max_age_ms() -> u64 {
    1_000
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            topic: String::new(),
            qos: default_qos(),
            batch_max_items: default_batch_max_items(),
            batch_max_bytes: default_batch_max_bytes(),
            batch_max_age_ms: default_batch_max_age_ms(),
        }
    }
}

/// Why a shadow document was refused.
///
/// Every variant is a *hard* rejection: the firmware keeps running the last
/// known-good configuration and reports the error. Silently coercing a bad
/// value (especially a security setting) is how gateways end up unencrypted
/// without anyone noticing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// Endpoint URL was empty or not `opc.tcp://`.
    BadEndpoint(String),
    /// A security policy or mode other than `None` was requested. Phase 1 has
    /// no key storage, so we refuse rather than downgrade.
    SecurityUnsupported {
        /// The offending field name.
        field: &'static str,
        /// The requested value.
        value: String,
    },
    /// A numeric field was outside its permitted range.
    OutOfRange {
        /// The offending field name.
        field: &'static str,
        /// The requested value, rendered.
        value: String,
        /// Inclusive lower bound.
        min: u64,
        /// Inclusive upper bound.
        max: u64,
    },
    /// The telemetry topic was empty or not publishable.
    BadTopic(String),
    /// More tags than the firmware can hold.
    TooManyTags {
        /// Requested count.
        requested: usize,
        /// Hard cap.
        max: usize,
    },
    /// The `cfg.sha256` field was not a 64-character hex digest.
    BadDigest(String),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsError::BadEndpoint(e) => {
                write!(f, "endpoint must be an opc.tcp:// URL, got {e:?}")
            }
            SettingsError::SecurityUnsupported { field, value } => write!(
                f,
                "{field}={value:?} is not supported; this firmware only implements {SECURITY_NONE:?}"
            ),
            SettingsError::OutOfRange {
                field,
                value,
                min,
                max,
            } => write!(f, "{field}={value} outside {min}..={max}"),
            SettingsError::BadTopic(t) => write!(f, "invalid MQTT topic {t:?}"),
            SettingsError::TooManyTags { requested, max } => {
                write!(f, "{requested} tags requested, firmware cap is {max}")
            }
            SettingsError::BadDigest(d) => write!(f, "cfg.sha256 is not a hex digest: {d:?}"),
        }
    }
}

impl std::error::Error for SettingsError {}

fn range(field: &'static str, value: u64, min: u64, max: u64) -> Result<(), SettingsError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(SettingsError::OutOfRange {
            field,
            value: value.to_string(),
            min,
            max,
        })
    }
}

/// MQTT topics must be non-empty, free of wildcards and of the null character.
fn validate_topic(topic: &str) -> Result<(), SettingsError> {
    if topic.is_empty()
        || topic.len() > 256
        || topic.contains(['+', '#', '\0'])
        || topic.starts_with('$')
    {
        return Err(SettingsError::BadTopic(topic.to_string()));
    }
    Ok(())
}

impl DesiredSettings {
    /// Validates the document against the firmware's hard limits.
    pub fn validate(&self) -> Result<(), SettingsError> {
        self.instance.validate()?;
        self.telemetry.validate()?;
        self.cfg.validate()?;
        Ok(())
    }
}

impl InstanceSettings {
    /// Validates connection parameters.
    pub fn validate(&self) -> Result<(), SettingsError> {
        if !self.endpoint.starts_with("opc.tcp://") || self.endpoint.len() < "opc.tcp://x".len() {
            return Err(SettingsError::BadEndpoint(self.endpoint.clone()));
        }
        // Default-deny: anything but `None` needs a key store we do not have.
        if !self.sec_policy.eq_ignore_ascii_case(SECURITY_NONE) {
            return Err(SettingsError::SecurityUnsupported {
                field: "sec_policy",
                value: self.sec_policy.clone(),
            });
        }
        if !self.sec_mode.eq_ignore_ascii_case(SECURITY_NONE) {
            return Err(SettingsError::SecurityUnsupported {
                field: "sec_mode",
                value: self.sec_mode.clone(),
            });
        }
        range(
            "session_timeout_ms",
            self.session_timeout_ms as u64,
            10_000,
            600_000,
        )?;
        range("keepalive_ms", self.keepalive_ms as u64, 1_000, 60_000)?;
        range("publish_ms", self.publish_ms as u64, 50, 3_600_000)?;
        Ok(())
    }
}

impl TelemetrySettings {
    /// Validates batching parameters.
    pub fn validate(&self) -> Result<(), SettingsError> {
        validate_topic(&self.topic)?;
        range("qos", self.qos as u64, 0, 1)?;
        range("batch_max_items", self.batch_max_items as u64, 1, 2_000)?;
        range(
            "batch_max_bytes",
            self.batch_max_bytes as u64,
            512,
            MAX_BATCH_BYTES as u64,
        )?;
        range("batch_max_age_ms", self.batch_max_age_ms, 100, 60_000)?;
        Ok(())
    }
}

impl CfgRef {
    /// Validates the bundle pointer.
    pub fn validate(&self) -> Result<(), SettingsError> {
        if self.n > MAX_TAGS {
            return Err(SettingsError::TooManyTags {
                requested: self.n,
                max: MAX_TAGS,
            });
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SettingsError::BadDigest(self.sha256.clone()));
        }
        validate_topic(&self.topic)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = r#"{
      "enabled": true,
      "instance": {
        "endpoint": "opc.tcp://172.17.0.1:4855",
        "ns": 2,
        "ns_uri": "urn:example:server",
        "id_type": "s",
        "sec_mode": "None",
        "sec_policy": "None",
        "session_timeout_ms": 60000,
        "keepalive_ms": 10000,
        "publish_ms": 1000
      },
      "telemetry": {
        "topic": "dt/gw-1/opcua",
        "qos": 1,
        "batch_max_items": 200,
        "batch_max_bytes": 16384,
        "batch_max_age_ms": 1000
      },
      "cfg": {
        "v": 7,
        "n": 250,
        "sha256": "9f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a0",
        "topic": "cmd/gw-1/opcua/tags/v7"
      }
    }"#;

    fn golden() -> DesiredSettings {
        serde_json::from_str(GOLDEN).unwrap()
    }

    #[test]
    fn golden_document_parses_and_validates() {
        let s = golden();
        s.validate().unwrap();
        assert_eq!(s.instance.ns, 2);
        assert_eq!(s.instance.id_type, IdType::S);
        assert_eq!(s.cfg.v, 7);
        assert_eq!(s.telemetry.batch_max_bytes, 16384);
    }

    #[test]
    fn golden_document_round_trips() {
        let s = golden();
        let back: DesiredSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn omitted_fields_fall_back_to_defaults() {
        let s: DesiredSettings = serde_json::from_str(
            r#"{"instance":{"endpoint":"opc.tcp://h:4840"},
                "telemetry":{"topic":"dt/gw/opcua"},
                "cfg":{"v":1,"n":1,
                       "sha256":"0011223300112233001122330011223300112233001122330011223300112233",
                       "topic":"cmd/gw/opcua/tags/v1"}}"#,
        )
        .unwrap();
        s.validate().unwrap();
        assert!(s.enabled);
        assert_eq!(s.instance.sec_policy, SECURITY_NONE);
        assert_eq!(s.instance.publish_ms, 1_000);
        assert_eq!(s.telemetry.qos, 1);
        assert_eq!(s.telemetry.batch_max_items, 200);
    }

    #[test]
    fn non_none_security_is_a_hard_failure() {
        let mut s = golden();
        s.instance.sec_policy = "Basic256Sha256".into();
        assert_eq!(
            s.validate(),
            Err(SettingsError::SecurityUnsupported {
                field: "sec_policy",
                value: "Basic256Sha256".into()
            })
        );

        let mut s = golden();
        s.instance.sec_mode = "SignAndEncrypt".into();
        assert!(matches!(
            s.validate(),
            Err(SettingsError::SecurityUnsupported { field: "sec_mode", .. })
        ));
    }

    #[test]
    fn endpoint_scheme_is_enforced() {
        let mut s = golden();
        s.instance.endpoint = "http://172.17.0.1:4855".into();
        assert!(matches!(s.validate(), Err(SettingsError::BadEndpoint(_))));
    }

    #[test]
    fn publish_interval_bounds() {
        let mut s = golden();
        s.instance.publish_ms = 10;
        assert!(matches!(
            s.validate(),
            Err(SettingsError::OutOfRange { field: "publish_ms", .. })
        ));
        s.instance.publish_ms = 50;
        s.validate().unwrap();
    }

    #[test]
    fn tag_count_cap_is_enforced() {
        let mut s = golden();
        s.cfg.n = MAX_TAGS + 1;
        assert_eq!(
            s.validate(),
            Err(SettingsError::TooManyTags {
                requested: MAX_TAGS + 1,
                max: MAX_TAGS
            })
        );
    }

    #[test]
    fn batch_byte_budget_cannot_exceed_the_broker_limit() {
        let mut s = golden();
        s.telemetry.batch_max_bytes = MAX_BATCH_BYTES + 1;
        assert!(matches!(
            s.validate(),
            Err(SettingsError::OutOfRange { field: "batch_max_bytes", .. })
        ));
    }

    #[test]
    fn wildcard_and_reserved_topics_are_rejected() {
        for bad in ["", "dt/+/opcua", "dt/gw/#", "$aws/rules/x"] {
            let mut s = golden();
            s.telemetry.topic = bad.into();
            assert!(matches!(s.validate(), Err(SettingsError::BadTopic(_))), "{bad}");
        }
    }

    #[test]
    fn digest_must_be_64_hex_chars() {
        let mut s = golden();
        s.cfg.sha256 = "9f2c".into();
        assert!(matches!(s.validate(), Err(SettingsError::BadDigest(_))));
    }

    #[test]
    fn qos_two_is_rejected_because_aws_iot_does_not_support_it() {
        let mut s = golden();
        s.telemetry.qos = 2;
        assert!(matches!(
            s.validate(),
            Err(SettingsError::OutOfRange { field: "qos", .. })
        ));
    }
}
