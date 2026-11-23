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
