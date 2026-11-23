//! Shared helpers for integration tests: temporary static roots, a server running
//! on an ephemeral port, and a raw-socket HTTP client.
#![allow(dead_code)]

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use webserver::config::Config;
use webserver::router::Router;
use webserver::server::{Server, ShutdownHandle};

/// Socket timeout for the test client, so a broken server cannot hang a test.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// A unique directory under `%TEMP%`, removed (with its contents) on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "webserver-it-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            nanos
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `contents` to `rel` (forward-slash relative path), creating parent directories.
    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let mut p = self.path.clone();
        for seg in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(seg);
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&p, contents).expect("write temp file");
        p
    }

    /// Creates a subdirectory (and its parents).
    pub fn mkdir(&self, rel: &str) -> PathBuf {
        let mut p = self.path.clone();
        for seg in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(seg);
        }
        fs::create_dir_all(&p).expect("create dir");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A `Server` running `run()` on a background thread. `Drop` shuts it down and joins.
pub struct TestServer {
    addr: SocketAddr,
    shutdown: ShutdownHandle,
    handle: Option<JoinHandle<io::Result<()>>>,
    done: mpsc::Receiver<()>,
    /// Kept alive so the static root exists for the server's lifetime.
    root: TempDir,
}

impl TestServer {
    /// Starts a server with an empty temp static root.
    pub fn start(overrides: impl FnOnce(&mut Config), router: Router) -> TestServer {
        Self::start_with_root(TempDir::new(), overrides, router)
    }

    /// Starts a server serving `root`. `overrides` runs after `port: 0` and the root
    /// are set, so it may change any field.
    pub fn start_with_root(
        root: TempDir,
        overrides: impl FnOnce(&mut Config),
        router: Router,
    ) -> TestServer {
        let mut config = Config {
            port: 0,
            static_root: root.path().to_path_buf(),
            ..Config::default()
        };
        overrides(&mut config);
        let server = Server::bind(config, router).expect("Server::bind");
        let addr = server.local_addr();
        let shutdown = server.shutdown_handle();
        let (done_tx, done) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("test-server".to_string())
            .spawn(move || {
                let r = server.run();
                let _ = done_tx.send(());
                r
            })
            .expect("spawn server thread");
        TestServer {
            addr,
            shutdown,
            handle: Some(handle),
            done,
            root,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    pub fn root(&self) -> &TempDir {
        &self.root
    }

    /// Waits up to `timeout` for `run` to return on its own (e.g. `max_requests`).
    /// Returns true if it returned in time; the thread is then joined and
    /// `run`'s result asserted `Ok`.
    pub fn join_timeout(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.handle.as_ref() else {
            return true;
        };
        let finished = handle.is_finished() || self.done.recv_timeout(timeout).is_ok();
        if finished {
            let handle = self.handle.take().unwrap();
            handle
                .join()
                .expect("server thread panicked")
                .expect("run returned an error");
        }
        finished
    }

    /// Triggers shutdown and waits up to `timeout` for `run` to return.
    pub fn stop_and_join(&mut self, timeout: Duration) -> bool {
        self.shutdown.shutdown();
        self.join_timeout(timeout)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(handle) = self.handle.take() {
            // Don't double-panic while a failing test is already unwinding.
            let _ = handle.join();
        }
    }
}

/// A parsed HTTP response read from a raw socket.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Every byte received, unparsed.
    pub raw: Vec<u8>,
}

impl RawResponse {
    /// First header value whose name matches case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Number of headers whose name matches case-insensitively.
    pub fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .count()
    }

    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Connects with 10 s timeouts on connect, read, and write.
pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, CLIENT_TIMEOUT)?;
    s.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    s.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    Ok(s)
}

/// Reads until EOF. A reset or abort after at least one byte is treated as EOF,
/// because the server may reset after it has written the full response.
pub fn read_to_eof(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if !out.is_empty()
                    && matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) =>
            {
                return Ok(out)
            }
            Err(e) => return Err(e),
        }
    }
}

/// Writes `bytes` exactly as given and returns everything the server sends back.
pub fn raw_exchange(addr: SocketAddr, bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut s = connect(addr)?;
    s.write_all(bytes)?;
    s.flush()?;
    read_to_eof(&mut s)
}

/// Writes `bytes` exactly, reads to EOF, and parses the response. Panics on
/// I/O errors or a malformed response, which fails the calling test.
pub fn raw_request(addr: SocketAddr, bytes: &[u8]) -> RawResponse {
    let raw = raw_exchange(addr, bytes).expect("raw request I/O");
    parse_raw_response(raw).unwrap_or_else(|e| panic!("bad response: {e}"))
}

/// Connects, sends nothing, and reads whatever the server sends (e.g. a 503) until EOF.
pub fn read_without_request(addr: SocketAddr) -> io::Result<Vec<u8>> {
    let mut s = connect(addr)?;
    read_to_eof(&mut s)
}

/// Sends `GET {path} HTTP/1.1` with a Host header.
pub fn get(addr: SocketAddr, path: &str) -> RawResponse {
    raw_request(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes(),
    )
}

/// Parses a full HTTP/1.x response. Uses `Content-Length` to bound the body when
/// present and checks that exactly that many bytes followed the head.
pub fn parse_raw_response(raw: Vec<u8>) -> Result<RawResponse, String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("no end of head in {:?}", String::from_utf8_lossy(&raw)))?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|e| format!("head not UTF-8: {e}"))?;
    let mut lines = head.split("\r\n");

    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(format!("bad status line: {status_line:?}"));
    }
    let status: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad status code: {status_line:?}"))?;
    let reason = parts.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("bad header line: {line:?}"))?;
        headers.push((name.to_string(), value.trim().to_string()));
    }

    let rest = &raw[head_end + 4..];
    let body = match headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
    {
        Some((_, v)) => {
            let n: usize = v.parse().map_err(|_| format!("bad Content-Length {v:?}"))?;
            if rest.len() != n {
                return Err(format!(
                    "Content-Length {n} but {} body bytes received",
                    rest.len()
                ));
            }
            rest.to_vec()
        }
        None => rest.to_vec(),
    };

    Ok(RawResponse {
        status,
        reason,
        headers,
        body,
        raw,
    })
}

/// Half-closes the write side, for tests that need the server to see EOF.
pub fn shutdown_write(stream: &TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
}
