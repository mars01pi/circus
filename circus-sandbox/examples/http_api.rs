//! Serves the authenticated HTTP API until Ctrl+C.
//!
//! Run with an explicit token and tenant; the example does not invent or look
//! up credentials. This starts the transport only. A successful start says
//! nothing about Docker availability; sandbox creation still requires a local
//! Engine and an already-present Linux image.
//!
//! ```bash
//! CIRCUS_HTTP_TOKEN=example-token CIRCUS_HTTP_TENANT=local \
//!   cargo run --locked --example http_api
//! ```

use std::error::Error;
use std::net::SocketAddr;

use circus_sandbox::http::{HttpConfig, HttpServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let token = std::env::var("CIRCUS_HTTP_TOKEN").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "set CIRCUS_HTTP_TOKEN to an explicit non-empty bearer token",
        )
    })?;
    let tenant = std::env::var("CIRCUS_HTTP_TENANT").unwrap_or_else(|_| "local".to_string());
    let bind: SocketAddr = std::env::var("CIRCUS_HTTP_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;
    let config = HttpConfig::new(bind, token, [tenant])?;
    let server = HttpServer::bind(config).await?;
    println!("listening on http://{}", server.local_addr());
    let served = server.serve().await?;
    println!(
        "shutdown: {} sandboxes, {} snapshots, {} unconfirmed cleanups",
        served.report.sandboxes,
        served.report.snapshots,
        served.report.failures.len()
    );
    if let Some(error) = served.error {
        eprintln!("accept loop failed: {error}");
    }
    if !served.report.complete() {
        eprintln!("shutdown did not confirm all cleanup");
        std::process::exit(1);
    }
    Ok(())
}
