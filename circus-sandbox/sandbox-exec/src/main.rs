//! Standalone stdio proxy for one provider command inside an owned sandbox.
//!
//! The host supplies an absolute configuration path and the guest argv exactly
//! as the provider appended it. stdout carries guest stdout bytes only; every
//! diagnostic goes to stderr.

mod archive;
mod backend;
mod cli;
mod config;
mod proxy;
mod run;
mod state;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match cli::parse(std::env::args_os().skip(1)) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("circus-sandbox-exec: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::Config::load(&cli.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("circus-sandbox-exec: {error}");
            return ExitCode::from(2);
        }
    };
    match run::execute(config, cli.argv).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("circus-sandbox-exec: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
