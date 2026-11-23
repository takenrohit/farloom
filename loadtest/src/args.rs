//! Command-line argument parsing for the load tester.

use std::fmt;

pub const USAGE: &str = "\
Usage: loadtest [OPTIONS]

Options:
  --addr <HOST:PORT>       Server address            (default 127.0.0.1:7878)
  --path <PATH>            Request path              (default /)
  --requests <N>           Total requests, >= 1      (default 1000)
  --concurrency <N>        Client threads, >= 1      (default 8)
  -h, --help               Print this help
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadArgs {
    pub addr: String,
    pub path: String,
    pub requests: u64,
    pub concurrency: usize,
}

impl Default for LoadArgs {
    fn default() -> Self {
        LoadArgs {
            addr: "127.0.0.1:7878".to_string(),
            path: "/".to_string(),
            requests: 1000,
            concurrency: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    UnknownOption(String),
    MissingValue { option: &'static str },
    InvalidValue { option: &'static str, value: String, expected: &'static str },
    MustBePositive { option: &'static str },
    HelpRequested,
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgsError::UnknownOption(opt) => write!(f, "unknown option '{opt}'"),
            ArgsError::MissingValue { option } => write!(f, "missing value for {option}"),
            ArgsError::InvalidValue { option, value, expected } => {
                write!(f, "invalid value '{value}' for {option}: expected {expected}")
            }
            ArgsError::MustBePositive { option } => write!(f, "{option} must be at least 1"),
            ArgsError::HelpRequested => write!(f, "help requested"),
        }
    }
}

impl std::error::Error for ArgsError {}

const OPTIONS: [&str; 4] = ["--addr", "--path", "--requests", "--concurrency"];

impl LoadArgs {
    /// Parses arguments after the program name. Accepts `--opt value` and
    /// `--opt=value`; the last occurrence of an option wins.
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<LoadArgs, ArgsError> {
        let mut out = LoadArgs::default();
        let mut iter = args.into_iter();

        while let Some(arg) = iter.next() {
            if arg == "--help" || arg == "-h" {
                return Err(ArgsError::HelpRequested);
            }
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (arg.clone(), None),
            };
            let option = *OPTIONS
                .iter()
                .find(|o| **o == name)
                .ok_or_else(|| ArgsError::UnknownOption(arg.clone()))?;
            let value = match inline {
                Some(v) => v,
                None => iter.next().ok_or(ArgsError::MissingValue { option })?,
            };

            match option {
                "--addr" => out.addr = value,
                "--path" => out.path = value,
                "--requests" => out.requests = parse_num(option, &value, "positive integer")?,
                "--concurrency" => {
                    out.concurrency = parse_num(option, &value, "positive integer")?
                }
                _ => unreachable!("option table and match arms are out of sync"),
            }
        }

        if out.requests == 0 {
            return Err(ArgsError::MustBePositive { option: "--requests" });
        }
        if out.concurrency == 0 {
            return Err(ArgsError::MustBePositive { option: "--concurrency" });
        }
        Ok(out)
    }
}

fn parse_num<T: std::str::FromStr>(
    option: &'static str,
    value: &str,
    expected: &'static str,
) -> Result<T, ArgsError> {
    value.parse::<T>().map_err(|_| ArgsError::InvalidValue {
        option,
        value: value.to_string(),
        expected,
    })
}
