//! Integration tests for API key authentication flow.
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
use tokio::net::TcpListener;

use twl_auth::sha256_hex;
use twl_config::{BackendConfig, Config, ProviderKind};
use twl_server::run_with_listener;

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
        "twl-int-apikey-{}-{seq}-{nonce}",
        std::process::id()
    ))
}

/// Remove the test database file.
fn cleanup_db(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Spawn a mock HTTP upstream that delegates each request to `handler`.
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

/// Build a minimal `Config` with optional `require_api_key` gating.
fn test_proxy_config_with_require_key(
    upstream: &str,
    db_path: &Path,
    gpu_watts: f64,
    require_api_key: bool,
) -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        upstream: upstream.to_string(),
        db_path: db_path.to_str().unwrap().to_string(),
        pricing: twl_pricing::Pricing::default(),
        max_concurrency: 4,
        dashboard_auth: None,
        gpu_watts,
        public_usage: false,
        require_api_key,
        retention_days: 90,
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
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    }
}

/// Create a test user with an API key in the database at `db_path`.
async fn create_test_user_and_key(db_path: &Path) -> (String, i64) {
    let database = twl_store::Database::open(db_path.to_str().unwrap()).expect("open test db");
    let identity = twl_store::IdentityStore::new(&database).expect("create identity store");
    let user_id = identity
        .create_user("apiuser", "dummy_hash", false)
        .expect("create user");

    let nonce = format!("{db_path:?}");
    let raw_key = format!("twl_{}", &format!("{:0>32}", &nonce[..nonce.len().min(32)]));
    let key_hash = sha256_hex(&raw_key);
    let prefix = raw_key.chars().take(10).collect::<String>();

    let key_id = identity
        .insert_api_key(user_id, None, &key_hash, &prefix)
        .expect("insert api key");
    drop(identity);
    drop(database);
    tokio::time::sleep(Duration::from_millis(100)).await;
    (raw_key, key_id)
}

/// Seed the database with users (alice, bob, admin), their API keys, and token_usage rows.
fn seed_users_and_usage(db_path: &Path) -> (String, String, String, i64, i64) {
    let database = twl_store::Database::open(db_path.to_str().unwrap()).expect("open db");
    let identity = twl_store::IdentityStore::new(&database).expect("create identity store");

    let alice_id = identity
        .create_user("alice", "dummy", false)
        .expect("create alice");
    let bob_id = identity
        .create_user("bob", "dummy", false)
        .expect("create bob");
    let _admin_id = identity
        .create_user("admin", "dummy", true)
        .expect("create admin");

    let alice_raw = "twl_alice_key_abcdefghij".to_string();
    let bob_raw = "twl_bob_key_abcdefghijk".to_string();
    let admin_raw = "twl_admin_key_abcdefghijkl".to_string();

    let alice_hash = sha256_hex(&alice_raw);
    let alice_prefix: String = alice_raw.chars().take(10).collect();
    let alice_key_id = identity
        .insert_api_key(alice_id, None, &alice_hash, &alice_prefix)
        .expect("alice key");

    let bob_hash = sha256_hex(&bob_raw);
    let bob_prefix: String = bob_raw.chars().take(10).collect();
    let bob_key_id = identity
        .insert_api_key(bob_id, None, &bob_hash, &bob_prefix)
        .expect("bob key");

    let admin_hash = sha256_hex(&admin_raw);
    let admin_prefix: String = admin_raw.chars().take(10).collect();
    identity
        .insert_api_key(_admin_id, None, &admin_hash, &admin_prefix)
        .expect("admin key");

    drop(identity);
    drop(database);

    let conn = rusqlite::Connection::open(db_path).expect("open conn");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Two rows for alice
    conn.execute(
        "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens, api_key_id)
         VALUES (?1, 'alice-model', '/v1/chat/completions', 10, 20, 5, 35, ?2)",
        rusqlite::params![now - 60, alice_key_id],
    )
    .expect("alice usage 1");
    conn.execute(
        "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens, api_key_id)
         VALUES (?1, 'alice-model', '/v1/chat/completions', 5, 10, 0, 15, ?2)",
        rusqlite::params![now, alice_key_id],
    )
    .expect("alice usage 2");

    // One row for bob
    conn.execute(
        "INSERT INTO token_usage (ts, model, endpoint, input_tokens, output_tokens, cached_tokens, total_tokens, api_key_id)
         VALUES (?1, 'bob-model', '/v1/chat/completions', 100, 200, 0, 300, ?2)",
        rusqlite::params![now, bob_key_id],
    )
    .expect("bob usage");

    drop(conn);
    (alice_raw, bob_raw, admin_raw, alice_id, bob_id)
}

#[tokio::test]
async fn test_api_key_optional_keyless() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _headers, _body| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let config = test_proxy_config_with_require_key(&upstream_url, &db_path, 0.0, false);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_api_key_optional_valid_key_and_stripped() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, headers, _body| {
        assert!(
            headers.get("authorization").is_none(),
            "twl_ key must be stripped from upstream request"
        );
        assert!(
            headers.get("x-api-key").is_none(),
            "twl_ key must be stripped from upstream request"
        );
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (raw_key, _key_id) = create_test_user_and_key(&db_path).await;

    let config = test_proxy_config_with_require_key(&upstream_url, &db_path, 0.0, false);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {raw_key}"))
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "valid key should succeed");

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_api_key_optional_invalid_key() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _headers, _body| {
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("should not reach"))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let config = test_proxy_config_with_require_key(&upstream_url, &db_path, 0.0, false);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/test"))
        .header(
            "Authorization",
            "Bearer twl_invalidkey123456789012345678901234567890",
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"].as_str().unwrap(),
        "invalid or missing API key"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_api_key_required_keyless_gets_401() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _headers, _body| {
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("should not reach"))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    create_test_user_and_key(&db_path).await;

    let config = test_proxy_config_with_require_key(&upstream_url, &db_path, 0.0, true);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Keyless → 401
    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"].as_str().unwrap(),
        "invalid or missing API key"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_api_key_required_valid_key_succeeds() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, headers, _body| {
        assert!(headers.get("authorization").is_none());
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (raw_key, _key_id) = create_test_user_and_key(&db_path).await;

    let config = test_proxy_config_with_require_key(&upstream_url, &db_path, 0.0, true);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {raw_key}"))
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_api_key_usage_attribution() {
    let (upstream_url, _upstream_handle) = start_mock_upstream(|_, _headers, _body| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(
                r#"{"model":"m","usage":{"prompt_tokens":5,"completion_tokens":10}}"#,
            ))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let (raw_key, expected_key_id) = create_test_user_and_key(&db_path).await;

    // Use a custom config that has model "m" in the static catalog so the
    // proxy routes the request to the mock upstream instead of returning 404.
    let backends = vec![BackendConfig {
        upstream: upstream_url.clone(),
        provider: ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        models: vec!["m".into()],
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
        retention_days: 90,
        backends,
        flat_passthrough: false,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {raw_key}"))
        .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    // Consume body so the full response is received before we abort.
    let _resp_body = resp.text().await.unwrap();

    // Accounting is persisted by a dedicated background thread after the
    // response stream finishes. Poll for the row instead of assuming that
    // thread has committed before the client finishes consuming the body.
    let mut usage_conn = None;
    for _ in 0..100 {
        if let Ok(conn) = rusqlite::Connection::open(&db_path) {
            let count = conn.query_row("SELECT COUNT(*) FROM token_usage", [], |row| {
                row.get::<_, i64>(0)
            });
            if matches!(count, Ok(1)) {
                usage_conn = Some(conn);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let conn = usage_conn.expect("token_usage row should have been recorded within the timeout");

    proxy_handle.abort();
    let _ = proxy_handle.await;

    // Verify the token_usage row carries the correct api_key_id.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM token_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1, "exactly one token_usage row");

    let stored_key_id: Option<i64> = conn
        .query_row("SELECT api_key_id FROM token_usage LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored_key_id, Some(expected_key_id));

    drop(conn);
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_api_key_non_twl_auth_passthrough() {
    let captured_auth = Arc::new(std::sync::Mutex::new(None::<String>));
    let cap = captured_auth.clone();

    let (upstream_url, _upstream_handle) = start_mock_upstream(move |_, headers, _body| {
        let auth = headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string());
        *cap.lock().unwrap() = auth;
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap()
    })
    .await;

    let db_path = temp_db_path();
    let config = test_proxy_config_with_require_key(&upstream_url, &db_path, 0.0, false);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/test"))
        .header("content-type", "application/json")
        .header("Authorization", "Bearer sk-openai-key-123")
        .body(r#"{"model":"m"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let captured = captured_auth.lock().unwrap().take();
    assert_eq!(
        captured.as_deref(),
        Some("Bearer sk-openai-key-123"),
        "non-twl_ Authorization must pass through to upstream"
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

#[tokio::test]
async fn test_dashboard_per_user_usage_containment() {
    let db_path = temp_db_path();
    let (alice_key, _bob_key, admin_key, alice_id, bob_id) = seed_users_and_usage(&db_path);

    let config = test_proxy_config_with_require_key("http://127.0.0.1:1", &db_path, 0.0, false);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Helper: GET /api/dashboard?days=1 with optional user_id
    async fn dashboard_req(
        client: &reqwest::Client,
        base_url: &str,
        key: &str,
        user_id: Option<i64>,
    ) -> serde_json::Value {
        let mut url = format!("{base_url}/api/dashboard?days=1");
        if let Some(id) = user_id {
            url = format!("{url}&user_id={id}");
        }
        client
            .get(&url)
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    }

    // 1. Alice with no override: only sees alice-model, scope is her own
    let body = dashboard_req(&client, &proxy_url, &alice_key, None).await;
    assert_eq!(body["viewer_is_admin"], false);
    assert_eq!(body["scope"]["mode"], "user");
    assert_eq!(body["scope"]["user_id"], alice_id);
    let models = body["models"].as_array().unwrap();
    assert!(
        models.iter().any(|m| m["model"] == "alice-model"),
        "alice sees alice-model"
    );
    assert!(
        !models.iter().any(|m| m["model"] == "bob-model"),
        "alice does not see bob-model"
    );

    // 2. Alice with user_id=bob: still only sees alice-model, scope still her own
    let body = dashboard_req(&client, &proxy_url, &alice_key, Some(bob_id)).await;
    let models = body["models"].as_array().unwrap();
    assert!(
        models.iter().any(|m| m["model"] == "alice-model"),
        "alice with bogus user_id still sees alice-model"
    );
    assert!(
        !models.iter().any(|m| m["model"] == "bob-model"),
        "alice with bogus user_id still does not see bob-model"
    );
    assert_eq!(body["scope"]["user_id"], alice_id);

    // 3. Admin with no override: sees both models
    let body = dashboard_req(&client, &proxy_url, &admin_key, None).await;
    assert_eq!(body["viewer_is_admin"], true);
    let models = body["models"].as_array().unwrap();
    assert!(
        models.iter().any(|m| m["model"] == "alice-model"),
        "admin sees alice-model"
    );
    assert!(
        models.iter().any(|m| m["model"] == "bob-model"),
        "admin sees bob-model"
    );
    // 4. Admin with user_id=alice: filtered to alice only
    let body = dashboard_req(&client, &proxy_url, &admin_key, Some(alice_id)).await;
    let models = body["models"].as_array().unwrap();
    assert!(
        models.iter().any(|m| m["model"] == "alice-model"),
        "admin filtered to alice sees alice-model"
    );
    assert!(
        !models.iter().any(|m| m["model"] == "bob-model"),
        "admin filtered to alice does not see bob-model"
    );
    assert_eq!(body["scope"]["user_id"], alice_id);

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}

/// Regression: /v1/chat/completion with require_api_key=true rejects keyless
/// requests with the proxy's JSON 401 error, not a dashboard redirect or
/// plain "Unauthorized" text. The same request with a valid Bearer key
/// reaches the upstream successfully.
#[tokio::test]
async fn test_api_key_required_chat_completion_path() {
    let upstream_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let called = upstream_called.clone();
    let called_check = called.clone();

    let (upstream_url, _upstream_handle) =
        start_mock_upstream(move |uri, _headers, _body| {
            assert_eq!(uri.path(), "/v1/chat/completion");
            called.store(true, std::sync::atomic::Ordering::Relaxed);
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"id":"1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop","index":0}]}"#,
                ))
                .unwrap()
        })
        .await;

    let db_path = temp_db_path();
    let (raw_key, _key_id) = create_test_user_and_key(&db_path).await;

    // Custom config: require_api_key=true with "test-model" in the backend
    // models list so the proxy routes to the upstream instead of 404.
    let backends = vec![BackendConfig {
        upstream: upstream_url.clone(),
        provider: ProviderKind::LlamaCpp,
        api_key: None,
        extra_headers: Vec::new(),
        models: vec!["test-model".into()],
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
        require_api_key: true,
        retention_days: 90,
        backends,
        flat_passthrough: false,
        load_balancing: Default::default(),
        fair_queue: Default::default(),
        dashboard: Default::default(),
        tls: Default::default(),
        metrics_token: None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{port}");
    let proxy_handle = tokio::spawn(run_with_listener(listener, config));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Keyless request: must get 401 JSON, not a redirect ---
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completion"))
        .header("content-type", "application/json")
        .body(r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // Must NOT be a redirect (303 → /login).
    assert!(
        resp.headers().get("location").is_none(),
        "must not be a dashboard redirect"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"].as_str().unwrap(),
        "invalid or missing API key"
    );

    // --- Valid key request: must reach upstream ---
    upstream_called.store(false, std::sync::atomic::Ordering::Relaxed);
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completion"))
        .header("content-type", "application/json")
        .header("Authorization", format!("Bearer {raw_key}"))
        .body(r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        called_check.load(std::sync::atomic::Ordering::Relaxed),
        "request with valid key must reach upstream"
    );
    let resp_text = resp.text().await.unwrap();
    assert!(resp_text.contains("chat.completion"));

    proxy_handle.abort();
    let _ = proxy_handle.await;
    cleanup_db(&db_path);
}
