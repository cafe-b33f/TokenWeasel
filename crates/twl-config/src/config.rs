use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::cli::Cli;
use http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use twl_pricing::Pricing;
use twl_provider::ProviderKind;
use url::{Position, Url};

struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

fn redacted_if_present<T>(value: &Option<T>) -> Option<Redacted> {
    value.as_ref().map(|_| Redacted)
}

/// Requests carrying a prompt-prefix hash are always pinned to a backend by
/// that hash (with deterministic spill on overload); the strategy governs
/// only prefix-less placement.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LbStrategy {
    /// Distribute prefix-less requests cyclically across eligible backends.
    RoundRobin,
    /// Prefer the eligible backend with the fewest active requests.
    #[default]
    LeastLoaded,
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// Backend selection policy and overload tolerance.
pub struct LoadBalancing {
    /// Strategy used for requests without a prompt-prefix affinity hash.
    pub strategy: LbStrategy,
    /// Multiplier applied to the measured overload threshold. Must be >= 1.0.
    pub overload_factor: f64,
}

/// Rolling-window admission policy that prioritizes lower-usage users when
/// proxy capacity is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FairQueueConfig {
    /// Whether fair-queue admission is active.
    pub enabled: bool,
    /// Rolling usage window, in seconds, used to rank waiting users.
    pub window_seconds: u64,
}

impl Default for FairQueueConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_seconds: 30 * 60,
        }
    }
}

impl Default for LoadBalancing {
    fn default() -> Self {
        LoadBalancing {
            strategy: LbStrategy::LeastLoaded,
            overload_factor: 1.25,
        }
    }
}

/// Controls which usage-dashboard components and rows are displayed.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// Identifiers of summary cards to display.
    pub cards: Vec<String>,
    /// Identifiers of graphs to display.
    pub graphs: Vec<String>,
    /// Optional maximum number of rows returned by dashboard tables.
    pub max_rows: Option<usize>,
    /// Socket peer IPs allowed to supply forwarding headers for login
    /// throttling. Empty by default so client headers are not trusted.
    pub trusted_proxies: Vec<std::net::IpAddr>,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        DashboardConfig {
            cards: vec![
                "cost".to_string(),
                "energy".to_string(),
                "tokens".to_string(),
                "requests".to_string(),
                "io".to_string(),
                "cache_hit".to_string(),
            ],
            graphs: vec!["today".to_string(), "over_time".to_string()],
            max_rows: None,
            trusted_proxies: Vec::new(),
        }
    }
}

/// TLS listener settings and certificate identity configuration.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Whether the server accepts HTTPS connections.
    pub enabled: bool,
    /// Path to the PEM-encoded certificate chain.
    pub cert_path: String,
    /// Path to the PEM-encoded private key corresponding to the certificate.
    pub key_path: String,
    /// DNS names for which certificates may be provisioned or selected.
    pub hostnames: Vec<String>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cert_path: "tls/cert.pem".to_string(),
            key_path: "tls/key.pem".to_string(),
            hostnames: vec!["localhost".to_string()],
        }
    }
}

impl TlsConfig {
    /// Missing paths are compared after making them absolute and removing
    /// lexical `.`/`..` components; existing paths are additionally
    /// canonicalized so symlink aliases cannot bypass the check.
    pub fn validate_distinct_paths(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }

        let cert = comparable_tls_path(Path::new(&self.cert_path))?;
        let key = comparable_tls_path(Path::new(&self.key_path))?;
        if cert.normalized == key.normalized
            || matches!((&cert.canonical, &key.canonical), (Some(a), Some(b)) if a == b)
        {
            return Err(ConfigError::Validation(
                "TLS certificate and private key paths must resolve to different files".to_string(),
            ));
        }

        Ok(())
    }
}

struct ComparableTlsPath {
    normalized: PathBuf,
    canonical: Option<PathBuf>,
}

fn comparable_tls_path(path: &Path) -> Result<ComparableTlsPath, ConfigError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| ConfigError::Bare(format!("failed to resolve TLS path: {e}")))?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    let canonical = if path
        .try_exists()
        .map_err(|e| ConfigError::Bare(format!("failed to inspect TLS path: {e}")))?
    {
        Some(
            std::fs::canonicalize(path)
                .map_err(|e| ConfigError::Bare(format!("failed to canonicalize TLS path: {e}")))?,
        )
    } else {
        None
    };

    Ok(ComparableTlsPath {
        normalized,
        canonical,
    })
}

#[derive(serde::Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardConfigFile {
    pub(crate) cards: Option<Vec<String>>,
    pub(crate) graphs: Option<Vec<String>>,
    pub(crate) max_rows: Option<usize>,
    pub(crate) trusted_proxies: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsConfigFile {
    pub(crate) enabled: Option<bool>,
    pub(crate) cert_path: Option<String>,
    pub(crate) key_path: Option<String>,
    pub(crate) hostnames: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoadBalancingFile {
    pub(crate) strategy: Option<LbStrategy>,
    pub(crate) overload_factor: Option<f64>,
}

#[derive(serde::Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct FairQueueConfigFile {
    pub(crate) enabled: Option<bool>,
    pub(crate) window_seconds: Option<u64>,
}

/// Every variant preserves the exact error message that the crate
/// historically returned as a `String`, so tests asserting on error
/// messages continue to pass.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// An explicitly selected or discovered configuration file could not be loaded.
    #[error("config file error ({path}): {message}")]
    ConfigFile {
        /// Path of the configuration file that failed.
        path: String,
        /// Underlying I/O or deserialization error text.
        message: String,
    },
    /// An unqualified configuration-loading error.
    #[error("{0}")]
    Bare(String),
    /// A resolved value or combination of values violates a configuration contract.
    #[error("{0}")]
    Validation(String),
}

/// Fully resolved configuration the server runs with, produced by
/// [`Config::resolve`] at startup.
pub struct Config {
    /// Socket address on which the proxy listens.
    pub bind: String,
    /// Upstream base URL, trailing slash trimmed. In backends mode this is
    /// `backends[0].upstream`.
    pub upstream: String,
    /// Path to the persistent usage database.
    pub db_path: String,
    /// Sanitized model pricing table used for cost accounting.
    pub pricing: Pricing,
    /// Per-workload concurrency cap, applied independently to proxy
    /// requests, API-key validation jobs, and management handlers - not a
    /// combined server-wide total.
    pub max_concurrency: usize,
    /// GPU power draw in watts; zero disables energy accounting.
    pub gpu_watts: f64,
    /// Always non-empty.
    pub backends: Vec<BackendConfig>,
    /// Whether legacy single-upstream catch-all model routing is enabled.
    pub flat_passthrough: bool,
    /// When present, the server creates or rotates this user's password at
    /// startup.
    pub dashboard_auth: Option<(String, String)>,
    /// Serve /usage and its read-only data APIs without authentication.
    pub public_usage: bool,
    /// Whether proxy requests must present a valid TokenWeasel API key.
    pub require_api_key: bool,
    /// Backend selection and overload policy.
    pub load_balancing: LoadBalancing,
    /// Capacity-waiting fairness policy.
    pub fair_queue: FairQueueConfig,
    /// Usage-dashboard presentation and proxy-trust settings.
    pub dashboard: DashboardConfig,
    /// Zero disables pruning.
    pub retention_days: u64,
    /// HTTPS listener and certificate settings.
    pub tls: TlsConfig,
    /// Bearer token for the `/metrics` endpoint; `None` disables it.
    pub metrics_token: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("upstream", &self.upstream)
            .field("db_path", &self.db_path)
            .field("pricing", &self.pricing)
            .field("max_concurrency", &self.max_concurrency)
            .field("gpu_watts", &self.gpu_watts)
            .field("backends", &self.backends)
            .field("flat_passthrough", &self.flat_passthrough)
            .field("dashboard_auth", &redacted_if_present(&self.dashboard_auth))
            .field("public_usage", &self.public_usage)
            .field("require_api_key", &self.require_api_key)
            .field("load_balancing", &self.load_balancing)
            .field("fair_queue", &self.fair_queue)
            .field("dashboard", &self.dashboard)
            .field("retention_days", &self.retention_days)
            .field("tls", &self.tls)
            .field("metrics_token", &redacted_if_present(&self.metrics_token))
            .finish()
    }
}

/// Config-file layer: base values that the CLI and env var layers override.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigFile {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub upstream: Option<String>,
    pub db_path: Option<String>,
    pub pricing: Option<Pricing>,
    pub max_concurrency: Option<usize>,
    pub gpu_watts: Option<f64>,
    pub backends: Option<Vec<BackendConfigFile>>,
    pub dashboard_user: Option<String>,
    pub dashboard_password: Option<String>,
    #[serde(default)]
    pub public_usage: Option<bool>,
    #[serde(default)]
    pub require_api_key: Option<bool>,
    pub load_balancing: Option<LoadBalancingFile>,
    pub fair_queue: Option<FairQueueConfigFile>,
    pub dashboard: Option<DashboardConfigFile>,
    #[serde(default)]
    pub retention_days: Option<u64>,
    pub tls: Option<TlsConfigFile>,
    pub metrics_token: Option<String>,
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("upstream", &self.upstream)
            .field("db_path", &self.db_path)
            .field("pricing", &self.pricing)
            .field("max_concurrency", &self.max_concurrency)
            .field("gpu_watts", &self.gpu_watts)
            .field("backends", &self.backends)
            .field("dashboard_user", &self.dashboard_user)
            .field(
                "dashboard_password",
                &redacted_if_present(&self.dashboard_password),
            )
            .field("public_usage", &self.public_usage)
            .field("require_api_key", &self.require_api_key)
            .field("load_balancing", &self.load_balancing)
            .field("fair_queue", &self.fair_queue)
            .field("dashboard", &self.dashboard)
            .field("retention_days", &self.retention_days)
            .field("tls", &self.tls)
            .field("metrics_token", &redacted_if_present(&self.metrics_token))
            .finish()
    }
}

/// Per-entry configuration loaded from the `backends` array in the config file.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BackendConfigFile {
    /// Provider protocol spoken by the backend.
    pub provider: Option<ProviderKind>,
    /// Base URL to which provider requests are forwarded.
    pub upstream: Option<String>,
    /// Literal credential sent to the upstream provider.
    pub api_key: Option<String>,
    /// Environment variable from which to read the upstream credential.
    pub api_key_env: Option<String>,
    /// Additional HTTP headers attached to upstream requests.
    pub headers: Option<HashMap<String, String>>,
    /// Static model identifiers exposed by this backend.
    pub models: Option<Vec<String>>,
    /// Interval in seconds for refreshing the backend's model catalog.
    pub models_poll_secs: Option<u64>,
    /// GPU power draw in watts; zero disables energy accounting.
    pub gpu_watts: Option<f64>,
    /// Per-model kind overrides, e.g. `{"embed-model": "embedding"}`. They
    /// win over any upstream-detected `type` for both static and
    /// watchdog-discovered models.
    pub model_types: Option<HashMap<String, twl_provider::ModelKind>>,
    /// Allowlist of model IDs this backend may serve; models not listed are
    /// hidden from `/v1/models` and not routable. Applies to both static
    /// `models` and polled models.
    pub model_filter: Option<Vec<String>>,
}

impl std::fmt::Debug for BackendConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendConfigFile")
            .field("provider", &self.provider)
            .field("upstream", &self.upstream)
            .field("api_key", &redacted_if_present(&self.api_key))
            .field("api_key_env", &self.api_key_env)
            .field("headers", &redacted_if_present(&self.headers))
            .field("models", &self.models)
            .field("models_poll_secs", &self.models_poll_secs)
            .field("gpu_watts", &self.gpu_watts)
            .field("model_types", &self.model_types)
            .field("model_filter", &self.model_filter)
            .finish()
    }
}

/// A resolved backend. In flat (single-upstream) mode a single llamacpp
/// backend with `models_poll_secs: 30` is synthesized.
#[derive(Clone)]
pub struct BackendConfig {
    /// Normalized base URL to which requests are forwarded.
    pub upstream: String,
    /// Provider adapter used to translate requests and responses.
    pub provider: ProviderKind,
    /// Optional credential sent to the upstream provider.
    pub api_key: Option<String>,
    /// Validated additional headers sent with upstream requests.
    pub extra_headers: Vec<(String, String)>,
    /// Static model identifiers assigned to this backend.
    pub models: Vec<String>,
    /// Optional model-catalog refresh interval in seconds.
    pub models_poll_secs: Option<u64>,
    /// Zero disables energy accounting.
    pub gpu_watts: f64,
    /// Per-model kind overrides that win over upstream-detected `type`.
    pub model_types: HashMap<String, twl_provider::ModelKind>,
    /// Optional allowlist restricting advertised and routable model IDs.
    pub model_filter: Option<Vec<String>>,
}

impl std::fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendConfig")
            .field("upstream", &self.upstream)
            .field("provider", &self.provider)
            .field("api_key", &redacted_if_present(&self.api_key))
            .field("extra_headers", &Redacted)
            .field("models", &self.models)
            .field("models_poll_secs", &self.models_poll_secs)
            .field("gpu_watts", &self.gpu_watts)
            .field("model_types", &self.model_types)
            .field("model_filter", &self.model_filter)
            .finish()
    }
}

impl Config {
    /// Resolve the runtime configuration with precedence CLI > env >
    /// config file > defaults.
    ///
    /// The config path comes from `--config`, then `CONFIG_PATH`, then an
    /// implicit `config.json`. An explicitly named file must exist and
    /// parse; the implicit default may be absent. A `backends` array in the
    /// file selects backends mode, which rejects flat `upstream`/`gpu_watts`
    /// from any source.
    pub fn resolve(cli: Cli) -> Result<Config, ConfigError> {
        let config_path = resolve_config_path(cli.config.as_deref());

        // Load config file if a path was provided; for the implicit default
        // the file is optional (we fall back to built-ins).
        let cfg_file: Option<ConfigFile> = match &config_path {
            Some(path) => {
                let cf = load_config_file(path).map_err(|inner| {
                    let msg = match &inner {
                        ConfigError::Bare(m) => m.clone(),
                        _ => inner.to_string(),
                    };
                    ConfigError::ConfigFile {
                        path: path.to_string(),
                        message: msg,
                    }
                })?;
                Some(cf)
            }
            None => {
                if Path::new("config.json").exists() {
                    let cf = load_config_file("config.json").map_err(|inner| {
                        let msg = match &inner {
                            ConfigError::Bare(m) => m.clone(),
                            _ => inner.to_string(),
                        };
                        ConfigError::ConfigFile {
                            path: "config.json".to_string(),
                            message: msg,
                        }
                    })?;
                    Some(cf)
                } else {
                    None
                }
            }
        };

        let retention_days = resolve_retention_days(cfg_file.as_ref())?;

        let has_backends = cfg_file.as_ref().is_some_and(|c| c.backends.is_some());
        let has_flat_fields = cfg_file
            .as_ref()
            .is_some_and(|c| c.upstream.is_some() || c.gpu_watts.is_some());

        if has_backends && has_flat_fields {
            return Err(ConfigError::Validation(
                "backends array and flat upstream/gpu_watts fields are mutually exclusive"
                    .to_string(),
            ));
        }

        if has_backends {
            // CLI --upstream and UPSTREAM_URL are not allowed in backends mode.
            if cli.upstream.is_some() {
                return Err(ConfigError::Validation(
                    "the --upstream flag is not supported when a backends array is configured"
                        .to_string(),
                ));
            }
            if std::env::var("UPSTREAM_URL").is_ok() {
                return Err(ConfigError::Validation(
                    "UPSTREAM_URL env var is not supported when a backends array is configured"
                        .to_string(),
                ));
            }

            let cfg_file = cfg_file.as_ref().ok_or_else(|| {
                ConfigError::Validation(
                    "internal error: config file unexpectedly absent".to_string(),
                )
            })?;
            let backends_arr = cfg_file.backends.as_ref().ok_or_else(|| {
                ConfigError::Validation(
                    "internal error: backends array unexpectedly absent".to_string(),
                )
            })?;
            let backends = resolve_backends(backends_arr)?;

            let load_balancing = resolve_load_balancing(Some(cfg_file))?;
            let dashboard = resolve_dashboard(Some(cfg_file))?;

            Ok(Config {
                bind: resolve_bind(
                    cli.host.as_deref(),
                    cli.port,
                    cfg_file.host.as_deref(),
                    cfg_file.port,
                ),
                upstream: backends[0].upstream.clone(),
                db_path: resolve_string(
                    cli.db.as_deref(),
                    "DB_PATH",
                    cfg_file.db_path.as_deref(),
                    "proxydb.db",
                ),
                pricing: cfg_file
                    .pricing
                    .clone()
                    .map(Pricing::sanitized)
                    .unwrap_or_default(),
                max_concurrency: resolve_max_concurrency(
                    cli.max_concurrency,
                    cfg_file.max_concurrency,
                )?,
                gpu_watts: backends[0].gpu_watts,
                backends,
                flat_passthrough: false,
                public_usage: cfg_file.public_usage.unwrap_or(false),
                require_api_key: cfg_file.require_api_key.unwrap_or(true),
                retention_days,
                tls: resolve_tls(
                    cli.tls,
                    cli.tls_cert.as_deref(),
                    cli.tls_key.as_deref(),
                    cfg_file.tls.as_ref(),
                )?,
                dashboard_auth: resolve_dashboard_auth(Some(cfg_file)),
                load_balancing,
                fair_queue: resolve_fair_queue(Some(cfg_file))?,
                dashboard,
                metrics_token: resolve_metrics_token(Some(cfg_file))?,
            })
        } else {
            // Flat mode: single backend.
            let gpu_watts = resolve_gpu_watts(cfg_file.as_ref())?;
            let upstream = resolve_upstream(
                cli.upstream.as_deref(),
                cfg_file.as_ref().and_then(|c| c.upstream.as_deref()),
            )?;

            let backend = BackendConfig {
                upstream: upstream.clone(),
                provider: ProviderKind::LlamaCpp,
                api_key: None,
                extra_headers: Vec::new(),
                models: Vec::new(),
                models_poll_secs: Some(30),
                gpu_watts,
                model_types: HashMap::new(),
                model_filter: None,
            };

            let load_balancing = resolve_load_balancing(cfg_file.as_ref())?;
            let dashboard = resolve_dashboard(cfg_file.as_ref())?;

            Ok(Config {
                bind: resolve_bind(
                    cli.host.as_deref(),
                    cli.port,
                    cfg_file.as_ref().and_then(|c| c.host.as_deref()),
                    cfg_file.as_ref().and_then(|c| c.port),
                ),
                upstream,
                db_path: resolve_string(
                    cli.db.as_deref(),
                    "DB_PATH",
                    cfg_file.as_ref().and_then(|c| c.db_path.as_deref()),
                    "proxydb.db",
                ),
                pricing: cfg_file
                    .as_ref()
                    .and_then(|c| c.pricing.clone())
                    .map(Pricing::sanitized)
                    .unwrap_or_default(),
                max_concurrency: resolve_max_concurrency(
                    cli.max_concurrency,
                    cfg_file.as_ref().and_then(|c| c.max_concurrency),
                )?,
                gpu_watts,
                backends: vec![backend],
                flat_passthrough: true,
                public_usage: cfg_file
                    .as_ref()
                    .and_then(|c| c.public_usage)
                    .unwrap_or(false),
                require_api_key: cfg_file
                    .as_ref()
                    .and_then(|c| c.require_api_key)
                    .unwrap_or(true),
                retention_days,
                tls: resolve_tls(
                    cli.tls,
                    cli.tls_cert.as_deref(),
                    cli.tls_key.as_deref(),
                    cfg_file.as_ref().and_then(|c| c.tls.as_ref()),
                )?,
                dashboard_auth: resolve_dashboard_auth(cfg_file.as_ref()),
                load_balancing,
                fair_queue: resolve_fair_queue(cfg_file.as_ref())?,
                dashboard,
                metrics_token: resolve_metrics_token(cfg_file.as_ref())?,
            })
        }
    }

    /// Returns an owned snapshot of all resolved backends.
    pub fn backends(&self) -> Vec<BackendConfig> {
        self.backends.clone()
    }
}

pub(crate) fn resolve_fair_queue(cfg: Option<&ConfigFile>) -> Result<FairQueueConfig, ConfigError> {
    let file = cfg.and_then(|c| c.fair_queue.as_ref());
    let enabled = file.and_then(|f| f.enabled).unwrap_or(false);
    let window_seconds = file.and_then(|f| f.window_seconds).unwrap_or(30 * 60);
    if window_seconds == 0 {
        return Err(ConfigError::Validation(
            "fair_queue.window_seconds must be greater than zero".to_string(),
        ));
    }
    Ok(FairQueueConfig {
        enabled,
        window_seconds,
    })
}

pub(crate) fn resolve_retention_days(cfg: Option<&ConfigFile>) -> Result<u64, ConfigError> {
    const SECONDS_PER_DAY: i64 = 86_400;

    let days = cfg.and_then(|config| config.retention_days).unwrap_or(90);
    let valid = i64::try_from(days)
        .ok()
        .and_then(|days| days.checked_mul(SECONDS_PER_DAY))
        .is_some();

    if valid {
        Ok(days)
    } else {
        Err(ConfigError::Validation(format!(
            "retention_days must be at most {}",
            i64::MAX / SECONDS_PER_DAY
        )))
    }
}

/// Returns `None` when neither `--config` nor `CONFIG_PATH` was provided;
/// the caller then falls back to an implicit, optional `config.json`.
fn resolve_config_path(cli_config: Option<&str>) -> Option<String> {
    cli_config
        .map(|s| s.to_string())
        .or_else(|| std::env::var("CONFIG_PATH").ok())
}

fn load_config_file(path: &str) -> Result<ConfigFile, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Bare(e.to_string()))?;
    serde_json::from_str::<ConfigFile>(&text).map_err(|e| ConfigError::Bare(e.to_string()))
}

fn resolve_string(cli: Option<&str>, env_name: &str, cfg: Option<&str>, default: &str) -> String {
    cli.map(|s| s.to_string())
        .or_else(|| std::env::var(env_name).ok())
        .or_else(|| cfg.map(|s| s.to_string()))
        .unwrap_or_else(|| default.to_string())
}

/// Only an *absent* `MAX_CONCURRENCY` env var falls through to the config
/// file or default; an invalid one is an error rather than ignored.
pub(crate) fn resolve_max_concurrency(
    cli_max: Option<usize>,
    cfg_max: Option<usize>,
) -> Result<usize, ConfigError> {
    use std::env::VarError;

    fn validate(value: usize, source: &str) -> Result<usize, ConfigError> {
        if (1..=crate::cli::MAX_CONCURRENCY_LIMIT).contains(&value) {
            Ok(value)
        } else {
            Err(ConfigError::Validation(format!(
                "max_concurrency from {source} must be between 1 and {}",
                crate::cli::MAX_CONCURRENCY_LIMIT
            )))
        }
    }

    if let Some(value) = cli_max {
        return validate(value, "CLI");
    }

    match std::env::var("MAX_CONCURRENCY") {
        Ok(raw) => {
            let value = raw.parse::<usize>().map_err(|_| {
                ConfigError::Validation(format!(
                    "MAX_CONCURRENCY must be an integer between 1 and {}",
                    crate::cli::MAX_CONCURRENCY_LIMIT
                ))
            })?;
            validate(value, "MAX_CONCURRENCY")
        }
        Err(VarError::NotUnicode(_)) => Err(ConfigError::Validation(format!(
            "MAX_CONCURRENCY must be valid Unicode and an integer between 1 and {}",
            crate::cli::MAX_CONCURRENCY_LIMIT
        ))),
        Err(VarError::NotPresent) => match cfg_max {
            Some(value) => validate(value, "config file"),
            None => Ok(32),
        },
    }
}

pub(crate) fn resolve_upstream(
    cli_upstream: Option<&str>,
    config_upstream: Option<&str>,
) -> Result<String, ConfigError> {
    let upstream = cli_upstream
        .map(|s| s.to_string())
        .or_else(|| std::env::var("UPSTREAM_URL").ok())
        .or_else(|| config_upstream.map(|s| s.to_string()))
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    validate_upstream(&upstream)
}

fn validate_upstream(upstream: &str) -> Result<String, ConfigError> {
    let parsed = Url::parse(upstream).map_err(|error| {
        ConfigError::Validation(format!("upstream must be a valid absolute URL: {error}"))
    })?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::Validation(
            "upstream URL scheme must be http or https".to_string(),
        ));
    }
    if parsed.host_str().is_none() {
        return Err(ConfigError::Validation(
            "upstream URL must include a host".to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ConfigError::Validation(
            "upstream URL must not include a query string or fragment".to_string(),
        ));
    }

    // Upstream is a base URL. Request and watchdog endpoint paths are appended
    // to this normalized path by the server.
    let mut normalized = String::from(&parsed[..Position::BeforePath]);
    normalized.push_str(parsed.path().trim_end_matches('/'));
    Ok(normalized)
}

/// Any CLI `--host` or `--port` makes `BIND_ADDR` be ignored entirely;
/// otherwise `BIND_ADDR` (a complete `host:port`) wins over config values.
pub(crate) fn resolve_bind(
    cli_host: Option<&str>,
    cli_port: Option<u16>,
    cfg_host: Option<&str>,
    cfg_port: Option<u16>,
) -> String {
    match (cli_host, cli_port) {
        (Some(_), _) | (None, Some(_)) => {
            // Explicit CLI intent: ignore BIND_ADDR, pick each component
            // from CLI -> config -> default.
            let host = cli_host.or(cfg_host).unwrap_or("127.0.0.1");
            let port = cli_port.or(cfg_port).unwrap_or(3000);
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            format!("{host}:{port}")
        }
        (None, None) => {
            // No CLI host or port - try BIND_ADDR as a complete address.
            if let Ok(bind_addr) = std::env::var("BIND_ADDR") {
                if !bind_addr.is_empty() {
                    return bind_addr;
                }
            }
            // Fall back to config components (or defaults).
            let host = cfg_host.unwrap_or("127.0.0.1");
            let port = cfg_port.unwrap_or(3000);
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            format!("{host}:{port}")
        }
    }
}

/// Returns `None` when no non-empty password is configured; the server then
/// generates a random initial admin password when the user database is empty.
pub(crate) fn resolve_dashboard_auth(cfg_file: Option<&ConfigFile>) -> Option<(String, String)> {
    let user = std::env::var("DASHBOARD_USER")
        .ok()
        .or_else(|| cfg_file.and_then(|c| c.dashboard_user.clone()))
        .unwrap_or_else(|| "admin".to_string());

    let password = std::env::var("DASHBOARD_PASSWORD")
        .ok()
        .or_else(|| cfg_file.and_then(|c| c.dashboard_password.clone()));

    password.filter(|p| !p.is_empty()).map(|p| (user, p))
}

/// There is deliberately no CLI flag - secrets on argv leak via process
/// lists. An empty token counts as unset (endpoint disabled).
pub(crate) fn resolve_metrics_token(
    cfg_file: Option<&ConfigFile>,
) -> Result<Option<String>, ConfigError> {
    let token = std::env::var("METRICS_TOKEN")
        .ok()
        .or_else(|| cfg_file.and_then(|c| c.metrics_token.clone()))
        .filter(|t| !t.is_empty());

    if let Some(t) = &token {
        if t.len() < 16 {
            return Err(ConfigError::Validation(
                "metrics_token must be at least 16 characters".to_string(),
            ));
        }
    }
    Ok(token)
}

pub(crate) fn resolve_gpu_watts(cfg: Option<&ConfigFile>) -> Result<f64, ConfigError> {
    match cfg.and_then(|c| c.gpu_watts) {
        None => Ok(0.0),
        Some(v) => validate_gpu_watts(v),
    }
}

pub(crate) fn resolve_load_balancing(
    cfg_file: Option<&ConfigFile>,
) -> Result<LoadBalancing, ConfigError> {
    match cfg_file.and_then(|c| c.load_balancing.as_ref()) {
        None => Ok(LoadBalancing::default()),
        Some(lb) => {
            let strategy = lb.strategy.unwrap_or(LbStrategy::LeastLoaded);
            let overload_factor = lb.overload_factor.unwrap_or(1.25);
            if !overload_factor.is_finite() || overload_factor < 1.0 {
                return Err(ConfigError::Validation(format!(
                    "load_balancing.overload_factor must be finite and >= 1.0, got {overload_factor}"
                )));
            }
            Ok(LoadBalancing {
                strategy,
                overload_factor,
            })
        }
    }
}

pub(crate) fn resolve_dashboard(
    cfg_file: Option<&ConfigFile>,
) -> Result<DashboardConfig, ConfigError> {
    const VALID_CARDS: &[&str] = &["cost", "energy", "tokens", "requests", "io", "cache_hit"];
    const VALID_GRAPHS: &[&str] = &["today", "over_time"];

    match cfg_file.and_then(|c| c.dashboard.as_ref()) {
        None => Ok(DashboardConfig::default()),
        Some(d) => {
            let cards = match &d.cards {
                Some(list) => {
                    let mut seen = HashSet::new();
                    for card in list {
                        if !VALID_CARDS.contains(&card.as_str()) {
                            return Err(ConfigError::Validation(format!(
                                "invalid dashboard card identifier: '{card}'"
                            )));
                        }
                        if !seen.insert(card.as_str()) {
                            return Err(ConfigError::Validation(format!(
                                "duplicate dashboard card identifier: '{card}'"
                            )));
                        }
                    }
                    list.clone()
                }
                None => DashboardConfig::default().cards,
            };

            let graphs = match &d.graphs {
                Some(list) => {
                    let mut seen = HashSet::new();
                    for graph in list {
                        if !VALID_GRAPHS.contains(&graph.as_str()) {
                            return Err(ConfigError::Validation(format!(
                                "invalid dashboard graph identifier: '{graph}'"
                            )));
                        }
                        if !seen.insert(graph.as_str()) {
                            return Err(ConfigError::Validation(format!(
                                "duplicate dashboard graph identifier: '{graph}'"
                            )));
                        }
                    }
                    list.clone()
                }
                None => DashboardConfig::default().graphs,
            };

            let max_rows = match d.max_rows {
                Some(v) if v < 1 => {
                    return Err(ConfigError::Validation(format!(
                        "dashboard.max_rows must be at least 1, got {v}"
                    )));
                }
                v => v,
            };

            let trusted_proxies = d
                .trusted_proxies
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|raw| {
                    raw.parse::<std::net::IpAddr>().map_err(|_| {
                        ConfigError::Validation(format!(
                            "dashboard.trusted_proxies contains invalid IP address: '{raw}'"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(DashboardConfig {
                cards,
                graphs,
                max_rows,
                trusted_proxies,
            })
        }
    }
}

pub(crate) fn resolve_tls(
    cli_tls: Option<bool>,
    cli_cert: Option<&str>,
    cli_key: Option<&str>,
    cfg: Option<&TlsConfigFile>,
) -> Result<TlsConfig, ConfigError> {
    // --- enabled ---
    let enabled = match cli_tls {
        Some(v) => v,
        None => match std::env::var("TLS_ENABLED") {
            Ok(val) => {
                let low = val.to_lowercase();
                match low.as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => {
                        return Err(ConfigError::Validation(
                            "TLS_ENABLED must be true or false".to_string(),
                        ))
                    }
                }
            }
            Err(_) => cfg.and_then(|c| c.enabled).unwrap_or(true),
        },
    };

    // --- cert_path ---
    let cert_path = resolve_string(
        cli_cert,
        "TLS_CERT_PATH",
        cfg.and_then(|c| c.cert_path.as_deref()),
        "tls/cert.pem",
    );

    // --- key_path ---
    let key_path = resolve_string(
        cli_key,
        "TLS_KEY_PATH",
        cfg.and_then(|c| c.key_path.as_deref()),
        "tls/key.pem",
    );

    // --- hostnames ---
    let hostnames = match cfg.and_then(|c| c.hostnames.as_ref()) {
        Some(list) if !list.is_empty() => list.clone(),
        _ => vec!["localhost".to_string()],
    };

    // --- validation (only when enabled) ---
    if enabled {
        if cert_path.is_empty() {
            return Err(ConfigError::Validation(
                "TLS is enabled but cert_path is empty".to_string(),
            ));
        }
        if key_path.is_empty() {
            return Err(ConfigError::Validation(
                "TLS is enabled but key_path is empty".to_string(),
            ));
        }
        for hostname in &hostnames {
            if hostname.is_empty() {
                return Err(ConfigError::Validation(
                    "TLS hostname entry must not be empty".to_string(),
                ));
            }
        }
    }

    let tls = TlsConfig {
        enabled,
        cert_path,
        key_path,
        hostnames,
    };
    tls.validate_distinct_paths()?;
    Ok(tls)
}

pub(crate) fn validate_gpu_watts(value: f64) -> Result<f64, ConfigError> {
    if !value.is_finite() || value < 0.0 {
        Err(ConfigError::Validation(format!(
            "gpu_watts must be finite and non-negative, got {value}"
        )))
    } else {
        Ok(value)
    }
}

pub(crate) fn resolve_backends(
    entries: &[BackendConfigFile],
) -> Result<Vec<BackendConfig>, ConfigError> {
    if entries.is_empty() {
        return Err(ConfigError::Validation(
            "backends array must not be empty".to_string(),
        ));
    }

    let resolved: Result<Vec<BackendConfig>, ConfigError> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let upstream = match &entry.upstream {
                Some(s) => validate_upstream(s).map_err(|error| {
                    ConfigError::Validation(format!("backend[{i}]: {error}"))
                })?,
                None => {
                    return Err(ConfigError::Validation(format!(
                        "backend[{i}]: upstream is required"
                    )));
                }
            };

            if entry.api_key.is_some() && entry.api_key_env.is_some() {
                return Err(ConfigError::Validation(format!(
                    "backend[{i}]: api_key and api_key_env are mutually exclusive"
                )));
            }

            let api_key = match &entry.api_key {
                Some(k) => Some(k.clone()),
                None => {
                    if let Some(env_name) = &entry.api_key_env {
                        match std::env::var(env_name) {
                            Ok(val) if !val.is_empty() => Some(val),
                            Ok(_) => {
                                return Err(ConfigError::Validation(format!(
                                    "backend[{i}]: api_key_env env var \"{env_name}\" is empty"
                                )));
                            }
                            Err(_) => {
                                return Err(ConfigError::Validation(format!(
                                    "backend[{i}]: api_key_env env var \"{env_name}\" is not set"
                                )));
                            }
                        }
                    } else {
                        None
                    }
                }
            };

            let gpu_watts = entry.gpu_watts.unwrap_or(0.0);
            validate_gpu_watts(gpu_watts)
                .map_err(|e| ConfigError::Validation(format!("backend[{i}]: {e}")))?;

            let models_poll_secs = if let Some(v) = entry.models_poll_secs {
                if v < 1 {
                    return Err(ConfigError::Validation(format!(
                        "backend[{i}]: models_poll_secs must be >= 1, got {v}"
                    )));
                }
                Some(v)
            } else {
                None
            };

            if entry.models.is_none() && entry.models_poll_secs.is_none() {
                return Err(ConfigError::Validation(format!(
                    "backend[{i}]: each backend must have at least one of models or models_poll_secs"
                )));
            }

            let extra_headers = resolve_backend_headers(entry.headers.as_ref()).map_err(|error| {
                ConfigError::Validation(format!("backend[{i}]: {error}"))
            })?;
            if api_key.is_some()
                && extra_headers
                    .iter()
                    .any(|(name, _)| name == AUTHORIZATION.as_str())
            {
                return Err(ConfigError::Validation(format!(
                    "backend[{i}]: authorization header cannot be combined with api_key or api_key_env"
                )));
            }

            // Validate model_filter (allowlist of model IDs this backend serves).
            if let Some(filter) = &entry.model_filter {
                if filter.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "backend[{i}]: model_filter must not be empty"
                    )));
                }
                let mut seen: HashSet<String> = HashSet::new();
                for id in filter {
                    if id.is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "backend[{i}]: model_filter entries must not be empty"
                        )));
                    }
                    if !seen.insert(id.clone()) {
                        return Err(ConfigError::Validation(format!(
                            "backend[{i}]: duplicate model_filter entry: '{id}'"
                        )));
                    }
                }
            }

            let models = entry.models.clone().unwrap_or_default();

            Ok(BackendConfig {
                upstream,
                provider: entry.provider.unwrap_or_default(),
                api_key,
                extra_headers,
                models,
                models_poll_secs,
                gpu_watts,
                model_types: entry.model_types.clone().unwrap_or_default(),
                model_filter: entry.model_filter.clone(),
            })
        })
        .collect();

    resolved
}

/// Parse configured backend headers into a deterministic, canonical form.
///
/// `HeaderName` canonicalizes names to lowercase. Sorting the parsed entries
/// both makes duplicate diagnostics independent of `HashMap` iteration order
/// and gives all downstream consumers stable injection order.
fn resolve_backend_headers(
    headers: Option<&HashMap<String, String>>,
) -> Result<Vec<(String, String)>, ConfigError> {
    let Some(headers) = headers else {
        return Ok(Vec::new());
    };

    let mut resolved = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let parsed_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ConfigError::Validation(format!("invalid HTTP header name: {name:?}")))?;
        HeaderValue::from_bytes(value.as_bytes()).map_err(|_| {
            ConfigError::Validation(format!(
                "invalid HTTP header value for {parsed_name}: contains bytes not permitted in a header value"
            ))
        })?;
        resolved.push((parsed_name.as_str().to_string(), value.clone()));
    }

    resolved.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    if let Some(pair) = resolved.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(ConfigError::Validation(format!(
            "duplicate HTTP header name (case-insensitive): {:?}",
            pair[0].0
        )));
    }

    Ok(resolved)
}
