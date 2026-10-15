//! Lowercase hex in both directions, for signatures and digests. Written out
//! rather than pulled in: it is twenty lines and one less crate in the graph.

/// Lowercase hex of `bytes`.
pub(crate) fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Decodes an even-length string of hex digits (either case). `None` for
/// anything else, including a sign character, which `from_str_radix` would
/// otherwise accept.
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_non_hex() {
        assert_eq!(encode(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(decode("00ABff"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(decode("+f"), None);
        assert_eq!(decode("abc"), None);
        assert_eq!(decode("zz"), None);
    }
}
