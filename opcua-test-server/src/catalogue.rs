//! The tag catalogue: the single source of truth for every OPC UA test.
//!
//! Every consumer reads this module, and that is the point. The server
//! ([`crate::TestServer`]) creates a node for every tag that is `present`, and
//! the cloud-side documents ([`crate::documents`]) build the tag bundle from the
//! same list. Maintained separately, one typo in an address would come back as
//! `BadNodeIdUnknown` and look exactly like a gateway bug.
//!
//! Each tag also declares the JSON [`Encoding`] the gateway must produce for it
//! (§4.3 of `docs/OPCUA_CLIENT_REQUIREMENTS.md`), so a test asserts on the
//! actual telemetry bytes rather than merely counting them. A new value type is
//! one new entry here; the tests pick up its assertion for free.

use std::str::FromStr;

use opcua_types::{ByteString, Guid, StatusCode, Variant};

/// Namespace the server registers. The gateway resolves it against the
/// server's `NamespaceArray`; the numeric index in the settings is only a
/// fallback.
pub const NAMESPACE_URI: &str = "urn:ergousha:opcua-test";

/// Scan rate of the fast group.
///
/// Each distinct rate becomes one OPC UA subscription on the device, so keeping
/// this list short is itself part of the design under test.
pub const FAST_MS: u32 = 1_000;
/// Scan rate of the slow group.
pub const SLOW_MS: u32 = 5_000;

/// The first integer an IEEE-754 double cannot represent exactly (2^53 + 1).
///
/// A gateway that routes 64-bit integers through `f64` corrupts values from
/// here on, silently; the encoder must emit a tagged string instead.
pub const ABOVE_2_53: i64 = 9_007_199_254_740_993;

/// Largest integer that survives a JSON round trip through a double.
pub const SAFE_INT: i64 = 9_007_199_254_740_991;

/// StatusCode the faulty tag carries while the fault is injected.
pub const FAULT_STATUS: StatusCode = StatusCode::BadDeviceFailure;

/// The shape a tag's value must take in the telemetry JSON (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// A bare JSON number.
    Number,
    /// `true` / `false`.
    Bool,
    /// A JSON string.
    String,
    /// A JSON array.
    Array,
    /// `{"$t":"i64","v":"…"}` — a signed integer beyond 2^53.
    I64,
    /// `{"$t":"u64","v":"…"}` — an unsigned integer beyond 2^53.
    U64,
    /// `{"$t":"b64","v":"…"}` — a base64 ByteString.
    B64,
    /// `{"$t":"guid","v":"…"}`.
    Guid,
}

impl Encoding {
    /// True when `value` has this encoding.
    pub fn matches(self, value: &serde_json::Value) -> bool {
        use serde_json::Value;
        let tagged = |kind: &str| {
            value.get("$t").and_then(Value::as_str) == Some(kind)
                && value.get("v").is_some_and(Value::is_string)
        };
        let tagged_int = |kind: &str| {
            tagged(kind)
                && value["v"]
                    .as_str()
                    .and_then(|v| v.parse::<i128>().ok())
                    .is_some_and(|v| v.abs() > SAFE_INT as i128)
        };
        match self {
            Encoding::Number => value.is_number(),
            Encoding::Bool => value.is_boolean(),
            Encoding::String => value.is_string(),
            Encoding::Array => value.is_array(),
            Encoding::I64 => tagged_int("i64"),
            Encoding::U64 => tagged_int("u64"),
            Encoding::B64 => tagged("b64"),
            Encoding::Guid => tagged("guid"),
        }
    }
}

/// One tag in the scenario.
#[derive(Debug, Clone)]
pub struct Tag {
    /// Bare SCADA-style address. The gateway renders it as `ns=<n>;s=<address>`.
    pub address: &'static str,
    /// Sampling interval, and therefore which subscription the tag lands in.
    pub scan_rate_ms: u32,
    /// `false` means the node is deliberately *not* created, which is how the
    /// per-item failure path gets exercised.
    pub present: bool,
    /// The value at server tick `n`; tick 0 is the initial value.
    pub value: fn(u64) -> Variant,
    /// `false` for a tag written exactly once, at startup.
    pub changes: bool,
    /// When set, the tag carries this StatusCode while the fault is injected.
    pub bad_status: Option<StatusCode>,
    /// JSON the gateway must emit for it; `None` for an absent tag.
    pub encoding: Option<Encoding>,
    /// What this tag proves, in one line.
    pub proves: &'static str,
}

fn line1_temp(t: u64) -> Variant {
    let v = 20.0 + 5.0 * (t as f64 / 4.0).sin();
    Variant::Double((v * 1000.0).round() / 1000.0)
}

/// Deliberately a value that is NOT exact in binary floating point, so the
/// f32 -> f64 widening is observable: a naive `as f64` would surface
/// 1.100000023841858 instead of 1.1.
fn line1_pressure(t: u64) -> Variant {
    Variant::Float(((10 + t % 10) as f32) / 10.0)
}

fn line1_running(t: u64) -> Variant {
    Variant::Boolean((t / 4).is_multiple_of(2))
}

fn line1_state(t: u64) -> Variant {
    Variant::from(["RUN", "IDLE", "FAULT"][((t / 6) % 3) as usize])
}

fn line1_counter(t: u64) -> Variant {
    Variant::UInt32(t as u32)
}

fn line1_big_counter(t: u64) -> Variant {
    Variant::Int64(ABOVE_2_53 + t as i64)
}

fn line1_serial(t: u64) -> Variant {
    Variant::UInt64(ABOVE_2_53 as u64 + 7 + t)
}

fn line1_blob(t: u64) -> Variant {
    Variant::ByteString(ByteString::from(vec![(t % 256) as u8, 1, 2, 0xfe, 0xff]))
}

fn line1_batch_id(_: u64) -> Variant {
    Variant::Guid(Box::new(
        Guid::from_str(BATCH_ID).expect("BATCH_ID is a valid GUID"),
    ))
}

fn line1_profile(t: u64) -> Variant {
    Variant::from((0..5).map(|i| (t + i) as f64).collect::<Vec<f64>>())
}

fn line1_static(_: u64) -> Variant {
    Variant::Double(42.0)
}

fn line1_faulty(_: u64) -> Variant {
    Variant::Double(0.0)
}

fn line2_level(t: u64) -> Variant {
    Variant::Double(50.0 + (t % 20) as f64)
}

fn line2_mode(t: u64) -> Variant {
    Variant::Int16(((t % 3) + 1) as i16)
}

fn absent(_: u64) -> Variant {
    Variant::Empty
}

/// The GUID `Line1.BatchId` holds.
pub const BATCH_ID: &str = "72962b91-fa75-4ae6-8d28-b404dc7daf63";

/// Value `Line1.Faulty` takes once the fault is cleared.
pub const FAULT_CLEARED_VALUE: f64 = 123.45;

const TAGS: &[Tag] = &[
    // ---- fast group ------------------------------------------------------
    Tag {
        address: "Line1.Temp",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_temp,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Number),
        proves: "plain Double -> bare JSON number",
    },
    Tag {
        address: "Line1.Pressure",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_pressure,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Number),
        proves: "Float widened via its shortest decimal form, not `as f64`",
    },
    Tag {
        address: "Line1.Running",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_running,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Bool),
        proves: "Boolean -> JSON true/false",
    },
    Tag {
        address: "Line1.State",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_state,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::String),
        proves: "String -> JSON string",
    },
    Tag {
        address: "Line1.Counter",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_counter,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Number),
        proves: "UInt32 -> bare JSON number (below 2^53)",
    },
    Tag {
        address: "Line1.BigCounter",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_big_counter,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::I64),
        proves: "Int64 past 2^53 -> {\"$t\":\"i64\"} tagged string, losslessly",
    },
    Tag {
        address: "Line1.Serial",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_serial,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::U64),
        proves: "UInt64 past 2^53 -> {\"$t\":\"u64\"} tagged string",
    },
    Tag {
        address: "Line1.Blob",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_blob,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::B64),
        proves: "ByteString -> {\"$t\":\"b64\"} base64",
    },
    Tag {
        address: "Line1.BatchId",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_batch_id,
        changes: false,
        bad_status: None,
        encoding: Some(Encoding::Guid),
        proves: "Guid -> {\"$t\":\"guid\"}",
    },
    Tag {
        address: "Line1.Profile",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_profile,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Array),
        proves: "array Variant -> JSON array",
    },
    Tag {
        address: "Line1.Static",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_static,
        changes: false,
        bad_status: None,
        encoding: Some(Encoding::Number),
        proves: "a never-changing tag reports once, not every scan \
                 (report-by-exception: the subscription is not polling)",
    },
    Tag {
        address: "Line1.Faulty",
        scan_rate_ms: FAST_MS,
        present: true,
        value: line1_faulty,
        changes: false,
        bad_status: Some(FAULT_STATUS),
        encoding: Some(Encoding::Number),
        proves: "a Bad StatusCode travels as the optional 4th row element",
    },
    // ---- slow group ------------------------------------------------------
    Tag {
        address: "Line2.Level",
        scan_rate_ms: SLOW_MS,
        present: true,
        value: line2_level,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Number),
        proves: "a second scan rate becomes a second subscription",
    },
    Tag {
        address: "Line2.Mode",
        scan_rate_ms: SLOW_MS,
        present: true,
        value: line2_mode,
        changes: true,
        bad_status: None,
        encoding: Some(Encoding::Number),
        proves: "Int16 -> bare JSON number",
    },
    // ---- deliberately absent ---------------------------------------------
    Tag {
        address: "Line1.DoesNotExist",
        scan_rate_ms: FAST_MS,
        present: false,
        value: absent,
        changes: false,
        bad_status: None,
        encoding: None,
        proves: "one unknown NodeId is reported as failed WITHOUT taking the \
                 other tags down (finding A4 in the requirements)",
    },
];

/// The full catalogue, including the deliberately absent tag.
pub fn tags() -> Vec<Tag> {
    TAGS.to_vec()
}

/// Tags the server actually creates.
pub fn present() -> Vec<Tag> {
    TAGS.iter().filter(|t| t.present).cloned().collect()
}

/// Addresses that must be reported as failed.
pub fn missing_addresses() -> Vec<&'static str> {
    TAGS.iter()
        .filter(|t| !t.present)
        .map(|t| t.address)
        .collect()
}

/// Looks a tag up by address.
pub fn tag(address: &str) -> Option<Tag> {
    TAGS.iter().find(|t| t.address == address).cloned()
}

/// Distinct scan rates in `tags`, i.e. how many subscriptions they need.
pub fn subscription_count(tags: &[Tag]) -> usize {
    let mut rates: Vec<u32> = tags.iter().map(|t| t.scan_rate_ms).collect();
    rates.sort_unstable();
    rates.dedup();
    rates.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_unique() {
        let mut addresses: Vec<_> = TAGS.iter().map(|t| t.address).collect();
        addresses.sort_unstable();
        addresses.dedup();
        assert_eq!(addresses.len(), TAGS.len());
    }

    #[test]
    fn every_present_tag_declares_an_encoding_and_every_absent_one_does_not() {
        for tag in TAGS {
            assert_eq!(tag.present, tag.encoding.is_some(), "{}", tag.address);
        }
    }

    #[test]
    fn initial_values_have_a_type() {
        for tag in TAGS.iter().filter(|t| t.present) {
            assert!(
                (tag.value)(0).data_type().is_some(),
                "{} has an untyped initial value",
                tag.address
            );
        }
    }

    #[test]
    fn pressure_is_not_exact_in_binary() {
        // The whole point of the tag: 1.1f32 widened naively is not 1.1.
        let Variant::Float(f) = line1_pressure(1) else {
            panic!("Line1.Pressure must be a Float");
        };
        assert_ne!(f as f64, 1.1);
        assert_eq!(f.to_string(), "1.1");
    }

    #[test]
    fn big_counters_really_are_past_double_precision() {
        const { assert!(ABOVE_2_53 > SAFE_INT) };
        assert_ne!(ABOVE_2_53 as f64 as i64, ABOVE_2_53);
    }

    #[test]
    fn encoding_predicates() {
        use serde_json::json;
        assert!(Encoding::Number.matches(&json!(1.5)));
        assert!(!Encoding::Number.matches(&json!(true)));
        assert!(Encoding::Bool.matches(&json!(false)));
        assert!(Encoding::I64.matches(&json!({"$t": "i64", "v": "9007199254740993"})));
        assert!(!Encoding::I64.matches(&json!({"$t": "i64", "v": "12"})));
        assert!(Encoding::U64.matches(&json!({"$t": "u64", "v": "9007199254741000"})));
        assert!(Encoding::B64.matches(&json!({"$t": "b64", "v": "AAEC/v8="})));
        assert!(Encoding::Guid.matches(&json!({"$t": "guid", "v": BATCH_ID})));
        assert!(!Encoding::Guid.matches(&json!(BATCH_ID)));
    }
}
