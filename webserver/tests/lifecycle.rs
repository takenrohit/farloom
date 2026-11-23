//! Integration tests for back-pressure (503), read timeouts, shutdown,
//! `max_requests`, bind conflicts, and concurrent clients.

mod common;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    connect, get, parse_raw_response, raw_exchange, read_to_eof, read_without_request, TempDir,
    TestServer,
};
use webserver::config::Config;
use webserver::http::response::{Response, StatusCode};
use webserver::router::Router;
use webserver::server::{Server, StartupError};

/// Router with GET /block, which signals `entered` and then waits up to 10 s for `release`.
fn blocking_router() -> (Router, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    let router = Router::with_builtins().get("/block", move |_| {
        let _ = entered_tx.lock().unwrap().send(());
        let _ = release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
        Response::new(StatusCode::Ok).with_body("text/plain", b"released".to_vec())
    });
    (router, entered_rx, release_tx)
}

#[test]
fn full_queue_gets_503_and_queued_requests_complete() {
    let (router, entered_rx, release_tx) = blocking_router();
    let server = TestServer::start(
        |c| {
            c.workers = 1;
            c.queue_capacity = 1;
        },
        router,
    );
    let addr = server.addr();

    // A occupies the only worker.
    let a = thread::spawn(move || get(addr, "/block"));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("/block handler entered");

    // B fills the single queue slot. Its handshake completes before C's, and the
    // acceptor takes connections from the backlog in order, so B is queued first.
    let mut b = connect(addr).expect("connect B");
    b.write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n\r\n")
        .expect("write B");
    // Brief pause so the acceptor's 25 ms poll has certainly picked B up.
    thread::sleep(Duration::from_millis(100));

    // C sends nothing and must be rejected by the acceptor.
    let raw = read_without_request(addr).expect("read 503 for C");
    let resp = parse_raw_response(raw).unwrap_or_else(|e| panic!("bad 503: {e}"));
    assert_eq!(resp.status, 503);
    assert_eq!(resp.reason, "Service Unavailable");
    assert_eq!(resp.header("Retry-After"), Some("1"));
    assert!(
        resp.header("Content-Type")
            .is_some_and(|ct| ct.starts_with("text/plain")),
        "{:?}",
        resp.header("Content-Type")
    );
    assert_eq!(resp.header("Connection"), Some("close"));
    assert!(resp.body_str().contains("503"));

    release_tx.send(()).unwrap();
    let ra = a.join().expect("client A panicked");
    assert_eq!(ra.status, 200);
    assert_eq!(ra.body_str(), "released");

    let rb = parse_raw_response(read_to_eof(&mut b).expect("read B"))
        .unwrap_or_else(|e| panic!("bad response for B: {e}"));
    assert_eq!(rb.status, 200);
    assert_eq!(rb.body_str(), r#"{"status":"ok"}"#);
}

/// Waits for the server to close `s`: a read must return EOF or a reset, not data
/// or a client-side timeout, within `limit`.
fn assert_closed_within(mut s: TcpStream, limit: Duration) {
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let start = Instant::now();
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!(
            "expected close, got {n} bytes: {:?}",
            String::from_utf8_lossy(&buf[..n])
        ),
        Err(e) => assert!(
            matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ),
            "expected EOF or reset, got {e:?}"
        ),
    }
    let elapsed = start.elapsed();
    assert!(elapsed < limit, "closed after {elapsed:?}");
}

#[test]
fn idle_and_partial_connections_closed_after_read_timeout() {
    let server = TestServer::start(
        |c| c.read_timeout = Duration::from_secs(1),
        Router::with_builtins(),
    );

    let idle = connect(server.addr()).expect("connect idle");
    assert_closed_within(idle, Duration::from_secs(3));

    let mut partial = connect(server.addr()).expect("connect partial");
    partial
        .write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n")
        .expect("write partial head");
    assert_closed_within(partial, Duration::from_secs(3));

    // The server still serves after the timeouts.
    assert_eq!(get(server.addr(), "/health").status, 200);
}

#[test]
fn shutdown_returns_quickly_and_stops_listening() {
    let mut server = TestServer::start(|_| {}, Router::with_builtins());
    let addr = server.addr();
    assert_eq!(get(addr, "/health").status, 200);

    let start = Instant::now();
    assert!(
        server.stop_and_join(Duration::from_secs(1)),
        "run did not return within 1 s"
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err(),
        "listener still accepting after shutdown"
    );
}

#[test]
fn max_requests_stops_run_after_n_connections() {
    let mut server = TestServer::start(|c| c.max_requests = Some(3), Router::with_builtins());
    for _ in 0..3 {
        assert_eq!(get(server.addr(), "/health").status, 200);
    }
    assert!(
        server.join_timeout(Duration::from_secs(2)),
        "run did not return after 3 connections"
    );
}

#[test]
fn bind_conflict_is_startup_bind_error() {
    let taken = TcpListener::bind("127.0.0.1:0").expect("pre-bind");
    let root = TempDir::new();
    let config = Config {
        host: "127.0.0.1".to_string(),
        port: taken.local_addr().unwrap().port(),
        static_root: root.path().to_path_buf(),
        ..Config::default()
    };
    match Server::bind(config, Router::with_builtins()) {
        Err(StartupError::Bind { .. }) => {}
        Err(e) => panic!("expected Bind error, got {e:?}"),
        Ok(_) => panic!("bind on a taken port succeeded"),
    }
}

#[test]
fn concurrent_clients_get_200_or_503() {
    let server = TestServer::start(|c| c.workers = 4, Router::with_builtins());
    let addr: SocketAddr = server.addr();
    let clients: Vec<_> = (0..32)
        .map(|_| {
            thread::spawn(move || {
                let raw = raw_exchange(addr, b"GET /health HTTP/1.1\r\nHost: test\r\n\r\n")
                    .expect("client I/O error");
                parse_raw_response(raw).unwrap_or_else(|e| panic!("bad response: {e}"))
            })
        })
        .collect();

    let mut ok = 0;
    for c in clients {
        let resp = c.join().expect("client thread panicked");
        match resp.status {
            200 => {
                assert_eq!(resp.body_str(), r#"{"status":"ok"}"#);
                ok += 1;
            }
            503 => assert_eq!(resp.header("Retry-After"), Some("1")),
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(ok > 0, "no client got 200");
}
