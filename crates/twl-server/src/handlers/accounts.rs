//! User and API key management endpoints.
//!
//! Implements authenticated endpoints for user CRUD and API key management.
//! Admin-only handlers check `AuthUser::is_admin` and return 403 otherwise.

use std::sync::Arc;

use axum::{
    extract::{Extension, Json, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use crate::auth::AuthUser;
use crate::routes::{problem, run_db_problem};
use crate::state::AppState;

/// Request body for creating a user.
#[derive(serde::Deserialize)]
pub(crate) struct CreateUserRequest {
    username: String,
    password: String,
    #[serde(default)]
    admin: Option<bool>,
}

/// Response body for a created user.
#[derive(serde::Serialize)]
pub(crate) struct CreateUserResponse {
    id: i64,
    username: String,
    admin: bool,
}

/// Response body for a single user (admin GET).
#[derive(serde::Serialize)]
pub(crate) struct UserResponse {
    id: i64,
    username: String,
    admin: bool,
    created_ts: i64,
}

/// Request body for creating an API key.
#[derive(serde::Deserialize)]
pub(crate) struct CreateApiKeyRequest {
    name: Option<String>,
}

/// Request body for resetting another user's password (admin only).
#[derive(serde::Deserialize)]
pub(crate) struct ResetPasswordRequest {
    password: String,
}

/// Request body for changing own password.
#[derive(serde::Deserialize)]
pub(crate) struct ChangePasswordRequest {
    old_password: String,
    new_password: String,
}

/// Response body for a created API key.
#[derive(serde::Serialize)]
pub(crate) struct CreateApiKeyResponse {
    id: i64,
    name: Option<String>,
    prefix: String,
    key: String,
    note: String,
}

/// A single API key entry in the list response.
#[derive(serde::Serialize)]
struct ApiKeyEntry {
    id: i64,
    name: Option<String>,
    prefix: String,
    created_ts: i64,
}

/// Response body for listing API keys.
#[derive(serde::Serialize)]
struct ListApiKeysResponse {
    keys: Vec<ApiKeyEntry>,
}

/// Create a new user (admin only). Validates username and password, returns
/// 201 with Location header on success. Returns 400/409 Problem Details on
/// validation failure or duplicate username.
pub async fn create_user_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateUserRequest>,
) -> impl IntoResponse {
    // Admin check.
    if !auth_user.is_admin {
        return problem(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "admin access required".into(),
        )
        .into_response();
    }

    let is_admin = body.admin.unwrap_or(false);

    let identity = state.identity.clone();
    let username = body.username.clone();
    let password = body.password.clone();
    let username_for_blocking = username.clone();

    match tokio::task::spawn_blocking(move || {
        twl_auth::accounts::create_user(&identity, &username_for_blocking, &password, is_admin)
    })
    .await
    {
        Ok(Ok(id)) => {
            let location = format!("/api/users/{id}");
            let mut resp = (
                StatusCode::CREATED,
                Json(serde_json::json!(CreateUserResponse {
                    id,
                    username,
                    admin: is_admin,
                })),
            )
                .into_response();
            resp.headers_mut()
                .insert(header::LOCATION, location.parse().unwrap());
            resp
        }
        Ok(Err(twl_auth::accounts::AccountError::InvalidUsername(detail))) => {
            problem(StatusCode::BAD_REQUEST, "Bad Request", detail).into_response()
        }
        Ok(Err(twl_auth::accounts::AccountError::WeakPassword(detail))) => {
            problem(StatusCode::BAD_REQUEST, "Bad Request", detail).into_response()
        }
        Ok(Err(twl_auth::accounts::AccountError::UsernameTaken)) => problem(
            StatusCode::CONFLICT,
            "Conflict",
            "username already exists".into(),
        )
        .into_response(),
        Ok(Err(e)) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            e.to_string(),
        )
        .into_response(),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            "database task panicked".into(),
        )
        .into_response(),
    }
}

/// Create a new API key for the authenticated user. Returns 201 with the
/// raw key (one-time display), id, name, and prefix.
pub async fn create_api_key(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    body: Option<Json<CreateApiKeyRequest>>,
) -> impl IntoResponse {
    let name = body.and_then(|b| b.name.clone());
    let name_for_blocking = name.clone();

    let identity = state.identity.clone();
    let user_id = auth_user.id;

    let result = tokio::task::spawn_blocking(move || {
        twl_auth::accounts::create_api_key(&identity, user_id, name_for_blocking)
    })
    .await;

    let (id, raw_key, prefix) = match result {
        Ok(Ok((id, raw_key))) => {
            // Re-derive prefix from the raw key (same logic as generate_api_key).
            let prefix: String = raw_key.chars().take(10).collect();
            (id, raw_key, prefix)
        }
        Ok(Err(e)) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                e.to_string(),
            )
            .into_response()
        }
        Err(_) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "database task panicked".into(),
            )
            .into_response()
        }
    };

    let location = format!("/api/keys/{id}");
    let mut resp = (
        StatusCode::CREATED,
        Json(serde_json::json!(CreateApiKeyResponse {
            id,
            name,
            prefix,
            key: raw_key,
            note: "this is the only time the key will be shown; it is stored hashed".into(),
        })),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::LOCATION, location.parse().unwrap());
    resp
}

/// List all API keys for the authenticated user. Returns a JSON array of
/// key entries (id, name, prefix, created_ts). The hash is never exposed.
pub async fn list_api_keys(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let identity = state.identity.clone();
    let user_id = auth_user.id;
    let rows = match run_db_problem(
        move || identity.list_api_keys(user_id),
        "database task panicked",
    )
    .await
    {
        Ok(rows) => rows,
        Err(resp) => return resp,
    };

    let keys: Vec<ApiKeyEntry> = rows
        .into_iter()
        .map(|r| ApiKeyEntry {
            id: r.id,
            name: r.name,
            prefix: r.prefix,
            created_ts: r.created_ts,
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!(ListApiKeysResponse { keys })),
    )
        .into_response()
}

/// Get the current authenticated user's profile (id, username, admin).
pub async fn me_api(Extension(auth_user): Extension<AuthUser>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": auth_user.id,
            "username": auth_user.username,
            "admin": auth_user.is_admin,
        })),
    )
        .into_response()
}

/// List all users (admin only). Returns id, username, admin, and created_ts
/// for each user. Password hashes are never exposed.
pub async fn list_users_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if !auth_user.is_admin {
        return problem(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "admin access required".into(),
        )
        .into_response();
    }

    let identity = state.identity.clone();
    let result: Result<Vec<_>, Response> =
        run_db_problem(move || identity.list_users(), "database task panicked").await;
    match result {
        Ok(users) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "users": users
            })),
        )
            .into_response(),
        Err(resp) => resp,
    }
}

/// Get a single user by id (admin only). Returns 404 Problem Details when
/// the user is not found.
pub async fn get_user_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    if !auth_user.is_admin {
        return problem(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "admin access required".into(),
        )
        .into_response();
    }

    let identity = state.identity.clone();
    let row = match run_db_problem(
        move || identity.get_user_by_id(id),
        "database task panicked",
    )
    .await
    {
        Ok(row) => row,
        Err(resp) => return resp,
    };

    match row {
        Some(row) => (
            StatusCode::OK,
            Json(serde_json::json!(UserResponse {
                id: row.id,
                username: row.username,
                admin: row.is_admin,
                created_ts: row.created_ts,
            })),
        )
            .into_response(),
        None => {
            problem(StatusCode::NOT_FOUND, "Not Found", "user not found".into()).into_response()
        }
    }
}

/// Delete a user by id (admin only). Returns 404 if not found, 409 if the
/// last admin, 204 on success.
pub async fn delete_user_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    if !auth_user.is_admin {
        return problem(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "admin access required".into(),
        )
        .into_response();
    }

    let identity = state.identity.clone();

    match tokio::task::spawn_blocking(move || twl_auth::accounts::delete_user(&identity, id)).await
    {
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(twl_auth::accounts::AccountError::NotFound)) => {
            problem(StatusCode::NOT_FOUND, "Not Found", "user not found".into()).into_response()
        }
        Ok(Err(twl_auth::accounts::AccountError::LastAdmin)) => problem(
            StatusCode::CONFLICT,
            "Conflict",
            "the last admin cannot be deleted".into(),
        )
        .into_response(),
        Ok(Err(e)) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            e.to_string(),
        )
        .into_response(),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            "database task panicked".into(),
        )
        .into_response(),
    }
}

/// Reset another user's password (admin only). Validates the new password
/// (min 8 chars), hashes it, updates the database, and revokes their API keys;
/// the password change invalidates any active session (forcing
/// re-authentication). Returns 204 on success.
pub async fn reset_password_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<ResetPasswordRequest>,
) -> impl IntoResponse {
    if !auth_user.is_admin {
        return problem(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "admin access required".into(),
        )
        .into_response();
    }

    let identity = state.identity.clone();
    let password = body.password.clone();

    match tokio::task::spawn_blocking(move || {
        twl_auth::accounts::reset_password(&identity, id, &password)
    })
    .await
    {
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(twl_auth::accounts::AccountError::WeakPassword(detail))) => {
            problem(StatusCode::BAD_REQUEST, "Bad Request", detail).into_response()
        }
        Ok(Err(twl_auth::accounts::AccountError::NotFound)) => {
            problem(StatusCode::NOT_FOUND, "Not Found", "user not found".into()).into_response()
        }
        Ok(Err(e)) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            e.to_string(),
        )
        .into_response(),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            "database task panicked".into(),
        )
        .into_response(),
    }
}

/// Change the authenticated user's own password. Validates old_password,
/// then hashes and stores new_password. Returns 204 on success, 403 if
/// old_password is wrong.
pub async fn change_password_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    let identity = state.identity.clone();
    let user_id = auth_user.id;
    let old_password = body.old_password.clone();
    let new_password = body.new_password.clone();

    match tokio::task::spawn_blocking(move || {
        twl_auth::accounts::change_password(&identity, user_id, &old_password, &new_password)
    })
    .await
    {
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(twl_auth::accounts::AccountError::WeakPassword(detail))) => {
            problem(StatusCode::BAD_REQUEST, "Bad Request", detail).into_response()
        }
        Ok(Err(twl_auth::accounts::AccountError::WrongPassword)) => problem(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "old password is wrong".into(),
        )
        .into_response(),
        Ok(Err(twl_auth::accounts::AccountError::ConcurrentPasswordChange)) => problem(
            StatusCode::CONFLICT,
            "Conflict",
            "password changed during this request; retry with the current password".into(),
        )
        .into_response(),
        Ok(Err(e)) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            e.to_string(),
        )
        .into_response(),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            "database task panicked".into(),
        )
        .into_response(),
    }
}

/// Delete one of the authenticated user's own API keys. Returns 404 when
/// the key is not found or not owned by the caller. Returns 204 on success.
pub async fn delete_api_key_api(
    Extension(auth_user): Extension<AuthUser>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let identity = state.identity.clone();
    let user_id = auth_user.id;
    let rows = match run_db_problem(
        move || identity.delete_api_key(id, user_id),
        "database task panicked",
    )
    .await
    {
        Ok(rows) => rows,
        Err(resp) => return resp,
    };

    if rows == 0 {
        problem(
            StatusCode::NOT_FOUND,
            "Not Found",
            "api key not found".into(),
        )
        .into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}
