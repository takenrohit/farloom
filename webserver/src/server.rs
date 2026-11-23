//! Server and acceptor: shared types for the acceptor thread and the worker pool.
//!
//! TODO(task 9.6): implement `Server` (`bind`, `local_addr`, `shutdown_handle`, `run`)
//! and the acceptor loop with the 503 rejection path.

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::http::parser::Limits;
use crate::router::Router;
use crate::static_files::StaticFiles;

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

#[cfg(test)]
mod tests {
    use super::*;

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
