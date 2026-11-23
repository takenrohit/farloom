//! Percent-encoding validation and decoding for request targets (RFC 3986 section 2.1).
//!
//! `+` is not treated as an encoded space: in paths it is a literal `+`.

/// A `%` that is not followed by two hex digits.
/// `index` is the byte offset of the offending `%` in the input string.
#[derive(Debug, PartialEq, Eq)]
pub struct PercentError {
    pub index: usize,
}

impl std::fmt::Display for PercentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid percent-encoding at byte {}", self.index)
    }
}

impl std::error::Error for PercentError {}

/// Value of one ASCII hex digit, or `None`.
fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes the `%XX` sequence starting at `i` (where `bytes[i] == b'%'`).
fn decode_at(bytes: &[u8], i: usize) -> Result<u8, PercentError> {
    let hi = bytes.get(i + 1).copied().and_then(hex_value);
    let lo = bytes.get(i + 2).copied().and_then(hex_value);
    match (hi, lo) {
        (Some(h), Some(l)) => Ok(h << 4 | l),
        _ => Err(PercentError { index: i }),
    }
}

/// Checks that every `%` in `s` is followed by two hex digits.
pub fn validate(s: &str) -> Result<(), PercentError> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            decode_at(bytes, i)?;
            i += 3;
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Decodes `%XX` sequences into raw bytes. Other bytes, including `+`, are copied unchanged.
/// The result may not be valid UTF-8; callers decide how to handle that.
pub fn percent_decode(s: &str) -> Result<Vec<u8>, PercentError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(decode_at(bytes, i)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}
