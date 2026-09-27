//! The out-of-band tag bundle: a retained MQTT message carrying the full tag
//! list, grouped by scan rate so 250 tags fit in a few kilobytes.
//!
//! The bundle is deliberately *not* in the shadow: AWS IoT caps a shadow
//! document at 8 KB, and echoing 250 addresses back in `reported` would blow
//! that budget twice over. The shadow carries only `cfg {v, n, sha256, topic}`
//! and the device applies a bundle only when both the version and the digest
//! match, which makes the two planes impossible to desynchronise silently.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::codec::{hex_eq_ignore_case, sha256_hex};
use crate::node::{node_id_string, IdType, NodeError};
use crate::settings::CfgRef;
use crate::{MAX_BUNDLE_BYTES, MAX_TAGS};

/// Permitted scan-rate range, in milliseconds.
pub const MIN_SCAN_RATE_MS: u32 = 50;
/// Permitted scan-rate range, in milliseconds.
pub const MAX_SCAN_RATE_MS: u32 = 3_600_000;

/// Wire form of the bundle. Field names are single letters on purpose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagBundle {
    /// Bundle version; must equal `cfg.v` from the shadow.
    pub v: u32,
    /// Groups of tags that share a scan rate (and therefore a subscription).
    pub g: Vec<TagGroup>,
}

/// A set of addresses sharing scan rate, deadband and identifier type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagGroup {
    /// Scan rate / sampling interval in milliseconds.
    pub r: u32,
    /// Optional absolute deadband. Parsed and carried; not applied until the
    /// report-by-exception phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<f64>,
    /// Optional per-group override of the instance-level `id_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub i: Option<IdType>,
    /// Bare tag addresses.
    pub a: Vec<String>,
}

/// One tag, after the groups have been flattened.
#[derive(Debug, Clone, PartialEq)]
pub struct TagSpec {
    /// Bare address as it appeared in the bundle. Doubles as the telemetry key.
    pub address: String,
    /// Fully rendered NodeId string, e.g. `ns=2;s=Chan1.Dev1.Tag0001`.
    pub node_id: String,
    /// Sampling interval in milliseconds.
    pub scan_rate_ms: u32,
    /// Absolute deadband, if the group specified one.
    pub deadband: Option<f64>,
}

/// Why a bundle was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum BundleError {
    /// Payload larger than the NVS budget; refused before parsing.
    TooLarge {
        /// Received size.
        len: usize,
        /// Hard cap.
        max: usize,
    },
    /// Payload was not valid JSON or did not match the schema.
    Malformed(String),
    /// `bundle.v` did not match `cfg.v`.
    VersionMismatch {
        /// Version the shadow asked for.
        expected: u32,
        /// Version the bundle claims.
        actual: u32,
    },
    /// SHA-256 of the payload did not match `cfg.sha256`.
    DigestMismatch {
        /// Digest the shadow asked for.
        expected: String,
        /// Digest actually computed.
        actual: String,
    },
    /// Tag count did not match `cfg.n`.
    CountMismatch {
        /// Count the shadow asked for.
        expected: usize,
        /// Count actually present.
        actual: usize,
    },
    /// More tags than the firmware can hold.
    TooManyTags {
        /// Requested count.
        requested: usize,
        /// Hard cap.
        max: usize,
    },
    /// A scan rate was outside the permitted range.
    BadScanRate(u32),
    /// The same address appeared twice; the telemetry key would be ambiguous.
    DuplicateAddress(String),
    /// An address could not be turned into a NodeId.
    BadAddress(String, NodeError),
    /// The bundle contained no tags at all.
    Empty,
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BundleError::TooLarge { len, max } => write!(f, "bundle is {len} B, cap is {max} B"),
            BundleError::Malformed(e) => write!(f, "malformed bundle: {e}"),
            BundleError::VersionMismatch { expected, actual } => {
                write!(f, "bundle version {actual}, shadow asked for {expected}")
            }
            BundleError::DigestMismatch { expected, actual } => {
                write!(f, "bundle sha256 {actual}, shadow asked for {expected}")
            }
            BundleError::CountMismatch { expected, actual } => {
                write!(f, "bundle has {actual} tags, shadow declared {expected}")
            }
            BundleError::TooManyTags { requested, max } => {
                write!(f, "{requested} tags requested, firmware cap is {max}")
            }
            BundleError::BadScanRate(r) => write!(
                f,
                "scan rate {r} ms outside {MIN_SCAN_RATE_MS}..={MAX_SCAN_RATE_MS}"
            ),
            BundleError::DuplicateAddress(a) => write!(f, "duplicate address {a:?}"),
            BundleError::BadAddress(a, e) => write!(f, "address {a:?}: {e}"),
            BundleError::Empty => write!(f, "bundle contains no tags"),
        }
    }
}

impl std::error::Error for BundleError {}

/// Verifies `payload` against the shadow's `cfg` pointer and expands it into a
/// flat, ordered tag list.
///
/// Verification order matters: cheap structural checks run before the SHA-256
/// so a hostile or corrupt retained message cannot make the device hash
/// megabytes.
pub fn parse_and_verify(
    payload: &[u8],
    cfg: &CfgRef,
    default_ns: u16,
    default_id_type: IdType,
) -> Result<Vec<TagSpec>, BundleError> {
    if payload.len() > MAX_BUNDLE_BYTES {
        return Err(BundleError::TooLarge {
            len: payload.len(),
            max: MAX_BUNDLE_BYTES,
        });
    }

    let digest = sha256_hex(payload);
    if !hex_eq_ignore_case(&digest, &cfg.sha256) {
        return Err(BundleError::DigestMismatch {
            expected: cfg.sha256.clone(),
            actual: digest,
        });
    }

    let bundle: TagBundle =
        serde_json::from_slice(payload).map_err(|e| BundleError::Malformed(e.to_string()))?;

    if bundle.v != cfg.v {
        return Err(BundleError::VersionMismatch {
            expected: cfg.v,
            actual: bundle.v,
        });
    }

    let tags = expand(&bundle, default_ns, default_id_type)?;

    if tags.len() != cfg.n {
        return Err(BundleError::CountMismatch {
            expected: cfg.n,
            actual: tags.len(),
        });
    }

    Ok(tags)
}

/// Flattens the grouped wire form into per-tag specs, validating as it goes.
///
/// Order is preserved so that a re-delivered identical bundle produces an
/// identical plan and the diff comes out empty.
pub fn expand(
    bundle: &TagBundle,
    default_ns: u16,
    default_id_type: IdType,
) -> Result<Vec<TagSpec>, BundleError> {
    let total: usize = bundle.g.iter().map(|g| g.a.len()).sum();
    if total == 0 {
        return Err(BundleError::Empty);
    }
    if total > MAX_TAGS {
        return Err(BundleError::TooManyTags {
            requested: total,
            max: MAX_TAGS,
        });
    }

    let mut seen: HashSet<&str> = HashSet::with_capacity(total);
    let mut out = Vec::with_capacity(total);

    for group in &bundle.g {
        if !(MIN_SCAN_RATE_MS..=MAX_SCAN_RATE_MS).contains(&group.r) {
            return Err(BundleError::BadScanRate(group.r));
        }
        let id_type = group.i.unwrap_or(default_id_type);

        for address in &group.a {
            if !seen.insert(address.as_str()) {
                return Err(BundleError::DuplicateAddress(address.clone()));
            }
            let node_id = node_id_string(default_ns, id_type, address)
                .map_err(|e| BundleError::BadAddress(address.clone(), e))?;
            out.push(TagSpec {
                address: address.clone(),
                node_id,
                scan_rate_ms: group.r,
                deadband: group.d,
            });
        }
    }

    Ok(out)
}

/// Distinct scan rates present in `tags`, ascending. One subscription each.
pub fn scan_rates(tags: &[TagSpec]) -> Vec<u32> {
    let mut rates: Vec<u32> = tags.iter().map(|t| t.scan_rate_ms).collect();
    rates.sort_unstable();
    rates.dedup();
    rates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_for(payload: &[u8], v: u32, n: usize) -> CfgRef {
        CfgRef {
            v,
            n,
            sha256: sha256_hex(payload),
            topic: "cmd/gw/opcua/tags/v7".into(),
        }
    }

    const RAW: &str = r#"{"v":7,"g":[
        {"r":1000,"d":0.0,"a":["slow","Chan1.Dev1.Tag0001"]},
        {"r":250,"a":["fast"]},
        {"r":250,"i":"i","a":["4711"]}
    ]}"#;

    #[test]
    fn groups_expand_in_order_with_rendered_node_ids() {
        let cfg = cfg_for(RAW.as_bytes(), 7, 4);
        let tags = parse_and_verify(RAW.as_bytes(), &cfg, 2, IdType::S).unwrap();

        assert_eq!(tags.len(), 4);
        assert_eq!(tags[0].address, "slow");
        assert_eq!(tags[0].node_id, "ns=2;s=slow");
        assert_eq!(tags[0].scan_rate_ms, 1000);
        assert_eq!(tags[0].deadband, Some(0.0));
        assert_eq!(tags[1].node_id, "ns=2;s=Chan1.Dev1.Tag0001");
        assert_eq!(tags[2].scan_rate_ms, 250);
        assert_eq!(tags[2].deadband, None);
        // Per-group id_type override.
        assert_eq!(tags[3].node_id, "ns=2;i=4711");
    }

    #[test]
    fn distinct_scan_rates_drive_subscription_count() {
        let cfg = cfg_for(RAW.as_bytes(), 7, 4);
        let tags = parse_and_verify(RAW.as_bytes(), &cfg, 2, IdType::S).unwrap();
        assert_eq!(scan_rates(&tags), vec![250, 1000]);
    }

    #[test]
    fn digest_mismatch_is_refused() {
        let mut cfg = cfg_for(RAW.as_bytes(), 7, 4);
        cfg.sha256 = "0".repeat(64);
        assert!(matches!(
            parse_and_verify(RAW.as_bytes(), &cfg, 2, IdType::S),
            Err(BundleError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn digest_comparison_ignores_hex_case() {
        let mut cfg = cfg_for(RAW.as_bytes(), 7, 4);
        cfg.sha256 = cfg.sha256.to_uppercase();
        assert!(parse_and_verify(RAW.as_bytes(), &cfg, 2, IdType::S).is_ok());
    }

    #[test]
    fn version_mismatch_is_refused() {
        let cfg = cfg_for(RAW.as_bytes(), 8, 4);
        assert_eq!(
            parse_and_verify(RAW.as_bytes(), &cfg, 2, IdType::S),
            Err(BundleError::VersionMismatch {
                expected: 8,
                actual: 7
            })
        );
    }

    #[test]
    fn count_mismatch_catches_a_truncated_publish() {
        let cfg = cfg_for(RAW.as_bytes(), 7, 250);
        assert_eq!(
            parse_and_verify(RAW.as_bytes(), &cfg, 2, IdType::S),
            Err(BundleError::CountMismatch {
                expected: 250,
                actual: 4
            })
        );
    }

    #[test]
    fn malformed_json_is_refused() {
        let raw = b"{\"v\":7,\"g\":";
        let cfg = cfg_for(raw, 7, 1);
        assert!(matches!(
            parse_and_verify(raw, &cfg, 2, IdType::S),
            Err(BundleError::Malformed(_))
        ));
    }

    #[test]
    fn oversized_payload_is_refused_before_hashing() {
        let raw = vec![b'x'; MAX_BUNDLE_BYTES + 1];
        let cfg = CfgRef {
            v: 7,
            n: 1,
            sha256: "0".repeat(64),
            topic: "cmd/gw/opcua/tags/v7".into(),
        };
        assert_eq!(
            parse_and_verify(&raw, &cfg, 2, IdType::S),
            Err(BundleError::TooLarge {
                len: MAX_BUNDLE_BYTES + 1,
                max: MAX_BUNDLE_BYTES
            })
        );
    }

    #[test]
    fn duplicate_addresses_are_refused() {
        let b = TagBundle {
            v: 1,
            g: vec![
                TagGroup {
                    r: 1000,
                    d: None,
                    i: None,
                    a: vec!["a".into()],
                },
                TagGroup {
                    r: 250,
                    d: None,
                    i: None,
                    a: vec!["a".into()],
                },
            ],
        };
        assert_eq!(
            expand(&b, 2, IdType::S),
            Err(BundleError::DuplicateAddress("a".into()))
        );
    }

    #[test]
    fn scan_rate_bounds_are_enforced() {
        for r in [0, MIN_SCAN_RATE_MS - 1, MAX_SCAN_RATE_MS + 1] {
            let b = TagBundle {
                v: 1,
                g: vec![TagGroup {
                    r,
                    d: None,
                    i: None,
                    a: vec!["a".into()],
                }],
            };
            assert_eq!(expand(&b, 2, IdType::S), Err(BundleError::BadScanRate(r)));
        }
    }

    #[test]
    fn tag_cap_is_enforced_during_expansion() {
        let b = TagBundle {
            v: 1,
            g: vec![TagGroup {
                r: 1000,
                d: None,
                i: None,
                a: (0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect(),
            }],
        };
        assert_eq!(
            expand(&b, 2, IdType::S),
            Err(BundleError::TooManyTags {
                requested: MAX_TAGS + 1,
                max: MAX_TAGS
            })
        );
    }

    #[test]
    fn empty_bundle_is_refused() {
        let b = TagBundle { v: 1, g: vec![] };
        assert_eq!(expand(&b, 2, IdType::S), Err(BundleError::Empty));
    }

    #[test]
    fn bad_address_names_the_offender() {
        let b = TagBundle {
            v: 1,
            g: vec![TagGroup {
                r: 1000,
                d: None,
                i: Some(IdType::I),
                a: vec!["nope".into()],
            }],
        };
        assert!(matches!(
            expand(&b, 2, IdType::S),
            Err(BundleError::BadAddress(a, _)) if a == "nope"
        ));
    }

    /// The whole two-plane design rests on 250 tags fitting comfortably inside
    /// both the retained MQTT message and the NVS partition.
    #[test]
    fn two_hundred_fifty_tags_stay_well_inside_the_budget() {
        let bundle = TagBundle {
            v: 7,
            g: vec![TagGroup {
                r: 1000,
                d: None,
                i: None,
                a: (1..=MAX_TAGS)
                    .map(|i| format!("Chan1.Dev1.Tag{i:04}"))
                    .collect(),
            }],
        };
        let raw = serde_json::to_vec(&bundle).unwrap();
        // Realistic 18-character addresses land around 5.3 KB, comfortably
        // inside both the NVS budget and the MQTT input buffer.
        let ceiling = MAX_BUNDLE_BYTES * 7 / 10;
        assert!(
            raw.len() < ceiling,
            "250-tag bundle is {} B, expected under {} B",
            raw.len(),
            ceiling
        );

        let cfg = cfg_for(&raw, 7, MAX_TAGS);
        assert_eq!(
            parse_and_verify(&raw, &cfg, 2, IdType::S).unwrap().len(),
            MAX_TAGS
        );
    }
}
