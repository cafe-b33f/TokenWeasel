//! Integration tests for the reverse proxy.
//!
//! Spins up a local mock upstream and the proxy to verify end-to-end
//! behaviour: redirect passthrough, header filtering, SSE streaming,
//! upstream failure (502 Bad Gateway + energy recording), client disconnect
//! (proxy remains responsive), energy recording on successful proxy,
//! and dashboard API energy data (with and without gpu_watts).
//!
//! Test modules:
//!
//! - `body` - Request-body inspection and `stream_options` injection.
//! - `headers` - Hop-by-hop header filtering (static + dynamic).
//! - `stream_classify` - Content-Type-based stream classification (SSE, NDJSON).
//! - `usage` - Token usage extraction from multiple upstream response shapes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    Router,
};
use bytes::Bytes;
use futures_util::stream;
use tokio::net::TcpListener;

use twl_config::{BackendConfig, Config, ProviderKind};
use twl_server::run_with_listener;

/// Temporary database path for integration tests.
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
    std::env::temp_dir().join(format!("twl-int-{}-{seq}-{nonce}", std::process::id()))
}

/// Clean up a temporary database file.
fn cleanup_db(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Poll until `table` holds exactly `expected` rows, returning an open
/// connection for follow-up assertions. Accounting records are persisted on a
/// dedicated writer thread after the response has already been sent (and
/// aborting the proxy task does not drain that thread), so tests must wait
/// for the row to land rather than race the writer.
async fn wait_for_rows(db_path: &Path, table: &str, expected: i64) -> rusqlite::Connection {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    for _ in 0..100 {
        if let Ok(conn) = rusqlite::Connection::open(db_path) {
            conn.busy_timeout(Duration::from_secs(5))
                .expect("busy_timeout");
            if conn.query_row(&sql, [], |row| row.get::<_, i64>(0)) == Ok(expected) {
                return conn;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("expected {expected} rows in {table} within the polling timeout");
}

/// Start a mock upstream server that calls the given handler for each request.
/// Returns the base URL and a handle to the server task.
async fn start_mock_upstream(
    handler: impl Fn(&Uri, HeaderMap, Body) -> Response + Send + Sync + Clone + 'static,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        let router = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Body| {
            let handler = handler.clone();
            async move { handler(&uri, headers, body) }
        });
        axum::serve(listener, router).await.expect("mock upstream");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    (url, handle)
}

/// Build the proxy config for full-proxy integration tests (no dashboard auth).
fn test_proxy_config(upstream: &str, db_path: &Path, gpu_watts: f64) -> Config {
    test_proxy_config_inner(upstream, db_path, gpu_watts, None)
}

/// Build the proxy config with dashboard credentials.
fn test_proxy_config_with_auth(upstream: &str, db_path: &Path, gpu_watts: f64) -> Config {
    test_proxy_config_inner(
        upstream,
        db_path,
        gpu_watts,
        Some(("admin".to_string(), "testpass123".to_string())),
    )
}

/// Log in through the browser-session endpoint and return a Cookie header value.
async fn login_cookie(client: &reqwest::Client, proxy_url: &str) -> String {
    let resp = client
        .post(format!("{proxy_url}/api/login"))
        .json(&serde_json::json!({
            "username": "admin",
            "password": "testpass123",
        }))
        .send()
        .await
        .expect("login request");
    assert_eq!(resp.status(), StatusCode::OK);
    resp.headers()
        .get("set-cookie")
        .expect("login sets session cookie")
        .to_str()
        .expect("set-cookie is valid")
        .split(';')
        .next()
        .expect("set-cookie has name/value")
        .to_string()
}

/// Inner helper that all config builders delegate to.
fn test_proxy_config_inner(
    upstream: &str,
    db_path: &Path,
    gpu_watts: f64,
    dashboard_auth: Option<(String, String)>,
) -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        upstream: upstream.to_string(),
        db_path: db_path.to_str().unwrap().to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 4,
        dashboard_auth,
        gpu_watts,
        public_usage: false,
        require_api_key: false,
        backends: vec![BackendConfig {
            upstream: upstream.to_string(),
            provider: ProviderKind::LlamaCpp,
            api_key: None,
            extra_headers: Vec::new(),
            models: Vec::new(),
            models_poll_secs: Some(30),
            model_types: Default::default(),
            model_filter: None,
            gpu_watts,
        }],
        flat_passthrough: true,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        retention_days: 90,
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    }
}

/// Helper: start the proxy on a random port.
/// Returns the proxy base URL and the proxy server handle.
async fn start_proxy(
    upstream: &str,
    db_path: &Path,
    gpu_watts: f64,
) -> (
    String,
    tokio::task::JoinHandle<Result<(), twl_server::ServerError>>,
) {
    let config = test_proxy_config(upstream, db_path, gpu_watts);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");

    let handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;
    (url, handle)
}

#[tokio::test]
async fn redirect_passthrough() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _, _| {
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", "http://example.com/target")
            .body(Body::from("Moved"))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) = start_proxy(&upstream_url, &db_path, 0.0).await;

    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers().get("Location").unwrap().to_str().unwrap(),
        "http://example.com/target"
    );
    assert_eq!(resp.text().await.unwrap(), "Moved");

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn header_filtering() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, headers, _body| {
        // Verify the proxy did NOT forward the dynamic hop-by-hop header
        let has_x_custom = headers.iter().any(|(k, _)| k.as_str() == "x-custom");
        assert!(!has_x_custom, "proxy should not forward x-custom");

        // Verify the proxy forwarded normal headers
        let has_host = headers.iter().any(|(k, _)| k.as_str() == "host");
        assert!(has_host, "proxy should forward host");

        // Return a response with hop-by-hop headers marked in Connection
        Response::builder()
            .status(StatusCode::OK)
            .header("Connection", "x-custom")
            .header("x-custom", "should-not-forward")
            .header("x-forwarded", "ok")
            .body(Body::empty())
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) = start_proxy(&upstream_url, &db_path, 0.0).await;

    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .header("Connection", "x-custom")
        .header("x-custom", "forward-me")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();

    // Verify the proxy did NOT forward x-custom to the client
    assert!(
        resp.headers().get("x-custom").is_none(),
        "proxy should not forward x-custom back to client"
    );
    // x-forwarded should be forwarded
    assert_eq!(
        resp.headers().get("x-forwarded").unwrap().to_str().unwrap(),
        "ok"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn sse_streaming() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _, _| {
        let sse_data =
            "data: {\"model\":\"m\",\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n";
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) = start_proxy(&upstream_url, &db_path, 0.0).await;

    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Content-Type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );

    let body = resp.text().await.unwrap();
    assert!(body.contains("usage"), "SSE body should contain usage");

    wait_for_rows(&db_path, "token_usage", 1).await;

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn upstream_failure() {
    // When the upstream is unreachable the proxy returns 502 Bad Gateway.
    // The test must NOT accept a raw transport error - only a proper 502
    // proves the proxy handled the failure correctly.
    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) = start_proxy("http://127.0.0.1:1/", &db_path, 100.0).await;

    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .expect("reqwest must receive a response from the proxy");

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    // When gpu_watts > 0 exactly one energy row is recorded for the failed
    // transport, with elapsed_secs measuring from start to the failure.
    let conn = wait_for_rows(&db_path, "energy", 1).await;
    let gpu_watts: f64 = conn
        .query_row("SELECT gpu_watts FROM energy LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(gpu_watts, 100.0);
    let elapsed: f64 = conn
        .query_row("SELECT elapsed_secs FROM energy LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        elapsed >= 0.0,
        "elapsed_secs ({elapsed}s) should be non-negative"
    );

    // token_usage should remain empty - no response body to parse.
    let usage_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM token_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(usage_count, 0);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn client_disconnect() {
    // Start a slow upstream that streams data slowly.
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _, _| {
        let stream = stream::unfold(0, |state| async move {
            if state < 10 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some((
                    Ok::<Bytes, reqwest::Error>(Bytes::from(
                        "data: {\"model\":\"m\"}\n".to_string(),
                    )),
                    state + 1,
                ))
            } else {
                None
            }
        });
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let config = test_proxy_config(&upstream_url, &db_path, 0.0);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().unwrap();
    let proxy_url = format!("http://{local_addr}");

    let proxy_handle = tokio::spawn(run_with_listener(listener, config));

    // Make a request and drop early (simulating disconnect)
    let client = reqwest::Client::new();
    let resp = client.get(format!("{proxy_url}/test")).send().await;
    if let Ok(mut resp) = resp {
        // Try to read one chunk. If it times out, the client disconnects.
        let chunk = tokio::time::timeout(Duration::from_millis(50), resp.chunk()).await;
        // chunk.is_err() means the read timed out (client disconnect)
        // Either way, we drop the response body
        drop(chunk);
    }

    // Verify the proxy is still responsive
    let health_resp = client.get(format!("{proxy_url}/health")).send().await;
    assert!(
        health_resp.is_ok(),
        "proxy should still be responsive after client disconnect"
    );

    // Abort the proxy task (it won't shut down on its own)
    proxy_handle.abort();
    // Give it a moment to actually finish
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(proxy_handle.is_finished());

    cleanup_db(&db_path);
}

#[tokio::test]
async fn energy_recording_on_successful_proxy() {
    // Verify that a successful proxied request records an energy entry
    // with elapsed_secs that includes the header-wait delay when gpu_watts > 0.
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _, _| {
        std::thread::sleep(Duration::from_millis(150));
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (proxy_url, proxy_handle) = start_proxy(&upstream_url, &db_path, 150.0).await;

    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":false}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "{\"content\":\"hello\"}");

    // Poll until the background writer has committed the energy row.
    let conn = wait_for_rows(&db_path, "energy", 1).await;

    let elapsed: f64 = conn
        .query_row("SELECT elapsed_secs FROM energy LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        elapsed >= 0.1,
        "elapsed_secs ({elapsed}s) should include header-wait delay (≥ 0.1s)"
    );

    let gpu_watts: f64 = conn
        .query_row("SELECT gpu_watts FROM energy LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(gpu_watts, 150.0);

    // Token usage should NOT be recorded (no usage block in response).
    let usage_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM token_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        usage_count, 0,
        "no token usage for a response without a usage block"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Verify that `/api/dashboard` returns energy_days as an array even when
/// gpu_watts=0, and includes historical data persisted before gpu_watts was
/// set to 0. An empty database yields an empty array and zero total.
#[tokio::test]
async fn dashboard_api_energy_with_gpu_watts_zero() {
    // Use the full router (management routes) so /api/dashboard is reachable.
    let db_path = temp_db_path();
    let config = test_proxy_config_with_auth("http://127.0.0.1:1/", &db_path, 0.0);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let proxy_url = format!("http://{addr}");

    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Insert an energy record directly so there is history to read.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        // Insert a record with a recent timestamp.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO energy (ts, elapsed_secs, gpu_watts) VALUES (?1, ?2, ?3)",
            rusqlite::params![now, 3600.0, 500.0], // 1h at 500W = 0.5 kWh
        )
        .unwrap();
        drop(conn);
    }

    // Call /api/dashboard - gpu_watts=0 but energy history exists.
    let client = reqwest::Client::new();
    let cookie = login_cookie(&client, &proxy_url).await;
    let resp = client
        .get(format!("{proxy_url}/api/dashboard?days=1"))
        .header("Cookie", cookie)
        .send()
        .await
        .expect("dashboard request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();

    // energy_days must be present (not omitted) and be an array.
    assert!(
        body.get("energy_days").is_some(),
        "energy_days must be present when gpu_watts=0"
    );
    let energy_days = body["energy_days"]
        .as_array()
        .expect("energy_days must be an array");
    assert!(
        !energy_days.is_empty(),
        "historical energy rows must be visible even with gpu_watts=0"
    );
    // total_kwh should be ~0.5
    let total_kwh = body["total_kwh"]
        .as_f64()
        .expect("total_kwh must be a number");
    assert!(
        (total_kwh - 0.5).abs() < 0.001,
        "total_kwh should be ~0.5, got {total_kwh}"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Verify that `/api/dashboard` returns an empty energy_days array and zero
/// total_kwh when the database is empty (no energy records at all).
#[tokio::test]
async fn dashboard_api_energy_empty_db() {
    let db_path = temp_db_path();
    let config = test_proxy_config_with_auth("http://127.0.0.1:1/", &db_path, 0.0);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let proxy_url = format!("http://{addr}");

    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let cookie = login_cookie(&client, &proxy_url).await;
    let resp = client
        .get(format!("{proxy_url}/api/dashboard?days=1"))
        .header("Cookie", cookie)
        .send()
        .await
        .expect("dashboard request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();

    let energy_days = body["energy_days"]
        .as_array()
        .expect("energy_days must be an array");
    assert!(
        energy_days.is_empty(),
        "energy_days should be empty when no records exist"
    );
    let total_kwh = body["total_kwh"]
        .as_f64()
        .expect("total_kwh must be a number");
    assert_eq!(
        total_kwh, 0.0,
        "total_kwh should be zero with no energy data"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Verify that `/api/dashboard` requires an authenticated session.
///
/// - Without credentials → 401.
/// - With wrong login credentials → 401.
/// - With a valid session cookie → 200.
#[tokio::test]
async fn dashboard_requires_auth() {
    let db_path = temp_db_path();
    let config = test_proxy_config_with_auth("http://127.0.0.1:1/", &db_path, 0.0);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let proxy_url = format!("http://{addr}");

    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // 1. No credentials → 401 without WWW-Authenticate to avoid the native
    //    browser auth dialog.
    let resp = client
        .get(format!("{proxy_url}/api/dashboard?days=1"))
        .send()
        .await
        .expect("dashboard request without auth");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().get("www-authenticate").is_none(),
        "no WWW-Authenticate for API paths"
    );

    // 2. Wrong login credentials → 401.
    let resp = client
        .post(format!("{proxy_url}/api/login"))
        .json(&serde_json::json!({
            "username": "admin",
            "password": "wrongpass",
        }))
        .send()
        .await
        .expect("login request with wrong credentials");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 3. Valid session → 200.
    let cookie = login_cookie(&client, &proxy_url).await;
    let resp = client
        .get(format!("{proxy_url}/api/dashboard?days=1"))
        .header("Cookie", cookie)
        .send()
        .await
        .expect("dashboard request with session auth");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("energy_days").is_some(),
        "response must contain energy_days"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Verify that `/api/dashboard` returns a global "all" scope without
/// authentication when `public_usage` is enabled.
///
/// The response must:
/// - not error (200) even though no Authorization header is present
/// - set viewer_is_admin to false
/// - return scope mode "all" with a null user_id
/// - omit the `keys` field (no per-key label leakage)
/// - omit the `users` field (user list must not be exposed)
#[tokio::test]
async fn public_dashboard_returns_global_all_scope_without_auth() {
    let db_path = temp_db_path();
    let mut config = test_proxy_config("http://127.0.0.1:1/", &db_path, 0.0);
    config.public_usage = true;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");

    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // No Authorization header at all - public_usage=true must allow this.
    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/api/dashboard?days=1"))
        .send()
        .await
        .expect("dashboard request");

    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body["viewer_is_admin"], serde_json::json!(false));
    assert_eq!(body["scope"]["mode"], serde_json::json!("all"));
    assert_eq!(body["scope"]["user_id"], serde_json::Value::Null);
    assert!(
        body.get("keys").is_none(),
        "public usage must not expose per-key labels"
    );
    assert!(
        body.get("users").is_none(),
        "public usage must not expose the user list"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

// ─── Step 7: integration tests for multi-backend routing ───

/// Build a multi-backend config for integration tests.
///
/// * `upstreams` - one base URL per backend.
/// * `models_per_backend` - static model lists (seeded into the catalog at
///   construction).
/// * `default_poll_secs` - `models_poll_secs` for the first backend
///   (the default). `None` for the others.
/// * `api_keys` - optional API key per backend.
/// * `gpu_watts` - shared GPU power setting.
fn multi_backend_config(
    upstreams: Vec<String>,
    models_per_backend: Vec<Vec<String>>,
    default_poll_secs: Option<u64>,
    api_keys: Vec<Option<String>>,
    db_path: &Path,
    gpu_watts: f64,
) -> Config {
    assert_eq!(upstreams.len(), models_per_backend.len());
    assert_eq!(upstreams.len(), api_keys.len());

    let backends: Vec<BackendConfig> = upstreams
        .iter()
        .enumerate()
        .map(|(i, upstream)| BackendConfig {
            upstream: upstream.clone(),
            provider: ProviderKind::LlamaCpp,
            api_key: api_keys[i].clone(),
            extra_headers: Vec::new(),
            models: models_per_backend[i].clone(),
            models_poll_secs: if i == 0 { default_poll_secs } else { None },
            model_types: Default::default(),
            model_filter: None,
            gpu_watts,
        })
        .collect();

    Config {
        bind: "127.0.0.1:0".to_string(),
        upstream: upstreams[0].clone(),
        db_path: db_path.to_str().unwrap().to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 4,
        dashboard_auth: None,
        gpu_watts,
        public_usage: false,
        require_api_key: false,
        backends,
        flat_passthrough: false,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        retention_days: 90,
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    }
}

/// Bind a TCP listener, accept connections, and immediately close them
/// without writing any response. The connecting client observes a
/// non-connect transport error (connection accepted then closed by peer)
/// rather than a connection-refused connect error.
async fn start_accept_and_close_upstream() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Immediately drop the stream without writing any
            // response so the peer sees a closed connection.
            drop(stream);
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    (url, handle)
}

/// Start a mock upstream that returns a `/v1/models` listing when the
/// request path matches, and a generic JSON response otherwise.
async fn start_mock_with_models(
    models: Vec<String>,
    hit_counter: Arc<AtomicUsize>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}", addr.port());

    let models_json = models;

    let handle = tokio::spawn(async move {
        let router = Router::new().fallback(move |uri: Uri, _headers: HeaderMap, _body: Body| {
            let models = models_json.clone();
            let counter = Arc::clone(&hit_counter);
            async move {
                let path = uri.path();
                if path == "/v1/models" {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "object": "list",
                                "data": models.iter().map(|m| {
                                    serde_json::json!({ "id": m, "object": "model" })
                                }).collect::<Vec<_>>(),
                            })
                            .to_string(),
                        ))
                        .unwrap();
                }
                // Non-`/v1/models` path - count it as a proxy hit.
                counter.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"content":"ok"}"#))
                    .unwrap()
            }
        });
        axum::serve(listener, router).await.expect("mock upstream");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    (url, handle)
}

/// Start the proxy with a multi-backend config.
async fn start_multi_proxy(
    config: Config,
) -> (
    String,
    tokio::task::JoinHandle<Result<(), twl_server::ServerError>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");

    let handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;
    (url, handle)
}

/// Scenario 1: Two backends, static models `model-a` / `model-b`.
/// A chat request for model-a hits backend 1 only; model-b hits backend 2 only.
#[tokio::test]
async fn test_multi_backend_static_routing() {
    let counter0 = Arc::new(AtomicUsize::new(0));
    let counter1 = Arc::new(AtomicUsize::new(0));

    let (url0, _h0) = start_mock_with_models(vec!["model-a".into()], counter0.clone()).await;
    let (url1, _h1) = start_mock_with_models(vec!["model-b".into()], counter1.clone()).await;

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![url0, url1],
        vec![vec!["model-a".into()], vec!["model-b".into()]],
        Some(30), // poll interval (won't be used; models are already seeded)
        vec![None, None],
        &db_path,
        0.0,
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let client = reqwest::Client::new();

    // model-a → backend 0
    let _ = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"model-a","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    // model-b → backend 1
    let _ = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"model-b","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(
        counter0.load(Ordering::SeqCst),
        1,
        "model-a should hit backend 0 exactly once"
    );
    assert_eq!(
        counter1.load(Ordering::SeqCst),
        1,
        "model-b should hit backend 1 exactly once"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Scenario 2: Unknown model → 404 with `code: "model_not_found"`, zero
/// upstream hits, energy table stays empty.
#[tokio::test]
async fn test_unknown_model_404_no_energy() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (url, _h) = start_mock_with_models(vec!["model-a".into()], counter.clone()).await;

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![url],
        vec![vec!["model-a".into()]],
        Some(30),
        vec![None],
        &db_path,
        100.0, // gpu_watts > 0 so any energy would be recorded
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"ghost","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["code"].as_str().unwrap(),
        "model_not_found",
        "error code must be model_not_found"
    );

    // Zero upstream hits - unknown model is rejected locally.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "unknown model must not reach upstream"
    );

    // Energy table must be empty - rejected requests don't meter.
    proxy_handle.abort();
    let _ = proxy_handle.await;

    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let energy_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM energy", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        energy_count, 0,
        "energy table must stay empty for rejected requests"
    );

    cleanup_db(&db_path);
}

/// Assert that a model-less request is rejected with HTTP 400.
///
/// Neither backend counter is incremented because the proxy never
/// forwards the request upstream.
#[tokio::test]
async fn test_modelless_request_is_rejected() {
    let counter0 = Arc::new(AtomicUsize::new(0));
    let counter1 = Arc::new(AtomicUsize::new(0));

    let (url0, _h0) = start_mock_with_models(vec!["model-a".into()], counter0.clone()).await;
    let (url1, _h1) = start_mock_with_models(vec!["model-b".into()], counter1.clone()).await;

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![url0, url1],
        vec![vec!["model-a".into()], vec!["model-b".into()]],
        Some(30),
        vec![None, None],
        &db_path,
        0.0,
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let client = reqwest::Client::new();

    // GET /props has no model field - the proxy must reject it with 400.
    let resp = client
        .get(format!("{proxy_url}/props"))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "model-less request must be rejected with 400"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["message"].as_str().unwrap(),
        "request must specify a model"
    );

    // Neither backend should have been reached.
    assert_eq!(
        counter0.load(Ordering::SeqCst),
        0,
        "model-less request must not hit backend 0"
    );
    assert_eq!(
        counter1.load(Ordering::SeqCst),
        0,
        "model-less request must not hit backend 1"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Scenario 4: `/v1/models` → merged list contains model-a and model-b;
/// upstream hit counters unchanged by the call.
#[tokio::test]
async fn test_v1_models_merged_no_proxy_hits() {
    let counter0 = Arc::new(AtomicUsize::new(0));
    let counter1 = Arc::new(AtomicUsize::new(0));

    let (url0, _h0) = start_mock_with_models(vec!["model-a".into()], counter0.clone()).await;
    let (url1, _h1) = start_mock_with_models(vec!["model-b".into()], counter1.clone()).await;

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![url0, url1],
        vec![vec!["model-a".into()], vec!["model-b".into()]],
        Some(30),
        vec![None, None],
        &db_path,
        0.0,
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{proxy_url}/v1/models"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let data = body["data"]
        .as_array()
        .expect("/v1/models data must be an array");
    let ids: Vec<&str> = data
        .iter()
        .filter_map(|v| v.get("id").and_then(|i| i.as_str()))
        .collect();
    assert!(
        ids.contains(&"model-a"),
        "merged /v1/models must contain model-a"
    );
    assert!(
        ids.contains(&"model-b"),
        "merged /v1/models must contain model-b"
    );

    // The local `/v1/models` route must not hit upstreams.
    assert_eq!(
        counter0.load(Ordering::SeqCst),
        0,
        "GET /v1/models must not reach upstream 0"
    );
    assert_eq!(
        counter1.load(Ordering::SeqCst),
        0,
        "GET /v1/models must not reach upstream 1"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Scenario 5: Watchdog - mock 1 with `models_poll_secs: 1` serves
/// `/v1/models` listing `model-c`; after a short wait a request for model-c
/// routes to mock 1 and merged `/v1/models` contains model-c.
#[tokio::test]
async fn test_watchdog_poll_updates_routing() {
    let counter = Arc::new(AtomicUsize::new(0));

    // Custom mock: /v1/models always returns ["model-c"], everything else counts.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}", addr.port());
    let counter_wd = counter.clone();

    let _handle = tokio::spawn(async move {
        let router = Router::new().fallback(move |uri: Uri, _headers: HeaderMap, _body: Body| {
            let c = Arc::clone(&counter_wd);
            async move {
                if uri.path() == "/v1/models" {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "object": "list",
                                "data": [serde_json::json!({ "id": "model-c", "object": "model" })],
                            })
                            .to_string(),
                        ))
                        .unwrap();
                }
                c.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"content":"ok"}"#))
                    .unwrap()
            }
        });
        axum::serve(listener, router).await.expect("mock upstream");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let db_path = temp_db_path();
    // No static models - the watchdog must populate the catalog.
    let backends = vec![BackendConfig {
        upstream: url,
        provider: ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        models: vec![],
        models_poll_secs: Some(1), // poll every 1 second
        model_types: Default::default(),
        model_filter: None,
        gpu_watts: 0.0,
    }];
    let config = Config {
        bind: "127.0.0.1:0".to_string(),
        upstream: "".to_string(),
        db_path: db_path.to_str().unwrap().to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 4,
        dashboard_auth: None,
        gpu_watts: 0.0,
        public_usage: false,
        require_api_key: false,
        backends,
        flat_passthrough: false,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        retention_days: 90,
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    };
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    // Wait for the watchdog to poll at least once (1 s interval + propagation).
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Now a request for model-c should succeed (watchdog added it).
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"model-c","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "model-c should be routable after watchdog poll"
    );

    // Verify the upstream was actually hit.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "model-c request should hit the upstream once"
    );

    // Verify the merged /v1/models now includes model-c.
    let resp = client
        .get(format!("{proxy_url}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let data = body["data"]
        .as_array()
        .expect("/v1/models data must be an array");
    let ids: Vec<&str> = data
        .iter()
        .filter_map(|v| v.get("id").and_then(|i| i.as_str()))
        .collect();
    assert!(
        ids.contains(&"model-c"),
        "watchdog should have added model-c to the merged catalog"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
    // Mock handle abandoned - axum::serve needs listener close to exit.
}

/// Scenario 6: Auth injection - backend configured with an api_key; mock
/// asserts it receives `authorization: Bearer <key>` even when the client
/// sends its own different Authorization header (replacement, not duplication).
#[tokio::test]
async fn test_auth_injection_replaces_client_header() {
    let backend_key = "sk-backend-key-123";
    let client_key = "sk-client-key-456";

    let counter = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}", addr.port());
    let backend_key = backend_key.to_string();
    let client_key = client_key.to_string();
    let backend_key_cfg = backend_key.clone();
    let counter_for_mock = counter.clone();

    let _handle = tokio::spawn(async move {
        let router = Router::new().fallback(move |_uri: Uri, headers: HeaderMap, _body: Body| {
            let bk = backend_key.clone();
            let counter = Arc::clone(&counter_for_mock);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // Only the backend's key should be present.
                let auth = headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string());
                let expected = format!("Bearer {bk}");
                assert_eq!(
                    auth.as_deref(),
                    Some(expected.as_str()),
                    "proxy must replace client auth with backend auth"
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"content":"ok"}"#))
                    .unwrap()
            }
        });
        axum::serve(listener, router).await.expect("mock upstream");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let db_path = temp_db_path();
    // No watchdog: disable polling so only the explicit request hits the mock.
    let backends = vec![BackendConfig {
        upstream: url,
        provider: ProviderKind::LlamaCpp,
        api_key: Some(backend_key_cfg),
        extra_headers: Vec::new(),
        models: vec!["model-a".into()],
        models_poll_secs: None,
        model_types: Default::default(),
        model_filter: None,
        gpu_watts: 0.0,
    }];
    let config = Config {
        bind: "127.0.0.1:0".to_string(),
        upstream: "".to_string(),
        db_path: db_path.to_str().unwrap().to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 4,
        dashboard_auth: None,
        gpu_watts: 0.0,
        public_usage: false,
        require_api_key: false,
        backends,
        flat_passthrough: false,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        retention_days: 90,
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    };
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    // Client sends its own Authorization header - proxy should replace it.
    let _ = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {client_key}"))
        .body(r#"{"model":"model-a","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    eprintln!("[auth-test] request sent");

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "request should reach upstream"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
    // Mock handle abandoned - axum::serve needs listener close to exit.
}

/// Scenario 7: KV-cache-aware affinity routing.
///
/// Part 1 - identical prompt prefix across 12 sequential requests:
/// KV-prefix affinity must pin all traffic to one backend.
///
/// Part 2 - 24 requests, each with a distinct prompt prefix:
/// traffic must be distributed across both backends.
#[tokio::test]
async fn test_prefix_affinity_and_distribution() {
    let counter0 = Arc::new(AtomicUsize::new(0));
    let counter1 = Arc::new(AtomicUsize::new(0));

    let (url0, _h0) = start_mock_with_models(vec!["shared".into()], counter0.clone()).await;
    let (url1, _h1) = start_mock_with_models(vec!["shared".into()], counter1.clone()).await;

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![url0, url1],
        vec![vec!["shared".into()], vec!["shared".into()]],
        Some(30),
        vec![None, None],
        &db_path,
        0.0,
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let client = reqwest::Client::new();

    // ── Part 1: identical prompt prefix → one backend ──
    for _ in 0..12 {
        let _ = client
            .post(format!("{proxy_url}/v1/chat/completions"))
            .header("content-type", "application/json")
            .body(
                r#"{"model":"shared","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Hello, how are you?"}]}"#,
            )
            .send()
            .await
            .unwrap();
    }

    let count0 = counter0.load(Ordering::SeqCst);
    let count1 = counter1.load(Ordering::SeqCst);
    assert!(
        (count0 == 12 && count1 == 0) || (count0 == 0 && count1 == 12),
        "identical-prefix conversations must pin to one backend for cache reuse; got ({count0}, {count1})"
    );

    // ── Part 2: distinct prefixes → both backends ──
    counter0.store(0, Ordering::SeqCst);
    counter1.store(0, Ordering::SeqCst);

    for i in 0..24 {
        let user_content = format!("Distinct message number {i}");
        let body = serde_json::json!({
            "model": "shared",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": user_content}
            ]
        });
        let _ = client
            .post(format!("{proxy_url}/v1/chat/completions"))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
    }

    let count0 = counter0.load(Ordering::SeqCst);
    let count1 = counter1.load(Ordering::SeqCst);
    assert!(
        count0 > 0 && count1 > 0,
        "distinct conversations must be distributed across both backends; got ({count0}, {count1})"
    );
    assert_eq!(
        count0 + count1,
        24,
        "total hits must equal 24; got ({count0} + {count1})"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
    // Mock handles abandoned - axum::serve needs listener close to exit.
}

/// Scenario 8: Transport-level failover - first backend unreachable, second
/// backend live. Request must be proxied to the live second backend.
#[tokio::test]
async fn test_transport_failover() {
    let (live_url, _live_handle) = start_mock_upstream(|_, _, _| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"from-backup"}"#))
            .unwrap()
    })
    .await;

    let dead_url = "http://127.0.0.1:1/".to_string();

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![dead_url, live_url],
        vec![vec!["failover-model".into()], vec!["failover-model".into()]],
        None,
        vec![None, None],
        &db_path,
        0.0,
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"failover-model","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["content"].as_str().unwrap(),
        "from-backup",
        "response body must come from the live second backend"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Verify that a non-connect transport error (TCP accepted then closed)
/// does NOT trigger failover. Only connect-level errors (connection
/// refused / unreachable) should fail over.
#[tokio::test]
async fn test_no_failover_on_non_connect_error() {
    let (dead_url, _dead_handle) = start_accept_and_close_upstream().await;

    let hits = Arc::new(AtomicUsize::new(0));
    let (live_url, _live_handle) =
        start_mock_with_models(vec!["failover-model".into()], hits.clone()).await;

    let db_path = temp_db_path();
    let config = multi_backend_config(
        vec![dead_url, live_url],
        vec![vec!["failover-model".into()], vec!["failover-model".into()]],
        None,
        vec![None, None],
        &db_path,
        0.0,
    );
    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    let client = reqwest::Client::new();

    // The embeddings endpoint carries no prompt prefix, so routing uses
    // the default least-loaded strategy and therefore tries the first
    // backend before the second.
    let resp = client
        .post(format!("{proxy_url}/v1/embeddings"))
        .header("content-type", "application/json")
        .body(r#"{"model":"failover-model","input":"hi"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "a non-connect error on the first backend must surface as 502 and must not fail over"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the live second backend must never be reached because non-connect errors do not fail over"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Happy path: a POST /v1/embeddings is proxied verbatim and records a
/// single token_usage row tagged with the embeddings endpoint.
#[tokio::test]
async fn embeddings_passthrough_records_usage() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _, _| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "object": "list",
                    "data": [{"object": "embedding", "embedding": [0.1, 0.2, 0.3], "index": 0}],
                    "model": "embed-model",
                    "usage": {"prompt_tokens": 8, "total_tokens": 8}
                })
                .to_string(),
            ))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let mut config = test_proxy_config_inner(&upstream_url, &db_path, 0.0, None);
    // Seed the catalog with embed-model and tag it as an embeddings model.
    config.backends[0].models.push("embed-model".to_string());
    config.backends[0].model_types.insert(
        "embed-model".to_string(),
        twl_provider::ModelKind::Embedding,
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{proxy_url}/v1/embeddings"))
        .header("content-type", "application/json")
        .body(r#"{"model":"embed-model","input":"hello world"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    // Body is forwarded verbatim from the mock upstream.
    assert_eq!(body["model"].as_str().unwrap(), "embed-model");
    assert_eq!(body["data"][0]["embedding"][0].as_f64().unwrap(), 0.1);

    let conn = wait_for_rows(&db_path, "token_usage", 1).await;

    let (endpoint, input, output, total): (String, i64, i64, i64) = conn
        .query_row(
            "SELECT endpoint, input_tokens, output_tokens, total_tokens \
             FROM token_usage LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(endpoint, "/v1/embeddings");
    assert_eq!(input, 8);
    assert_eq!(output, 0);
    assert_eq!(total, 8);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// An embeddings model discovered via the watchdog poll (no static entry)
/// still routes through POST /v1/embeddings once the catalog is populated,
/// and the config override tags it as an embeddings model.
#[tokio::test]
async fn embeddings_model_discovered_via_watchdog_routes() {
    let counter = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}", addr.port());
    let counter_wd = counter.clone();

    let _handle = tokio::spawn(async move {
        let router = Router::new().fallback(move |uri: Uri, _headers: HeaderMap, _body: Body| {
            let c = Arc::clone(&counter_wd);
            async move {
                if uri.path() == "/v1/models" {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "object": "list",
                                "data": [serde_json::json!({ "id": "embed-model", "object": "model" })],
                            })
                            .to_string(),
                        ))
                        .unwrap();
                }
                c.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"object":"list","data":[{"object":"embedding","embedding":[0.1],"index":0}],"model":"embed-model"}"#,
                    ))
                    .unwrap()
            }
        });
        axum::serve(listener, router).await.expect("mock upstream");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let db_path = temp_db_path();
    let mut config = test_proxy_config_inner(&url, &db_path, 0.0, None);
    // No static models - the watchdog must populate the catalog. Enable
    // polling and tag embed-model as an embeddings model.
    config.backends[0].models_poll_secs = Some(1);
    config.backends[0].model_types.insert(
        "embed-model".to_string(),
        twl_provider::ModelKind::Embedding,
    );
    config.flat_passthrough = false;

    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;

    // Wait for the watchdog to poll at least once.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{proxy_url}/v1/embeddings"))
        .header("content-type", "application/json")
        .body(r#"{"model":"embed-model","input":"hello"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "embed-model should route after watchdog poll"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "embeddings request should hit the upstream once"
    );

    // The merged /v1/models reflects the watchdog-discovered, tagged model.
    let resp = client
        .get(format!("{proxy_url}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let data = body["data"]
        .as_array()
        .expect("/v1/models data must be an array");
    let embed = data
        .iter()
        .find(|v| v["id"] == "embed-model")
        .expect("embed-model must appear in /v1/models");
    assert_eq!(embed["type"].as_str().unwrap(), "embedding");

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
    // Mock handle abandoned - axum::serve needs listener close to exit.
}

/// GET /v1/models exposes a scalar "type":"embedding" on tagged models only;
/// untagged models carry no "type" field (byte-identical to before).
#[tokio::test]
async fn models_endpoint_reports_embedding_type() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _, _| {
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let mut config = test_proxy_config_inner(&upstream_url, &db_path, 0.0, None);
    config.backends[0].models.push("embed-model".to_string());
    config.backends[0].models.push("chat-model".to_string());
    config.backends[0].models_poll_secs = None;
    config.backends[0].model_types.insert(
        "embed-model".to_string(),
        twl_provider::ModelKind::Embedding,
    );

    let (proxy_url, proxy_handle) = start_multi_proxy(config).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{proxy_url}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let data = body["data"]
        .as_array()
        .expect("/v1/models data must be an array");

    let embed = data
        .iter()
        .find(|v| v["id"] == "embed-model")
        .expect("embed-model must be present");
    assert_eq!(embed["type"].as_str().unwrap(), "embedding");

    let chat = data
        .iter()
        .find(|v| v["id"] == "chat-model")
        .expect("chat-model must be present");
    assert!(
        chat.get("type").is_none(),
        "chat model must not carry a type field"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}
