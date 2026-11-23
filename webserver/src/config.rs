//! Command-line configuration: defaults, parsing, and static-root validation.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Usage text printed by `main` for `--help` / `-h`.
pub const USAGE: &str = "\
Usage: webserver [OPTIONS]

Options (both `--opt value` and `--opt=value` are accepted):
  --host <HOST>                 Bind address             [default: 127.0.0.1]
  --port <PORT>                 Port, 0 = ephemeral      [default: 7878]
  --workers <N>                 Worker threads, >= 1     [default: 4]
  --queue-capacity <N>          Job queue capacity, >= 1 [default: 64]
  --static-root <DIR>           Static file directory    [default: ./public]
  --read-timeout-secs <SECS>    Socket read timeout, >= 1  [default: 5]
  --write-timeout-secs <SECS>   Socket write timeout, >= 1 [default: 5]
  --max-body-bytes <BYTES>      Maximum request body     [default: 1048576]
  --max-requests <N>            Shut down after N accepted connections, >= 1
  -h, --help                    Print this help
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub workers: usize,
    pub queue_capacity: usize,
    pub static_root: PathBuf,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_body_bytes: u64,
    pub max_requests: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: "127.0.0.1".to_string(),
            port: 7878,
            workers: 4,
            queue_capacity: 64,
            static_root: PathBuf::from("./public"),
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            max_body_bytes: 1_048_576,
            max_requests: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    UnknownOption(String),
    MissingValue { option: &'static str },
    InvalidValue { option: &'static str, value: String, expected: &'static str },
    MustBePositive { option: &'static str },
    HelpRequested,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnknownOption(opt) => write!(f, "unknown option '{opt}'"),
            ConfigError::MissingValue { option } => write!(f, "missing value for {option}"),
            ConfigError::InvalidValue { option, value, expected } => {
                write!(f, "invalid value '{value}' for {option}: expected {expected}")
            }
            ConfigError::MustBePositive { option } => {
                write!(f, "{option} must be greater than 0")
            }
            ConfigError::HelpRequested => write!(f, "help requested"),
        }
    }
}

impl std::error::Error for ConfigError {}

const HOST: &str = "--host";
const PORT: &str = "--port";
const WORKERS: &str = "--workers";
const QUEUE_CAPACITY: &str = "--queue-capacity";
const STATIC_ROOT: &str = "--static-root";
const READ_TIMEOUT: &str = "--read-timeout-secs";
const WRITE_TIMEOUT: &str = "--write-timeout-secs";
const MAX_BODY: &str = "--max-body-bytes";
const MAX_REQUESTS: &str = "--max-requests";

const OPTIONS: [&str; 9] = [
    HOST,
    PORT,
    WORKERS,
    QUEUE_CAPACITY,
    STATIC_ROOT,
    READ_TIMEOUT,
    WRITE_TIMEOUT,
    MAX_BODY,
    MAX_REQUESTS,
];

fn parse_num<T: std::str::FromStr>(
    option: &'static str,
    value: &str,
    expected: &'static str,
) -> Result<T, ConfigError> {
    value.parse::<T>().map_err(|_| ConfigError::InvalidValue {
        option,
        value: value.to_string(),
        expected,
    })
}

impl Config {
    /// Parses arguments after the program name. Pure: no filesystem access.
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Config, ConfigError> {
        let mut cfg = Config::default();
        let mut iter = args.into_iter();

        while let Some(arg) = iter.next() {
            if arg == "--help" || arg == "-h" {
                return Err(ConfigError::HelpRequested);
            }
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };
            let option = *OPTIONS
                .iter()
                .find(|o| **o == name)
                .ok_or_else(|| ConfigError::UnknownOption(arg.clone()))?;

            let value = match inline {
                Some(v) => v,
                None => match iter.next() {
                    // A following option is not a value: `--port --workers 2`.
                    Some(v) if !(v.starts_with("--") && v.len() > 2) => v,
                    _ => return Err(ConfigError::MissingValue { option }),
                },
            };

            match option {
                HOST => cfg.host = value,
                PORT => cfg.port = parse_num(option, &value, "integer 0-65535")?,
                WORKERS => cfg.workers = parse_num(option, &value, "positive integer")?,
                QUEUE_CAPACITY => {
                    cfg.queue_capacity = parse_num(option, &value, "positive integer")?
                }
                STATIC_ROOT => cfg.static_root = PathBuf::from(value),
                READ_TIMEOUT => {
                    cfg.read_timeout = Duration::from_secs(parse_num(
                        option,
                        &value,
                        "positive integer number of seconds",
                    )?)
                }
                WRITE_TIMEOUT => {
                    cfg.write_timeout = Duration::from_secs(parse_num(
                        option,
                        &value,
                        "positive integer number of seconds",
                    )?)
                }
                MAX_BODY => {
                    cfg.max_body_bytes = parse_num(option, &value, "non-negative integer")?
                }
                MAX_REQUESTS => {
                    cfg.max_requests = Some(parse_num(option, &value, "positive integer")?)
                }
                _ => unreachable!("option table and match are out of sync"),
            }
        }

        if cfg.workers == 0 {
            return Err(ConfigError::MustBePositive { option: WORKERS });
        }
        if cfg.queue_capacity == 0 {
            return Err(ConfigError::MustBePositive { option: QUEUE_CAPACITY });
        }
        if cfg.read_timeout.is_zero() {
            return Err(ConfigError::MustBePositive { option: READ_TIMEOUT });
        }
        if cfg.write_timeout.is_zero() {
            return Err(ConfigError::MustBePositive { option: WRITE_TIMEOUT });
        }
        if cfg.max_requests == Some(0) {
            return Err(ConfigError::MustBePositive { option: MAX_REQUESTS });
        }
        Ok(cfg)
    }

    /// Checks that `static_root` exists and is a directory; returns its canonical path.
    pub fn validate_static_root(&self) -> std::io::Result<PathBuf> {
        let canonical = std::fs::canonicalize(&self.static_root)?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a directory", self.static_root.display()),
            ));
        }
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn parse(list: &[&str]) -> Result<Config, ConfigError> {
        Config::from_args(args(list))
    }

    /// Unique temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "webserver-config-{tag}-{}-{nanos}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn empty_args_give_defaults() {
        assert_eq!(parse(&[]), Ok(Config::default()));
    }

    #[test]
    fn zero_workers_is_rejected() {
        assert_eq!(
            parse(&["--workers", "0"]),
            Err(ConfigError::MustBePositive { option: "--workers" })
        );
    }

    #[test]
    fn zero_queue_capacity_is_rejected() {
        assert_eq!(
            parse(&["--queue-capacity", "0"]),
            Err(ConfigError::MustBePositive { option: "--queue-capacity" })
        );
    }

    #[test]
    fn zero_port_is_accepted() {
        assert_eq!(parse(&["--port", "0"]).unwrap().port, 0);
    }

    #[test]
    fn separate_and_inline_values_are_equivalent() {
        let separate = parse(&[
            "--host", "0.0.0.0", "--port", "8080", "--workers", "8", "--queue-capacity", "16",
            "--static-root", "site", "--read-timeout-secs", "2", "--write-timeout-secs", "3",
            "--max-body-bytes", "10", "--max-requests", "7",
        ])
        .unwrap();
        let inline = parse(&[
            "--host=0.0.0.0", "--port=8080", "--workers=8", "--queue-capacity=16",
            "--static-root=site", "--read-timeout-secs=2", "--write-timeout-secs=3",
            "--max-body-bytes=10", "--max-requests=7",
        ])
        .unwrap();
        assert_eq!(separate, inline);
        assert_eq!(
            separate,
            Config {
                host: "0.0.0.0".to_string(),
                port: 8080,
                workers: 8,
                queue_capacity: 16,
                static_root: PathBuf::from("site"),
                read_timeout: Duration::from_secs(2),
                write_timeout: Duration::from_secs(3),
                max_body_bytes: 10,
                max_requests: Some(7),
            }
        );
    }

    #[test]
    fn last_value_wins() {
        assert_eq!(parse(&["--port", "1", "--port=2", "--port", "3"]).unwrap().port, 3);
    }

    #[test]
    fn unknown_option_is_rejected() {
        assert_eq!(
            parse(&["--bogus", "1"]),
            Err(ConfigError::UnknownOption("--bogus".to_string()))
        );
        assert_eq!(
            parse(&["positional"]),
            Err(ConfigError::UnknownOption("positional".to_string()))
        );
    }

    #[test]
    fn missing_value_is_rejected() {
        assert_eq!(
            parse(&["--port"]),
            Err(ConfigError::MissingValue { option: "--port" })
        );
        assert_eq!(
            parse(&["--port", "--workers", "2"]),
            Err(ConfigError::MissingValue { option: "--port" })
        );
    }

    #[test]
    fn invalid_value_display_names_option() {
        let err = parse(&["--port", "70000"]).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { option: "--port", .. }));
        let msg = err.to_string();
        assert!(msg.contains("--port"), "{msg}");
        assert!(msg.contains("70000"), "{msg}");

        let err = parse(&["--workers=abc"]).unwrap_err();
        assert!(err.to_string().contains("--workers"));
        let err = parse(&["--workers", "0"]).unwrap_err();
        assert!(err.to_string().contains("--workers"));
    }

    #[test]
    fn help_is_requested() {
        assert_eq!(parse(&["--help"]), Err(ConfigError::HelpRequested));
        assert_eq!(parse(&["-h"]), Err(ConfigError::HelpRequested));
        assert_eq!(parse(&["--port", "1", "--help"]), Err(ConfigError::HelpRequested));
    }

    #[test]
    fn existing_directory_static_root_is_canonicalized() {
        let tmp = TempDir::new("dir");
        let cfg = Config { static_root: tmp.0.clone(), ..Config::default() };
        let canonical = cfg.validate_static_root().unwrap();
        assert_eq!(canonical, std::fs::canonicalize(&tmp.0).unwrap());
    }

    #[test]
    fn nonexistent_static_root_fails() {
        let tmp = TempDir::new("missing");
        let cfg = Config { static_root: tmp.0.join("does-not-exist"), ..Config::default() };
        assert!(cfg.validate_static_root().is_err());
    }

    #[test]
    fn file_static_root_fails() {
        let tmp = TempDir::new("file");
        let file = tmp.0.join("index.html");
        std::fs::write(&file, b"hi").unwrap();
        let cfg = Config { static_root: file, ..Config::default() };
        let err = cfg.validate_static_root().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
