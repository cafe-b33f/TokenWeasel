//! Command-line argument parsing via `clap`.
//!
//! Defines the [`Cli`] struct with every field as `Option<T>` to capture only
//! what was explicitly supplied on the command line. Resolution against
//! environment variables and built-in defaults happens in [`crate::config`].

use clap::Parser;

/// Largest concurrency cap accepted by configuration. Tokio panics when a
/// semaphore is constructed with more permits than this value.
pub(crate) const MAX_CONCURRENCY_LIMIT: usize = tokio::sync::Semaphore::MAX_PERMITS;

fn parse_max_concurrency(value: &str) -> Result<usize, String> {
    let value = value.parse::<usize>().map_err(|_| {
        format!("max concurrency must be an integer between 1 and {MAX_CONCURRENCY_LIMIT}")
    })?;
    if !(1..=MAX_CONCURRENCY_LIMIT).contains(&value) {
        return Err(format!(
            "max concurrency must be between 1 and {MAX_CONCURRENCY_LIMIT}"
        ));
    }
    Ok(value)
}

/// Reverse proxy for OpenAI-compatible LLM backends with usage accounting.
///
/// Parsed from command-line arguments using `clap`. All fields are `Option`
/// to capture only what was explicitly provided - unresolved values are
/// filled in by `Config::resolve`.
///
/// See the [module-level docs](crate) for the full precedence chain (CLI > env > config > default).
#[derive(Parser, Default, Debug)]
#[command(
    name = "twl",
    about = "TokenWeasel - reverse proxy for OpenAI-compatible LLM backends with usage accounting",
    long_about = None,
    version,
    after_help = "Environment fallbacks:
  BIND_ADDR, UPSTREAM_URL, DB_PATH, MAX_CONCURRENCY, CONFIG_PATH
  DASHBOARD_USER, DASHBOARD_PASSWORD
  TLS_ENABLED, TLS_CERT_PATH, TLS_KEY_PATH

Backend API-key environment variable names are configured with api_key_env."
)]
pub struct Cli {
    /// Port to listen on [default: 3000]
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Host/interface to bind; use 0.0.0.0 to expose [default: 127.0.0.1]
    #[arg(long, visible_alias = "bind")]
    pub host: Option<String>,

    /// OpenAI-compatible backend base URL [default: http://127.0.0.1:8080]
    #[arg(short, long)]
    pub upstream: Option<String>,

    /// SQLite database path [default: proxydb.db]
    #[arg(long)]
    pub db: Option<String>,

    /// Per-workload concurrency cap. Independently limits admitted proxy
    /// requests, API-key validation jobs, and local management handlers; it is
    /// not a combined server-wide total. Proxy requests may buffer up to 64 MiB
    /// of request body. Range: 1..=tokio::sync::Semaphore::MAX_PERMITS.
    /// [default: 32]
    #[arg(long, value_parser = parse_max_concurrency)]
    pub max_concurrency: Option<usize>,

    /// Runtime configuration file (JSON). If present, its values serve as the
    /// base configuration; CLI flags and env vars take precedence. [default: config.json]
    #[arg(long)]
    pub config: Option<String>,

    /// Enable or disable TLS [default: true]
    #[arg(long)]
    pub tls: Option<bool>,

    /// Path to the TLS certificate PEM [default: tls/cert.pem]
    #[arg(long)]
    pub tls_cert: Option<String>,

    /// Path to the TLS private key PEM [default: tls/key.pem]
    #[arg(long)]
    pub tls_key: Option<String>,
}

impl Cli {
    /// Parse process arguments, exiting with usage on `-h`/`--help`,
    /// `--version`, or a parse error (clap handles the exit codes).
    ///
    /// This is the standard entry point used by `main`.
    pub fn parse() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    /// Parse from an arbitrary argument iterator (arguments after the program
    /// name). A dummy argv\[0\] is prepended for clap.
    ///
    /// # Example
    ///
    /// ```
    /// use twl_config::Cli;
    ///
    /// let cli = Cli::from_args(vec!["--port".to_string(), "9000".to_string(), "--upstream".to_string(), "http://x".to_string()]);
    /// assert_eq!(cli.port, Some(9000));
    /// assert_eq!(cli.upstream.as_deref(), Some("http://x"));
    /// ```
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let argv = std::iter::once("twl".to_string()).chain(args);
        <Self as Parser>::parse_from(argv)
    }
}
