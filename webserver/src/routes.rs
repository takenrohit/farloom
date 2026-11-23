//! Built-in route handlers: `GET /health` and `POST /api/echo`.

use crate::http::request::Request;
use crate::http::response::{Response, StatusCode};
use crate::router::Router;

/// `GET /health`: 200 with `{"status":"ok"}` as JSON.
pub fn health(_: &Request) -> Response {
    Response::new(StatusCode::Ok).with_body("application/json", br#"{"status":"ok"}"#.to_vec())
}

/// `POST /api/echo`: 200 with the request body and the request's `Content-Type`,
/// or `application/octet-stream` when the request has none.
pub fn echo(req: &Request) -> Response {
    let content_type = req
        .headers
        .get("content-type")
        .unwrap_or("application/octet-stream");
    Response::new(StatusCode::Ok).with_body(content_type, req.body.clone())
}

impl Router {
    /// A router with the built-in routes registered.
    pub fn with_builtins() -> Router {
        Router::new().get("/health", health).post("/api/echo", echo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::{Headers, Method, Version};
    use crate::router::RouteMatch;

    fn req(method: Method, path: &str, headers: Headers, body: &[u8]) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: None,
            version: Version::Http11,
            headers,
            body: body.to_vec(),
        }
    }

    fn content_type(resp: &Response) -> Option<&str> {
        resp.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn health_returns_json_ok() {
        let resp = health(&req(Method::Get, "/health", Headers::default(), b""));
        assert_eq!(resp.status, StatusCode::Ok);
        assert_eq!(content_type(&resp), Some("application/json"));
        assert_eq!(resp.body, br#"{"status":"ok"}"#);
    }

    #[test]
    fn echo_copies_body_and_content_type() {
        let mut headers = Headers::default();
        headers.insert("Content-Type", "text/plain");
        let resp = echo(&req(Method::Post, "/api/echo", headers, b"hello"));
        assert_eq!(resp.status, StatusCode::Ok);
        assert_eq!(content_type(&resp), Some("text/plain"));
        assert_eq!(resp.body, b"hello");

        let resp = echo(&req(Method::Post, "/api/echo", Headers::default(), b"\x00\xff"));
        assert_eq!(content_type(&resp), Some("application/octet-stream"));
        assert_eq!(resp.body, b"\x00\xff");
    }

    #[test]
    fn builtins_register_expected_routes() {
        let r = Router::with_builtins();
        assert!(matches!(r.lookup(&Method::Get, "/health"), RouteMatch::Found(_)));
        assert!(matches!(r.lookup(&Method::Post, "/api/echo"), RouteMatch::Found(_)));
        match r.lookup(&Method::Get, "/api/echo") {
            RouteMatch::MethodNotAllowed { allow } => assert_eq!(allow, vec![Method::Post]),
            _ => panic!("expected MethodNotAllowed"),
        }
    }
}
