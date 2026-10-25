mod config;
mod io;
mod mcp_config;
mod outcome;
mod protocol;
mod secrets;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;

use protocol::{Response, VERSION};

#[tokio::main]
async fn main() -> ExitCode {
    let output = io::Output::stdout();
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let runner = if args.len() == 2 && args[0] == "--config" {
        config::load(&PathBuf::from(&args[1])).await
    } else {
        Err(())
    };
    let Ok(runner) = runner else {
        let _ = output
            .send(&Response::Error {
                v: VERSION,
                code: "configuration",
                message: "Invalid host bridge configuration",
            })
            .await;
        eprintln!("circus-stdio-bridge: invalid host configuration");
        return ExitCode::from(2);
    };
    match session::serve(runner, io::stdin(), &output).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => {
            let _ = output
                .send(&Response::Error {
                    v: VERSION,
                    code,
                    message: "Bridge terminated; success is not confirmed",
                })
                .await;
            eprintln!("circus-stdio-bridge: connection terminated ({code})");
            ExitCode::from(2)
        }
    }
}
