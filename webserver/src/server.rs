//! Server and acceptor: binds the listener, runs the acceptor loop on the calling
//! thread, hands connections to the worker pool, and answers 503 when the queue is full.

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::connection::{close_gracefully, handle_connection};
use crate::http::parser::Limits;
use crate::http::response::{write_response, Response, StatusCode};
use crate::log;
use crate::pool::{SubmitError, ThreadPool};
use crate::router::Router;
use crate::static_files::StaticFiles;

/// How long the acceptor sleeps when no connection is pending (bounds shutdown latency).
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Time the acceptor spends draining a rejected socket after writing the 503.
const REJECT_CLOSE_BUDGET: Duration = Duration::from_millis(50);

/// One accepted client connection, moved from the acceptor through the queue to one worker.
pub struct Connection {
    pub stream: TcpStream,
    pub peer: SocketAddr,
}

/// Read-only application state shared by all workers behind an `Arc`.
pub struct AppContext {
    pub router: Router,
    pub static_files: StaticFiles,
    pub limits: Limits,
}

/// Cloneable flag that asks the acceptor loop to stop. Callable from any thread.
#[derive(Clone, Debug, Default)]
pub struct ShutdownHandle(Arc<AtomicBool>);

impl ShutdownHandle {
    pub fn new() -> ShutdownHandle {
        ShutdownHandle(Arc::new(AtomicBool::new(false)))
    }

    pub fn shutdown(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_shutdown(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Errors that stop the server from starting (exit code 1).
#[derive(Debug)]
pub enum StartupError {
    StaticRoot { path: PathBuf, source: io::Error },
    Bind { addr: String, source: io::Error },
    Spawn(io::Error),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::StaticRoot { path, source } => {
                write!(f, "invalid static root '{}': {source}", path.display())
            }
            StartupError::Bind { addr, source } => {
                write!(f, "failed to bind to {addr}: {source}")
            }
            StartupError::Spawn(source) => write!(f, "failed to spawn worker thread: {source}"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartupError::StaticRoot { source, .. }
            | StartupError::Bind { source, .. }
            | StartupError::Spawn(source) => Some(source),
        }
    }
}

/// A bound server: listener, worker pool, and shutdown flag.
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    pool: ThreadPool<Connection>,
    shutdown: ShutdownHandle,
    config: Config,
}

impl Server {
    /// Validates the static root, binds the listener (non-blocking), spawns the
    /// worker pool, and writes the startup log line.
    pub fn bind(config: Config, router: Router) -> Result<Server, StartupError> {
        let root = config
            .validate_static_root()
            .map_err(|source| StartupError::StaticRoot {
                path: config.static_root.clone(),
                source,
            })?;

        let bind_addr = format!("{}:{}", config.host, config.port);
        let bind_err = |source| StartupError::Bind {
            addr: bind_addr.clone(),
            source,
        };
        let listener =
            TcpListener::bind((config.host.as_str(), config.port)).map_err(bind_err)?;
        let local_addr = listener.local_addr().map_err(bind_err)?;
        listener.set_nonblocking(true).map_err(bind_err)?;

        let ctx = Arc::new(AppContext {
            router,
            static_files: StaticFiles::new(root.clone()),
            limits: Limits::new(config.max_body_bytes),
        });
        let pool = ThreadPool::new(config.workers, config.queue_capacity, move |conn| {
            handle_connection(&ctx, conn)
        })
        .map_err(StartupError::Spawn)?;

        log::startup(
            local_addr,
            config.workers,
            config.queue_capacity,
            &display_path(&root),
        );

        Ok(Server {
            listener,
            local_addr,
            pool,
            shutdown: ShutdownHandle::new(),
            config,
        })
    }

    /// The address actually bound (includes the OS-assigned port when `port` was 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Runs the acceptor loop on the calling thread until shutdown is requested,
    /// then closes the listener and drops the pool, which drains the queue and
    /// joins every worker before this returns.
    pub fn run(self) -> io::Result<()> {
        let Server {
            listener,
            pool,
            shutdown,
            config,
            ..
        } = self;

        let mut accepted: u64 = 0;
        while !shutdown.is_shutdown() {
            match listener.accept() {
                Ok((stream, peer)) => {
                    accepted += 1;
                    if config.max_requests == Some(accepted) {
                        shutdown.shutdown();
                    }
                    if let Err(e) = prepare(&stream, &config) {
                        log::error("socket_setup", &format!("client={peer} {e}"));
                        continue;
                    }
                    match pool.try_submit(Connection { stream, peer }) {
                        Ok(()) => {}
                        Err(SubmitError::Full(conn)) | Err(SubmitError::Closed(conn)) => {
                            reject_503(conn)
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(ACCEPT_POLL_INTERVAL)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log::error("accept", &e.to_string());
                    thread::sleep(ACCEPT_POLL_INTERVAL);
                }
            }
        }

        // Stop accepting first, then close the queue and join the workers.
        drop(listener);
        drop(pool);
        Ok(())
    }
}

/// Puts an accepted socket into blocking mode (Windows inherits non-blocking from
/// the listener) and applies the configured read and write timeouts.
fn prepare(stream: &TcpStream, config: &Config) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(config.read_timeout))?;
    stream.set_write_timeout(Some(config.write_timeout))?;
    Ok(())
}

/// Writes `503 Service Unavailable` with `Retry-After: 1`, closes the socket, and logs it.
fn reject_503(conn: Connection) {
    let Connection { stream, peer } = conn;
    let resp = Response::error(StatusCode::ServiceUnavailable).with_header("Retry-After", "1");
    if let Err(e) = write_response(&mut &stream, &resp) {
        log::error("write", &format!("client={peer} {e}"));
    }
    close_gracefully(stream, REJECT_CLOSE_BUDGET);
    log::rejected(peer);
}

/// Strips the Windows verbatim prefix (`\\?\C:\x` → `C:\x`, `\\?\UNC\h\s` → `\\h\s`)
/// so the startup log shows a familiar path. Display only.
fn display_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "webserver-server-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn test_config(root: &TempDir) -> Config {
        Config {
            port: 0,
            workers: 2,
            queue_capacity: 4,
            static_root: root.0.clone(),
            read_timeout: Duration::from_secs(2),
            write_timeout: Duration::from_secs(2),
            ..Config::default()
        }
    }

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn bind_run_health_and_shutdown() {
        let root = temp_dir();
        let server = Server::bind(test_config(&root), Router::with_builtins()).unwrap();
        let addr = server.local_addr();
        assert_ne!(addr.port(), 0);
        let handle = server.shutdown_handle();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let t = thread::spawn(move || {
            let r = server.run();
            let _ = done_tx.send(());
            r
        });

        let resp = get(addr, "/health");
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
        assert!(resp.contains("Connection: close\r\n"));
        assert!(resp.ends_with(r#"{"status":"ok"}"#));

        let start = Instant::now();
        handle.shutdown();
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("run returns within 1 s of shutdown");
        assert!(start.elapsed() < Duration::from_secs(1));
        t.join().unwrap().unwrap();
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err());
    }

    #[test]
    fn max_requests_stops_run() {
        let root = temp_dir();
        let config = Config {
            max_requests: Some(2),
            ..test_config(&root)
        };
        let server = Server::bind(config, Router::with_builtins()).unwrap();
        let addr = server.local_addr();
        let t = thread::spawn(move || server.run());
        assert!(get(addr, "/health").starts_with("HTTP/1.1 200 OK"));
        assert!(get(addr, "/health").starts_with("HTTP/1.1 200 OK"));
        t.join().unwrap().unwrap();
    }

    #[test]
    fn bind_errors_for_missing_root_and_port_conflict() {
        let root = temp_dir();
        let missing = Config {
            static_root: root.0.join("nope"),
            ..test_config(&root)
        };
        match Server::bind(missing, Router::new()) {
            Err(StartupError::StaticRoot { path, .. }) => assert!(path.ends_with("nope")),
            other => panic!("expected StaticRoot error, got {:?}", other.err()),
        }

        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let conflict = Config {
            port: taken.local_addr().unwrap().port(),
            ..test_config(&root)
        };
        assert!(matches!(
            Server::bind(conflict, Router::new()),
            Err(StartupError::Bind { .. })
        ));
    }

    #[test]
    fn display_path_strips_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\srv\public")), PathBuf::from(r"C:\srv\public"));
        assert_eq!(display_path(Path::new(r"\\?\UNC\host\share")), PathBuf::from(r"\\host\share"));
        assert_eq!(display_path(Path::new("/srv/public")), PathBuf::from("/srv/public"));
    }

    #[test]
    fn shutdown_handle_is_shared_across_clones() {
        let a = ShutdownHandle::new();
        let b = a.clone();
        assert!(!a.is_shutdown());
        b.shutdown();
        assert!(a.is_shutdown());
        assert!(!ShutdownHandle::default().is_shutdown());
    }

    #[test]
    fn shutdown_visible_from_other_thread() {
        let h = ShutdownHandle::new();
        let h2 = h.clone();
        std::thread::spawn(move || h2.shutdown()).join().unwrap();
        assert!(h.is_shutdown());
    }

    #[test]
    fn startup_error_display_names_path_and_addr() {
        let e = StartupError::StaticRoot {
            path: PathBuf::from("missing_dir"),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        };
        assert!(e.to_string().contains("missing_dir"));
        let e = StartupError::Bind {
            addr: "127.0.0.1:7878".to_string(),
            source: io::Error::new(io::ErrorKind::AddrInUse, "in use"),
        };
        assert!(e.to_string().contains("127.0.0.1:7878"));
        assert!(std::error::Error::source(&e).is_some());
    }
}
