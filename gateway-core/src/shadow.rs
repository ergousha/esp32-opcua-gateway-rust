//! AWS IoT Device Shadow wire format for the `opcua` named shadow.
//!
//! Only the pure parts live here: topic construction and payload
//! encode/decode. MQTT itself is the firmware's problem.
//!
//! One subtlety drives the design: `/update/delta` carries **only the changed
//! fields**, so it can never be deserialised into a complete
//! [`DesiredSettings`]. Treating a delta as a config would silently apply a
//! document with defaults in every unchanged field. Instead a delta is used
//! purely as a *signal* to re-issue `/get` and read the authoritative
//! document — see [`is_relevant_delta`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::health::Reported;
use crate::settings::DesiredSettings;

/// Name of the named shadow this firmware owns.
pub const SHADOW_NAME: &str = "opcua";

/// The five topics needed to operate one named shadow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowTopics {
    /// Publish an empty payload here to request the document.
    pub get: String,
    /// The document arrives here.
    pub get_accepted: String,
    /// Errors for `get` arrive here (404 when no shadow exists yet).
    pub get_rejected: String,
    /// Publish `state.reported` here.
    pub update: String,
    /// Changed-fields notifications arrive here.
    pub update_delta: String,
    /// Errors for `update` arrive here.
    pub update_rejected: String,
}

impl ShadowTopics {
    /// Builds the topic set for `thing_name`'s `opcua` shadow.
    pub fn new(thing_name: &str) -> Self {
        let base = format!("$aws/things/{thing_name}/shadow/name/{SHADOW_NAME}");
        Self {
            get: format!("{base}/get"),
            get_accepted: format!("{base}/get/accepted"),
            get_rejected: format!("{base}/get/rejected"),
            update: format!("{base}/update"),
            update_delta: format!("{base}/update/delta"),
            update_rejected: format!("{base}/update/rejected"),
        }
    }

    /// Topics that must be subscribed before `get` is published.
    pub fn subscriptions(&self) -> [&str; 4] {
        [
            &self.get_accepted,
            &self.get_rejected,
            &self.update_delta,
            &self.update_rejected,
        ]
    }
}

/// Why a shadow payload could not be used.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowError {
    /// Payload was not valid JSON, or the envelope was not shaped as expected.
    Malformed(String),
    /// The shadow exists but has no `state.desired`.
    NoDesired,
    /// The broker rejected the request.
    Rejected {
        /// HTTP-like status code, e.g. 404 when the shadow does not exist.
        code: u32,
        /// Human-readable reason.
        message: String,
    },
}

impl fmt::Display for ShadowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShadowError::Malformed(e) => write!(f, "malformed shadow payload: {e}"),
            ShadowError::NoDesired => write!(f, "shadow has no desired state"),
            ShadowError::Rejected { code, message } => {
                write!(f, "shadow request rejected ({code}): {message}")
            }
        }
    }
}

impl std::error::Error for ShadowError {}

#[derive(Debug, Deserialize)]
struct GetAcceptedEnvelope {
    state: StateEnvelope,
    #[serde(default)]
    version: u64,
}

#[derive(Debug, Deserialize)]
struct StateEnvelope {
    #[serde(default)]
    desired: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RejectedEnvelope {
    code: u32,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct DeltaEnvelope {
    #[serde(default)]
    state: serde_json::Value,
    #[serde(default)]
    version: u64,
}

/// A successfully retrieved desired document.
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredDocument {
    /// Shadow version, monotonically increasing per shadow.
    pub version: u64,
    /// The parsed and *not yet validated* desired settings.
    pub settings: DesiredSettings,
}

/// Parses a `/get/accepted` payload.
///
/// The desired sub-document is extracted first and deserialised separately so
/// that a malformed `desired` is reported as such instead of being masked by
/// the surrounding envelope.
pub fn parse_get_accepted(payload: &[u8]) -> Result<DesiredDocument, ShadowError> {
    let env: GetAcceptedEnvelope =
        serde_json::from_slice(payload).map_err(|e| ShadowError::Malformed(e.to_string()))?;
    let desired = env.state.desired.ok_or(ShadowError::NoDesired)?;
    if desired.is_null() {
        return Err(ShadowError::NoDesired);
    }
    let settings: DesiredSettings =
        serde_json::from_value(desired).map_err(|e| ShadowError::Malformed(e.to_string()))?;
    Ok(DesiredDocument {
        version: env.version,
        settings,
    })
}

/// Parses a `/get/rejected` or `/update/rejected` payload.
pub fn parse_rejected(payload: &[u8]) -> ShadowError {
    match serde_json::from_slice::<RejectedEnvelope>(payload) {
        Ok(r) => ShadowError::Rejected {
            code: r.code,
            message: r.message,
        },
        Err(e) => ShadowError::Malformed(e.to_string()),
    }
}

/// Decides whether an `/update/delta` payload concerns this firmware.
///
/// A delta that only touches fields we do not own (someone else writing into
/// the same shadow) must not trigger a re-read, or two writers will ping-pong.
/// Returns the shadow version alongside the decision so the caller can ignore
/// stale deltas.
pub fn is_relevant_delta(payload: &[u8]) -> Result<(u64, bool), ShadowError> {
    let env: DeltaEnvelope =
        serde_json::from_slice(payload).map_err(|e| ShadowError::Malformed(e.to_string()))?;
    let relevant = env
        .state
        .as_object()
        .map(|o| {
            o.keys()
                .any(|k| matches!(k.as_str(), "enabled" | "instance" | "telemetry" | "cfg"))
        })
        .unwrap_or(false);
    Ok((env.version, relevant))
}

#[derive(Debug, Serialize)]
struct ReportedUpdate<'a> {
    state: ReportedState<'a>,
}

#[derive(Debug, Serialize)]
struct ReportedState<'a> {
    reported: &'a Reported,
}

/// Encodes a `state.reported` update for the `/update` topic.
pub fn encode_reported(reported: &Reported) -> Vec<u8> {
    // `Reported` is bounded by construction (see its tests), so this cannot
    // exceed the 8 KB shadow limit.
    serde_json::to_vec(&ReportedUpdate {
        state: ReportedState { reported },
    })
    .expect("Reported is always serialisable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::DriverState;

    const ACCEPTED: &str = r#"{
      "state": {
        "desired": {
          "enabled": true,
          "instance": {"endpoint": "opc.tcp://h:4840", "ns": 2},
          "telemetry": {"topic": "dt/gw/opcua"},
          "cfg": {"v": 7, "n": 2,
                  "sha256": "9f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a09f2ce1a0",
                  "topic": "cmd/gw/opcua/tags/v7"}
        },
        "reported": {"fw": "0.0.1"}
      },
      "metadata": {},
      "version": 42,
      "timestamp": 1753660800
    }"#;

    #[test]
    fn topics_follow_the_named_shadow_convention() {
        let t = ShadowTopics::new("gw-1");
        assert_eq!(t.get, "$aws/things/gw-1/shadow/name/opcua/get");
        assert_eq!(
            t.get_accepted,
            "$aws/things/gw-1/shadow/name/opcua/get/accepted"
        );
        assert_eq!(t.update, "$aws/things/gw-1/shadow/name/opcua/update");
        assert_eq!(
            t.update_delta,
            "$aws/things/gw-1/shadow/name/opcua/update/delta"
        );
        assert_eq!(t.subscriptions().len(), 4);
    }

    #[test]
    fn get_accepted_yields_version_and_settings() {
        let doc = parse_get_accepted(ACCEPTED.as_bytes()).unwrap();
        assert_eq!(doc.version, 42);
        assert_eq!(doc.settings.instance.endpoint, "opc.tcp://h:4840");
        assert_eq!(doc.settings.cfg.v, 7);
        doc.settings.validate().unwrap();
    }

    #[test]
    fn a_shadow_without_desired_is_reported_as_such() {
        assert_eq!(
            parse_get_accepted(br#"{"state":{"reported":{}},"version":1}"#),
            Err(ShadowError::NoDesired)
        );
        assert_eq!(
            parse_get_accepted(br#"{"state":{"desired":null},"version":1}"#),
            Err(ShadowError::NoDesired)
        );
    }

    #[test]
    fn a_malformed_desired_is_not_masked_by_the_envelope() {
        let payload = br#"{"state":{"desired":{"instance":{}}},"version":1}"#;
        assert!(matches!(
            parse_get_accepted(payload),
            Err(ShadowError::Malformed(_))
        ));
    }

    #[test]
    fn rejections_carry_the_code() {
        let err = parse_rejected(br#"{"code":404,"message":"No shadow exists with name: opcua"}"#);
        assert_eq!(
            err,
            ShadowError::Rejected {
                code: 404,
                message: "No shadow exists with name: opcua".into()
            }
        );
    }

    #[test]
    fn deltas_touching_our_fields_are_relevant() {
        let (v, relevant) = is_relevant_delta(br#"{"version":9,"state":{"cfg":{"v":8}}}"#).unwrap();
        assert_eq!(v, 9);
        assert!(relevant);
    }

    #[test]
    fn deltas_touching_only_foreign_fields_are_ignored() {
        let (_, relevant) =
            is_relevant_delta(br#"{"version":9,"state":{"someone_elses_field":1}}"#).unwrap();
        assert!(!relevant);
    }

    #[test]
    fn reported_update_is_wrapped_in_the_state_envelope() {
        let mut r = Reported::new("0.0.1");
        r.state = DriverState::Running;
        r.cfg_v = 7;
        let encoded = encode_reported(&r);
        let parsed: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(parsed["state"]["reported"]["cfg_v"], 7);
        assert_eq!(parsed["state"]["reported"]["state"], "running");
        assert!(encoded.len() < 8 * 1024);
    }
}
