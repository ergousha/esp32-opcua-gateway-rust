//! Device-independent representation of an OPC UA value and its JSON encoding.
//!
//! The OPC UA `Variant` type is far richer than JSON. Rather than lose data
//! silently (the previous spike used `format!("{:?}")`, which is unparseable),
//! anything JSON cannot represent losslessly is wrapped in a tagged object:
//!
//! ```json
//! {"$t": "i64", "v": "-9223372036854775808"}
//! ```
//!
//! Plain JSON numbers/strings/booleans are emitted whenever they round-trip
//! exactly, which keeps the common case (floats and bools) compact.

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};

use crate::codec::base64_encode;

/// Largest integer magnitude an IEEE-754 double represents exactly, and
/// therefore the largest integer a JSON consumer is guaranteed to read back.
pub const SAFE_INT: i64 = 9_007_199_254_740_991; // 2^53 - 1

/// Maximum number of elements kept when an OPC UA value is an array.
///
/// Arrays are rare in gateway workloads and unbounded ones would blow the
/// telemetry batch budget, so they are truncated rather than dropped.
pub const MAX_ARRAY_ELEMENTS: usize = 64;

/// A value read from the OPC UA server, reduced to something the telemetry
/// encoder understands.
#[derive(Debug, Clone, PartialEq)]
pub enum TagValue {
    /// No value (OPC UA `Variant::Empty`, or a Bad status with no value).
    Null,
    /// Boolean.
    Bool(bool),
    /// Any floating point value, and any integer small enough to be exact.
    F64(f64),
    /// A signed integer that may exceed [`SAFE_INT`].
    I64(i64),
    /// An unsigned integer that may exceed [`SAFE_INT`].
    U64(u64),
    /// Text.
    Str(String),
    /// Opaque bytes; encoded as tagged base64.
    Bytes(Vec<u8>),
    /// A GUID in canonical form; encoded as a tagged string.
    Guid(String),
    /// A homogeneous array, already truncated to [`MAX_ARRAY_ELEMENTS`].
    Array(Vec<TagValue>),
    /// A Variant flavour with no useful JSON projection (ExtensionObject,
    /// DiagnosticInfo, ...). Carries the OPC UA type name for diagnosis.
    Unsupported(String),
}

impl TagValue {
    /// Builds an [`TagValue::Array`], truncating to [`MAX_ARRAY_ELEMENTS`].
    pub fn array(mut items: Vec<TagValue>) -> Self {
        items.truncate(MAX_ARRAY_ELEMENTS);
        TagValue::Array(items)
    }

    /// Rough serialised size in bytes, used by the batcher to stay under the
    /// MQTT payload budget without serialising twice.
    ///
    /// Deliberately pessimistic: the batcher must never *under*-estimate, or a
    /// batch could exceed the broker's payload limit and be rejected whole.
    /// The publisher still re-checks the real encoded length before sending.
    pub fn estimated_bytes(&self) -> usize {
        match self {
            TagValue::Null => 4,
            TagValue::Bool(_) => 5,
            // Covers the tagged `{"$t":"f64","v":"-Infinity"}` form too.
            TagValue::F64(_) => 30,
            // Covers the tagged `{"$t":"i64","v":"-9223372036854775808"}` form.
            TagValue::I64(_) | TagValue::U64(_) => 42,
            TagValue::Str(s) => s.len() + 8,
            TagValue::Bytes(b) => b.len().div_ceil(3) * 4 + 20,
            TagValue::Guid(g) => g.len() + 22,
            TagValue::Array(items) => {
                2 + items.iter().map(|i| i.estimated_bytes() + 1).sum::<usize>()
            }
            TagValue::Unsupported(t) => t.len() + 30,
        }
    }
}

fn serialize_tagged<S: Serializer>(ser: S, tag: &str, value: &str) -> Result<S::Ok, S::Error> {
    let mut m = ser.serialize_map(Some(2))?;
    m.serialize_entry("$t", tag)?;
    m.serialize_entry("v", value)?;
    m.end()
}

impl Serialize for TagValue {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            TagValue::Null => ser.serialize_unit(),
            TagValue::Bool(b) => ser.serialize_bool(*b),
            TagValue::F64(f) => {
                // JSON has no NaN/Infinity; serde_json would refuse to encode
                // them and take the whole batch down with it.
                if f.is_finite() {
                    ser.serialize_f64(*f)
                } else if f.is_nan() {
                    serialize_tagged(ser, "f64", "NaN")
                } else if f.is_sign_positive() {
                    serialize_tagged(ser, "f64", "Infinity")
                } else {
                    serialize_tagged(ser, "f64", "-Infinity")
                }
            }
            TagValue::I64(i) => {
                // Note: `i.abs()` would panic on `i64::MIN`, hence the range test.
                if (-SAFE_INT..=SAFE_INT).contains(i) {
                    ser.serialize_i64(*i)
                } else {
                    serialize_tagged(ser, "i64", &i.to_string())
                }
            }
            TagValue::U64(u) => {
                if *u <= SAFE_INT as u64 {
                    ser.serialize_u64(*u)
                } else {
                    serialize_tagged(ser, "u64", &u.to_string())
                }
            }
            TagValue::Str(s) => ser.serialize_str(s),
            TagValue::Bytes(b) => serialize_tagged(ser, "b64", &base64_encode(b)),
            TagValue::Guid(g) => serialize_tagged(ser, "guid", g),
            TagValue::Array(items) => {
                let mut s = ser.serialize_seq(Some(items.len()))?;
                for item in items {
                    s.serialize_element(item)?;
                }
                s.end()
            }
            TagValue::Unsupported(t) => serialize_tagged(ser, "unsupported", t),
        }
    }
}

/// True when an OPC UA StatusCode has severity `Good`.
///
/// Severity lives in the top two bits: `00` Good, `01` Uncertain, `1x` Bad.
/// Sub-codes such as `Good_Overflow` are still Good and are not worth the
/// bytes it would cost to report them on every sample.
pub fn is_good(status: u32) -> bool {
    status >> 30 == 0
}

/// True when an OPC UA StatusCode has severity `Bad`.
pub fn is_bad(status: u32) -> bool {
    status >> 30 >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(v: &TagValue) -> String {
        serde_json::to_string(v).unwrap()
    }

    #[test]
    fn scalars_encode_as_plain_json() {
        assert_eq!(json(&TagValue::Null), "null");
        assert_eq!(json(&TagValue::Bool(true)), "true");
        assert_eq!(json(&TagValue::F64(23.5)), "23.5");
        assert_eq!(json(&TagValue::Str("RUN".into())), "\"RUN\"");
        assert_eq!(json(&TagValue::I64(-1234)), "-1234");
        assert_eq!(json(&TagValue::U64(1234)), "1234");
    }

    #[test]
    fn integers_beyond_double_precision_are_tagged() {
        assert_eq!(json(&TagValue::I64(SAFE_INT)), SAFE_INT.to_string());
        assert_eq!(
            json(&TagValue::I64(SAFE_INT + 1)),
            r#"{"$t":"i64","v":"9007199254740992"}"#
        );
        assert_eq!(
            json(&TagValue::I64(-SAFE_INT - 1)),
            r#"{"$t":"i64","v":"-9007199254740992"}"#
        );
        assert_eq!(
            json(&TagValue::U64(u64::MAX)),
            r#"{"$t":"u64","v":"18446744073709551615"}"#
        );
    }

    #[test]
    fn non_finite_doubles_do_not_break_the_batch() {
        assert_eq!(json(&TagValue::F64(f64::NAN)), r#"{"$t":"f64","v":"NaN"}"#);
        assert_eq!(
            json(&TagValue::F64(f64::INFINITY)),
            r#"{"$t":"f64","v":"Infinity"}"#
        );
        assert_eq!(
            json(&TagValue::F64(f64::NEG_INFINITY)),
            r#"{"$t":"f64","v":"-Infinity"}"#
        );
    }

    #[test]
    fn opaque_types_are_tagged() {
        assert_eq!(
            json(&TagValue::Bytes(b"foobar".to_vec())),
            r#"{"$t":"b64","v":"Zm9vYmFy"}"#
        );
        assert_eq!(
            json(&TagValue::Guid(
                "72962b91-fa75-4ae6-8d28-b404dc7daf63".into()
            )),
            r#"{"$t":"guid","v":"72962b91-fa75-4ae6-8d28-b404dc7daf63"}"#
        );
        assert_eq!(
            json(&TagValue::Unsupported("ExtensionObject".into())),
            r#"{"$t":"unsupported","v":"ExtensionObject"}"#
        );
    }

    #[test]
    fn arrays_encode_as_json_arrays_and_are_truncated() {
        assert_eq!(
            json(&TagValue::array(vec![
                TagValue::F64(1.0),
                TagValue::Bool(false)
            ])),
            "[1.0,false]"
        );
        let long = TagValue::array(vec![TagValue::F64(0.0); MAX_ARRAY_ELEMENTS + 10]);
        let TagValue::Array(items) = long else {
            unreachable!()
        };
        assert_eq!(items.len(), MAX_ARRAY_ELEMENTS);
    }

    #[test]
    fn status_severity_classification() {
        assert!(is_good(0x0000_0000)); // Good
        assert!(is_good(0x002F_0000)); // Good_Overflow-ish sub-code
        assert!(!is_good(0x4000_0000)); // Uncertain
        assert!(!is_bad(0x4000_0000));
        assert!(is_bad(0x8000_0000)); // Bad
        assert!(is_bad(0x8034_0000)); // BadNodeIdUnknown-ish
    }

    #[test]
    fn estimated_bytes_is_within_an_order_of_magnitude() {
        for v in [
            TagValue::Null,
            TagValue::Bool(true),
            TagValue::F64(1.25),
            TagValue::I64(i64::MIN),
            TagValue::U64(u64::MAX),
            TagValue::Str("a moderately long tag value".into()),
            TagValue::Bytes(vec![7; 30]),
            TagValue::Guid("72962b91-fa75-4ae6-8d28-b404dc7daf63".into()),
            TagValue::array(vec![TagValue::F64(1.0); 8]),
        ] {
            let actual = serde_json::to_string(&v).unwrap().len();
            assert!(
                v.estimated_bytes() >= actual,
                "estimate {} under-counts actual {} for {v:?}",
                v.estimated_bytes(),
                actual
            );
        }
    }
}
