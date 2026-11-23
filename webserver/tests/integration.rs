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
