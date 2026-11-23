//! CLI entry point: parse the configuration, start the server, and map errors to exit codes.
//!
//! Exit codes: 0 on success or `--help`, 2 for configuration errors, 1 for startup or runtime errors.

use std::env;
use std::process::ExitCode;

use webserver::config::{Config, ConfigError, USAGE};
use webserver::router::Router;
use webserver::server::Server;

fn main() -> ExitCode {
    let config = match Config::from_args(env::args().skip(1)) {
        Ok(config) => config,
        Err(ConfigError::HelpRequested) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("run with --help to see the available options");
            return ExitCode::from(2);
        }
    };

    let server = match Server::bind(config, Router::with_builtins()) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };

    match server.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: server stopped: {e}");
            ExitCode::from(1)
        }
    }
}
