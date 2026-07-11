//! Management endpoints: health check, usage dashboard, and JSON APIs.
//!
//! Serves the proxy's own HTTP endpoints (`/health`, `/usage`, `/api/*`).
//! All DB queries run off the async executor via `spawn_blocking` so the
//! SQLite mutex does not block the axum task pool.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use chrono::TimeDelta;

use crate::auth::AuthUser;
use crate::localtime::date_to_start_timestamp;
use crate::state::AppState;
use crate::WebAssets;
use twl_pricing::Cost;

/// Per-user scope for dashboard data queries.
#[derive(Clone, Copy)]
enum UsageScope {
    All,
    User(i64),
}

/// Resolve the effective usage scope from the authenticated user and
/// query parameters. Non-admin users are always scoped to themselves.
fn resolve_usage_scope(auth: &Option<AuthUser>, params: &HashMap<String, String>) -> UsageScope {
    match auth {
        None => UsageScope::All,
        Some(user) if !user.is_admin => UsageScope::User(user.id),
        Some(_user) => {
            // Admin: allow optional user_id override
            if let Some(id_str) = params.get("user_id") {
                if let Ok(id) = id_str.parse::<i64>() {
                    return UsageScope::User(id);
                }
            }
            UsageScope::All
        }
    }
}

/// Liveness probe returning `{"status":"ok"}`. Shadows the upstream's `/health`.
pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Serves the cached model catalog at `/v1/models`. No upstream calls per request.
pub async fn models_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let entries = state.registry.model_entries();
    Json(serde_json::json!({
        "object": "list",
        "data": entries,
    }))
}

/// Permanent redirect (301) from `/` to the usage dashboard.
pub async fn redirect_root() -> Redirect {
    Redirect::permanent("/usage")
}

/// Serves the dashboard HTML page (embedded at build time).
pub async fn dashboard() -> impl IntoResponse {
    serve_asset("/dashboard.html", mime_type("dashboard.html"))
}

/// Serves the account/admin HTML page (embedded at build time).
pub async fn account() -> impl IntoResponse {
    serve_asset("/account.html", mime_type("account.html"))
}

/// Serves the login HTML page (embedded at build time).
pub async fn login() -> impl IntoResponse {
    serve_asset("/login.html", mime_type("login.html"))
}

/// Serves embedded web assets by path (`/assets/*rest`). Returns 404 for
/// empty paths or missing files.
pub async fn web_file(Path(rest): Path<String>) -> impl IntoResponse {
    if rest.is_empty() {
        return not_found();
    }
    let url_path = format!("/{rest}");
    let mime = mime_type(&rest);

    serve_asset(&url_path, mime)
}

/// Look up an asset in the embedded files and return it with the right MIME type.
fn serve_asset(path: &str, mime: &str) -> Response<axum::body::Body> {
    let inner = path.strip_prefix('/').unwrap_or(path);
    match WebAssets::get(inner) {
        Some(file) => {
            let mut response = Response::new(axum::body::Body::from(file.data.into_owned()));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, mime.parse().unwrap());
            response
        }
        None => not_found(),
    }
}

/// Return the MIME type for a file extension. Defaults to `application/octet-stream`.
fn mime_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Return a 404 "Not found" response.
fn not_found() -> Response<axum::body::Body> {
    let mut response = Response::new(axum::body::Body::from("Not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// Aggregated model stats plus daily time series. The dashboard fetches this
/// once and renders everything.
///
/// Query param `days` (default 30, range 1-3650) controls the daily time
/// series window. Energy data is always included. Each DB query failure
/// returns a 500 JSON error; failures are independent.
pub async fn dashboard_api(
    auth_user: Option<Extension<AuthUser>>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let pricing = &state.pricing;

    let auth = auth_user.map(|e| e.0);
    let is_admin = auth.as_ref().map(|u| u.is_admin).unwrap_or(false);
    let scope = resolve_usage_scope(&auth, &params);

    // Parse the time-range parameter
    let days = params
        .get("days")
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 3650);

    // Compute the since-timestamp for the same calendar-day window used by `daily()`.
    let now = chrono::Local::now();
    let start_date = now.date_naive() - TimeDelta::days(days - 1);
    let since_ts = date_to_start_timestamp(&chrono::Local, start_date);

    // Fetch model-level stats (filtered to same window as daily data).
    let state_for_stats = state.clone();
    let rows = match run_db(
        move || match scope {
            UsageScope::User(id) => state_for_stats.usage.stats_for_user(since_ts, id),
            UsageScope::All => state_for_stats.usage.stats(since_ts),
        },
        "stats query failed",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Fetch daily time series (same window as stats)
    let state_for_daily = state.clone();
    let daily_rows = match run_db(
        move || match scope {
            UsageScope::User(id) => state_for_daily.usage.daily_for_user(since_ts, id),
            UsageScope::All => state_for_daily.usage.daily(since_ts),
        },
        "daily query failed",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Fetch energy data (always - gpu_watts=0 disables future recording
    // but must not hide persisted history). Errors return 500, consistent
    // with the stats / daily error handling above.
    let (energy_days_json, total_kwh) = match scope {
        UsageScope::User(_) => (Vec::new(), 0.0),
        UsageScope::All => {
            let state_for_energy = state.clone();
            let energy_rows = match run_db(
                move || state_for_energy.usage.daily_energy(since_ts),
                "energy query task failed",
            )
            .await
            {
                Ok(v) => v,
                Err(resp) => return resp,
            };
            let energy_days: Vec<serde_json::Value> = energy_rows
                .iter()
                .map(|e| serde_json::json!({ "day": e.day, "kwh": e.kwh }))
                .collect();
            let total: f64 = energy_rows.iter().map(|e| e.kwh).sum();
            (energy_days, total)
        }
    };

    // Fetch the per-account breakdown (same window): per-user rows for the
    // admin-wide scope, per-key rows for a single user's scope.
    // When public_usage is true, the dashboard is served without auth, so
    // we must NOT expose these labels (which contain account usernames).
    let key_rows: Vec<twl_store::KeyStats> = if state.public_usage {
        // Skip the query entirely; the "keys" field is omitted from the response below.
        Vec::new()
    } else {
        match scope {
            UsageScope::User(id) => {
                let state_for_keys = state.clone();
                match run_db(
                    move || state_for_keys.usage.stats_by_key_for_user(since_ts, id),
                    "key stats query failed",
                )
                .await
                {
                    Ok(v) => v,
                    Err(resp) => return resp,
                }
            }
            UsageScope::All => {
                let state_for_keys = state.clone();
                match run_db(
                    move || state_for_keys.usage.stats_by_user(since_ts),
                    "key stats query failed",
                )
                .await
                {
                    Ok(v) => v,
                    Err(resp) => return resp,
                }
            }
        }
    };

    // Compute per-model totals (including cost)
    let mut grand_total_tokens = 0i64;
    let mut grand_total_cost = 0f64;
    let models: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let cost = pricing.cost(&r.model, r.input_tokens, r.output_tokens, r.cached_tokens);
            grand_total_tokens += r.total_tokens;
            grand_total_cost += cost.total;
            model_row_json(
                &r.model,
                r.requests,
                r.input_tokens,
                r.output_tokens,
                r.cached_tokens,
                r.total_tokens,
                &cost,
            )
        })
        .collect();

    // Add cost to daily rows
    let days_with_cost: Vec<serde_json::Value> = daily_rows
        .into_iter()
        .map(|r| {
            let cost = pricing.cost(&r.model, r.input_tokens, r.output_tokens, r.cached_tokens);
            let mut row = model_row_json(
                &r.model,
                r.requests,
                r.input_tokens,
                r.output_tokens,
                r.cached_tokens,
                r.total_tokens,
                &cost,
            );
            row.as_object_mut()
                .unwrap()
                .insert("day".into(), r.day.clone().into());
            row
        })
        .collect();

    let mut resp_obj = serde_json::json!({
        "models": models,
        "days": days_with_cost,
        "grand_total_tokens": grand_total_tokens,
        "grand_total_cost": grand_total_cost,
        "currency": pricing.currency,
        "unit": pricing.unit,
        "prices": pricing.models,
        "energy_days": energy_days_json,
        "total_kwh": total_kwh,
    });
    resp_obj["dashboard"] = serde_json::json!({
        "cards": &state.dashboard.cards,
        "graphs": &state.dashboard.graphs,
        "max_rows": state.dashboard.max_rows,
    });
    resp_obj["viewer_is_admin"] = serde_json::json!(is_admin);
    let scope_json = match scope {
        UsageScope::User(id) => serde_json::json!({ "mode": "user", "user_id": id }),
        UsageScope::All => serde_json::json!({ "mode": "all", "user_id": null }),
    };
    resp_obj["scope"] = scope_json;

    if !state.public_usage {
        // Only include per-key stats when auth is required.
        // public_usage=true -> no auth -> labels (usernames) must not leak.
        // The frontend handles a missing `keys` field via `data.keys || []`.
        resp_obj["keys"] = serde_json::json!(key_rows);
    }
    Json(resp_obj).into_response()
}

/// Format a database error as a 500 JSON response.
fn db_error(e: twl_store::DbError) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

/// Format a generic server error as a 500 JSON response.
fn server_error(msg: &str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

/// Build an RFC 9457 Problem Details JSON response.
pub(crate) fn problem(status: StatusCode, title: &'static str, detail: String) -> Response {
    let body = serde_json::json!({
        "type": "about:blank",
        "title": title,
        "status": status.as_u16(),
        "detail": detail,
    });
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/problem+json".parse().unwrap(),
    );
    resp
}

/// Run a blocking DB call on the blocking thread pool. Returns 500 on error.
pub(crate) async fn run_db<T, F>(f: F, msg: &str) -> Result<T, axum::response::Response>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, twl_store::DbError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(db_error(e)),
        Err(_) => Err(server_error(msg)),
    }
}

/// Run a blocking DB call and format errors as RFC 9457 Problem Details.
pub(crate) async fn run_db_problem<T, F>(f: F, panic_detail: &'static str) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, twl_store::DbError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            e.to_string(),
        )),
        Err(_) => Err(problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal Server Error",
            panic_detail.into(),
        )),
    }
}

/// Build a per-model / per-day JSON row with the stat fields used by the
/// dashboard (includes both `prompt_tokens`/`input_tokens` and
/// `completion_tokens`/`output_tokens` for compatibility).
fn model_row_json(
    model: &str,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    total_tokens: i64,
    cost: &Cost,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "requests": requests,
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cached_tokens": cached_tokens,
        "total_tokens": total_tokens,
        "cost": cost,
    })
}

/// Five-minute bucket time series for the current local day. Always returns
/// 288 buckets (00:00-23:59); idle buckets have zero counters.
pub async fn today_api(
    auth_user: Option<Extension<AuthUser>>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let auth = auth_user.map(|e| e.0);
    let scope = resolve_usage_scope(&auth, &params);
    day_buckets_response(state, 0, "today", scope).await
}

/// Five-minute bucket time series for the previous local day (288 buckets).
pub async fn yesterday_api(
    auth_user: Option<Extension<AuthUser>>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let auth = auth_user.map(|e| e.0);
    let scope = resolve_usage_scope(&auth, &params);
    day_buckets_response(state, 1, "yesterday", scope).await
}

/// Compute five-minute bucket time series for a day relative to today.
async fn day_buckets_response(
    state: Arc<AppState>,
    days_ago: i64,
    label: &str,
    scope: UsageScope,
) -> axum::response::Response {
    let now = chrono::Local::now();
    let start_date = now.date_naive() - TimeDelta::days(days_ago);
    let day_start = date_to_start_timestamp(&chrono::Local, start_date);
    let day_end = date_to_start_timestamp(&chrono::Local, start_date + TimeDelta::days(1));

    let state_for_db = state.clone();
    let err_msg = format!("{label} buckets task failed");
    let buckets = match run_db(
        move || match scope {
            UsageScope::User(id) => state_for_db
                .usage
                .today_buckets_for_user(day_start, day_end, id),
            UsageScope::All => state_for_db.usage.today_buckets(day_start, day_end),
        },
        &err_msg,
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let buckets_json: Vec<serde_json::Value> = buckets
        .iter()
        .map(|b| {
            serde_json::json!({
                "bucket": b.bucket,
                "input_tokens": b.input_tokens,
                "output_tokens": b.output_tokens,
                "cached_tokens": b.cached_tokens,
                "requests": b.requests,
            })
        })
        .collect();

    Json(serde_json::json!({ "buckets": buckets_json })).into_response()
}
