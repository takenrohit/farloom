//! Response types and serialization: `StatusCode`, `Response`, `write_response`.

use std::io::{self, Write};

use super::request::Method;

/// The status codes this server sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Ok,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    PayloadTooLarge,
    InternalServerError,
    ServiceUnavailable,
}

impl StatusCode {
    pub fn code(self) -> u16 {
        match self {
            StatusCode::Ok => 200,
            StatusCode::BadRequest => 400,
            StatusCode::NotFound => 404,
            StatusCode::MethodNotAllowed => 405,
            StatusCode::PayloadTooLarge => 413,
            StatusCode::InternalServerError => 500,
            StatusCode::ServiceUnavailable => 503,
        }
    }

    /// Reason phrases from Requirement 9.4.
    pub fn reason(self) -> &'static str {
        match self {
            StatusCode::Ok => "OK",
            StatusCode::BadRequest => "Bad Request",
            StatusCode::NotFound => "Not Found",
            StatusCode::MethodNotAllowed => "Method Not Allowed",
            StatusCode::PayloadTooLarge => "Payload Too Large",
            StatusCode::InternalServerError => "Internal Server Error",
            StatusCode::ServiceUnavailable => "Service Unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: StatusCode,
    /// Handler-set headers, in order. `Content-Length` and `Connection`
    /// are owned by the writer and ignored here.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

impl Response {
    pub fn new(status: StatusCode) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Sets the body and its `Content-Type`, replacing any existing `Content-Type`.
    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Response {
        self.headers
            .retain(|(n, _)| !n.eq_ignore_ascii_case("content-type"));
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = body;
        self
    }

    /// Error response: `text/plain; charset=utf-8` with body `"{code} {reason}\n"`.
    pub fn error(status: StatusCode) -> Response {
        let body = format!("{} {}\n", status.code(), status.reason());
        Response::new(status).with_body(TEXT_PLAIN, body.into_bytes())
    }

    /// 405 error response with an `Allow` header built from `allow`, sorted and deduplicated.
    pub fn method_not_allowed(allow: &[Method]) -> Response {
        let mut methods = allow.to_vec();
        methods.sort();
        methods.dedup();
        let value = methods
            .iter()
            .map(Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        Response::error(StatusCode::MethodNotAllowed).with_header("Allow", &value)
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    /// Serializes the full response into one buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            self.status.code(),
            self.status.reason()
        );
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("connection")
            {
                continue;
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if !self.body.is_empty() && !self.has_header("content-type") {
            head.push_str("Content-Type: application/octet-stream\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        head.push_str("Connection: close\r\n\r\n");

        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Writes the whole response with one `write_all`, then flushes.
pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> io::Result<()> {
    w.write_all(&resp.to_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_serializes_with_owned_headers() {
        let bytes = Response::error(StatusCode::NotFound).to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            "HTTP/1.1 404 Not Found\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 14\r\n\
             Connection: close\r\n\r\n\
             404 Not Found\n"
        );
    }

    #[test]
    fn handler_framing_headers_are_dropped_and_default_type_added() {
        let resp = Response::new(StatusCode::Ok)
            .with_header("content-length", "999")
            .with_header("CONNECTION", "keep-alive")
            .with_header("X-Test", "1");
        let resp = Response { body: b"abc".to_vec(), ..resp };
        let mut out = Vec::new();
        write_response(&mut out, &resp).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 200 OK\r\nX-Test: 1\r\nContent-Type: application/octet-stream\r\n\
             Content-Length: 3\r\nConnection: close\r\n\r\nabc"
        );
    }

    #[test]
    fn method_not_allowed_sorts_allow_list() {
        let resp = Response::method_not_allowed(&[Method::Post, Method::Get]);
        assert_eq!(resp.status.code(), 405);
        assert!(resp
            .headers
            .contains(&("Allow".to_string(), "GET, POST".to_string())));
    }

    #[test]
    fn empty_body_has_no_content_type() {
        let text = String::from_utf8(Response::new(StatusCode::Ok).to_bytes()).unwrap();
        assert_eq!(
            text,
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }
}
