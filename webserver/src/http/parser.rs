//! HTTP/1.x request parser (RFC 9112 subset): request line, headers, and a
//! `Content-Length`-delimited body. Generic over `BufRead`, so tests can feed
//! byte slices and the server can feed `BufReader<&TcpStream>`.

use std::io::{self, BufRead, Read};

use super::percent;
use super::request::{Headers, Method, Request, Version};

/// Size limits applied while parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Budget for request line + headers + CRLFs + final empty line.
    pub max_head_bytes: usize,
    /// Largest accepted `Content-Length`.
    pub max_body_bytes: u64,
}

impl Limits {
    pub const DEFAULT_MAX_HEAD_BYTES: usize = 8192;

    pub fn new(max_body_bytes: u64) -> Limits {
        Limits { max_head_bytes: Self::DEFAULT_MAX_HEAD_BYTES, max_body_bytes }
    }
}

#[derive(Debug)]
pub enum ParseError {
    /// Malformed input; the reason goes into the log line. Maps to 400.
    BadRequest(&'static str),
    /// `Content-Length` above the limit (or overflowing `u64`). Body not read. Maps to 413.
    PayloadTooLarge,
    /// EOF after some bytes of head or body were received.
    Incomplete,
    /// Read timed out (`WouldBlock` on Unix, `TimedOut` on Windows).
    Timeout,
    /// EOF before any byte was received.
    ConnectionClosed,
    /// Any other I/O error.
    Io(io::Error),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::BadRequest(r) => write!(f, "bad request: {r}"),
            ParseError::PayloadTooLarge => f.write_str("payload too large"),
            ParseError::Incomplete => f.write_str("incomplete request"),
            ParseError::Timeout => f.write_str("read timed out"),
            ParseError::ConnectionClosed => f.write_str("connection closed before request"),
            ParseError::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for ParseError {}

fn map_io(e: io::Error) -> ParseError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ParseError::Timeout,
        io::ErrorKind::UnexpectedEof => ParseError::Incomplete,
        _ => ParseError::Io(e),
    }
}

/// RFC 9110 `tchar`.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_tchar)
}

/// Reads one CRLF-terminated line, charging its length against `remaining`.
/// Returns the line without the CRLF.
fn read_line_limited<R: BufRead>(
    reader: &mut R,
    remaining: &mut usize,
    first: bool,
) -> Result<String, ParseError> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take(*remaining as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(map_io)?;
    if n == 0 {
        return Err(if first { ParseError::ConnectionClosed } else { ParseError::Incomplete });
    }
    if buf.len() > *remaining {
        return Err(ParseError::BadRequest("header section too large"));
    }
    if buf.last() != Some(&b'\n') {
        // Under budget and no newline: the reader hit EOF mid-line.
        return Err(ParseError::Incomplete);
    }
    if !buf.ends_with(b"\r\n") {
        return Err(ParseError::BadRequest("line not terminated by CRLF"));
    }
    *remaining -= buf.len();
    buf.truncate(buf.len() - 2);
    String::from_utf8(buf).map_err(|_| ParseError::BadRequest("line is not valid UTF-8"))
}

fn parse_request_line(line: &str) -> Result<(Method, String, Option<String>, Version), ParseError> {
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() != 3 {
        return Err(ParseError::BadRequest("request line must have three parts"));
    }
    let (method, target, version) = (parts[0], parts[1], parts[2]);

    if !is_token(method) {
        return Err(ParseError::BadRequest("invalid method token"));
    }
    if !target.starts_with('/') {
        return Err(ParseError::BadRequest("request target must be origin-form"));
    }
    // RFC 3986 targets are ASCII; non-ASCII must arrive percent-encoded. Rejecting raw
    // non-ASCII also keeps Unicode controls (U+0085, U+2028, C1 codes) out of the logs.
    if target.bytes().any(|b| b <= 0x20 || b >= 0x7F || b == b'#') {
        return Err(ParseError::BadRequest("invalid character in request target"));
    }
    let version = match version {
        "HTTP/1.1" => Version::Http11,
        "HTTP/1.0" => Version::Http10,
        _ => return Err(ParseError::BadRequest("unsupported HTTP version")),
    };
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    percent::validate(path).map_err(|_| ParseError::BadRequest("invalid percent-encoding"))?;
    if let Some(q) = query {
        percent::validate(q).map_err(|_| ParseError::BadRequest("invalid percent-encoding"))?;
    }
    Ok((Method::parse(method), path.to_string(), query.map(str::to_string), version))
}

fn parse_header_line(line: &str, headers: &mut Headers) -> Result<(), ParseError> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return Err(ParseError::BadRequest("obsolete line folding"));
    }
    let (name, value) = line
        .split_once(':')
        .ok_or(ParseError::BadRequest("header line without colon"))?;
    if !is_token(name) {
        return Err(ParseError::BadRequest("invalid header name"));
    }
    let value = value.trim_matches(|c| c == ' ' || c == '\t');
    if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7F) {
        return Err(ParseError::BadRequest("control character in header value"));
    }
    headers.insert(name, value);
    Ok(())
}

/// Returns the declared body length, or `None` when there is no `Content-Length`.
fn body_length(headers: &Headers, limits: &Limits) -> Result<Option<u64>, ParseError> {
    if headers.get("transfer-encoding").is_some() {
        return Err(ParseError::BadRequest("Transfer-Encoding not supported"));
    }
    let mut canonical: Option<&str> = None;
    let mut raw: Option<&str> = None;
    for v in headers.get_all("content-length") {
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseError::BadRequest("invalid Content-Length"));
        }
        let stripped = v.trim_start_matches('0');
        let stripped = if stripped.is_empty() { "0" } else { stripped };
        match canonical {
            Some(c) if c != stripped => {
                return Err(ParseError::BadRequest("conflicting Content-Length"))
            }
            Some(_) => {}
            None => {
                canonical = Some(stripped);
                raw = Some(v);
            }
        }
    }
    let Some(raw) = raw else { return Ok(None) };
    // All digits, so the only possible parse failure is overflow.
    let n: u64 = raw.parse().map_err(|_| ParseError::PayloadTooLarge)?;
    if n > limits.max_body_bytes {
        return Err(ParseError::PayloadTooLarge);
    }
    Ok(Some(n))
}

/// Parses one request from `reader`. On `PayloadTooLarge`, no body bytes are consumed.
pub fn parse_request<R: BufRead>(reader: &mut R, limits: &Limits) -> Result<Request, ParseError> {
    let mut remaining = limits.max_head_bytes;

    let line = read_line_limited(reader, &mut remaining, true)?;
    let (method, path, query, version) = parse_request_line(&line)?;

    let mut headers = Headers::default();
    loop {
        let line = read_line_limited(reader, &mut remaining, false)?;
        if line.is_empty() {
            break;
        }
        parse_header_line(&line, &mut headers)?;
    }

    let body = match body_length(&headers, limits)? {
        None | Some(0) => Vec::new(),
        Some(n) => {
            let mut body = vec![0u8; n as usize];
            reader.read_exact(&mut body).map_err(map_io)?;
            body
        }
    };

    Ok(Request { method, path, query, version, headers, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Result<Request, ParseError> {
        let mut r = bytes;
        parse_request(&mut r, &Limits::new(1024))
    }

    #[test]
    fn parses_simple_get() {
        let req = parse(b"GET /index.html?x=1 HTTP/1.1\r\nHost: Example\r\n\r\n").unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/index.html");
        assert_eq!(req.query.as_deref(), Some("x=1"));
        assert_eq!(req.version, Version::Http11);
        assert_eq!(req.headers.get("host"), Some("Example"));
        assert!(req.body.is_empty());
    }

    #[test]
    fn parses_post_with_body_and_leaves_trailing_bytes() {
        let input = b"POST /api/echo HTTP/1.0\r\nContent-Length:  5 \r\n\r\nhelloEXTRA";
        let mut r: &[u8] = input;
        let req = parse_request(&mut r, &Limits::new(1024)).unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.version, Version::Http10);
        assert_eq!(req.headers.get("Content-Length"), Some("5"));
        assert_eq!(req.body, b"hello");
        assert_eq!(r, b"EXTRA");
    }

    #[test]
    fn empty_input_is_connection_closed() {
        assert!(matches!(parse(b""), Err(ParseError::ConnectionClosed)));
    }

    #[test]
    fn rejects_bare_lf_and_transfer_encoding() {
        assert!(matches!(parse(b"GET / HTTP/1.1\n\n"), Err(ParseError::BadRequest(_))));
        assert!(matches!(
            parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(ParseError::BadRequest(_))
        ));
    }

    #[test]
    fn oversized_body_is_payload_too_large_without_reading() {
        let input = b"POST / HTTP/1.1\r\nContent-Length: 2000\r\n\r\nBODY";
        let mut r: &[u8] = input;
        assert!(matches!(parse_request(&mut r, &Limits::new(1024)), Err(ParseError::PayloadTooLarge)));
        assert_eq!(r, b"BODY");
    }

    #[test]
    fn truncated_body_is_incomplete() {
        assert!(matches!(
            parse(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc"),
            Err(ParseError::Incomplete)
        ));
    }

    fn assert_bad(bytes: &[u8]) {
        match parse(bytes) {
            Err(ParseError::BadRequest(_)) => {}
            other => panic!(
                "expected BadRequest for {:?}, got {:?}",
                String::from_utf8_lossy(bytes),
                other
            ),
        }
    }

    /// Yields `data` once, then fails every read with `kind`.
    struct FailAfter {
        data: Vec<u8>,
        pos: usize,
        kind: io::ErrorKind,
    }

    impl Read for FailAfter {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos < self.data.len() {
                let n = buf.len().min(self.data.len() - self.pos);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            } else {
                Err(io::Error::new(self.kind, "mock"))
            }
        }
    }

    fn parse_failing(prefix: &[u8], kind: io::ErrorKind) -> Result<Request, ParseError> {
        let mut r = io::BufReader::new(FailAfter { data: prefix.to_vec(), pos: 0, kind });
        parse_request(&mut r, &Limits::new(1024))
    }

    #[test]
    fn timeouts_map_to_timeout_before_and_mid_body() {
        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
            assert!(matches!(parse_failing(b"", kind), Err(ParseError::Timeout)), "{kind:?}");
            assert!(
                matches!(parse_failing(b"GET / HTTP/1.1\r\nHo", kind), Err(ParseError::Timeout)),
                "{kind:?} mid-head"
            );
            assert!(
                matches!(
                    parse_failing(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc", kind),
                    Err(ParseError::Timeout)
                ),
                "{kind:?} mid-body"
            );
        }
    }

    #[test]
    fn head_of_exactly_limit_succeeds_and_one_more_fails() {
        // "GET / HTTP/1.1\r\n" (16) + "X: " (3) + pad + "\r\n" (2) + "\r\n" (2)
        let build = |total: usize| {
            let pad = total - 23;
            let mut v = b"GET / HTTP/1.1\r\nX: ".to_vec();
            v.extend(std::iter::repeat(b'a').take(pad));
            v.extend_from_slice(b"\r\n\r\n");
            assert_eq!(v.len(), total);
            v
        };
        assert!(parse(&build(8192)).is_ok());
        assert_bad(&build(8193));
    }

    #[test]
    fn content_length_rules() {
        let req = parse(b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();
        assert_eq!(req.body, b"hello");

        assert_bad(b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!");
        for v in ["+5", "-1", "5,5", " "] {
            assert_bad(format!("POST / HTTP/1.1\r\nContent-Length:{v}\r\n\r\n").as_bytes());
        }
        assert!(matches!(
            parse(b"POST / HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n"),
            Err(ParseError::PayloadTooLarge)
        ));
    }

    #[test]
    fn header_names_lowercased_and_values_trimmed() {
        let req = parse(b"GET / HTTP/1.1\r\nX-Custom-HEADER: \t value \t\r\n\r\n").unwrap();
        assert_eq!(req.headers.get("x-custom-header"), Some("value"));
        assert!(format!("{:?}", req.headers).contains("\"x-custom-header\""));
    }

    #[test]
    fn malformed_heads_are_bad_request() {
        assert_bad(b"GET / HTTP/1.1\r\nA: b\r\n continued\r\n\r\n"); // obs-fold
        assert_bad(b"GET / HTTP/1.1\r\nHost : x\r\n\r\n"); // whitespace before colon
        assert_bad(b"GET /a%zz HTTP/1.1\r\n\r\n");
        assert_bad(b"GET /a?q=%4 HTTP/1.1\r\n\r\n");
        assert_bad(b"GET /a#frag HTTP/1.1\r\n\r\n");
        assert_bad(b"GET http://example.com/ HTTP/1.1\r\n\r\n");
        assert_bad(b"GET / HTTP/2.0\r\n\r\n");
    }

    #[test]
    fn raw_non_ascii_target_is_bad_request_but_encoded_is_fine() {
        assert_bad("GET /caf\u{e9} HTTP/1.1\r\n\r\n".as_bytes());
        assert_bad("GET /a\u{2028}b HTTP/1.1\r\n\r\n".as_bytes());
        assert_bad("GET /a?q=\u{85} HTTP/1.1\r\n\r\n".as_bytes());
        let req = parse(b"GET /caf%C3%A9 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.path, "/caf%C3%A9");
    }

    #[test]
    fn lowercase_method_is_other() {
        let req = parse(b"get / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.method, Method::Other("get".into()));
    }
}
