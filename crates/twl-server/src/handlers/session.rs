//! Session login/logout handlers.
//!
//! Session storage and cookie handling are provided by `tower-sessions` through
//! `axum-login`; this module only verifies credentials and updates the auth
//! session.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::auth::{self, LoginCredentials, LoginSession};
use crate::state::AppState;

/// Request body for `/api/login`.
#[derive(serde::Deserialize)]
pub(crate) struct LoginRequest {
    username: String,
    password: String,
}

/// POST `/api/login` -- accept `{"username", "password"}`, verify credentials,
/// create an authenticated session, and return `200 {"ok":true}`.
/// Bad credentials return 401.
pub async fn login_api(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    mut auth_session: LoginSession,
    Json(body): Json<LoginRequest>,
) -> Response {
    let client_key = auth::login_client_key(&headers, Some(peer), &state.dashboard.trusted_proxies);

    if let Err(remaining) = state.login_throttle.check(&client_key) {
        return locked_out(remaining);
    }

    let credentials = LoginCredentials {
        username: body.username,
        password: body.password,
    };

    let user = match auth_session.authenticate(credentials).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            state.login_throttle.record_failure(&client_key);
            return bad_credentials();
        }
        Err(e) => {
            tracing::warn!(error = %e, "login authentication failed");
            return internal_error();
        }
    };

    if let Err(e) = auth_session.login(&user).await {
        tracing::warn!(error = %e, "failed to persist login session");
        return internal_error();
    }

    state.login_throttle.record_success(&client_key);

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

/// POST `/api/logout` -- clear the authenticated session. Returns
/// `200 {"ok":true}` regardless of whether a user was logged in.
pub async fn logout_api(mut auth_session: LoginSession) -> Response {
    if let Err(e) = auth_session.logout().await {
        tracing::warn!(error = %e, "failed to clear login session");
    }

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

fn bad_credentials() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "invalid credentials"})),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal server error"})),
    )
        .into_response()
}

fn locked_out(remaining: i64) -> Response {
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({"error": "too many failed login attempts, account temporarily locked"})),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, remaining.to_string().parse().unwrap());
    resp
}
