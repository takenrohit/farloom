//! Real-TCP integration tests against a server on an ephemeral port.

mod common;

use std::time::Duration;

use common::{get, TestServer};
use webserver::router::Router;

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
