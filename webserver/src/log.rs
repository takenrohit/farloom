//! Line-oriented `key=value` logger.
//!
//! Every function builds the complete line, including the trailing `\n`, in one
//! `String` and writes it with a single `write_all` on a locked `stdout()` or
//! `stderr()`. Holding the lock for that one write keeps lines from different
//! workers from interleaving. Write errors are ignored so logging can never
//! crash a worker.
//!
//! Line formats (all start with `ts=<unix ms> level=<LEVEL> event=<event>`):
//!
//! | Event          | Stream | Fields after `event=`                                       |
//! |----------------|--------|-------------------------------------------------------------|
//! | startup        | stdout | `addr= workers= queue_capacity= static_root=`               |
//! | access         | stdout | `worker= client= method= path= status= dur_ms=`             |
//! | rejected       | stdout | `client= status=503`                                        |
//! | worker_exit    | stdout | `worker=`                                                   |
//! | path_traversal | stderr | `worker= client= path= rule=` (level WARN)                  |
//! | errors         | stderr | `worker= msg="..."` (level ERROR)                           |
//!
//! Values that may come from a client or from a panic (paths, methods, messages)
//! are passed through [`escape_value`] so they cannot contain CR or LF and
//! therefore cannot start a fake log line.

use std::cell::Cell;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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

thread_local! {
    /// Per-thread label that overrides the thread name in log lines.
    static LABEL: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Sets (or clears with `None`) the label used for the `worker=` field on this
/// thread. `Server::run` sets `acceptor`, because it runs on the caller's
/// thread, which is named `main` in the binary.
pub fn set_thread_label(label: Option<&'static str>) {
    LABEL.with(|l| l.set(label));
}

/// Label of the current thread: the value from [`set_thread_label`], else the
/// thread name (`worker-{id}` for pool threads), else `acceptor`.
pub fn current_worker() -> String {
    if let Some(label) = LABEL.with(Cell::get) {
        return label.to_string();
    }
    thread::current().name().unwrap_or("acceptor").to_string()
}

/// Escapes `\`, `"`, CR, LF, and every other control or line-separator
/// character so the value stays on one line and cannot inject terminal escape
/// sequences. Other controls become `\u{XX}`.
///
/// The result is not wrapped in quotes; use it for unquoted fields such as
/// `path=`. Callers outside this module (for example the static file handler)
/// use it when building `fields` for [`warn`] or [`info_event`].
pub fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32))
            }
            _ => out.push(c),
        }
    }
    out
}

/// Strips the Windows verbatim prefix (`\\?\C:\x` → `C:\x`, `\\?\UNC\h\s` → `\\h\s`)
/// that `fs::canonicalize` adds, so log lines show familiar paths. Display only.
pub fn display_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Escapes a value and wraps it in double quotes (used for `msg=`).
fn quote(msg: &str) -> String {
    format!("\"{}\"", escape_value(msg))
}

/// Formats an access line. Pure; the result ends with `\n`.
pub fn format_access(r: &AccessRecord) -> String {
    format!(
        "ts={} level=INFO event=access worker={} client={} method={} path={} status={} dur_ms={}\n",
        r.ts_ms,
        escape_value(r.worker),
        r.client,
        escape_value(r.method),
        escape_value(r.path),
        r.status,
        r.dur_ms
    )
}

/// Formats an event line with the current timestamp. `fields` is appended as-is
/// (already `key=value` pairs, with client-controlled values escaped by the
/// caller); an empty `fields` adds nothing. CR and LF in `fields` are still
/// escaped as a backstop so the result is always exactly one line.
pub fn format_event(level: &str, event: &str, fields: &str) -> String {
    let mut line = format!("ts={} level={} event={}", now_ms(), level, event);
    if !fields.is_empty() {
        line.push(' ');
        if fields.contains(['\r', '\n']) {
            line.push_str(&fields.replace('\r', "\\r").replace('\n', "\\n"));
        } else {
            line.push_str(fields);
        }
    }
    line.push('\n');
    line
}

/// Formats the startup line. Pure; the result ends with `\n`.
pub fn format_startup(
    addr: SocketAddr,
    workers: usize,
    queue_capacity: usize,
    static_root: &Path,
) -> String {
    // The root comes from the operator, not a client, and is printed verbatim so
    // Windows paths stay readable (`C:\srv\public`, as in the design example).
    // `format_event` still escapes any CR/LF.
    format_event(
        "INFO",
        "startup",
        &format!(
            "addr={addr} workers={workers} queue_capacity={queue_capacity} static_root={}",
            static_root.display()
        ),
    )
}

/// Formats a 503 rejection line. Pure; the result ends with `\n`.
pub fn format_rejected(client: SocketAddr) -> String {
    format_event("INFO", "rejected", &format!("client={client} status=503"))
}

/// Formats a path-traversal warning line. Pure; the result ends with `\n`.
pub fn format_path_traversal(worker: &str, client: SocketAddr, path: &str, rule: &str) -> String {
    format_event(
        "WARN",
        "path_traversal",
        &format!(
            "worker={} client={client} path={} rule={}",
            escape_value(worker),
            escape_value(path),
            escape_value(rule)
        ),
    )
}

/// Formats an error line. Pure; the result ends with `\n`.
pub fn format_error(worker: &str, event: &str, msg: &str) -> String {
    format_event(
        "ERROR",
        event,
        &format!("worker={} msg={}", escape_value(worker), quote(msg)),
    )
}

fn write_stdout(line: &str) {
    let _ = std::io::stdout().lock().write_all(line.as_bytes());
}

fn write_stderr(line: &str) {
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// Writes the startup line to stdout.
pub fn startup(addr: SocketAddr, workers: usize, queue_capacity: usize, static_root: &Path) {
    write_stdout(&format_startup(addr, workers, queue_capacity, static_root));
}

/// Writes an access line to stdout.
pub fn access(r: &AccessRecord) {
    write_stdout(&format_access(r));
}

/// Writes a 503 rejection line to stdout.
pub fn rejected(client: SocketAddr) {
    write_stdout(&format_rejected(client));
}

/// Writes a path-traversal WARN line to stderr, tagged with the current worker name.
pub fn path_traversal(client: SocketAddr, path: &str, rule: &str) {
    write_stderr(&format_path_traversal(&current_worker(), client, path, rule));
}

/// Writes an INFO event line to stdout.
pub fn info_event(event: &str, fields: &str) {
    write_stdout(&format_event("INFO", event, fields));
}

/// Writes a WARN event line to stderr, tagged with the current worker name.
pub fn warn(event: &str, fields: &str) {
    let mut all = format!("worker={}", escape_value(&current_worker()));
    if !fields.is_empty() {
        all.push(' ');
        all.push_str(fields);
    }
    write_stderr(&format_event("WARN", event, &all));
}

/// Writes an ERROR event line to stderr, tagged with the current worker name.
pub fn error(event: &str, msg: &str) {
    write_stderr(&format_error(&current_worker(), event, msg));
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
    fn access_line_escapes_path() {
        let r = AccessRecord {
            ts_ms: 1,
            worker: "worker-0",
            client: "127.0.0.1:1".parse().unwrap(),
            method: "GET",
            path: "/a\r\nts=0 level=INFO",
            status: 404,
            dur_ms: 0,
        };
        let line = format_access(&r);
        assert!(line.contains("path=/a\\r\\nts=0 level=INFO "));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.contains('\r'));
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
    fn startup_and_traversal_formats() {
        let addr: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let s = format_startup(addr, 4, 64, Path::new("C:\\srv\\public"));
        assert!(s.starts_with("ts="));
        assert!(s.ends_with(
            " level=INFO event=startup addr=127.0.0.1:7878 workers=4 queue_capacity=64 static_root=C:\\srv\\public\n"
        ));
        let t = format_path_traversal("worker-0", addr, "/..%2fCargo.toml", "dotdot");
        assert!(t.ends_with(
            " level=WARN event=path_traversal worker=worker-0 client=127.0.0.1:7878 path=/..%2fCargo.toml rule=dotdot\n"
        ));
        let r = format_rejected(addr);
        assert!(r.ends_with(" level=INFO event=rejected client=127.0.0.1:7878 status=503\n"));
    }

    #[test]
    fn error_line_quotes_and_escapes_msg() {
        let e = format_error("worker-3", "handler_panic", "boom\r\n\"x\"\\");
        assert!(e.ends_with(
            " level=ERROR event=handler_panic worker=worker-3 msg=\"boom\\r\\n\\\"x\\\"\\\\\"\n"
        ));
        assert_eq!(e.matches('\n').count(), 1);
    }

    /// Asserts `line` is `ts=<digits> <rest>` and returns `<rest>`.
    fn strip_ts(line: &str) -> &str {
        let after = line.strip_prefix("ts=").expect("line starts with ts=");
        let (ts, rest) = after.split_once(' ').expect("space after ts");
        assert!(!ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit()), "ts={ts}");
        rest
    }

    #[test]
    fn rejected_and_worker_exit_match_design_exactly() {
        let client: SocketAddr = "127.0.0.1:54013".parse().unwrap();
        assert_eq!(
            strip_ts(&format_rejected(client)),
            "level=INFO event=rejected client=127.0.0.1:54013 status=503\n"
        );
        assert_eq!(
            strip_ts(&format_event("INFO", "worker_exit", "worker=worker-1")),
            "level=INFO event=worker_exit worker=worker-1\n"
        );
    }

    #[test]
    fn startup_line_matches_design_exactly() {
        let addr: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let s = format_startup(addr, 4, 64, Path::new("C:\\Users\\rohit\\ideas\\public"));
        assert_eq!(
            strip_ts(&s),
            "level=INFO event=startup addr=127.0.0.1:7878 workers=4 queue_capacity=64 static_root=C:\\Users\\rohit\\ideas\\public\n"
        );
    }

    #[test]
    fn crlf_never_breaks_a_line() {
        let addr: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let lines = [
            format_error("worker-0", "io", "first\r\nts=0 level=INFO event=fake"),
            format_event("INFO", "x", "a=1\r\nts=0 level=INFO event=fake"),
            format_startup(addr, 1, 1, Path::new("C:\\a\r\nb")),
            format_path_traversal("worker-0", addr, "/x\r\ny", "dotdot"),
        ];
        for line in &lines {
            assert_eq!(line.matches('\n').count(), 1, "{line:?}");
            assert!(line.ends_with('\n'), "{line:?}");
            assert!(!line.contains('\r'), "{line:?}");
            assert!(line.contains("\\r\\n"), "{line:?}");
        }
        assert!(lines[0].ends_with(" msg=\"first\\r\\nts=0 level=INFO event=fake\"\n"));
    }

    #[test]
    fn escape_value_cases() {
        assert_eq!(escape_value(""), "");
        assert_eq!(escape_value("/plain/path.html"), "/plain/path.html");
        assert_eq!(escape_value("a\\b"), "a\\\\b");
        assert_eq!(escape_value("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(escape_value("\r"), "\\r");
        assert_eq!(escape_value("\n"), "\\n");
        assert_eq!(escape_value("ü\r\n✓"), "ü\\r\\n✓");
        assert_eq!(escape_value("a\tb"), "a\\tb");
        assert_eq!(escape_value("\u{1b}[31m"), "\\u{1b}[31m");
        assert_eq!(escape_value("\0\u{7f}\u{85}\u{9b}"), "\\u{0}\\u{7f}\\u{85}\\u{9b}");
        assert_eq!(escape_value("x\u{2028}y\u{2029}"), "x\\u{2028}y\\u{2029}");
    }

    #[test]
    fn display_path_strips_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\srv\public")), PathBuf::from(r"C:\srv\public"));
        assert_eq!(display_path(Path::new(r"\\?\UNC\host\share")), PathBuf::from(r"\\host\share"));
        assert_eq!(display_path(Path::new("/srv/public")), PathBuf::from("/srv/public"));
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

    #[test]
    fn thread_label_overrides_thread_name_and_can_be_cleared() {
        let (labelled, cleared) = thread::Builder::new()
            .name("main".into())
            .spawn(|| {
                set_thread_label(Some("acceptor"));
                let a = current_worker();
                set_thread_label(None);
                (a, current_worker())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(labelled, "acceptor");
        assert_eq!(cleared, "main");
    }
}
