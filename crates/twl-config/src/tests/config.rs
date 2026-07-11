//! Tests for [`twl_config::config`] resolution helpers.
//!
//! Tests that mutate process environment variables share a mutex and restore
//! the original value before releasing it.
//!
//! Covers: bind address resolution (CLI intent, BIND_ADDR interaction, IPv6
//! bracketing, config fallback), upstream URL trimming and precedence, max
//! concurrency validation, gpu_watts acceptance/rejection (positive, zero,
//! negative, NaN, Inf), config-file JSON parsing (full, partial, unknown
//! fields), backends array parsing, flat-mode back-compat, all validation
//! errors, env-var key resolution, defaults, and end-to-end `Config::resolve`
//! with explicit file paths.

use std::env;

use crate::config::{
    resolve_backends, resolve_bind, resolve_dashboard, resolve_fair_queue, resolve_gpu_watts,
    resolve_load_balancing, resolve_max_concurrency, resolve_metrics_token, resolve_retention_days,
    resolve_tls, resolve_upstream, validate_gpu_watts, BackendConfig, BackendConfigFile, Config,
    ConfigFile, DashboardConfig, FairQueueConfig, FairQueueConfigFile, LbStrategy, LoadBalancing,
    TlsConfig, TlsConfigFile,
};
use crate::ProviderKind;

/// Serializes tests that mutate environment variables so they never race.
pub(crate) static CONFIG_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn config_and_backend_debug_redact_all_secret_values() {
    const DASHBOARD_USER: &str = "M8_DASHBOARD_USER_SENTINEL";
    const DASHBOARD_PASSWORD: &str = "M8_DASHBOARD_PASSWORD_SENTINEL";
    const METRICS_TOKEN: &str = "M8_METRICS_TOKEN_SENTINEL";
    const BACKEND_KEY: &str = "M8_BACKEND_API_KEY_SENTINEL";
    const HEADER_VALUE: &str = "M8_EXTRA_HEADER_VALUE_SENTINEL";

    let backend = BackendConfig {
        upstream: "http://localhost:8080".to_string(),
        provider: ProviderKind::OpenRouter,
        api_key: Some(BACKEND_KEY.to_string()),
        extra_headers: vec![("x-private-token".to_string(), HEADER_VALUE.to_string())],
        models: vec!["model-a".to_string()],
        models_poll_secs: Some(30),
        model_types: Default::default(),
        model_filter: None,
        gpu_watts: 125.0,
    };

    let backend_debug = format!("{backend:?}");
    assert!(!backend_debug.contains(BACKEND_KEY), "{backend_debug}");
    assert!(!backend_debug.contains(HEADER_VALUE), "{backend_debug}");
    assert!(backend_debug.contains("[REDACTED]"), "{backend_debug}");

    let config = Config {
        bind: "127.0.0.1:3000".to_string(),
        upstream: "http://localhost:8080".to_string(),
        db_path: ":memory:".to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 8,
        gpu_watts: 125.0,
        backends: vec![backend],
        flat_passthrough: false,
        dashboard_auth: Some((DASHBOARD_USER.to_string(), DASHBOARD_PASSWORD.to_string())),
        public_usage: false,
        require_api_key: true,
        load_balancing: LoadBalancing::default(),
        fair_queue: FairQueueConfig::default(),
        dashboard: DashboardConfig::default(),
        retention_days: 90,
        tls: TlsConfig::default(),
        metrics_token: Some(METRICS_TOKEN.to_string()),
    };

    let config_debug = format!("{config:?}");
    for sentinel in [
        DASHBOARD_USER,
        DASHBOARD_PASSWORD,
        METRICS_TOKEN,
        BACKEND_KEY,
        HEADER_VALUE,
    ] {
        assert!(
            !config_debug.contains(sentinel),
            "secret {sentinel} leaked in {config_debug}"
        );
    }
    assert!(config_debug.contains("[REDACTED]"), "{config_debug}");
}

// ── bind resolution ---------------------------------------------------------

#[test]
fn bind_combines_host_and_port() {
    assert_eq!(
        resolve_bind(Some("0.0.0.0"), Some(8080), None, None),
        "0.0.0.0:8080"
    );
}

#[test]
fn fair_queue_defaults_disabled_with_thirty_minute_window() {
    let resolved = resolve_fair_queue(None).unwrap();
    assert!(!resolved.enabled);
    assert_eq!(resolved.window_seconds, 1800);
}

#[test]
fn fair_queue_resolves_custom_window() {
    let cfg = ConfigFile {
        fair_queue: Some(FairQueueConfigFile {
            enabled: Some(true),
            window_seconds: Some(300),
        }),
        ..ConfigFile::default()
    };
    let resolved = resolve_fair_queue(Some(&cfg)).unwrap();
    assert!(resolved.enabled);
    assert_eq!(resolved.window_seconds, 300);
}

#[test]
fn fair_queue_rejects_zero_window() {
    let cfg = ConfigFile {
        fair_queue: Some(FairQueueConfigFile {
            enabled: Some(true),
            window_seconds: Some(0),
        }),
        ..ConfigFile::default()
    };
    assert!(resolve_fair_queue(Some(&cfg)).is_err());
}

#[test]
fn retention_days_preserves_zero_and_accepts_largest_safe_value() {
    let disabled = ConfigFile {
        retention_days: Some(0),
        ..ConfigFile::default()
    };
    assert_eq!(resolve_retention_days(Some(&disabled)).unwrap(), 0);

    let largest_safe = (i64::MAX / 86_400) as u64;
    let config = ConfigFile {
        retention_days: Some(largest_safe),
        ..ConfigFile::default()
    };
    assert_eq!(resolve_retention_days(Some(&config)).unwrap(), largest_safe);
}

#[test]
fn config_resolve_rejects_retention_days_that_cannot_fit_seconds() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_retention_days_overflow.json");
    write_json(&tmp, r#"{"retention_days":18446744073709551615}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };

    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("retention_days"),
        "error should mention retention_days: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn bind_defaults_port_when_only_host_given() {
    assert_eq!(
        resolve_bind(Some("example"), None, None, None),
        "example:3000"
    );
}

#[test]
fn bind_defaults_host_when_only_port_given() {
    assert_eq!(resolve_bind(None, Some(9000), None, None), "127.0.0.1:9000");
}

#[test]
fn upstream_trims_trailing_slash() {
    assert_eq!(
        resolve_upstream(Some("http://host:8080///"), None).unwrap(),
        "http://host:8080"
    );
}

#[test]
fn flat_upstream_rejects_slash_only_and_scheme_only_urls() {
    assert!(resolve_upstream(Some("/"), None).is_err());
    assert!(resolve_upstream(Some("https://"), None).is_err());
    assert!(resolve_upstream(Some("relative/path"), None).is_err());
    assert!(resolve_upstream(Some("ftp://example.com"), None).is_err());
}

#[test]
fn upstream_base_rejects_query_and_fragment_components() {
    for upstream in [
        "https://host/base?token=secret/path",
        "https://host/base#ignored/path",
        "https://host/base?token=secret#fragment",
    ] {
        let error = resolve_upstream(Some(upstream), None).unwrap_err();
        assert!(
            error.to_string().contains("query string or fragment"),
            "unexpected validation error for {upstream}: {error}"
        );
    }

    assert_eq!(
        resolve_upstream(Some("https://host/base///"), None).unwrap(),
        "https://host/base"
    );
}

#[test]
fn upstream_precedence_cli_env_config_default() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = env::var("UPSTREAM_URL");
    env::remove_var("UPSTREAM_URL");

    // 1. No CLI, no env, no config → default
    assert_eq!(
        resolve_upstream(None, None).unwrap(),
        "http://127.0.0.1:8080"
    );

    // 2. Config value beats default
    assert_eq!(
        resolve_upstream(None, Some("http://config:9000")).unwrap(),
        "http://config:9000"
    );

    // 3. Env beats config
    env::set_var("UPSTREAM_URL", "http://env:9001");
    assert_eq!(
        resolve_upstream(None, Some("http://config:9000")).unwrap(),
        "http://env:9001"
    );

    // 4. CLI beats env
    assert_eq!(
        resolve_upstream(Some("http://cli:9002"), Some("http://config:9000")).unwrap(),
        "http://cli:9002"
    );

    // Restore original env var
    match orig {
        Ok(v) => env::set_var("UPSTREAM_URL", v),
        Err(_) => env::remove_var("UPSTREAM_URL"),
    }
    drop(guard);
}

#[test]
fn max_concurrency_cli_takes_precedence_over_env() {
    // When CLI is set, it wins regardless of the env var.
    assert_eq!(resolve_max_concurrency(Some(64), None).unwrap(), 64);
}

#[test]
fn max_concurrency_precedence_cli_env_config_default() {
    // CLI > env > config > default when supplied values are valid.
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = env::var("MAX_CONCURRENCY");
    env::remove_var("MAX_CONCURRENCY");

    // 1. No CLI, no env, no config → default
    assert_eq!(resolve_max_concurrency(None, None).unwrap(), 32);

    // 2. Config value beats default
    assert_eq!(resolve_max_concurrency(None, Some(16)).unwrap(), 16);

    // 4. Env beats config
    env::set_var("MAX_CONCURRENCY", "64");
    assert_eq!(resolve_max_concurrency(None, Some(16)).unwrap(), 64);

    // 7. Env beats config; CLI beats env
    env::set_var("MAX_CONCURRENCY", "64");
    assert_eq!(resolve_max_concurrency(Some(128), Some(16)).unwrap(), 128);

    // 10. CLI Some(1) beats env 10
    env::set_var("MAX_CONCURRENCY", "10");
    assert_eq!(resolve_max_concurrency(Some(1), Some(16)).unwrap(), 1);

    match orig {
        Ok(v) => env::set_var("MAX_CONCURRENCY", v),
        Err(_) => env::remove_var("MAX_CONCURRENCY"),
    }
    drop(guard);
}

fn assert_invalid_max_concurrency_env(value: &str) {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let original = env::var("MAX_CONCURRENCY");
    env::set_var("MAX_CONCURRENCY", value);

    let err = resolve_max_concurrency(None, Some(7)).unwrap_err();
    assert!(
        err.to_string().contains("MAX_CONCURRENCY"),
        "invalid environment value {value:?} must fail instead of falling through: {err}"
    );

    match original {
        Ok(value) => env::set_var("MAX_CONCURRENCY", value),
        Err(_) => env::remove_var("MAX_CONCURRENCY"),
    }
    drop(guard);
}

#[test]
fn max_concurrency_empty_env_fails_fast() {
    assert_invalid_max_concurrency_env("");
}

#[test]
fn max_concurrency_zero_env_fails_fast() {
    assert_invalid_max_concurrency_env("0");
}

#[test]
fn max_concurrency_unparseable_env_fails_fast() {
    assert_invalid_max_concurrency_env("not-a-number");
}

#[test]
fn max_concurrency_above_semaphore_limit_fails_for_env() {
    assert_invalid_max_concurrency_env(&(crate::cli::MAX_CONCURRENCY_LIMIT + 1).to_string());
}

#[test]
fn max_concurrency_zero_in_config_file_fails_startup() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let original = env::var("MAX_CONCURRENCY");
    env::remove_var("MAX_CONCURRENCY");
    let path = env::temp_dir().join("twl_max_concurrency_zero.json");
    write_json(&path, r#"{"max_concurrency":0}"#);

    let cli = crate::cli::Cli {
        config: Some(path.to_string_lossy().into_owned()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("max_concurrency from config file"),
        "zero config value must fail instead of using the default: {err}"
    );

    std::fs::remove_file(path).ok();
    match original {
        Ok(value) => env::set_var("MAX_CONCURRENCY", value),
        Err(_) => env::remove_var("MAX_CONCURRENCY"),
    }
    drop(guard);
}

#[test]
fn max_concurrency_above_semaphore_limit_fails_for_config_file() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let original = env::var("MAX_CONCURRENCY");
    env::remove_var("MAX_CONCURRENCY");
    let path = env::temp_dir().join("twl_max_concurrency_above_limit.json");
    write_json(
        &path,
        &format!(
            r#"{{"max_concurrency":{}}}"#,
            crate::cli::MAX_CONCURRENCY_LIMIT + 1
        ),
    );

    let cli = crate::cli::Cli {
        config: Some(path.to_string_lossy().into_owned()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(err.to_string().contains("max_concurrency from config file"));

    std::fs::remove_file(path).ok();
    match original {
        Ok(value) => env::set_var("MAX_CONCURRENCY", value),
        Err(_) => env::remove_var("MAX_CONCURRENCY"),
    }
    drop(guard);
}

/// When *either* CLI host or CLI port is present (explicit CLI intent), BIND_ADDR
/// is ignored. Each component follows CLI → config → default.
#[test]
fn cli_explicit_beats_bind_addr() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = std::env::var("BIND_ADDR");
    std::env::set_var("BIND_ADDR", "10.0.0.1:9999");

    // CLI --port present; BIND_ADDR must be ignored, config host fills in.
    assert_eq!(
        resolve_bind(None, Some(8080), Some("config-host"), None),
        "config-host:8080"
    );

    // CLI --host present; BIND_ADDR must be ignored, config port fills in.
    assert_eq!(
        resolve_bind(Some("cli-host"), None, None, Some(9000)),
        "cli-host:9000"
    );

    // Both CLI present - config is ignored entirely.
    assert_eq!(
        resolve_bind(Some("cli-h"), Some(1111), Some("cfg-h"), Some(2222)),
        "cli-h:1111"
    );

    // Restore original BIND_ADDR
    match orig {
        Ok(v) => std::env::set_var("BIND_ADDR", v),
        Err(_) => std::env::remove_var("BIND_ADDR"),
    }
    drop(guard);
}

/// Without any CLI host/port, a non-empty BIND_ADDR env var wins over config.
#[test]
fn bind_addr_beats_config() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = std::env::var("BIND_ADDR");
    std::env::set_var("BIND_ADDR", "env-bind:7777");

    assert_eq!(
        resolve_bind(None, None, Some("config-host"), Some(8888)),
        "env-bind:7777"
    );

    // Non-empty BIND_ADDR beats config defaults too.
    assert_eq!(
        resolve_bind(None, None, Some("config-host"), Some(8888)),
        "env-bind:7777"
    );

    match orig {
        Ok(v) => std::env::set_var("BIND_ADDR", v),
        Err(_) => std::env::remove_var("BIND_ADDR"),
    }
    drop(guard);
}

/// Config host/port beat the built-in defaults when no CLI and no BIND_ADDR.
#[test]
fn config_beats_defaults() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = std::env::var("BIND_ADDR");
    std::env::remove_var("BIND_ADDR");

    assert_eq!(
        resolve_bind(None, None, Some("my-host"), Some(5555)),
        "my-host:5555"
    );

    // Partial config: host only → port defaults.
    assert_eq!(
        resolve_bind(None, None, Some("my-host"), None),
        "my-host:3000"
    );

    // Partial config: port only → host defaults.
    assert_eq!(resolve_bind(None, None, None, Some(6666)), "127.0.0.1:6666");

    // No config at all → full defaults.
    assert_eq!(resolve_bind(None, None, None, None), "127.0.0.1:3000");

    match orig {
        Ok(v) => std::env::set_var("BIND_ADDR", v),
        Err(_) => std::env::remove_var("BIND_ADDR"),
    }
    drop(guard);
}

/// Partial CLI values use config fallback for the missing component.
#[test]
fn partial_cli_uses_config_fallback() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = std::env::var("BIND_ADDR");
    std::env::remove_var("BIND_ADDR");

    // CLI host + config port.
    assert_eq!(
        resolve_bind(Some("cli-h"), None, Some("cfg-h"), Some(4444)),
        "cli-h:4444"
    );

    // CLI port + config host.
    assert_eq!(
        resolve_bind(None, Some(5555), Some("cfg-h"), None),
        "cfg-h:5555"
    );

    match orig {
        Ok(v) => std::env::set_var("BIND_ADDR", v),
        Err(_) => std::env::remove_var("BIND_ADDR"),
    }
    drop(guard);
}

// --- bind resolution: IPv6 bracketing ---------------------------------------

#[test]
fn bind_cli_ipv6_is_bracketed() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    std::env::remove_var("BIND_ADDR");
    assert_eq!(
        resolve_bind(Some("::1"), Some(8080), None, None),
        "[::1]:8080"
    );
    // CLI host only - port defaults.
    assert_eq!(
        resolve_bind(Some("2001:db8::1"), None, None, None),
        "[2001:db8::1]:3000"
    );
    drop(guard);
}

#[test]
fn bind_config_ipv6_is_bracketed() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    std::env::remove_var("BIND_ADDR");
    assert_eq!(
        resolve_bind(None, None, Some("::1"), Some(8080)),
        "[::1]:8080"
    );
    assert_eq!(
        resolve_bind(None, None, Some("fe80::1"), None),
        "[fe80::1]:3000"
    );
    drop(guard);
}

#[test]
fn bind_already_bracketed_ipv6_unchanged() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    std::env::remove_var("BIND_ADDR");
    // Already bracketed - no double-bracketing.
    assert_eq!(
        resolve_bind(Some("[::1]"), Some(8080), None, None),
        "[::1]:8080"
    );
    assert_eq!(
        resolve_bind(None, None, Some("[2001:db8::1]"), Some(9000)),
        "[2001:db8::1]:9000"
    );
    drop(guard);
}

#[test]
fn bind_hostname_and_ipv4_unaffected() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    std::env::remove_var("BIND_ADDR");
    assert_eq!(
        resolve_bind(Some("example.com"), None, None, None),
        "example.com:3000"
    );
    assert_eq!(
        resolve_bind(Some("192.168.1.1"), Some(8080), None, None),
        "192.168.1.1:8080"
    );
    assert_eq!(
        resolve_bind(None, None, Some("10.0.0.5"), Some(5555)),
        "10.0.0.5:5555"
    );
    drop(guard);
}

// --- bind resolution ---------------------------------------------------------

const FULL_CONFIG: &str = r#"
{
    "host": "0.0.0.0",
    "port": 8080,
    "upstream": "http://upstream:8080/",
    "db_path": "/tmp/proxydb.db",
    "pricing": {
        "currency": "EUR",
        "unit": 1000000,
        "models": {
            "demo-model": {
                "input": 1.0,
                "output": 2.0,
                "input_cached": 0.5
            }
        }
    },
    "max_concurrency": 64,
    "gpu_watts": 250.5
}"#;

#[test]
fn config_file_parses_all_known_fields() {
    let cfg: ConfigFile = serde_json::from_str(FULL_CONFIG).expect("parse all fields");
    assert_eq!(cfg.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(cfg.port, Some(8080));
    assert_eq!(cfg.upstream.as_deref(), Some("http://upstream:8080/"));
    assert_eq!(cfg.db_path.as_deref(), Some("/tmp/proxydb.db"));
    assert!(cfg.pricing.is_some());
    let pricing = cfg.pricing.as_ref().unwrap();
    assert_eq!(pricing.currency, "EUR");
    assert_eq!(pricing.unit, 1000000.0);
    assert!(pricing.models.contains_key("demo-model"));
    assert_eq!(cfg.max_concurrency, Some(64));
    assert_eq!(cfg.gpu_watts, Some(250.5));
}

#[test]
fn config_file_unknown_field_rejected() {
    let result: Result<ConfigFile, _> =
        serde_json::from_str(r#"{"host": "x", "unknown_field": 42}"#);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unknown_field"),
        "error should mention unknown_field: {msg}"
    );
}

#[test]
fn config_file_gpu_wattss_typo_rejected() {
    let result: Result<ConfigFile, _> = serde_json::from_str(r#"{"gpu_wattss": 500}"#);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("gpu_wattss"),
        "error should mention gpu_wattss: {msg}"
    );
}

#[test]
fn config_file_partial_is_ok() {
    // Only a subset of fields - remaining options default to None.
    let cfg: ConfigFile =
        serde_json::from_str(r#"{"host": "myhost", "port": 9000}"#).expect("parse partial config");
    assert_eq!(cfg.host.as_deref(), Some("myhost"));
    assert_eq!(cfg.port, Some(9000));
    assert_eq!(cfg.upstream, None);
    assert_eq!(cfg.db_path, None);
    assert!(cfg.pricing.is_none());
    assert_eq!(cfg.max_concurrency, None);
    assert_eq!(cfg.gpu_watts, None);
}

#[test]
fn gpu_watts_none_defaults_to_zero() {
    let cfg = ConfigFile {
        gpu_watts: None,
        ..ConfigFile::default()
    };
    assert_eq!(resolve_gpu_watts(Some(&cfg)).unwrap(), 0.0);
}

#[test]
fn gpu_watts_positive_value_accepted() {
    let cfg = ConfigFile {
        gpu_watts: Some(500.0),
        ..ConfigFile::default()
    };
    assert_eq!(resolve_gpu_watts(Some(&cfg)).unwrap(), 500.0);
}

#[test]
fn gpu_watts_zero_explicit_is_zero() {
    let cfg = ConfigFile {
        gpu_watts: Some(0.0),
        ..ConfigFile::default()
    };
    assert_eq!(resolve_gpu_watts(Some(&cfg)).unwrap(), 0.0);
}

#[test]
fn gpu_watts_negative_rejected() {
    assert!(validate_gpu_watts(-10.0).is_err());
    let cfg = ConfigFile {
        gpu_watts: Some(-10.0),
        ..ConfigFile::default()
    };
    assert!(resolve_gpu_watts(Some(&cfg)).is_err());
}

#[test]
fn gpu_watts_nan_rejected() {
    assert!(validate_gpu_watts(f64::NAN).is_err());
    let cfg = ConfigFile {
        gpu_watts: Some(f64::NAN),
        ..ConfigFile::default()
    };
    assert!(resolve_gpu_watts(Some(&cfg)).is_err());
}

#[test]
fn gpu_watts_infinity_rejected() {
    assert!(validate_gpu_watts(f64::INFINITY).is_err());
    let cfg = ConfigFile {
        gpu_watts: Some(f64::INFINITY),
        ..ConfigFile::default()
    };
    assert!(resolve_gpu_watts(Some(&cfg)).is_err());
}

#[test]
fn gpu_watts_decimal_accepted() {
    let cfg = ConfigFile {
        gpu_watts: Some(250.5),
        ..ConfigFile::default()
    };
    assert_eq!(resolve_gpu_watts(Some(&cfg)).unwrap(), 250.5);
}

#[test]
fn resolve_gpu_watts_negative_returns_err() {
    let cfg = ConfigFile {
        gpu_watts: Some(-5.0),
        ..ConfigFile::default()
    };
    let err = resolve_gpu_watts(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("gpu_watts"),
        "error should mention gpu_watts: {err}"
    );
}

#[test]
fn resolve_missing_explicit_config_returns_err() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let cli = crate::cli::Cli {
        config: Some("/tmp/does-not-exist-12345.json".to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("config file error"),
        "error should mention config file error: {err}"
    );
}

fn write_json(path: &std::path::Path, content: &str) {
    std::fs::write(path, content).expect("write temp json");
}

#[test]
fn config_file_parses_backends_array() {
    let json = r#"{"host":"0.0.0.0","port":8080,"backends":[{"upstream":"http://gpu1:8080","gpu_watts":500,"models_poll_secs":30},{"provider":"deepseek","upstream":"https://api.deepseek.com","models":["deepseek-chat","deepseek-reasoner"]}]}"#;
    let cfg: ConfigFile = serde_json::from_str(json).expect("parse backends config");
    let backends = cfg.backends.as_ref().unwrap();
    assert_eq!(backends.len(), 2);

    assert_eq!(backends[0].upstream.as_deref(), Some("http://gpu1:8080"));
    assert_eq!(backends[0].gpu_watts, Some(500.0));
    assert_eq!(backends[0].models_poll_secs, Some(30));
    assert_eq!(backends[0].provider, None);
    assert_eq!(backends[0].api_key, None);
    assert_eq!(backends[0].api_key_env, None);
    assert_eq!(backends[0].models, None);

    assert_eq!(
        backends[1].upstream.as_deref(),
        Some("https://api.deepseek.com")
    );
    assert_eq!(backends[1].provider, Some(ProviderKind::DeepSeek));
    assert_eq!(
        backends[1].models.as_deref().map(|m: &[String]| m.len()),
        Some(2)
    );
}

#[test]
fn backends_and_flat_fields_mutually_exclusive() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_mutual.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x"}],"upstream":"http://y"}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("mutually exclusive"),
        "error should mention mutually exclusive: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backends_and_flat_gpu_watts_mutually_exclusive() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_mutual_gpu.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x"}],"gpu_watts":500}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("mutually exclusive"),
        "error should mention mutually exclusive: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backends_mode_rejects_cli_upstream() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_cli_upstream.json");
    write_json(&tmp, r#"{"backends":[{"upstream":"http://x"}]}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        upstream: Some("http://cli-upstream".to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("--upstream"),
        "error should mention --upstream: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backends_mode_rejects_upstream_url_env_var() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = std::env::var("UPSTREAM_URL");
    std::env::remove_var("UPSTREAM_URL");

    let tmp = std::env::temp_dir().join("test_upstream_url_env.json");
    write_json(&tmp, r#"{"backends":[{"upstream":"http://x"}]}"#);
    std::env::set_var("UPSTREAM_URL", "http://env-upstream");

    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("UPSTREAM_URL"),
        "error should mention UPSTREAM_URL: {err}"
    );

    std::fs::remove_file(&tmp).ok();
    match orig {
        Ok(v) => std::env::set_var("UPSTREAM_URL", v),
        Err(_) => std::env::remove_var("UPSTREAM_URL"),
    }
    drop(guard);
}

#[test]
fn flat_mode_single_backend_has_defaults() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_flat.json");
    write_json(&tmp, r#"{"host":"127.0.0.1","port":3000}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("flat mode config");
    let backends = config.backends();
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].models_poll_secs, Some(30));
    assert_eq!(backends[0].provider, ProviderKind::LlamaCpp);
    assert!(backends[0].api_key.is_none());
    assert!(backends[0].extra_headers.is_empty());
    assert!(backends[0].models.is_empty());
    assert!(!backends[0].upstream.is_empty());
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn flat_mode_preserves_flat_upstream_and_gpu_watts() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_flat_gpu.json");
    write_json(&tmp, r#"{"upstream":"http://flat:9000","gpu_watts":400}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("flat mode with gpu_watts");
    let backends = config.backends();
    assert_eq!(backends[0].upstream, "http://flat:9000");
    assert_eq!(backends[0].gpu_watts, 400.0);
    assert_eq!(backends[0].models_poll_secs, Some(30));
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn empty_backends_array_errors() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_empty.json");
    write_json(&tmp, r#"{"backends":[]}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("not be empty"),
        "error should mention empty: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn missing_upstream_errors() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_missing_upstream.json");
    write_json(&tmp, r#"{"backends":[{"models":["m"]}]}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("upstream is required"),
        "error should mention upstream is required: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn api_key_and_api_key_env_mutually_exclusive() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    // Ensure UPSTREAM_URL is clean for this backends-mode test.
    std::env::remove_var("UPSTREAM_URL");
    let tmp = std::env::temp_dir().join("test_api_key_both.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","api_key":"k","api_key_env":"FOO"}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("mutually exclusive"),
        "error should mention mutually exclusive: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn api_key_env_missing_var_errors() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = std::env::var("MISSING_KEY_VAR_12345");
    if orig.is_ok() {
        std::env::remove_var("MISSING_KEY_VAR_12345");
    }

    let tmp = std::env::temp_dir().join("test_missing_env.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","api_key_env":"MISSING_KEY_VAR_12345","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("not set"),
        "error should mention env var not set: {err}"
    );
    std::fs::remove_file(&tmp).ok();

    match orig {
        Ok(v) => std::env::set_var("MISSING_KEY_VAR_12345", v),
        Err(_) => std::env::remove_var("MISSING_KEY_VAR_12345"),
    }
    drop(guard);
}

#[test]
fn api_key_env_empty_var_errors() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_empty_env.json");
    std::env::set_var("EMPTY_KEY_VAR_12345", "");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","api_key_env":"EMPTY_KEY_VAR_12345","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("empty"),
        "error should mention empty: {err}"
    );
    std::fs::remove_file(&tmp).ok();
    std::env::remove_var("EMPTY_KEY_VAR_12345");
    drop(guard);
}

#[test]
fn gpu_watts_per_entry_negative_errors() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_neg_gpu.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"],"gpu_watts":-10}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("gpu_watts")
            || err.to_string().contains("non-negative")
            || err.to_string().contains("-10"),
        "error should mention gpu_watts negative: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn models_poll_secs_below_one_errors() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_bad_poll.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models_poll_secs":0}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("models_poll_secs"),
        "error should mention models_poll_secs: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn no_models_no_poll_errors() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_no_model_info.json");
    write_json(&tmp, r#"{"backends":[{"upstream":"http://x"}]}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("models") || err.to_string().contains("poll"),
        "error should mention models/poll: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn api_key_env_resolves_correctly() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();

    let tmp = std::env::temp_dir().join("test_env_key.json");
    std::env::set_var("MY_DEEPSEEK_KEY", "sk-env-resolved");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","api_key_env":"MY_DEEPSEEK_KEY","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("env-key resolved config");
    assert_eq!(
        config.backends()[0].api_key,
        Some("sk-env-resolved".to_string())
    );
    std::fs::remove_file(&tmp).ok();
    std::env::remove_var("MY_DEEPSEEK_KEY");
    drop(guard);
}

#[test]
fn explicit_api_key_resolves_correctly() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    // Ensure UPSTREAM_URL is clean for this backends-mode test.
    std::env::remove_var("UPSTREAM_URL");
    // When api_key (not api_key_env) is present, it resolves directly.
    let tmp = std::env::temp_dir().join("test_explicit_key.json");
    std::env::set_var("OTHER_KEY", "should-not-be-used");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","api_key":"sk-explicit","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("explicit-key config");
    assert_eq!(
        config.backends()[0].api_key,
        Some("sk-explicit".to_string())
    );
    std::fs::remove_file(&tmp).ok();
    std::env::remove_var("OTHER_KEY");
}

#[test]
fn provider_defaults_to_llamacpp() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_default_provider.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("default provider config");
    assert_eq!(config.backends()[0].provider, ProviderKind::LlamaCpp);
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn headers_resolve_to_extra_headers() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_headers.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"],"headers":{"X-Custom":"value1","X-Other":"value2"}}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("headers config");
    let headers = &config.backends()[0].extra_headers;
    assert_eq!(
        headers,
        &[
            ("x-custom".to_string(), "value1".to_string()),
            ("x-other".to_string(), "value2".to_string()),
        ]
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backend_headers_reject_invalid_http_name() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_invalid_backend_header_name.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"],"headers":{"Bad Header":"value"}}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };

    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("invalid HTTP header name"),
        "{err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backend_headers_reject_invalid_http_value() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_invalid_backend_header_value.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"],"headers":{"X-Test":"first\r\nInjected: yes"}}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };

    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("invalid HTTP header value"),
        "{err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backend_headers_reject_case_insensitive_duplicates() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_duplicate_backend_headers.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"],"headers":{"X-Token":"one","x-token":"two"}}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };

    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string()
            .contains("duplicate HTTP header name (case-insensitive)"),
        "{err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backend_authorization_header_rejects_api_key() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_backend_authorization_and_api_key.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"],"api_key":"secret","headers":{"Authorization":"Bearer explicit"}}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };

    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string()
            .contains("authorization header cannot be combined with api_key or api_key_env"),
        "{err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backends_default_gpu_watts_is_zero() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_default_gpu.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("default gpu_watts config");
    assert_eq!(config.backends()[0].gpu_watts, 0.0);
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backends_upstream_trailing_slash_trimmed() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_trim.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"http://x:8080///","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("trimmed upstream config");
    assert_eq!(config.backends()[0].upstream, "http://x:8080");
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn backends_mode_rejects_malformed_upstream_with_backend_context() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let original = std::env::var("UPSTREAM_URL");
    std::env::remove_var("UPSTREAM_URL");
    let tmp = std::env::temp_dir().join("test_malformed_backend_upstream.json");
    write_json(
        &tmp,
        r#"{"backends":[{"upstream":"not a URL","models":["m"]}]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_string_lossy().into_owned()),
        ..crate::cli::Cli::default()
    };

    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("backend[0]") && err.to_string().contains("absolute URL"),
        "backend URL validation should use the shared validator: {err}"
    );

    std::fs::remove_file(&tmp).ok();
    match original {
        Ok(value) => std::env::set_var("UPSTREAM_URL", value),
        Err(_) => std::env::remove_var("UPSTREAM_URL"),
    }
}

#[test]
fn backends_multi_entry_with_one_default() {
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let tmp = std::env::temp_dir().join("test_multi.json");
    write_json(
        &tmp,
        r#"{"backends":[
            {"upstream":"http://a","models":["a"]},
            {"upstream":"http://b","models_poll_secs":60},
            {"upstream":"http://c","models":["c"]}
        ]}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("multi backend config");
    assert_eq!(config.backends().len(), 3);
    assert_eq!(config.backends()[1].models_poll_secs, Some(60));
    assert_eq!(config.backends()[2].models_poll_secs, None);
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn require_api_key_default_true() {
    // When the config file does not specify require_api_key, it defaults to
    // true in both flat and backends mode.
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();

    // Flat mode.
    let tmp = std::env::temp_dir().join("test_req_api_key_flat_default.json");
    write_json(&tmp, r#"{"host":"127.0.0.1","port":3000}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("flat mode config");
    assert!(
        config.require_api_key,
        "require_api_key should default to true"
    );
    std::fs::remove_file(&tmp).ok();

    // Backends mode.
    let tmp2 = std::env::temp_dir().join("test_req_api_key_backends_default.json");
    write_json(
        &tmp2,
        r#"{"backends":[{"upstream":"http://x","models":["m"]}]}"#,
    );
    let cli2 = crate::cli::Cli {
        config: Some(tmp2.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config2 = Config::resolve(cli2).expect("backends mode config");
    assert!(
        config2.require_api_key,
        "require_api_key should default to true in backends mode"
    );
    std::fs::remove_file(&tmp2).ok();
}

#[test]
fn require_api_key_from_file() {
    // When the config file sets require_api_key: true, the resolved Config
    // honours it in both flat and backends mode.
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();

    // Flat mode.
    let tmp = std::env::temp_dir().join("test_req_api_key_flat_true.json");
    write_json(
        &tmp,
        r#"{"host":"127.0.0.1","port":3000,"require_api_key":true}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("flat mode config with require_api_key");
    assert!(
        config.require_api_key,
        "require_api_key should be true from file"
    );
    std::fs::remove_file(&tmp).ok();

    // Backends mode.
    let tmp2 = std::env::temp_dir().join("test_req_api_key_backends_true.json");
    write_json(
        &tmp2,
        r#"{"backends":[{"upstream":"http://x","models":["m"]}],"require_api_key":true}"#,
    );
    let cli2 = crate::cli::Cli {
        config: Some(tmp2.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config2 = Config::resolve(cli2).expect("backends mode config with require_api_key");
    assert!(
        config2.require_api_key,
        "require_api_key should be true from file in backends mode"
    );
    std::fs::remove_file(&tmp2).ok();

    // Also verify false is honoured explicitly.
    let tmp3 = std::env::temp_dir().join("test_req_api_key_false.json");
    write_json(&tmp3, r#"{"require_api_key":false}"#);
    let cli3 = crate::cli::Cli {
        config: Some(tmp3.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config3 = Config::resolve(cli3).expect("config with require_api_key: false");
    assert!(
        !config3.require_api_key,
        "require_api_key should be false from file"
    );
    std::fs::remove_file(&tmp3).ok();
}

#[test]
fn dashboard_resolve_none_returns_default() {
    // resolve_dashboard(None) should return a DashboardConfig equal to
    // DashboardConfig::default().
    let dc = resolve_dashboard(None).expect("default dashboard");
    let default = DashboardConfig::default();
    assert_eq!(dc.cards, default.cards);
    assert_eq!(dc.graphs, default.graphs);
    assert_eq!(dc.max_rows, None);
    assert!(dc.trusted_proxies.is_empty());
}

#[test]
fn dashboard_resolve_valid_cards_graphs_and_max_rows() {
    // A dashboard section with valid cards, graphs, and a positive max_rows
    // resolves to exactly those values.
    let dash = crate::config::DashboardConfigFile {
        cards: Some(vec!["cost".to_string(), "tokens".to_string()]),
        graphs: Some(vec!["over_time".to_string()]),
        max_rows: Some(10),
        trusted_proxies: Some(vec!["127.0.0.1".to_string(), "::1".to_string()]),
    };
    let cfg = ConfigFile {
        dashboard: Some(dash),
        ..ConfigFile::default()
    };
    let dc = resolve_dashboard(Some(&cfg)).expect("valid dashboard");
    assert_eq!(dc.cards, vec!["cost", "tokens"]);
    assert_eq!(dc.graphs, vec!["over_time"]);
    assert_eq!(dc.max_rows, Some(10));
    assert_eq!(
        dc.trusted_proxies,
        vec![
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap(),
            "::1".parse::<std::net::IpAddr>().unwrap()
        ]
    );
}

#[test]
fn dashboard_rejects_invalid_trusted_proxy_ip() {
    let dash = crate::config::DashboardConfigFile {
        cards: None,
        graphs: None,
        max_rows: None,
        trusted_proxies: Some(vec!["10.0.0.1/24".to_string()]),
    };
    let cfg = ConfigFile {
        dashboard: Some(dash),
        ..ConfigFile::default()
    };

    let error = resolve_dashboard(Some(&cfg)).unwrap_err();
    assert!(error.to_string().contains("invalid IP address"));
}

#[test]
fn dashboard_unknown_card_errors() {
    // An unknown card identifier produces an error mentioning that card.
    let dash = crate::config::DashboardConfigFile {
        cards: Some(vec!["bogus_card".to_string()]),
        graphs: None,
        max_rows: None,
        trusted_proxies: None,
    };
    let cfg = ConfigFile {
        dashboard: Some(dash),
        ..ConfigFile::default()
    };
    let err = resolve_dashboard(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("bogus_card"),
        "error should mention the unknown card: {err}"
    );
}

#[test]
fn dashboard_unknown_graph_errors() {
    // An unknown graph identifier produces an error mentioning that graph.
    let dash = crate::config::DashboardConfigFile {
        cards: None,
        graphs: Some(vec!["unknown_graph".to_string()]),
        max_rows: None,
        trusted_proxies: None,
    };
    let cfg = ConfigFile {
        dashboard: Some(dash),
        ..ConfigFile::default()
    };
    let err = resolve_dashboard(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("unknown_graph"),
        "error should mention the unknown graph: {err}"
    );
}

#[test]
fn dashboard_max_rows_zero_errors() {
    // A max_rows of zero produces an error mentioning max_rows.
    let dash = crate::config::DashboardConfigFile {
        cards: None,
        graphs: None,
        max_rows: Some(0),
        trusted_proxies: None,
    };
    let cfg = ConfigFile {
        dashboard: Some(dash),
        ..ConfigFile::default()
    };
    let err = resolve_dashboard(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("max_rows"),
        "error should mention max_rows: {err}"
    );
}

#[test]
fn dashboard_duplicate_card_errors() {
    // A cards list with a repeated identifier produces an error mentioning
    // duplicate.
    let dash = crate::config::DashboardConfigFile {
        cards: Some(vec!["cost".to_string(), "cost".to_string()]),
        graphs: None,
        max_rows: None,
        trusted_proxies: None,
    };
    let cfg = ConfigFile {
        dashboard: Some(dash),
        ..ConfigFile::default()
    };
    let err = resolve_dashboard(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("duplicate"),
        "error should mention duplicate: {err}"
    );
}

#[test]
fn load_balancing_default_when_absent() {
    // When the config file has no load_balancing section, the resolved
    // Config::load_balancing should equal Default::default() (LeastLoaded, 1.25).
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();

    let tmp = std::env::temp_dir().join("test_lb_absent.json");
    write_json(&tmp, r#"{"host":"127.0.0.1","port":3000}"#);
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config = Config::resolve(cli).expect("flat mode config");
    assert_eq!(
        config.load_balancing,
        LoadBalancing::default(),
        "load_balancing should be default when section is absent"
    );
    assert_eq!(config.load_balancing.strategy, LbStrategy::LeastLoaded);
    assert!((config.load_balancing.overload_factor - 1.25).abs() < f64::EPSILON);
    assert_eq!(
        config.retention_days, 90,
        "retention_days should default to 90"
    );
    std::fs::remove_file(&tmp).ok();

    // Also verify for backends mode.
    let tmp2 = std::env::temp_dir().join("test_lb_absent_backends.json");
    write_json(
        &tmp2,
        r#"{"backends":[{"upstream":"http://x","models":["m"]}]}"#,
    );
    let cli2 = crate::cli::Cli {
        config: Some(tmp2.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let config2 = Config::resolve(cli2).expect("backends mode config");
    assert_eq!(
        config2.load_balancing,
        LoadBalancing::default(),
        "load_balancing should be default when section is absent (backends mode)"
    );
    std::fs::remove_file(&tmp2).ok();
}

#[test]
fn load_balancing_overload_factor_below_one_errors() {
    // An overload_factor < 1.0 must produce a validation error.
    let _guard = CONFIG_TEST_MUTEX.lock().unwrap();

    let tmp = std::env::temp_dir().join("test_lb_bad_factor.json");
    write_json(
        &tmp,
        r#"{"host":"127.0.0.1","load_balancing":{"overload_factor":0.5}}"#,
    );
    let cli = crate::cli::Cli {
        config: Some(tmp.to_str().unwrap().to_string()),
        ..crate::cli::Cli::default()
    };
    let err = Config::resolve(cli).unwrap_err();
    assert!(
        err.to_string().contains("overload_factor"),
        "error should mention overload_factor: {err}"
    );
    assert!(
        err.to_string().contains("1.0"),
        "error should mention >= 1.0: {err}"
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn load_balancing_resolve_function_defaults() {
    // resolve_load_balancing with None returns the default.
    let lb = resolve_load_balancing(None).expect("default lb");
    assert_eq!(lb, LoadBalancing::default());
}

#[test]
fn load_balancing_resolve_function_non_finite_errors() {
    // f64::NAN and f64::INFINITY must produce an error.
    let lb_file = crate::config::LoadBalancingFile {
        strategy: None,
        overload_factor: Some(f64::NAN),
    };
    let cfg = ConfigFile {
        load_balancing: Some(lb_file),
        ..ConfigFile::default()
    };
    let err = resolve_load_balancing(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("overload_factor"),
        "error should mention overload_factor: {err}"
    );

    let lb_file = crate::config::LoadBalancingFile {
        strategy: None,
        overload_factor: Some(f64::INFINITY),
    };
    let cfg = ConfigFile {
        load_balancing: Some(lb_file),
        ..ConfigFile::default()
    };
    let err = resolve_load_balancing(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("overload_factor"),
        "error should mention overload_factor: {err}"
    );
}

// ── metrics token resolution ────────────────────────────────────────────────

#[test]
fn metrics_token_env_beats_file() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = env::var("METRICS_TOKEN");
    env::remove_var("METRICS_TOKEN");

    let cfg = ConfigFile {
        metrics_token: Some("file-token-0123456789".to_string()),
        ..ConfigFile::default()
    };

    // 1. No env → file value.
    assert_eq!(
        resolve_metrics_token(Some(&cfg)).unwrap().as_deref(),
        Some("file-token-0123456789")
    );

    // 2. Env beats file.
    env::set_var("METRICS_TOKEN", "env-token-0123456789");
    assert_eq!(
        resolve_metrics_token(Some(&cfg)).unwrap().as_deref(),
        Some("env-token-0123456789")
    );

    // 3. Nothing set → disabled.
    env::remove_var("METRICS_TOKEN");
    assert_eq!(resolve_metrics_token(None).unwrap(), None);

    match orig {
        Ok(v) => env::set_var("METRICS_TOKEN", v),
        Err(_) => env::remove_var("METRICS_TOKEN"),
    }
    drop(guard);
}

#[test]
fn metrics_token_too_short_rejected() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig = env::var("METRICS_TOKEN");
    env::remove_var("METRICS_TOKEN");

    let cfg = ConfigFile {
        metrics_token: Some("short".to_string()),
        ..ConfigFile::default()
    };
    let err = resolve_metrics_token(Some(&cfg)).unwrap_err();
    assert!(
        err.to_string().contains("metrics_token"),
        "error should mention metrics_token: {err}"
    );

    match orig {
        Ok(v) => env::set_var("METRICS_TOKEN", v),
        Err(_) => env::remove_var("METRICS_TOKEN"),
    }
    drop(guard);
}

// ── TLS resolution ──────────────────────────────────────────────────────────

#[test]
fn tls_defaults_enabled_with_paths() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig_tls_enabled = env::var("TLS_ENABLED");
    let orig_tls_cert = env::var("TLS_CERT_PATH");
    let orig_tls_key = env::var("TLS_KEY_PATH");
    env::remove_var("TLS_ENABLED");
    env::remove_var("TLS_CERT_PATH");
    env::remove_var("TLS_KEY_PATH");

    let result = resolve_tls(None, None, None, None).expect("default tls resolves");
    assert!(result.enabled);
    assert_eq!(result.cert_path, "tls/cert.pem");
    assert_eq!(result.key_path, "tls/key.pem");
    assert_eq!(result.hostnames, vec!["localhost"]);

    match orig_tls_enabled {
        Ok(v) => env::set_var("TLS_ENABLED", v),
        Err(_) => env::remove_var("TLS_ENABLED"),
    }
    match orig_tls_cert {
        Ok(v) => env::set_var("TLS_CERT_PATH", v),
        Err(_) => env::remove_var("TLS_CERT_PATH"),
    }
    match orig_tls_key {
        Ok(v) => env::set_var("TLS_KEY_PATH", v),
        Err(_) => env::remove_var("TLS_KEY_PATH"),
    }
    drop(guard);
}

#[test]
fn tls_cli_disables() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig_tls_enabled = env::var("TLS_ENABLED");
    let orig_tls_cert = env::var("TLS_CERT_PATH");
    let orig_tls_key = env::var("TLS_KEY_PATH");
    env::remove_var("TLS_ENABLED");
    env::remove_var("TLS_CERT_PATH");
    env::remove_var("TLS_KEY_PATH");

    let result = resolve_tls(Some(false), None, None, None).expect("cli disables tls");
    assert!(!result.enabled);

    match orig_tls_enabled {
        Ok(v) => env::set_var("TLS_ENABLED", v),
        Err(_) => env::remove_var("TLS_ENABLED"),
    }
    match orig_tls_cert {
        Ok(v) => env::set_var("TLS_CERT_PATH", v),
        Err(_) => env::remove_var("TLS_CERT_PATH"),
    }
    match orig_tls_key {
        Ok(v) => env::set_var("TLS_KEY_PATH", v),
        Err(_) => env::remove_var("TLS_KEY_PATH"),
    }
    drop(guard);
}

#[test]
fn tls_config_file_values() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig_tls_enabled = env::var("TLS_ENABLED");
    let orig_tls_cert = env::var("TLS_CERT_PATH");
    let orig_tls_key = env::var("TLS_KEY_PATH");
    env::remove_var("TLS_ENABLED");
    env::remove_var("TLS_CERT_PATH");
    env::remove_var("TLS_KEY_PATH");

    let tls_file = TlsConfigFile {
        enabled: Some(true),
        cert_path: Some("certs/server.pem".to_string()),
        key_path: Some("certs/server.key".to_string()),
        hostnames: Some(vec!["proxy.internal".to_string(), "10.0.0.5".to_string()]),
    };
    let result = resolve_tls(None, None, None, Some(&tls_file)).expect("file tls resolves");
    assert_eq!(result.cert_path, "certs/server.pem");
    assert_eq!(result.key_path, "certs/server.key");
    assert_eq!(result.hostnames, vec!["proxy.internal", "10.0.0.5"]);

    match orig_tls_enabled {
        Ok(v) => env::set_var("TLS_ENABLED", v),
        Err(_) => env::remove_var("TLS_ENABLED"),
    }
    match orig_tls_cert {
        Ok(v) => env::set_var("TLS_CERT_PATH", v),
        Err(_) => env::remove_var("TLS_CERT_PATH"),
    }
    match orig_tls_key {
        Ok(v) => env::set_var("TLS_KEY_PATH", v),
        Err(_) => env::remove_var("TLS_KEY_PATH"),
    }
    drop(guard);
}

#[test]
fn tls_cli_overrides_file_enabled() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig_tls_enabled = env::var("TLS_ENABLED");
    let orig_tls_cert = env::var("TLS_CERT_PATH");
    let orig_tls_key = env::var("TLS_KEY_PATH");
    env::remove_var("TLS_ENABLED");
    env::remove_var("TLS_CERT_PATH");
    env::remove_var("TLS_KEY_PATH");

    let tls_file = TlsConfigFile {
        enabled: Some(true),
        cert_path: None,
        key_path: None,
        hostnames: None,
    };
    let result = resolve_tls(Some(false), None, None, Some(&tls_file)).expect("cli overrides");
    assert!(!result.enabled);

    match orig_tls_enabled {
        Ok(v) => env::set_var("TLS_ENABLED", v),
        Err(_) => env::remove_var("TLS_ENABLED"),
    }
    match orig_tls_cert {
        Ok(v) => env::set_var("TLS_CERT_PATH", v),
        Err(_) => env::remove_var("TLS_CERT_PATH"),
    }
    match orig_tls_key {
        Ok(v) => env::set_var("TLS_KEY_PATH", v),
        Err(_) => env::remove_var("TLS_KEY_PATH"),
    }
    drop(guard);
}

#[test]
fn tls_empty_cert_path_errors() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig_tls_enabled = env::var("TLS_ENABLED");
    let orig_tls_cert = env::var("TLS_CERT_PATH");
    let orig_tls_key = env::var("TLS_KEY_PATH");
    env::remove_var("TLS_ENABLED");
    env::remove_var("TLS_CERT_PATH");
    env::remove_var("TLS_KEY_PATH");

    let tls_file = TlsConfigFile {
        enabled: Some(true),
        cert_path: Some("".to_string()),
        key_path: None,
        hostnames: None,
    };
    let result = resolve_tls(None, None, None, Some(&tls_file));
    assert!(result.is_err());

    match orig_tls_enabled {
        Ok(v) => env::set_var("TLS_ENABLED", v),
        Err(_) => env::remove_var("TLS_ENABLED"),
    }
    match orig_tls_cert {
        Ok(v) => env::set_var("TLS_CERT_PATH", v),
        Err(_) => env::remove_var("TLS_CERT_PATH"),
    }
    match orig_tls_key {
        Ok(v) => env::set_var("TLS_KEY_PATH", v),
        Err(_) => env::remove_var("TLS_KEY_PATH"),
    }
    drop(guard);
}

#[test]
fn tls_rejects_normalized_same_missing_destination() {
    let dir = std::env::temp_dir().join(format!(
        "twl-config-tls-path-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).expect("create test directory");
    let destination = dir.join("shared.pem");
    let alias = nested.join("..").join("shared.pem");

    let err = resolve_tls(
        Some(true),
        Some(destination.to_str().unwrap()),
        Some(alias.to_str().unwrap()),
        None,
    )
    .expect_err("same missing TLS destination must be rejected");

    assert!(err.to_string().contains("different files"));
    assert!(!destination.exists(), "validation must not create the file");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn tls_rejects_existing_symlink_alias() {
    use std::os::unix::fs::symlink;

    let dir = std::env::temp_dir().join(format!(
        "twl-config-tls-alias-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    let destination = dir.join("shared.pem");
    let alias = dir.join("alias.pem");
    std::fs::write(&destination, b"existing TLS material").expect("create existing file");
    symlink(&destination, &alias).expect("create TLS path alias");

    let err = resolve_tls(
        Some(true),
        Some(destination.to_str().unwrap()),
        Some(alias.to_str().unwrap()),
        None,
    )
    .expect_err("canonical aliases must be rejected");

    assert!(err.to_string().contains("different files"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tls_env_enabled_unparseable_errors() {
    let guard = CONFIG_TEST_MUTEX.lock().unwrap();
    let orig_tls_enabled = env::var("TLS_ENABLED");
    let orig_tls_cert = env::var("TLS_CERT_PATH");
    let orig_tls_key = env::var("TLS_KEY_PATH");
    env::remove_var("TLS_ENABLED");
    env::remove_var("TLS_CERT_PATH");
    env::remove_var("TLS_KEY_PATH");

    env::set_var("TLS_ENABLED", "notabool");
    let result = resolve_tls(None, None, None, None);
    assert!(result.is_err());

    match orig_tls_enabled {
        Ok(v) => env::set_var("TLS_ENABLED", v),
        Err(_) => env::remove_var("TLS_ENABLED"),
    }
    match orig_tls_cert {
        Ok(v) => env::set_var("TLS_CERT_PATH", v),
        Err(_) => env::remove_var("TLS_CERT_PATH"),
    }
    match orig_tls_key {
        Ok(v) => env::set_var("TLS_KEY_PATH", v),
        Err(_) => env::remove_var("TLS_KEY_PATH"),
    }
    drop(guard);
}

// Per-backend model_types overrides

#[test]
fn backend_model_types_parses_into_resolved_config() {
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["chat-model", "embed-model"],
        "model_types": {"embed-model": "embedding"}
    }"#;
    let file: BackendConfigFile = serde_json::from_str(json).unwrap();
    let backends = resolve_backends(&[file]).unwrap();
    assert_eq!(backends.len(), 1);
    assert_eq!(
        backends[0].model_types.get("embed-model"),
        Some(&twl_provider::ModelKind::Embedding)
    );
    assert_eq!(backends[0].model_types.len(), 1);
}

#[test]
fn backend_model_types_absent_is_empty() {
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["chat-model"]
    }"#;
    let file: BackendConfigFile = serde_json::from_str(json).unwrap();
    let backends = resolve_backends(&[file]).unwrap();
    assert!(
        backends[0].model_types.is_empty(),
        "absent model_types must resolve to an empty map"
    );
}

#[test]
fn backend_model_types_invalid_kind_errors() {
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["m"],
        "model_types": {"m": "embeddingx"}
    }"#;
    let result = serde_json::from_str::<BackendConfigFile>(json);
    assert!(
        result.is_err(),
        "an unknown model kind must fail to deserialize"
    );
}

#[test]
fn backend_model_filter_resolves_through_to_backend_config() {
    // A model_filter allowlist resolves and is carried through to the
    // resolved BackendConfig.
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["a", "b", "c"],
        "model_filter": ["a", "b"]
    }"#;
    let file: BackendConfigFile = serde_json::from_str(json).unwrap();
    let backends = resolve_backends(&[file]).unwrap();
    assert_eq!(backends.len(), 1);
    assert_eq!(
        backends[0].model_filter,
        Some(vec!["a".to_string(), "b".to_string()])
    );
}

#[test]
fn backend_model_filter_empty_array_errors() {
    // An empty model_filter array is rejected with a specific message.
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["a", "b"],
        "model_filter": []
    }"#;
    let file: BackendConfigFile = serde_json::from_str(json).unwrap();
    let err = resolve_backends(&[file]).unwrap_err();
    assert!(
        err.to_string().contains("model_filter must not be empty"),
        "error should mention empty model_filter: {err}"
    );
}

#[test]
fn backend_model_filter_duplicate_entry_errors() {
    // A repeated model_filter entry is rejected.
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["a", "b"],
        "model_filter": ["a", "a"]
    }"#;
    let file: BackendConfigFile = serde_json::from_str(json).unwrap();
    let err = resolve_backends(&[file]).unwrap_err();
    assert!(
        err.to_string()
            .contains("duplicate model_filter entry: 'a'"),
        "error should mention duplicate model_filter entry: {err}"
    );
}

#[test]
fn backend_model_filter_empty_string_entry_errors() {
    // An empty-string model_filter entry is rejected.
    let json = r#"{
        "upstream": "http://localhost:8080",
        "provider": "llamacpp",
        "models": ["a", "b"],
        "model_filter": ["a", ""]
    }"#;
    let file: BackendConfigFile = serde_json::from_str(json).unwrap();
    let err = resolve_backends(&[file]).unwrap_err();
    assert!(
        err.to_string()
            .contains("model_filter entries must not be empty"),
        "error should mention empty model_filter entry: {err}"
    );
}
