//! HTTP server assembly: build shared state, wire up routes, and serve.
//!
//! Exposes [`run`] (binds from config) and [`run_with_listener`] (accepts a
//! pre-bound `TcpListener` for tests and embedders).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    http::{HeaderName, HeaderValue},
    middleware::{from_fn, Next},
    response::Response,
    routing::{delete, get, post, put},
    Router,
};
use axum_login::AuthManagerLayerBuilder;
use thiserror::Error;
use tower_http::timeout::TimeoutBody;
use tower_sessions::{cookie::SameSite, Expiry, SessionManagerLayer};

use crate::auth::{hash_password, LoginBackend};
use crate::fair_queue::FairQueue;
use crate::handlers::accounts;
use crate::proxy::proxy_handler;
use crate::ratelimit::LoginThrottle;
use crate::retention;
use crate::routes;
use crate::session_store::{spawn_session_purger, PurgingMemoryStore};
use crate::sink::AccountingWriter;
use crate::state::AppState;
use crate::watchdog;
use twl_auth::{generate_random_password, password_has_minimum_length};
use twl_config::Config;
use twl_store::Database;
use twl_store::IdentityStore;
use twl_store::UsageStore;

/// Maximum idle interval while a client sends request headers/body data or
/// consumes response data. This mirrors the upstream client's read timeout:
/// long model-generation gaps remain valid, while abandoned connections
/// eventually stop retaining admission slots.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(300);

fn configure_header_read_timeout<A: axum_server::Address, Acceptor>(
    server: &mut axum_server::Server<A, Acceptor>,
    timeout: Duration,
) {
    server
        .http_builder()
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(timeout);
}

/// Startup errors for the TokenWeasel server.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Failed to bind the TCP listener to the configured address.
    #[error("failed to bind {address}: {err}")]
    BindFailure {
        /// The address the server tried to bind to.
        address: String,
        /// The underlying I/O error message.
        err: String,
    },

    /// Fatal server error after the listener was bound.
    #[error("server error: {err}")]
    ServeFailure {
        /// The underlying error message.
        err: String,
    },

    /// TLS/SSL handshake or configuration error.
    #[error("tls error: {err}")]
    Tls {
        /// The underlying TLS error message.
        err: String,
    },

    /// Failed to open the SQLite database at the configured path.
    #[error("failed to open sqlite db {path}: {err}")]
    DatabaseOpen {
        /// The database file path.
        path: String,
        /// The underlying error message.
        err: String,
    },

    /// General startup error with a preformatted message.
    #[error("{0}")]
    General(String),
}

impl From<rcgen::Error> for ServerError {
    fn from(e: rcgen::Error) -> Self {
        ServerError::Tls { err: e.to_string() }
    }
}

/// Build shared state and the router for both serve paths.
fn build_app(
    config: &Config,
    secure_session_cookie: bool,
) -> Result<(Arc<AppState>, Router), ServerError> {
    let state = build_state(config)?;
    watchdog::spawn_watchdog(Arc::clone(&state.registry));
    retention::spawn_pruner(Arc::clone(&state.usage), config.retention_days);
    let app = build_router(Arc::clone(&state), secure_session_cookie);
    Ok((state, app))
}

/// Bind the listener from config and serve until shutdown.
///
/// # Errors
///
/// Returns `Err(ServerError)` if the listener cannot bind, state construction
/// fails, or the axum server encounters a fatal error.
///
/// # Example
///
/// ```ignore
/// use twl_config::Config;
/// use twl_server::run;
///
/// let config = Config::resolve(cli).expect("valid config");
/// run(config).await.expect("server ran");
/// ```
pub async fn run(config: Config) -> Result<(), ServerError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match config.tls.enabled {
        true => {
            use std::net::ToSocketAddrs;
            let addr: SocketAddr = config
                .bind
                .to_socket_addrs()
                .map_err(|e| ServerError::BindFailure {
                    address: config.bind.clone(),
                    err: e.to_string(),
                })?
                .next()
                .ok_or(ServerError::BindFailure {
                    address: config.bind.clone(),
                    err: "no address resolved".to_string(),
                })?;
            let tls_config = crate::tls::load_or_generate(&config.tls).await?;
            let (state, app) = build_app(&config, true)?;
            let app = app.layer(from_fn(hsts_header));
            tracing::info!(
                "TokenWeasel listening on {} (https) -> upstream {} (db: {}, max_concurrency: {})",
                addr,
                config.upstream,
                config.db_path,
                state.max_concurrency,
            );
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                shutdown_handle.graceful_shutdown(None);
            });
            let mut server = axum_server::bind_rustls(addr, tls_config).handle(handle);
            configure_header_read_timeout(&mut server, CLIENT_IO_TIMEOUT);
            server
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .map_err(|e| ServerError::ServeFailure { err: e.to_string() })?;
            Ok(())
        }
        false => {
            let listener = tokio::net::TcpListener::bind(&config.bind)
                .await
                .map_err(|e| ServerError::BindFailure {
                    address: config.bind.clone(),
                    err: e.to_string(),
                })?;
            run_with_listener(listener, config).await
        }
    }
}

/// Serve on a pre-bound listener. Used by integration tests and embedders.
///
/// # Example
///
/// ```ignore
/// use tokio::net::TcpListener;
/// use twl_config::Config;
/// use twl_server::run_with_listener;
///
/// let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
/// let port = listener.local_addr().unwrap().port();
/// run_with_listener(listener, config).await;
/// ```
pub async fn run_with_listener(
    listener: tokio::net::TcpListener,
    config: Config,
) -> Result<(), ServerError> {
    let (state, app) = build_app(&config, false)?;

    tracing::info!(
        "TokenWeasel listening on {} -> upstream {} (db: {}, max_concurrency: {})",
        listener.local_addr().expect("local_addr"),
        config.upstream,
        config.db_path,
        state.max_concurrency,
    );

    let std_listener = listener
        .into_std()
        .map_err(|e| ServerError::ServeFailure { err: e.to_string() })?;
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_handle.graceful_shutdown(None);
    });
    let mut server = axum_server::from_tcp(std_listener)
        .map_err(|e| ServerError::ServeFailure { err: e.to_string() })?
        .handle(handle);
    configure_header_read_timeout(&mut server, CLIENT_IO_TIMEOUT);
    server
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(|e| ServerError::ServeFailure { err: e.to_string() })?;

    Ok(())
}

/// Build the HTTP client used to forward requests upstream.
///
/// Configured with 10 s connect timeout, 300 s read timeout (fires on idle
/// gaps, not between streaming chunks), 90 s pool idle timeout, and redirects
/// disabled (upstream 3xx pass through unchanged).
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(300))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("failed to build http client")
}

/// Map a resolved backend config entry to backend construction params.
fn backend_params(c: twl_config::BackendConfig) -> twl_backends::BackendParams {
    twl_backends::BackendParams {
        upstream: c.upstream,
        provider: c.provider,
        api_key: c.api_key,
        extra_headers: c.extra_headers,
        gpu_watts: c.gpu_watts,
        models_poll_secs: c.models_poll_secs,
        model_types: c.model_types,
        model_filter: c.model_filter,
    }
}

/// Static defense-in-depth response headers.
///
/// Guards against MIME-type sniffing (`X-Content-Type-Options: nosniff`),
/// clickjacking (`X-Frame-Options: DENY`), and referrer leakage
/// (`Referrer-Policy: no-referrer`). Existing headers are never
/// overwritten.
async fn security_headers(request: axum::http::Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;

    let headers = response.headers_mut();

    headers
        .entry(HeaderName::from_static("x-content-type-options"))
        .or_insert(HeaderValue::from_static("nosniff"));

    headers
        .entry(HeaderName::from_static("x-frame-options"))
        .or_insert(HeaderValue::from_static("DENY"));

    headers
        .entry(HeaderName::from_static("referrer-policy"))
        .or_insert(HeaderValue::from_static("no-referrer"));

    response
}

/// Set the `Strict-Transport-Security` header so clients enforce HTTPS.
///
/// This middleware is only applied on the TLS serve path.
async fn hsts_header(request: axum::http::Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;

    let headers = response.headers_mut();

    headers
        .entry(HeaderName::from_static("strict-transport-security"))
        .or_insert(HeaderValue::from_static(
            "max-age=31536000; includeSubDomains",
        ));

    response
}

/// Apply an idle timeout independently to incoming and outgoing body frames.
///
/// A request body timeout wakes handlers blocked in an extractor or
/// `to_bytes`. A response body timeout terminates a stream that stops making
/// progress, which drops the accounting body and its load/fair-queue guards.
async fn client_body_timeouts(
    request: axum::http::Request<Body>,
    next: Next,
    timeout: Duration,
) -> Response {
    let request = request.map(|body| Body::new(TimeoutBody::new(timeout, body)));
    let response = next.run(request).await;
    response.map(|body| Body::new(TimeoutBody::new(timeout, body)))
}

fn with_client_body_timeouts<S>(router: Router<S>, timeout: Duration) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(from_fn(move |request, next| {
        client_body_timeouts(request, next, timeout)
    }))
}

/// Build shared state from config. Fails if the database can't be opened.
///
/// Opens the SQLite database, constructs the energy sink, creates backends
/// and registry, loads pricing, builds the HTTP client, and bootstraps an
/// initial admin user when the database is empty.
fn build_state(config: &Config) -> Result<Arc<AppState>, ServerError> {
    let database = Database::open(&config.db_path).map_err(|e| ServerError::DatabaseOpen {
        path: config.db_path.clone(),
        err: e.to_string(),
    })?;

    let identity =
        Arc::new(
            IdentityStore::new(&database).map_err(|e| ServerError::DatabaseOpen {
                path: config.db_path.clone(),
                err: e.to_string(),
            })?,
        );

    let usage = Arc::new(
        UsageStore::new(&database).map_err(|e| ServerError::DatabaseOpen {
            path: config.db_path.clone(),
            err: e.to_string(),
        })?,
    );

    // Bootstrap: create initial admin user when the database is empty.
    {
        let count = identity
            .count_users()
            .map_err(|e| ServerError::General(format!("db count_users: {e}")))?;
        if count == 0 {
            let (username, password) = if let Some((user, pass)) = &config.dashboard_auth {
                // Validate configured password: at least 8 characters.
                if !password_has_minimum_length(pass) {
                    return Err(ServerError::General(
                        "dashboard password must be at least 8 characters".to_string(),
                    ));
                }
                (user.clone(), pass.clone())
            } else {
                let user = "admin".to_string();
                let password = generate_random_password();
                tracing::warn!(
                    "random initial admin password generated: `{password}`; \
                     operator should change it or set DASHBOARD_USER and DASHBOARD_PASSWORD"
                );
                (user, password)
            };
            let hash = hash_password(&password)
                .map_err(|e| ServerError::General(format!("failed to hash admin password: {e}")))?;
            identity
                .create_user(&username, &hash, true)
                .map_err(|e| ServerError::General(format!("failed to create admin user: {e}")))?;
            tracing::info!("admin account created");
        } else if let Some((user, pass)) = &config.dashboard_auth {
            // Table already has users but dashboard_auth is configured.
            // If the configured user exists, rotate their password.
            if !password_has_minimum_length(pass) {
                return Err(ServerError::General(
                    "dashboard password must be at least 8 characters".to_string(),
                ));
            }
            if let Some(row) = identity
                .get_user(user)
                .map_err(|e| ServerError::General(format!("db get_user: {e}")))?
            {
                // Only rotate when the configured password differs from the
                // stored one, so a plain restart does not re-hash and
                // invalidate the user's sessions.
                let password_matches = crate::auth::verify_password(pass, &row.password_hash)
                    .map_err(|error| {
                        ServerError::General(format!(
                            "failed to verify stored dashboard password: {error}"
                        ))
                    })?;
                if !password_matches {
                    let hash = hash_password(pass).map_err(|e| {
                        ServerError::General(format!("failed to hash dashboard password: {e}"))
                    })?;
                    identity.update_password(row.id, &hash).map_err(|e| {
                        ServerError::General(format!("failed to update dashboard password: {e}"))
                    })?;
                    tracing::info!("rotated password for user `{user}`");
                }
            }
        }
    }

    // Per-request usage and energy writes share a bounded, dedicated blocking
    // writer so response completion never runs SQLite on a Tokio worker.
    let accounting = AccountingWriter::spawn(usage.clone())
        .map_err(|e| ServerError::General(format!("failed to start accounting writer: {e}")))?;
    let energy_sink = Arc::new(accounting.clone()) as Arc<dyn twl_backends::energy::EnergySink>;

    let backend_configs = config.backends();
    let static_models: Vec<_> = backend_configs.iter().map(|c| c.models.clone()).collect();
    let backends: Vec<_> = backend_configs
        .into_iter()
        .map(|c| twl_backends::Backend::new(backend_params(c), Arc::clone(&energy_sink)))
        .collect();
    use twl_backends::LbStrategy as BackendsLb;
    let lb = &config.load_balancing;
    let lb_strategy = match lb.strategy {
        twl_config::LbStrategy::RoundRobin => BackendsLb::RoundRobin,
        twl_config::LbStrategy::LeastLoaded => BackendsLb::LeastLoaded,
    };
    let routing_mode = if config.flat_passthrough {
        twl_backends::RoutingMode::FlatPassthrough
    } else {
        twl_backends::RoutingMode::Catalog
    };
    let registry = twl_backends::BackendRegistry::new(backends, static_models, routing_mode)
        .with_load_balancing(lb_strategy, lb.overload_factor);

    let pricing = Arc::new(config.pricing.clone());

    Ok(Arc::new(AppState {
        client: build_client(),
        registry: Arc::new(registry),
        usage,
        accounting,
        identity,
        pricing,
        dashboard: config.dashboard.clone(),
        max_concurrency: config.max_concurrency,
        api_key_validation_slots: Arc::new(tokio::sync::Semaphore::new(config.max_concurrency)),
        fair_queue: FairQueue::new(
            config.max_concurrency,
            config.fair_queue.enabled,
            std::time::Duration::from_secs(config.fair_queue.window_seconds),
        ),
        public_usage: config.public_usage,
        require_api_key: config.require_api_key,
        login_throttle: Arc::new(LoginThrottle::new()),
        metrics_token: config.metrics_token.clone(),
    }))
}

/// Assemble the router: management endpoints plus a catch-all reverse proxy.
///
/// Management handlers share their own concurrency limit. Proxy execution is
/// instead enforced after API-key authentication by the fair queue, allowing
/// admission decisions to use the owning user identity without an outer gate
/// obscuring the queue's ordering.
fn build_router(state: Arc<AppState>, secure_session_cookie: bool) -> Router {
    let session_store = PurgingMemoryStore::default();
    spawn_session_purger(session_store.clone());
    let session_layer = SessionManagerLayer::new(session_store)
        .with_name("twl_session")
        .with_http_only(true)
        .with_same_site(SameSite::Strict)
        .with_secure(secure_session_cookie)
        .with_expiry(Expiry::OnInactivity(
            tower_sessions::cookie::time::Duration::days(7),
        ));
    let auth_layer = AuthManagerLayerBuilder::new(
        LoginBackend::new(Arc::clone(&state.identity)),
        session_layer,
    )
    .build();

    // Auth-protected routes: account/user/key management always protected.
    let mut protected = Router::new()
        .route("/account", get(routes::account))
        .route("/api/users", post(accounts::create_user_api))
        .route("/api/users", get(accounts::list_users_api))
        .route(
            "/api/users/{id}",
            get(accounts::get_user_api).delete(accounts::delete_user_api),
        )
        .route(
            "/api/users/{id}/password",
            put(accounts::reset_password_api),
        )
        .route("/api/me", get(accounts::me_api))
        .route("/api/me/password", put(accounts::change_password_api))
        .route("/api/keys", post(accounts::create_api_key))
        .route("/api/keys", get(accounts::list_api_keys))
        .route("/api/keys/{id}", delete(accounts::delete_api_key_api));

    // Unprotected routes: session login/logout and login page.
    let mut unprotected = Router::new()
        .route("/api/login", post(crate::handlers::session::login_api))
        .route("/api/logout", post(crate::handlers::session::logout_api))
        .route("/login", get(routes::login));

    if state.public_usage {
        // Dashboard is public: register on the unprotected router.
        unprotected = unprotected
            .route("/usage", get(routes::dashboard))
            .route("/api/dashboard", get(routes::dashboard_api))
            .route("/api/today", get(routes::today_api))
            .route("/api/yesterday", get(routes::yesterday_api));
    } else {
        // Dashboard is protected: register before auth middleware.
        protected = protected
            .route("/usage", get(routes::dashboard))
            .route("/api/dashboard", get(routes::dashboard_api))
            .route("/api/today", get(routes::today_api))
            .route("/api/yesterday", get(routes::yesterday_api));
    }

    protected = protected
        .route_layer(from_fn(crate::auth::require_auth))
        .with_state(Arc::clone(&state));

    unprotected = unprotected.with_state(Arc::clone(&state));

    let management = Router::new()
        // Redirect root to usage dashboard
        .route("/", get(routes::redirect_root))
        // Proxy's own management endpoints. `/health` is served by the proxy
        // itself and therefore shadows the upstream's own `/health`; the rest
        // don't collide with any llama.cpp route.
        .route("/health", get(routes::health))
        // `/metrics` authenticates with its own dedicated bearer token, so it
        // stays outside the session/API-key auth groups: user credentials
        // must not grant metrics access, and vice versa.
        .route("/metrics", get(crate::metrics::metrics_handler))
        .route("/v1/models", get(routes::models_handler))
        .route("/assets/{*rest}", get(routes::web_file))
        .merge(protected)
        .merge(unprotected)
        .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
            state.max_concurrency,
        ));

    let app = Router::new()
        .merge(management)
        // Everything else is reverse-proxied to the selected LLM backend.
        // This fallback deliberately sits outside the management limit; its
        // authentication and execution stages have independent bounds.
        .fallback(proxy_handler);

    with_client_body_timeouts(app, CLIENT_IO_TIMEOUT)
        .layer(from_fn(security_headers))
        .layer(auth_layer)
        .with_state(state)
}

/// Wait for SIGINT (all platforms) or SIGTERM (Unix) and log the signal.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining requests and usage writes");
}

#[cfg(test)]
mod client_timeout_tests {
    use std::{io, sync::Arc, time::Duration};

    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::{Request, StatusCode},
        routing::{get, post},
        Router,
    };
    use bytes::Bytes;
    use futures_util::stream;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::Notify,
    };
    use tower::ServiceExt;

    use crate::fair_queue::FairQueue;

    use super::{configure_header_read_timeout, with_client_body_timeouts};

    const TEST_TIMEOUT: Duration = Duration::from_millis(30);

    fn stalled_body() -> Body {
        Body::from_stream(stream::pending::<Result<Bytes, io::Error>>())
    }

    #[tokio::test]
    async fn stalled_proxy_request_body_times_out_and_releases_fair_queue_slot() {
        #[derive(Clone)]
        struct TestState {
            queue: Arc<FairQueue>,
            admitted: Arc<Notify>,
        }

        async fn consume(State(state): State<TestState>, body: Body) -> StatusCode {
            let _guard = state.queue.acquire(None).await;
            state.admitted.notify_one();
            let result = to_bytes(body, usize::MAX).await;
            assert!(result.is_err(), "the stalled request body must time out");
            StatusCode::REQUEST_TIMEOUT
        }

        let state = TestState {
            queue: FairQueue::new(1, false, Duration::from_secs(60)),
            admitted: Arc::new(Notify::new()),
        };
        let app = with_client_body_timeouts(
            Router::new()
                .route("/proxy", post(consume))
                .with_state(state.clone()),
            TEST_TIMEOUT,
        );

        let stalled =
            tokio::spawn(app.oneshot(Request::post("/proxy").body(stalled_body()).unwrap()));
        state.admitted.notified().await;

        let response = stalled
            .await
            .expect("request task")
            .expect("timeout response");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        let next = tokio::time::timeout(Duration::from_secs(1), state.queue.acquire(None))
            .await
            .expect("fair-queue slot released after request body timeout");
        drop(next);
    }

    #[tokio::test]
    async fn stalled_management_body_times_out_and_releases_concurrency_slot() {
        #[derive(Clone)]
        struct TestState(Arc<Notify>);

        async fn consume(State(state): State<TestState>, body: Body) -> StatusCode {
            state.0.notify_one();
            let _ = to_bytes(body, usize::MAX).await;
            StatusCode::OK
        }

        let state = TestState(Arc::new(Notify::new()));
        let management = Router::new()
            .route("/management", post(consume))
            .layer(tower::limit::GlobalConcurrencyLimitLayer::new(1))
            .with_state(state.clone());
        let app = with_client_body_timeouts(management, TEST_TIMEOUT);

        let stalled = tokio::spawn(
            app.clone()
                .oneshot(Request::post("/management").body(stalled_body()).unwrap()),
        );
        state.0.notified().await;

        let second = tokio::time::timeout(
            Duration::from_secs(1),
            app.oneshot(Request::post("/management").body(Body::empty()).unwrap()),
        )
        .await
        .expect("management concurrency slot released")
        .expect("second management response");
        assert_eq!(second.status(), StatusCode::OK);
        stalled.await.expect("stalled request task").unwrap();
    }

    #[tokio::test]
    async fn stalled_response_stream_times_out_and_releases_fair_queue_slot() {
        let queue = FairQueue::new(1, false, Duration::from_secs(60));
        let handler_queue = Arc::clone(&queue);
        let app = with_client_body_timeouts(
            Router::new().route(
                "/proxy",
                get(move || {
                    let queue = Arc::clone(&handler_queue);
                    async move {
                        let guard = queue.acquire(None).await;
                        let body = async_stream::stream! {
                            let _guard = guard;
                            std::future::pending::<()>().await;
                            yield Ok::<Bytes, io::Error>(Bytes::new());
                        };
                        Body::from_stream(body)
                    }
                }),
            ),
            TEST_TIMEOUT,
        );

        let response = app
            .oneshot(Request::get("/proxy").body(Body::empty()).unwrap())
            .await
            .expect("stream response");
        let result = to_bytes(response.into_body(), usize::MAX).await;
        assert!(
            result.is_err(),
            "the stalled response stream must terminate"
        );

        let next = tokio::time::timeout(Duration::from_secs(1), queue.acquire(None))
            .await
            .expect("fair-queue slot released after response timeout");
        drop(next);
    }

    #[tokio::test]
    async fn incomplete_request_headers_are_closed_after_header_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let std_listener = listener.into_std().expect("std listener");
        let mut server = axum_server::from_tcp(std_listener).expect("server");
        configure_header_read_timeout(&mut server, TEST_TIMEOUT);
        let server = tokio::spawn(async move {
            server
                .serve(
                    Router::new()
                        .route("/", get(|| async { "ok" }))
                        .into_make_service(),
                )
                .await
        });

        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost:")
            .await
            .expect("partial headers");

        let mut bytes = [0_u8; 1024];
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut bytes))
            .await
            .expect("server must close or reject incomplete headers")
            .expect("socket read");
        assert!(
            read == 0 || bytes[..read].starts_with(b"HTTP/1.1 408"),
            "header timeout should close the connection or return 408"
        );

        server.abort();
        let _ = server.await;
    }
}
