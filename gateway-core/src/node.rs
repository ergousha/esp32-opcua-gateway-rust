//! NodeId construction: `(namespace, id_type, address) -> "ns=2;s=Foo"`.
//!
//! The gateway never receives fully-qualified NodeIds from the cloud; the
//! bundle carries bare addresses (which is what SCADA exports look like) plus
//! an `id_type` and a namespace taken from the instance settings. Keeping the
//! address bare is what makes a 250-tag bundle fit in 8 KB.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::codec::is_base64;

/// Longest identifier we will accept. OPC UA allows 4096, but on this device a
/// long address multiplies across every tag, every batch and every NVS write.
pub const MAX_IDENTIFIER_LEN: usize = 128;

/// OPC UA NodeId identifier flavours, in their shadow-document spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdType {
    /// String identifier (`ns=2;s=Channel1.Device1.Tag`). The common case.
    #[default]
    S,
    /// Numeric identifier (`ns=2;i=1234`).
    I,
    /// GUID identifier (`ns=2;g=...`).
    G,
    /// Opaque / ByteString identifier, base64-encoded (`ns=2;b=...`).
    B,
}

impl IdType {
    /// The single character used in the NodeId string form.
    pub fn tag(self) -> char {
        match self {
            IdType::S => 's',
            IdType::I => 'i',
            IdType::G => 'g',
            IdType::B => 'b',
        }
    }
}

/// Why an address could not be turned into a NodeId.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeError {
    /// The address was empty.
    Empty,
    /// The address exceeded [`MAX_IDENTIFIER_LEN`].
    TooLong(usize),
    /// `id_type` was `i` but the address is not a `u32`.
    BadNumeric(String),
    /// `id_type` was `g` but the address is not a canonical GUID.
    BadGuid(String),
    /// `id_type` was `b` but the address is not valid base64.
    BadOpaque(String),
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::Empty => write!(f, "empty node address"),
            NodeError::TooLong(n) => {
                write!(f, "node address too long ({n} > {MAX_IDENTIFIER_LEN})")
            }
            NodeError::BadNumeric(a) => write!(f, "not a numeric node identifier: {a}"),
            NodeError::BadGuid(a) => write!(f, "not a GUID node identifier: {a}"),
            NodeError::BadOpaque(a) => write!(f, "not a base64 node identifier: {a}"),
        }
    }
}

impl std::error::Error for NodeError {}

/// Validates `address` for `id_type` and renders the NodeId string form.
///
/// The string form is what gets handed to the OPC UA stack's `NodeId::from_str`,
/// which keeps this module free of any dependency on the stack itself.
pub fn node_id_string(ns: u16, id_type: IdType, address: &str) -> Result<String, NodeError> {
    if address.is_empty() {
        return Err(NodeError::Empty);
    }
    if address.len() > MAX_IDENTIFIER_LEN {
        return Err(NodeError::TooLong(address.len()));
    }

    match id_type {
        IdType::S => {
            // A raw `;` would make the rendered NodeId ambiguous.
            if address.contains(';') {
                return Err(NodeError::BadOpaque(address.to_string()));
            }
        }
        IdType::I => {
            address
                .parse::<u32>()
                .map_err(|_| NodeError::BadNumeric(address.to_string()))?;
        }
        IdType::G => {
            if !is_guid(address) {
                return Err(NodeError::BadGuid(address.to_string()));
            }
        }
        IdType::B => {
            if !is_base64(address) {
                return Err(NodeError::BadOpaque(address.to_string()));
            }
        }
    }

    Ok(format!("ns={};{}={}", ns, id_type.tag(), address))
}

fn is_guid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for len in GROUPS {
        match parts.next() {
            Some(p) if p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Resolves the namespace index to use for tag addresses.
///
/// When the shadow supplies `ns_uri` we look it up in the server's
/// NamespaceArray, because namespace *indexes* are not stable across server
/// restarts while URIs are. If the URI is absent or unknown we fall back to the
/// literal `ns` index from the settings, which is what small servers need.
///
/// Returns the resolved index and whether the URI lookup succeeded, so the
/// caller can log the (silent-misconfiguration-prone) fallback.
pub fn resolve_namespace(
    ns_uri: Option<&str>,
    namespace_array: &[String],
    fallback: u16,
) -> (u16, bool) {
    let Some(uri) = ns_uri.filter(|u| !u.is_empty()) else {
        return (fallback, false);
    };
    match namespace_array.iter().position(|n| n == uri) {
        Some(idx) if idx <= u16::MAX as usize => (idx as u16, true),
        _ => (fallback, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_four_identifier_types() {
        assert_eq!(
            node_id_string(2, IdType::S, "Chan1.Dev1.Tag0001").unwrap(),
            "ns=2;s=Chan1.Dev1.Tag0001"
        );
        assert_eq!(node_id_string(0, IdType::I, "2255").unwrap(), "ns=0;i=2255");
        assert_eq!(
            node_id_string(3, IdType::G, "72962B91-FA75-4AE6-8D28-B404DC7DAF63").unwrap(),
            "ns=3;g=72962B91-FA75-4AE6-8D28-B404DC7DAF63"
        );
        assert_eq!(
            node_id_string(4, IdType::B, "Zm9vYmFy").unwrap(),
            "ns=4;b=Zm9vYmFy"
        );
    }

    #[test]
    fn rejects_malformed_identifiers() {
        assert_eq!(node_id_string(2, IdType::S, ""), Err(NodeError::Empty));
        assert_eq!(
            node_id_string(2, IdType::I, "12a"),
            Err(NodeError::BadNumeric("12a".into()))
        );
        assert_eq!(
            node_id_string(2, IdType::I, "4294967296"),
            Err(NodeError::BadNumeric("4294967296".into()))
        );
        assert_eq!(
            node_id_string(2, IdType::G, "72962B91-FA75-4AE6-8D28-B404DC7DAF6"),
            Err(NodeError::BadGuid(
                "72962B91-FA75-4AE6-8D28-B404DC7DAF6".into()
            ))
        );
        assert_eq!(
            node_id_string(2, IdType::B, "not base64!"),
            Err(NodeError::BadOpaque("not base64!".into()))
        );
        assert!(matches!(
            node_id_string(2, IdType::S, &"x".repeat(MAX_IDENTIFIER_LEN + 1)),
            Err(NodeError::TooLong(_))
        ));
    }

    #[test]
    fn rejects_semicolon_in_string_identifier() {
        assert!(node_id_string(2, IdType::S, "a;b").is_err());
    }

    #[test]
    fn namespace_uri_resolves_against_server_array() {
        let arr = vec![
            "http://opcfoundation.org/UA/".to_string(),
            "urn:server:local".to_string(),
            "urn:example:server".to_string(),
        ];
        assert_eq!(
            resolve_namespace(Some("urn:example:server"), &arr, 9),
            (2, true)
        );
    }

    #[test]
    fn namespace_falls_back_when_uri_absent_or_unknown() {
        let arr = vec!["http://opcfoundation.org/UA/".to_string()];
        assert_eq!(resolve_namespace(None, &arr, 4), (4, false));
        assert_eq!(resolve_namespace(Some(""), &arr, 4), (4, false));
        assert_eq!(resolve_namespace(Some("urn:nope"), &arr, 4), (4, false));
    }
}
