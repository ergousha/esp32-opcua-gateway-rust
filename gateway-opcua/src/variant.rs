//! Mapping OPC UA `Variant`/`DataValue` onto the transport-neutral
//! [`TagValue`].
//!
//! This is the only place in the firmware that knows about the OPC UA type
//! system. The spike used `format!("{:?}", variant)`, which produced strings
//! like `Double(23.5)` that no consumer can parse and that silently changed
//! shape with the library version.

use opcua_types::{DataValue, Variant};

use gateway_core::value::{TagValue, MAX_ARRAY_ELEMENTS};

/// A notification reduced to what the telemetry pipeline needs.
#[derive(Debug, Clone)]
pub struct RawSample {
    /// Client handle assigned when the monitored item was created.
    pub client_handle: u32,
    /// Source timestamp in ms since the Unix epoch, falling back to the server
    /// timestamp and finally to the caller-supplied receive time.
    pub ts_ms: i64,
    /// The value.
    pub value: TagValue,
    /// OPC UA StatusCode.
    pub status: u32,
}

/// Converts a `DataValue` notification.
///
/// `received_ms` is used only when the server supplied neither a source nor a
/// server timestamp; a sample with no time at all is useless for a historian.
pub fn from_data_value(handle: u32, dv: &DataValue, received_ms: i64) -> RawSample {
    let ts_ms = dv
        .source_timestamp
        .as_ref()
        .or(dv.server_timestamp.as_ref())
        .map(|t| t.as_chrono().timestamp_millis())
        .unwrap_or(received_ms);

    RawSample {
        client_handle: handle,
        ts_ms,
        value: dv
            .value
            .as_ref()
            .map(from_variant)
            .unwrap_or(TagValue::Null),
        status: dv.status.map(|s| s.bits()).unwrap_or(0),
    }
}

/// Converts a `Variant` to its JSON-friendly projection.
pub fn from_variant(v: &Variant) -> TagValue {
    match v {
        Variant::Empty => TagValue::Null,
        Variant::Boolean(b) => TagValue::Bool(*b),

        // Signed and unsigned integers keep their exactness: routing them
        // through f64 would corrupt 64-bit counters, which is precisely the
        // kind of silent data loss a gateway must not introduce.
        Variant::SByte(v) => TagValue::I64(*v as i64),
        Variant::Int16(v) => TagValue::I64(*v as i64),
        Variant::Int32(v) => TagValue::I64(*v as i64),
        Variant::Int64(v) => TagValue::I64(*v),
        Variant::Byte(v) => TagValue::U64(*v as u64),
        Variant::UInt16(v) => TagValue::U64(*v as u64),
        Variant::UInt32(v) => TagValue::U64(*v as u64),
        Variant::UInt64(v) => TagValue::U64(*v),

        // `0.1f32 as f64` is 0.10000000149011612; round-tripping through the
        // shortest decimal representation of the f32 keeps the number the
        // operator configured.
        Variant::Float(f) => TagValue::F64(shortest_f32(*f)),
        Variant::Double(d) => TagValue::F64(*d),

        Variant::String(s) => TagValue::Str(s.as_ref().to_string()),
        Variant::XmlElement(s) => TagValue::Str(s.to_string()),
        Variant::DateTime(dt) => TagValue::Str(dt.as_chrono().to_rfc3339()),
        Variant::Guid(g) => TagValue::Guid(g.to_string()),
        Variant::StatusCode(s) => TagValue::U64(s.bits() as u64),
        Variant::ByteString(b) => match &b.value {
            Some(bytes) => TagValue::Bytes(bytes.clone()),
            None => TagValue::Null,
        },
        Variant::QualifiedName(q) => {
            TagValue::Str(format!("{}:{}", q.namespace_index, q.name.as_ref()))
        }
        Variant::LocalizedText(t) => TagValue::Str(t.text.as_ref().to_string()),
        Variant::NodeId(n) => TagValue::Str(n.to_string()),
        Variant::ExpandedNodeId(n) => TagValue::Str(n.to_string()),

        // A nested Variant is just a wrapper.
        Variant::Variant(inner) => from_variant(inner),

        Variant::Array(array) => TagValue::array(
            array
                .values
                .iter()
                .take(MAX_ARRAY_ELEMENTS)
                .map(from_variant)
                .collect(),
        ),

        // Structured types have no stable JSON projection without the server's
        // type dictionary, which this device does not load. Report the type
        // name so the misconfiguration is diagnosable instead of silent.
        Variant::ExtensionObject(_) => TagValue::Unsupported("ExtensionObject".into()),
        Variant::DataValue(_) => TagValue::Unsupported("DataValue".into()),
        Variant::DiagnosticInfo(_) => TagValue::Unsupported("DiagnosticInfo".into()),
    }
}

/// Widens an `f32` via its shortest decimal representation.
fn shortest_f32(f: f32) -> f64 {
    if f.is_finite() {
        // `f32::to_string` emits the shortest string that round-trips to the
        // same f32, so parsing it as f64 yields the "expected" decimal.
        f.to_string().parse::<f64>().unwrap_or(f as f64)
    } else {
        f as f64
    }
}
