//! Authentication middleware for dashboard and API routes.
//!
//! Browser sessions are handled by `axum-login` on top of `tower-sessions`.
//! Programmatic access to management routes can also use `twl_` bearer API
//! keys. Proxied LLM requests keep their separate API-key path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::header::{AUTHORIZATION, LOCATION};
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum_login::{AuthSession, AuthUser as LoginAuthUser, AuthnBackend, UserId};
use thiserror::Error;

pub use twl_auth::{hash_password, verify_password, AuthUser};
use twl_store::{identity::UserRow, IdentityStore};

/// Type alias used by handlers that need to log users in or out.
pub(crate) type LoginSession = AuthSession<LoginBackend>;

/// Dummy hash used for timing-safe credential verification. When a username
/// is not found, we verify against this hash to prevent enumeration.
pub(crate) static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("this-is-a-dummy-password-for-timing-safety").unwrap());

/// Credentials accepted by the browser login form.
#[derive(Clone, Debug)]
pub(crate) struct LoginCredentials {
    /// Username from the login form.
    pub(crate) username: String,
    /// Password from the login form.
    pub(crate) password: String,
}

/// User type stored in `axum-login` sessions.
#[derive(Clone, Debug)]
pub(crate) struct LoginUser {
    id: i64,
    username: String,
    password_hash: String,
    is_admin: bool,
}

impl LoginUser {
    fn from_row(row: UserRow) -> Self {
        Self {
            id: row.id,
            username: row.username,
            password_hash: row.password_hash,
            is_admin: row.is_admin,
        }
    }

    fn auth_user(&self) -> AuthUser {
        AuthUser {
            id: self.id,
            username: self.username.clone(),
            is_admin: self.is_admin,
        }
    }
}

impl LoginAuthUser for LoginUser {
    type Id = i64;

    fn id(&self) -> Self::Id {
        self.id
    }

    fn session_auth_hash(&self) -> &[u8] {
        self.password_hash.as_bytes()
    }
}

/// Authentication backend backed by the existing identity store.
#[derive(Clone, Debug)]
pub(crate) struct LoginBackend {
    identity: Arc<IdentityStore>,
}

impl LoginBackend {
    /// Build an auth backend from the shared identity store.
    pub(crate) fn new(identity: Arc<IdentityStore>) -> Self {
        Self { identity }
    }
}

/// Errors from the login backend.
#[derive(Debug, Error)]
pub(crate) enum LoginError {
    /// Blocking task failed to join.
    #[error("auth task failed: {0}")]
    Join(String),
    /// Database/store operation failed.
    #[error(transparent)]
    Store(#[from] twl_store::DbError),
}

impl AuthnBackend for LoginBackend {
    type User = LoginUser;
    type Credentials = LoginCredentials;
    type Error = LoginError;

    async fn authenticate(
        &self,
        creds: Self::Credentials,
    ) -> Result<Option<Self::User>, Self::Error> {
        let identity = self.identity.clone();
        let username = creds.username.clone();
        let row = tokio::task::spawn_blocking(move || identity.get_user(&username))
            .await
            .map_err(|e| LoginError::Join(e.to_string()))??;

        let hash_to_verify = row
            .as_ref()
            .map(|row| row.password_hash.clone())
            .unwrap_or_else(|| DUMMY_HASH.clone());
        let valid = tokio::task::spawn_blocking(move || {
            match verify_password(&creds.password, &hash_to_verify) {
                Ok(valid) => valid,
                Err(error) => {
                    tracing::error!(%error, "stored password hash could not be verified");
                    false
                }
            }
        })
        .await
        .map_err(|error| LoginError::Join(error.to_string()))?;

        if valid {
            Ok(row.map(LoginUser::from_row))
        } else {
            Ok(None)
        }
    }

    async fn get_user(&self, user_id: &UserId<Self>) -> Result<Option<Self::User>, Self::Error> {
        let identity = self.identity.clone();
        let id = *user_id;
        let row = tokio::task::spawn_blocking(move || identity.get_user_by_id(id))
            .await
            .map_err(|e| LoginError::Join(e.to_string()))??;
        Ok(row.map(LoginUser::from_row))
    }
}

/// Axum middleware requiring a valid session or `twl_` bearer API key.
pub(crate) async fn require_auth(
    mut auth_session: LoginSession,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response<Body> {
    if let Some(user) = auth_session.user.take() {
        let mut req = req;
        req.extensions_mut().insert(user.auth_user());
        return next.run(req).await;
    }

    if let Some(auth_val) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = twl_auth::parse_bearer_credentials(auth_val) {
            if token.starts_with("twl_") {
                let key = token.to_string();
                let hash = twl_auth::sha256_hex(&key);
                let identity = auth_session.backend.identity.clone();
                let api_user =
                    tokio::task::spawn_blocking(move || identity.get_api_key_user(&hash)).await;

                return match api_user {
                    Ok(Ok(Some(row))) => {
                        let auth_user = AuthUser {
                            id: row.id,
                            username: row.username,
                            is_admin: row.is_admin,
                        };
                        let mut req = req;
                        req.extensions_mut().insert(auth_user);
                        next.run(req).await
                    }
                    Ok(Ok(None)) => auth_failed_response(req.uri().path()),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "API-key auth lookup failed");
                        internal_error_response()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "API-key auth task failed");
                        internal_error_response()
                    }
                };
            }
        }
    }

    auth_failed_response(req.uri().path())
}

/// Apply login throttling for a request before credential verification.
pub(crate) fn login_client_key(
    headers: &axum::http::HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
    trusted_proxies: &[std::net::IpAddr],
) -> String {
    let peer_addr = peer.map(|ci| ci.0);
    crate::ratelimit::client_ip(headers, peer_addr, trusted_proxies)
}

/// Produce the appropriate auth-failure response depending on the request path.
///
/// - `/usage` or `/account` -> 303 redirect to `/login` (browser navigation).
/// - Other paths -> 401 without `WWW-Authenticate` to avoid browser native
///   auth dialogs.
fn auth_failed_response(path: &str) -> Response<Body> {
    if path == "/usage" || path == "/account" {
        let mut resp = Response::new(Body::from(""));
        *resp.status_mut() = StatusCode::SEE_OTHER;
        resp.headers_mut()
            .insert(LOCATION, HeaderValue::try_from("/login").unwrap());
        return resp;
    }

    let mut resp = Response::new(Body::from("Unauthorized"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp
}

/// Build a 500 Internal Server Error response.
fn internal_error_response() -> Response<Body> {
    let mut resp = Response::new(Body::from("Internal Server Error"));
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp
}
