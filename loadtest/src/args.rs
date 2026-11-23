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
                "--path" => {
                    // The path is written into the request line verbatim, so it must not
                    // contain spaces or control characters (CR/LF would inject headers).
                    if !value.starts_with('/') || value.bytes().any(|b| b <= 0x20 || b == 0x7F) {
                        return Err(ArgsError::InvalidValue {
                            option,
                            value,
                            expected: "path starting with '/' without spaces or control characters",
                        });
                    }
                    out.path = value
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<LoadArgs, ArgsError> {
        LoadArgs::from_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_gives_defaults() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.addr, "127.0.0.1:7878");
        assert_eq!(a.path, "/");
        assert_eq!(a.requests, 1000);
        assert_eq!(a.concurrency, 8);
        assert_eq!(a, LoadArgs::default());
    }

    #[test]
    fn each_option_overrides_default_separate_value() {
        let a = parse(&["--addr", "10.0.0.1:80"]).unwrap();
        assert_eq!(a, LoadArgs { addr: "10.0.0.1:80".into(), ..LoadArgs::default() });

        let a = parse(&["--path", "/sleep"]).unwrap();
        assert_eq!(a, LoadArgs { path: "/sleep".into(), ..LoadArgs::default() });

        let a = parse(&["--requests", "5"]).unwrap();
        assert_eq!(a, LoadArgs { requests: 5, ..LoadArgs::default() });

        let a = parse(&["--concurrency", "3"]).unwrap();
        assert_eq!(a, LoadArgs { concurrency: 3, ..LoadArgs::default() });
    }

    #[test]
    fn each_option_overrides_default_inline_value() {
        let a = parse(&["--addr=localhost:9000"]).unwrap();
        assert_eq!(a, LoadArgs { addr: "localhost:9000".into(), ..LoadArgs::default() });

        let a = parse(&["--path=/index.html"]).unwrap();
        assert_eq!(a, LoadArgs { path: "/index.html".into(), ..LoadArgs::default() });

        let a = parse(&["--requests=42"]).unwrap();
        assert_eq!(a, LoadArgs { requests: 42, ..LoadArgs::default() });

        let a = parse(&["--concurrency=16"]).unwrap();
        assert_eq!(a, LoadArgs { concurrency: 16, ..LoadArgs::default() });
    }

    #[test]
    fn all_options_together_and_last_wins() {
        let a = parse(&[
            "--addr", "a:1", "--path=/p", "--requests", "7", "--concurrency=2", "--requests=9",
        ])
        .unwrap();
        assert_eq!(
            a,
            LoadArgs { addr: "a:1".into(), path: "/p".into(), requests: 9, concurrency: 2 }
        );
    }

    #[test]
    fn zero_requests_fails() {
        let e = parse(&["--requests", "0"]).unwrap_err();
        assert_eq!(e, ArgsError::MustBePositive { option: "--requests" });
        assert!(e.to_string().contains("--requests"));
        assert!(parse(&["--requests=0"]).is_err());
    }

    #[test]
    fn zero_concurrency_fails() {
        let e = parse(&["--concurrency", "0"]).unwrap_err();
        assert_eq!(e, ArgsError::MustBePositive { option: "--concurrency" });
        assert!(e.to_string().contains("--concurrency"));
        assert!(parse(&["--concurrency=0"]).is_err());
    }

    #[test]
    fn non_numeric_values_fail_and_name_option() {
        let e = parse(&["--requests", "abc"]).unwrap_err();
        assert!(matches!(e, ArgsError::InvalidValue { option: "--requests", .. }));
        assert!(e.to_string().contains("--requests"));
        assert!(e.to_string().contains("abc"));

        let e = parse(&["--concurrency=-1"]).unwrap_err();
        assert!(matches!(e, ArgsError::InvalidValue { option: "--concurrency", .. }));
        assert!(e.to_string().contains("--concurrency"));
    }

    #[test]
    fn unknown_option_fails_and_names_option() {
        let e = parse(&["--bogus"]).unwrap_err();
        assert_eq!(e, ArgsError::UnknownOption("--bogus".into()));
        assert!(e.to_string().contains("--bogus"));

        let e = parse(&["stray"]).unwrap_err();
        assert!(matches!(e, ArgsError::UnknownOption(_)));
    }

    #[test]
    fn missing_values_fail_and_name_option() {
        for opt in OPTIONS {
            let e = parse(&[opt]).unwrap_err();
            assert_eq!(e, ArgsError::MissingValue { option: opt });
            assert!(e.to_string().contains(opt), "message for {opt}: {e}");
        }
    }

    #[test]
    fn invalid_paths_fail_and_name_option() {
        for p in ["health", "/a b", "/a\r\nX: y", ""] {
            let e = parse(&["--path", p]).unwrap_err();
            assert!(matches!(e, ArgsError::InvalidValue { option: "--path", .. }), "{p:?}");
            assert!(e.to_string().contains("--path"));
        }
        assert_eq!(parse(&["--path", "/a?b=c"]).unwrap().path, "/a?b=c");
    }

    #[test]
    fn help_requested() {
        assert_eq!(parse(&["--help"]).unwrap_err(), ArgsError::HelpRequested);
        assert_eq!(parse(&["-h"]).unwrap_err(), ArgsError::HelpRequested);
        assert_eq!(
            parse(&["--requests", "5", "--help"]).unwrap_err(),
            ArgsError::HelpRequested
        );
    }
}
