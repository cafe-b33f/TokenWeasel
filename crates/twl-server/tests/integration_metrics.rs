//! Integration tests for the token-authenticated Prometheus /metrics endpoint.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use tokio::net::TcpListener;

use twl_auth::sha256_hex;
use twl_config::{BackendConfig, Config, ProviderKind};
use twl_server::run_with_listener;

/// Metrics token used by the tests (16+ characters).
const METRICS_TOKEN: &str = "test-metrics-token-0123456789";

/// Generate a unique temp database path for a single test run.
fn temp_db_path() -> PathBuf {
    // A process-wide counter guarantees uniqueness even when the clock is too
    // coarse to distinguish two tests that start in the same tick (e.g. on
    // Windows), which would otherwise make parallel tests share a SQLite file.
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "twl-int-metrics-{}-{seq}-{nonce}",
        std::process::id()
    ))
}

/// Remove the test database file.
fn cleanup_db(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Build a minimal `Config` with an optional metrics token.
fn test_proxy_config(db_path: &Path, metrics_token: Option<&str>) -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        upstream: "http://127.0.0.1:1".to_string(),
        db_path: db_path.to_str().unwrap().to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 4,
        dashboard_auth: None,
        gpu_watts: 0.0,
        public_usage: false,
        require_api_key: false,
        retention_days: 90,
        backends: vec![BackendConfig {
            upstream: "http://127.0.0.1:1".to_string(),
            provider: ProviderKind::LlamaCpp,
            api_key: None,
            extra_headers: Vec::new(),
            models: Vec::new(),
            models_poll_secs: Some(30),
            model_types: Default::default(),
            model_filter: None,
            gpu_watts: 0.0,
        }],
        flat_passthrough: true,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: metrics_token.map(|t| t.to_string()),
    }
}

/// Create an admin user with an `twl_` API key in the database at `db_path`.
fn create_admin_user_and_key(db_path: &Path) -> String {
    let database = twl_store::Database::open(db_path.to_str().unwrap()).expect("open test db");
    let identity = twl_store::IdentityStore::new(&database).expect("create identity store");
    let user_id = identity
        .create_user("metricsadmin", "dummy_hash", true)
        .expect("create admin user");

    let raw_key = "twl_metrics_admin_key_abcdefghij".to_string();
    let key_hash = sha256_hex(&raw_key);
    let prefix = raw_key.chars().take(10).collect::<String>();
    identity
        .insert_api_key(user_id, None, &key_hash, &prefix)
        .expect("insert api key");
    raw_key
}

/// Start the proxy on a random port and return its base URL and handle.
async fn start_proxy(config: Config) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let handle = tokio::spawn(async move {
        let _ = run_with_listener(listener, config).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (proxy_url, handle)
}

#[tokio::test]
async fn test_metrics_disabled_when_token_unconfigured() {
    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) = start_proxy(test_proxy_config(&db_path, None)).await;

    // Even a correct-looking bearer token gets 404: the endpoint is disabled.
    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/metrics"))
        .header("Authorization", format!("Bearer {METRICS_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_metrics_missing_or_wrong_token_gets_401() {
    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) =
        start_proxy(test_proxy_config(&db_path, Some(METRICS_TOKEN))).await;
    let client = reqwest::Client::new();

    // No Authorization header → 401.
    let resp = client
        .get(format!("{proxy_url}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().get("www-authenticate").is_none(),
        "401 must not trigger a browser auth dialog"
    );

    // Wrong bearer token → 401.
    let resp = client
        .get(format!("{proxy_url}/metrics"))
        .header("Authorization", "Bearer wrong-token-0123456789")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_metrics_user_api_key_does_not_grant_access() {
    let db_path = temp_db_path();
    let admin_key = create_admin_user_and_key(&db_path);
    let (proxy_url, proxy_handle) =
        start_proxy(test_proxy_config(&db_path, Some(METRICS_TOKEN))).await;

    // An admin's twl_ API key is a user credential; it must not work here.
    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/metrics"))
        .header("Authorization", format!("Bearer {admin_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_metrics_correct_token_gets_prometheus_body() {
    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) =
        start_proxy(test_proxy_config(&db_path, Some(METRICS_TOKEN))).await;

    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/metrics"))
        .header("Authorization", format!("Bearer {METRICS_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .expect("content-type header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/plain"),
        "content-type should be text/plain: {content_type}"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("twl_build_info"),
        "body should contain twl_build_info: {body}"
    );
    assert!(
        body.contains("twl_energy_kwh_total"),
        "body should contain twl_energy_kwh_total: {body}"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}
