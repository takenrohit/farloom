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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_case_dot_escapes_decode_to_dotdot() {
        assert_eq!(percent_decode("%2e%2E").unwrap(), b"..".to_vec());
        assert_eq!(validate("%2e%2E"), Ok(()));
    }

    #[test]
    fn malformed_sequences_report_index_of_percent() {
        for (input, index) in [
            ("%", 0),
            ("%4", 0),
            ("%zz", 0),
            ("/a%", 2),
            ("/a%4", 2),
            ("/ok%20/%zz", 7),
            ("%4g", 0),
            ("%g4", 0),
        ] {
            let expected = Err(PercentError { index });
            assert_eq!(validate(input), expected, "validate({input:?})");
            assert_eq!(
                percent_decode(input).map(|_| ()),
                expected,
                "percent_decode({input:?})"
            );
        }
    }

    #[test]
    fn plus_is_preserved() {
        assert_eq!(percent_decode("/a+b").unwrap(), b"/a+b".to_vec());
        assert_eq!(percent_decode("%2B+").unwrap(), b"++".to_vec());
    }

    #[test]
    fn encoded_utf8_decodes_to_original_bytes() {
        let original = "/caf\u{e9}/\u{65e5}\u{672c}/\u{1f980}";
        let encoded: String = original
            .bytes()
            .map(|b| {
                if b.is_ascii() {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        assert_eq!(encoded, "/caf%C3%A9/%E6%97%A5%E6%9C%AC/%F0%9F%A6%80");
        assert_eq!(percent_decode(&encoded).unwrap(), original.as_bytes());
    }

    #[test]
    fn raw_non_ascii_input_is_copied_unchanged() {
        let s = "/caf\u{e9}";
        assert_eq!(validate(s), Ok(()));
        assert_eq!(percent_decode(s).unwrap(), s.as_bytes());
    }

    #[test]
    fn decoding_may_produce_invalid_utf8() {
        assert_eq!(percent_decode("%FF%00").unwrap(), vec![0xFF, 0x00]);
    }

    #[test]
    fn empty_and_plain_strings() {
        assert_eq!(validate(""), Ok(()));
        assert_eq!(percent_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(percent_decode("/index.html").unwrap(), b"/index.html".to_vec());
    }
}
