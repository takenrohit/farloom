//! Per-connection request handling run on a pool worker.
//!
//! `handle_connection` parses one request, dispatches it, writes one
//! `Connection: close` response, closes the socket gracefully, and logs one
//! access line. `dispatch` is the pure routing step and needs no socket.

use std::io::{BufReader, ErrorKind, Read};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::time::{Duration, Instant};

use crate::http::parser::{parse_request, ParseError};
use crate::http::request::Request;
use crate::http::response::{write_response, Response, StatusCode};
use crate::log::{self, AccessRecord};
use crate::pool::panic_message;
use crate::router::RouteMatch;
use crate::server::{AppContext, Connection};

/// Time a worker spends draining the socket after sending a response.
const WORKER_CLOSE_BUDGET: Duration = Duration::from_millis(250);

/// Most bytes `close_gracefully` reads and discards before giving up.
const MAX_DRAIN_BYTES: usize = 64 * 1024;

/// Routes `req` without client information (traversal logs omit the client).
pub fn dispatch(ctx: &AppContext, req: &Request) -> Response {
    dispatch_from(ctx, req, None)
}

/// Routes `req`: registered handler (panic gives 500), 405 for a known path with
/// another method, or the static file handler when no route matches.
pub fn dispatch_from(ctx: &AppContext, req: &Request, client: Option<SocketAddr>) -> Response {
    match ctx.router.lookup(&req.method, &req.path) {
        RouteMatch::Found(handler) => match panic::catch_unwind(AssertUnwindSafe(|| handler(req))) {
            Ok(resp) => resp,
            Err(payload) => {
                log::error("handler_panic", &panic_message(&*payload));
                Response::error(StatusCode::InternalServerError)
            }
        },
        RouteMatch::MethodNotAllowed { allow } => Response::method_not_allowed(&allow),
        RouteMatch::NotFound => ctx.static_files.serve_from(req, client),
    }
}

/// The pool handler: handles exactly one request on `conn` and closes it.
pub fn handle_connection(ctx: &AppContext, conn: Connection) {
    let start = Instant::now();
    let Connection { stream, peer } = conn;

    let parsed = {
        let mut reader = BufReader::new(&stream);
        parse_request(&mut reader, &ctx.limits)
    };

    let (resp, method, path) = match parsed {
        Ok(req) => {
            let resp = dispatch_from(ctx, &req, Some(peer));
            (resp, req.method.as_str().to_string(), req.path)
        }
        Err(ParseError::BadRequest(reason)) => {
            log::error("parse_error", &format!("client={peer} {reason}"));
            (Response::error(StatusCode::BadRequest), "-".to_string(), "-".to_string())
        }
        Err(ParseError::PayloadTooLarge) => (
            Response::error(StatusCode::PayloadTooLarge),
            "-".to_string(),
            "-".to_string(),
        ),
        Err(ParseError::Timeout) => {
            log::error("timeout", &format!("client={peer} read timed out"));
            return;
        }
        Err(ParseError::Incomplete) => {
            log::error("incomplete", &format!("client={peer} connection closed mid-request"));
            return;
        }
        Err(ParseError::Io(e)) => {
            log::error("io", &format!("client={peer} {e}"));
            return;
        }
        Err(ParseError::ConnectionClosed) => return,
    };

    if let Err(e) = write_response(&mut &stream, &resp) {
        log::error("write", &format!("client={peer} {e}"));
        return;
    }
    // Measure before the drain so dur_ms reflects request handling, not the close budget.
    let dur_ms = start.elapsed().as_millis();
    close_gracefully(stream, WORKER_CLOSE_BUDGET);

    let worker = log::current_worker();
    log::access(&AccessRecord {
        ts_ms: log::now_ms(),
        worker: &worker,
        client: peer,
        method: &method,
        path: &path,
        status: resp.status.code(),
        dur_ms,
    });
}

/// Half-closes the write side, then reads and discards input until EOF, an
/// error, `MAX_DRAIN_BYTES`, or `budget` elapses. This lets the peer read the
/// response before the socket closes, instead of receiving a TCP RST because
/// unread bytes were left in the receive buffer.
pub fn close_gracefully(stream: TcpStream, budget: Duration) {
    let deadline = Instant::now() + budget;
    let _ = stream.shutdown(Shutdown::Write);
    let mut buf = [0u8; 4096];
    let mut drained = 0usize;
    let mut reader = &stream;
    while drained < MAX_DRAIN_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // set_read_timeout(Some(ZERO)) is an InvalidInput error, so stop instead.
        if remaining.is_zero() {
            break;
        }
        if stream.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => drained += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // Dropping `stream` closes the socket.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::parser::Limits;
    use crate::http::request::{Headers, Method, Version};
    use crate::router::Router;
    use crate::static_files::StaticFiles;
    use std::io::Write;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "webserver-conn-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn ctx(root: &TempDir, max_body: u64) -> AppContext {
        let router = Router::with_builtins().get("/boom", |_| panic!("boom handler"));
        AppContext {
            router,
            static_files: StaticFiles::new(std::fs::canonicalize(&root.0).unwrap()),
            limits: Limits::new(max_body),
        }
    }

    fn req(method: Method, path: &str) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: None,
            version: Version::Http11,
            headers: Headers::default(),
            body: Vec::new(),
        }
    }

    fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn dispatch_routes_found_405_panic_and_static() {
        let root = temp_dir();
        std::fs::write(root.0.join("a.txt"), b"hello").unwrap();
        let c = ctx(&root, 1024);

        let r = dispatch(&c, &req(Method::Get, "/health"));
        assert_eq!(r.status, StatusCode::Ok);
        assert_eq!(r.body, br#"{"status":"ok"}"#);

        let r = dispatch(&c, &req(Method::Get, "/api/echo"));
        assert_eq!(r.status, StatusCode::MethodNotAllowed);
        assert_eq!(header(&r, "Allow"), Some("POST"));

        let r = dispatch(&c, &req(Method::Get, "/boom"));
        assert_eq!(r.status, StatusCode::InternalServerError);

        let r = dispatch(&c, &req(Method::Get, "/a.txt"));
        assert_eq!(r.status, StatusCode::Ok);
        assert_eq!(r.body, b"hello");

        let r = dispatch(&c, &req(Method::Get, "/missing.txt"));
        assert_eq!(r.status, StatusCode::NotFound);

        let r = dispatch(&c, &req(Method::Post, "/a.txt"));
        assert_eq!(r.status, StatusCode::MethodNotAllowed);
        assert_eq!(header(&r, "Allow"), Some("GET"));
    }

    #[test]
    fn dispatch_panic_gives_500_and_context_still_serves_health() {
        let root = temp_dir();
        let c = ctx(&root, 1024);
        for _ in 0..2 {
            let r = dispatch(&c, &req(Method::Get, "/boom"));
            assert_eq!(r.status, StatusCode::InternalServerError);
            let r = dispatch(&c, &req(Method::Get, "/health"));
            assert_eq!(r.status, StatusCode::Ok);
            assert_eq!(r.body, br#"{"status":"ok"}"#);
        }
    }

    #[test]
    fn dispatch_wrong_method_on_multi_method_route_lists_allow() {
        let root = temp_dir();
        let router = Router::new()
            .get("/multi", |_| Response::new(StatusCode::Ok))
            .post("/multi", |_| Response::new(StatusCode::Ok));
        let c = AppContext {
            router,
            static_files: StaticFiles::new(std::fs::canonicalize(&root.0).unwrap()),
            limits: Limits::new(1024),
        };
        let r = dispatch(&c, &req(Method::Other("DELETE".into()), "/multi"));
        assert_eq!(r.status, StatusCode::MethodNotAllowed);
        assert_eq!(header(&r, "Allow"), Some("GET, POST"));
        // Registered methods still dispatch normally.
        assert_eq!(dispatch(&c, &req(Method::Post, "/multi")).status, StatusCode::Ok);
    }

    /// Sends `bytes` to a one-shot server running `handle_connection` and
    /// returns everything the server wrote before closing.
    fn roundtrip(c: AppContext, bytes: &[u8], half_close: bool) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let c = Arc::new(c);
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            handle_connection(&c, Connection { stream, peer });
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.write_all(bytes).unwrap();
        if half_close {
            client.shutdown(Shutdown::Write).unwrap();
        }
        let mut out = Vec::new();
        let _ = client.read_to_end(&mut out);
        server.join().unwrap();
        out
    }

    fn status_line(out: &[u8]) -> String {
        let s = String::from_utf8_lossy(out);
        s.split("\r\n").next().unwrap_or("").to_string()
    }

    #[test]
    fn handle_connection_serves_health_and_closes() {
        let root = temp_dir();
        let out = roundtrip(ctx(&root, 1024), b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n", false);
        let text = String::from_utf8_lossy(&out);
        assert_eq!(status_line(&out), "HTTP/1.1 200 OK");
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with(r#"{"status":"ok"}"#));
    }

    #[test]
    fn handle_connection_maps_parse_errors() {
        let root = temp_dir();
        let out = roundtrip(ctx(&root, 1024), b"GET / HTTP/1.1\n\n", false);
        assert_eq!(status_line(&out), "HTTP/1.1 400 Bad Request");

        // Body is never read; close_gracefully drains it so the client sees the 413.
        let mut big = b"POST /api/echo HTTP/1.1\r\nContent-Length: 11\r\n\r\n".to_vec();
        big.extend_from_slice(b"hello world");
        let out = roundtrip(ctx(&root, 10), &big, false);
        assert_eq!(status_line(&out), "HTTP/1.1 413 Payload Too Large");
    }

    #[test]
    fn handle_connection_incomplete_or_empty_sends_nothing() {
        let root = temp_dir();
        let out = roundtrip(
            ctx(&root, 1024),
            b"POST /api/echo HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc",
            true,
        );
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
        let out = roundtrip(ctx(&root, 1024), b"", true);
        assert!(out.is_empty());
    }

    #[test]
    fn handle_connection_times_out_without_response() {
        let root = temp_dir();
        let start = Instant::now();
        // Partial head, no half-close: the 2 s read timeout must fire.
        let out = roundtrip(ctx(&root, 1024), b"GET / HT", false);
        assert!(out.is_empty());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn close_gracefully_respects_budget_and_zero_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        // Peer never closes: drain must stop at the budget.
        let start = Instant::now();
        close_gracefully(server_side, Duration::from_millis(100));
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(90) && took < Duration::from_secs(2), "{took:?}");
        drop(client);

        let client = TcpStream::connect(addr).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        close_gracefully(server_side, Duration::ZERO);
        drop(client);
    }
}
