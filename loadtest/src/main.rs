//! Load tester: sends concurrent HTTP GET requests and reports results.
//! Uses only the Rust standard library.

mod args;
mod stats;

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use args::{ArgsError, LoadArgs, USAGE};
use stats::{parse_status_line, Stats};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let args = match LoadArgs::from_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(ArgsError::HelpRequested) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    // Resolve once up front: a bad address fails fast, and no DNS lookup runs per request.
    let targets = match resolve(&args.addr) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot resolve --addr '{}': {e}", args.addr);
            return ExitCode::from(2);
        }
    };

    let start = Instant::now();
    let stats = run_load(&args, &targets);
    let elapsed = start.elapsed();
    print!("{}", format_report(&stats, elapsed));
    if stats.total() == stats.errors {
        eprintln!("error: no responses received; is the server running at {}?", args.addr);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Resolves `addr` (`host:port`) to one or more socket addresses.
fn resolve(addr: &str) -> std::io::Result<Vec<SocketAddr>> {
    let targets: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
    if targets.is_empty() {
        return Err(std::io::Error::new(ErrorKind::NotFound, "no addresses found"));
    }
    Ok(targets)
}

/// Runs `args.requests` requests across `args.concurrency` threads and
/// returns the merged statistics.
fn run_load(args: &LoadArgs, targets: &[SocketAddr]) -> Stats {
    let counter = Arc::new(AtomicU64::new(0));
    let request = Arc::new(build_request(&args.addr, &args.path));
    let targets: Arc<[SocketAddr]> = Arc::from(targets);
    let mut handles = Vec::with_capacity(args.concurrency);

    for i in 0..args.concurrency {
        let counter = Arc::clone(&counter);
        let request = Arc::clone(&request);
        let targets = Arc::clone(&targets);
        let total = args.requests;
        let spawned = thread::Builder::new()
            .name(format!("client-{i}"))
            .spawn(move || {
                let mut local = Stats::default();
                while counter.fetch_add(1, Ordering::Relaxed) < total {
                    match send_one(&targets, &request) {
                        Some(code) => local.record_status(code),
                        None => local.record_error(),
                    }
                }
                local
            });
        match spawned {
            Ok(h) => handles.push(h),
            Err(e) => {
                // The threads already running take the remaining tickets.
                eprintln!("error: could not spawn client thread {i}: {e}; continuing with {i}");
                break;
            }
        }
    }

    let mut merged = Stats::default();
    for handle in handles {
        match handle.join() {
            Ok(s) => merged.merge(s),
            Err(_) => eprintln!("error: client thread panicked"),
        }
    }
    merged
}

fn build_request(addr: &str, path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").into_bytes()
}

/// Sends one request on a new connection. Returns the status code, or `None`
/// on a connection error or unparsable response.
fn send_one(targets: &[SocketAddr], request: &[u8]) -> Option<u16> {
    let mut stream = TcpStream::connect(targets).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.write_all(request).ok()?;

    let mut buf = Vec::new();
    if read_to_end_keeping(&mut stream, &mut buf).is_err() && buf.is_empty() {
        // A server that rejects with 503 may reset the connection after the
        // response, so an error after some bytes still counts as a response.
        return None;
    }
    parse_status_line(&buf)
}

/// Like `read_to_end`, but keeps the bytes read so far when an error occurs.
fn read_to_end_keeping(stream: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn format_report(stats: &Stats, elapsed: Duration) -> String {
    let total = stats.total();
    let secs = elapsed.as_secs_f64();
    let rps = if secs > 0.0 { total as f64 / secs } else { 0.0 };

    let mut out = String::new();
    out.push_str(&line("requests:", &total.to_string()));
    for (code, count) in &stats.by_status {
        out.push_str(&line(&format!("status {code}:"), &count.to_string()));
    }
    out.push_str(&line("errors:", &stats.errors.to_string()));
    out.push_str(&line("elapsed:", &format!("{secs:.3} s")));
    out.push_str(&line("throughput:", &format!("{rps:.1} req/s")));
    out
}

fn line(label: &str, value: &str) -> String {
    format!("{label:<13}{value}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn report_matches_design_format() {
        let mut s = Stats::default();
        for _ in 0..3 {
            s.record_status(200);
        }
        s.record_status(503);
        let report = format_report(&s, Duration::from_millis(2000));
        assert_eq!(
            report,
            "requests:    4\n\
             status 200:  3\n\
             status 503:  1\n\
             errors:      0\n\
             elapsed:     2.000 s\n\
             throughput:  2.0 req/s\n"
        );
    }

    #[test]
    fn run_load_counts_every_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests = 20;
        let server = thread::spawn(move || {
            for _ in 0..requests {
                let (mut s, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if s.read(&mut b).unwrap() == 0 {
                        break;
                    }
                    head.push(b[0]);
                }
                assert!(head.starts_with(b"GET /x HTTP/1.1\r\n"));
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        });
        let args = LoadArgs {
            addr: addr.clone(),
            path: "/x".to_string(),
            requests,
            concurrency: 4,
        };
        let stats = run_load(&args, &resolve(&addr).unwrap());
        server.join().unwrap();
        assert_eq!(stats.total(), requests);
        assert_eq!(stats.by_status.get(&200), Some(&requests));
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn unreachable_target_counts_errors() {
        // Bind then drop to get a port that is (almost certainly) closed.
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
        let args = LoadArgs { addr: addr.clone(), path: "/".into(), requests: 2, concurrency: 2 };
        let stats = run_load(&args, &resolve(&addr).unwrap());
        assert_eq!(stats.total(), 2);
        assert_eq!(stats.errors, 2);
    }

    #[test]
    fn resolve_rejects_bad_addresses() {
        assert!(resolve("127.0.0.1:7878").is_ok());
        assert!(resolve("no-port").is_err());
        assert!(resolve("127.0.0.1:notaport").is_err());
    }
}
