//! Small, dependency-free encoders used by the wire formats.
//!
//! We hand-roll base64/hex instead of pulling extra crates in: both are a few
//! lines, both are on the firmware's flash budget, and both are covered by
//! unit tests below.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;

        out.push(B64[(n >> 18) as usize & 0x3f] as char);
        out.push(B64[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// True when `s` is syntactically valid standard base64 (alphabet + padding).
pub fn is_base64(s: &str) -> bool {
    if s.is_empty() || s.len() % 4 != 0 {
        return false;
    }
    let bytes = s.as_bytes();
    let pad = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if pad > 2 {
        return false;
    }
    bytes[..bytes.len() - pad]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
}

/// Lowercase hex.
pub fn hex_encode(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(input.len() * 2);
    for b in input {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// SHA-256 of `data`, lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

/// Constant-shape, case-insensitive comparison of two hex digests.
pub fn hex_eq_ignore_case(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_covers_full_alphabet() {
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn base64_validation() {
        assert!(is_base64("Zm9vYmFy"));
        assert!(is_base64("Zg=="));
        assert!(!is_base64(""));
        assert!(!is_base64("Zg="));
        assert!(!is_base64("Zg===="));
        assert!(!is_base64("Zm9v*mFy"));
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_compare_is_case_insensitive() {
        assert!(hex_eq_ignore_case("9F2Ce1", "9f2ce1"));
        assert!(!hex_eq_ignore_case("9f2ce1", "9f2ce2"));
        assert!(!hex_eq_ignore_case("9f2ce1", "9f2ce"));
    }
}
