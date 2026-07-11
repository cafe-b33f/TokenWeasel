//! A lightweight reverse proxy for OpenAI-compatible LLM backends with
//! per-model usage and cost accounting, GPU energy metering, and a web
//! dashboard.

use twl_config::{Cli, Config};
use twl_server::run;

#[tokio::main]
/// Parse CLI arguments, resolve configuration, initialize tracing, and
/// start the proxy server.
async fn main() {
    init_tracing();
    let config = match Config::resolve(Cli::parse()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(config).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Initialize `tracing` from the `RUST_LOG` env var, defaulting to `info`.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
}
