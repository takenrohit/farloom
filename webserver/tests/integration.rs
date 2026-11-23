//! Real-TCP integration tests against a server on an ephemeral port.

mod common;

use std::io::{Read, Write};
use std::time::Duration;

use common::{connect, get, parse_raw_response, raw_request, RawResponse, TempDir, TestServer};
use webserver::router::Router;

const INDEX_HTML: &[u8] = b"<!doctype html>\n<html><body>Hello \xc3\xa9</body></html>\n";
const A_CSS: &[u8] = b"body { color: red; }\r\n/* trailing */\n";

/// Starts a server whose static root holds `index.html` and `a.css`.
fn static_server() -> TestServer {
    let root = TempDir::new();
    root.write("index.html", INDEX_HTML);
    root.write("a.css", A_CSS);
    TestServer::start_with_root(root, |_| {}, Router::with_builtins())
}

/// Sends a GET, reads the response, then checks the server closed the connection:
/// another read must return EOF (0) or a reset, never more data or a timeout.
fn get_expect_close(server: &TestServer, path: &str) -> RawResponse {
    let mut s = connect(server.addr()).expect("connect");
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes())
        .expect("write request");
    let raw = common::read_to_eof(&mut s).expect("read response to EOF");
    let mut buf = [0u8; 16];
    match s.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("unexpected {n} bytes after EOF"),
        Err(e) => assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            "expected EOF after response, got {e:?}"
        ),
    }
    parse_raw_response(raw).unwrap_or_else(|e| panic!("bad response: {e}"))
}

fn assert_static_ok(resp: &RawResponse, bytes: &[u8], mime: &str) {
    assert_eq!(resp.status, 200);
    assert_eq!(resp.reason, "OK");
    assert_eq!(resp.body, bytes);
    assert_eq!(resp.header("Content-Type"), Some(mime));
    assert_eq!(resp.header_count("Content-Length"), 1);
    assert_eq!(resp.header("Content-Length"), Some(bytes.len().to_string().as_str()));
    assert_eq!(resp.header("Connection"), Some("close"));
}

#[test]
fn static_index_served_via_root_and_explicit_path() {
    let server = static_server();
    for path in ["/", "/index.html"] {
        let resp = get_expect_close(&server, path);
        assert_static_ok(&resp, INDEX_HTML, "text/html; charset=utf-8");
    }
}

#[test]
fn static_css_served_with_exact_bytes_and_mime() {
    let server = static_server();
    let resp = get_expect_close(&server, "/a.css");
    assert_static_ok(&resp, A_CSS, "text/css; charset=utf-8");
}

#[test]
fn health_query_string_does_not_affect_routing() {
    let server = TestServer::start(|_| {}, Router::with_builtins());
    let resp = get(server.addr(), "/health?x=1");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("Content-Type"), Some("application/json"));
    assert_eq!(resp.header("Connection"), Some("close"));
    assert_eq!(resp.body_str(), r#"{"status":"ok"}"#);
}

fn post_echo(server: &TestServer, content_type: Option<&str>, body: &[u8]) -> RawResponse {
    let mut req = format!(
        "POST /api/echo HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(ct) = content_type {
        req.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    raw_request(server.addr(), &bytes)
}

#[test]
fn echo_returns_posted_bytes_and_content_type() {
    let server = TestServer::start(|_| {}, Router::with_builtins());
    let body: &[u8] = b"\x00\xff\xfe binary \x80\r\n\r\nend";
    let resp = post_echo(&server, Some("application/x-custom; v=1"), body);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, body);
    assert_eq!(resp.header("Content-Type"), Some("application/x-custom; v=1"));
    assert_eq!(resp.header("Content-Length"), Some(body.len().to_string().as_str()));
    assert_eq!(resp.header("Connection"), Some("close"));
}

#[test]
fn echo_without_content_type_is_octet_stream() {
    let server = TestServer::start(|_| {}, Router::with_builtins());
    let body: &[u8] = b"\xc0\xc1raw";
    let resp = post_echo(&server, None, body);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, body);
    assert_eq!(resp.header("Content-Type"), Some("application/octet-stream"));
    assert_eq!(resp.header("Connection"), Some("close"));
}

#[test]
fn health_returns_200() {
    let mut server = TestServer::start(|_| {}, Router::with_builtins());
    let resp = get(server.addr(), "/health");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.reason, "OK");
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert_eq!(resp.header("Connection"), Some("close"));
    assert_eq!(resp.body_str(), r#"{"status":"ok"}"#);
    assert!(server.stop_and_join(Duration::from_secs(1)));
}

// ---------------------------------------------------------------------------
// Error responses
// ---------------------------------------------------------------------------

/// Checks the shared error shape: status, reason, plain-text body naming the
/// code and reason, exact Content-Length, and `Connection: close`.
fn assert_error(resp: &RawResponse, code: u16, reason: &str) {
    assert_eq!(resp.status, code, "raw: {:?}", String::from_utf8_lossy(&resp.raw));
    assert_eq!(resp.reason, reason);
    assert_eq!(resp.header("Content-Type"), Some("text/plain; charset=utf-8"));
    assert_eq!(resp.header("Connection"), Some("close"));
    assert_eq!(resp.header_count("Content-Length"), 1);
    assert_eq!(
        resp.header("Content-Length"),
        Some(resp.body.len().to_string().as_str())
    );
    let body = resp.body_str();
    assert!(
        body.contains(&code.to_string()) && body.contains(reason),
        "error body {body:?} must contain {code} and {reason:?}"
    );
}

#[test]
fn bad_request_for_bare_lf_chunked_and_bad_percent_encoding() {
    let server = static_server();
    let cases: [&[u8]; 3] = [
        b"GET / HTTP/1.1\n\n",
        b"POST /api/echo HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        b"GET /%zz HTTP/1.1\r\nHost: test\r\n\r\n",
    ];
    for req in cases {
        let resp = raw_request(server.addr(), req);
        assert_error(&resp, 400, "Bad Request");
    }
}

#[test]
fn not_found_for_missing_file_and_traversal_attempts() {
    // Layout: <tmp>/Cargo.toml (must never be served) and <tmp>/public/ (static root).
    let root = TempDir::new();
    root.write("Cargo.toml", b"[package]\nname = \"secret\"\n");
    root.write("public/index.html", INDEX_HTML);
    let public = root.path().join("public");
    let server = TestServer::start_with_root(
        root,
        move |c| c.static_root = public,
        Router::with_builtins(),
    );
    for path in [
        "/missing.html",
        "/..%2f..%2fCargo.toml",
        "/..%2fCargo.toml",
        "/%5c..%5cCargo.toml",
    ] {
        let resp = get(server.addr(), path);
        assert_error(&resp, 404, "Not Found");
        assert!(!resp.body_str().contains("secret"), "{path} leaked Cargo.toml");
    }
    // Sanity check: the static root itself is served.
    assert_eq!(get(server.addr(), "/index.html").status, 200);
}

#[test]
fn method_not_allowed_lists_allowed_methods() {
    let server = static_server();
    let cases = [
        ("POST", "/index.html", "GET"),
        ("GET", "/api/echo", "POST"),
        ("DELETE", "/health", "GET"),
    ];
    for (method, path, allow) in cases {
        let req = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\r\n");
        let resp = raw_request(server.addr(), req.as_bytes());
        assert_error(&resp, 405, "Method Not Allowed");
        assert_eq!(resp.header("Allow"), Some(allow), "{method} {path}");
    }
}

#[test]
fn payload_too_large_is_delivered_without_body_sent() {
    let server = TestServer::start(|c| c.max_body_bytes = 10, Router::with_builtins());
    // Head only; the client keeps its write side open and never sends the body.
    let mut s = connect(server.addr()).expect("connect");
    s.write_all(b"POST /api/echo HTTP/1.1\r\nHost: test\r\nContent-Length: 11\r\n\r\n")
        .expect("write head");
    let raw = common::read_to_eof(&mut s).expect("must receive the 413, not a reset");
    let resp = parse_raw_response(raw).unwrap_or_else(|e| panic!("bad response: {e}"));
    assert_error(&resp, 413, "Payload Too Large");
}

#[test]
fn handler_panic_gives_500_and_server_keeps_serving() {
    let router = Router::with_builtins().get("/boom", |_| panic!("boom handler"));
    let mut server = TestServer::start(|_| {}, router);
    let resp = get(server.addr(), "/boom");
    assert_error(&resp, 500, "Internal Server Error");

    let resp = get(server.addr(), "/health");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), r#"{"status":"ok"}"#);
    assert!(server.stop_and_join(Duration::from_secs(2)));
}
