//! Line-oriented `key=value` logger.
//!
//! Every function builds the complete line, including the trailing `\n`, in one
//! `String` and writes it with a single `write_all` on a locked `stdout()` or
//! `stderr()`. Holding the lock for that one write keeps lines from different
//! workers from interleaving. Write errors are ignored so logging can never
//! crash a worker.

use std::io::Write;
use std::net::SocketAddr;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// One access-log entry, written after a worker finishes a request.
#[derive(Debug, Clone)]
pub struct AccessRecord<'a> {
    pub ts_ms: u128,
    pub worker: &'a str,
    pub client: SocketAddr,
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub dur_ms: u128,
}

/// Milliseconds since the Unix epoch (0 if the clock is before the epoch).
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Name of the current thread: `worker-{id}` for pool threads, `acceptor` otherwise.
fn current_worker() -> String {
    thread::current().name().unwrap_or("acceptor").to_string()
}

/// Formats an access line. Pure; the result ends with `\n`.
pub fn format_access(r: &AccessRecord) -> String {
    format!(
        "ts={} level=INFO event=access worker={} client={} method={} path={} status={} dur_ms={}\n",
        r.ts_ms, r.worker, r.client, r.method, r.path, r.status, r.dur_ms
    )
}

/// Formats an event line with the current timestamp. `fields` is appended as-is
/// (already `key=value` pairs); an empty `fields` adds nothing.
pub fn format_event(level: &str, event: &str, fields: &str) -> String {
    let mut line = format!("ts={} level={} event={}", now_ms(), level, event);
    if !fields.is_empty() {
        line.push(' ');
        line.push_str(fields);
    }
    line.push('\n');
    line
}

/// Escapes a value so it stays on one line inside double quotes.
fn quote(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len() + 2);
    out.push('"');
    for c in msg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_stdout(line: &str) {
    let _ = std::io::stdout().lock().write_all(line.as_bytes());
}

fn write_stderr(line: &str) {
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// Writes an access line to stdout.
pub fn access(r: &AccessRecord) {
    write_stdout(&format_access(r));
}

/// Writes an INFO event line to stdout.
pub fn info_event(event: &str, fields: &str) {
    write_stdout(&format_event("INFO", event, fields));
}

/// Writes a WARN event line to stderr, tagged with the current worker name.
pub fn warn(event: &str, fields: &str) {
    let mut all = format!("worker={}", current_worker());
    if !fields.is_empty() {
        all.push(' ');
        all.push_str(fields);
    }
    write_stderr(&format_event("WARN", event, &all));
}

/// Writes an ERROR event line to stderr, tagged with the current worker name.
pub fn error(event: &str, msg: &str) {
    let fields = format!("worker={} msg={}", current_worker(), quote(msg));
    write_stderr(&format_event("ERROR", event, &fields));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_line_has_all_fields_and_one_newline() {
        let r = AccessRecord {
            ts_ms: 1718000000456,
            worker: "worker-2",
            client: "127.0.0.1:54012".parse().unwrap(),
            method: "GET",
            path: "/index.html",
            status: 200,
            dur_ms: 3,
        };
        let line = format_access(&r);
        assert_eq!(
            line,
            "ts=1718000000456 level=INFO event=access worker=worker-2 client=127.0.0.1:54012 method=GET path=/index.html status=200 dur_ms=3\n"
        );
    }

    #[test]
    fn event_line_format() {
        let line = format_event("INFO", "worker_exit", "worker=worker-1");
        assert!(line.starts_with("ts="));
        assert!(line.ends_with(" level=INFO event=worker_exit worker=worker-1\n"));
        let bare = format_event("INFO", "x", "");
        assert!(bare.ends_with(" event=x\n"));
    }

    #[test]
    fn quote_keeps_message_on_one_line() {
        assert_eq!(quote("a\r\nb\"c\\"), "\"a\\r\\nb\\\"c\\\\\"");
    }

    #[test]
    fn worker_name_defaults_to_acceptor_for_unnamed_threads() {
        let name = thread::spawn(current_worker).join().unwrap();
        assert_eq!(name, "acceptor");
        let named = thread::Builder::new()
            .name("worker-7".into())
            .spawn(current_worker)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(named, "worker-7");
    }
}
