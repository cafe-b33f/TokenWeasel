//! Prometheus `/metrics` endpoint in text exposition format (version 0.0.4).
//!
//! Access is guarded by a dedicated, view-only bearer token
//! (`metrics_token` in the config, `METRICS_TOKEN` env var) that grants
//! access to `/metrics` and nothing else. Session logins and `twl_` API
//! keys deliberately do not work here, and the metrics token opens no other
//! route. When no token is configured the endpoint answers 404.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use crate::routes;
use crate::state::AppState;

/// Serves Prometheus metrics derived from the SQLite usage table.
///
/// The `*_total` counters are recomputed from the `token_usage` and `energy`
/// tables on every scrape, so retention pruning can make them decrease -
/// Prometheus treats such a drop as a counter reset.
pub(crate) async fn metrics_handler(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    // No token configured: the endpoint is disabled and not discoverable.
    let Some(expected) = &state.metrics_token else {
        return routes::problem(StatusCode::NOT_FOUND, "Not Found", "not found".to_string());
    };

    // Compare SHA-256 digests of the presented and configured tokens so the
    // comparison time does not depend on how many token bytes match.
    // No `WWW-Authenticate` header on failure (see auth.rs convention).
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(twl_auth::parse_bearer_credentials);
    let authorized =
        presented.is_some_and(|t| twl_auth::sha256_hex(t) == twl_auth::sha256_hex(expected));
    if !authorized {
        return routes::problem(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "invalid or missing metrics token".to_string(),
        );
    }

    // All-time per-model totals.
    let state_for_stats = state.clone();
    let rows =
        match routes::run_db(move || state_for_stats.usage.stats(0), "stats query failed").await {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    // All-time energy total (kWh summed over daily buckets).
    let state_for_energy = state.clone();
    let total_kwh: f64 = match routes::run_db(
        move || state_for_energy.usage.daily_energy(0),
        "energy query failed",
    )
    .await
    {
        Ok(v) => v.iter().map(|e| e.kwh).sum(),
        Err(resp) => return resp,
    };

    let body = render_metrics(&state, &rows, total_kwh);

    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/plain; version=0.0.4".parse().unwrap(),
    );
    resp
}

/// Render the Prometheus text exposition body from per-model stats and the
/// summed energy total.
fn render_metrics(state: &AppState, rows: &[twl_store::ModelStats], total_kwh: f64) -> String {
    use std::fmt::Write;

    let mut out = String::new();

    let _ = writeln!(out, "# HELP twl_build_info Build information.");
    let _ = writeln!(out, "# TYPE twl_build_info gauge");
    let _ = writeln!(
        out,
        "twl_build_info{{version=\"{}\"}} 1",
        escape_label_value(env!("CARGO_PKG_VERSION"))
    );

    let _ = writeln!(out, "# HELP twl_requests_total Proxied requests per model.");
    let _ = writeln!(out, "# TYPE twl_requests_total counter");
    for r in rows {
        let _ = writeln!(
            out,
            "twl_requests_total{{model=\"{}\"}} {}",
            escape_label_value(&r.model),
            r.requests
        );
    }

    let _ = writeln!(
        out,
        "# HELP twl_input_tokens_total Input/prompt tokens per model."
    );
    let _ = writeln!(out, "# TYPE twl_input_tokens_total counter");
    for r in rows {
        let _ = writeln!(
            out,
            "twl_input_tokens_total{{model=\"{}\"}} {}",
            escape_label_value(&r.model),
            r.input_tokens
        );
    }

    let _ = writeln!(
        out,
        "# HELP twl_output_tokens_total Output/completion tokens per model."
    );
    let _ = writeln!(out, "# TYPE twl_output_tokens_total counter");
    for r in rows {
        let _ = writeln!(
            out,
            "twl_output_tokens_total{{model=\"{}\"}} {}",
            escape_label_value(&r.model),
            r.output_tokens
        );
    }

    let _ = writeln!(
        out,
        "# HELP twl_cached_tokens_total Cached prompt tokens per model."
    );
    let _ = writeln!(out, "# TYPE twl_cached_tokens_total counter");
    for r in rows {
        let _ = writeln!(
            out,
            "twl_cached_tokens_total{{model=\"{}\"}} {}",
            escape_label_value(&r.model),
            r.cached_tokens
        );
    }

    let _ = writeln!(
        out,
        "# HELP twl_estimated_cost_total Estimated cost per model, from the configured price table."
    );
    let _ = writeln!(out, "# TYPE twl_estimated_cost_total counter");
    let currency = escape_label_value(&state.pricing.currency);
    for r in rows {
        let cost = state
            .pricing
            .cost(&r.model, r.input_tokens, r.output_tokens, r.cached_tokens);
        let _ = writeln!(
            out,
            "twl_estimated_cost_total{{model=\"{}\",currency=\"{}\"}} {}",
            escape_label_value(&r.model),
            currency,
            cost.total
        );
    }

    let _ = writeln!(
        out,
        "# HELP twl_energy_kwh_total Estimated GPU energy consumed, in kWh."
    );
    let _ = writeln!(out, "# TYPE twl_energy_kwh_total counter");
    let _ = writeln!(out, "twl_energy_kwh_total {total_kwh}");

    out
}

/// Escape a label value per the Prometheus text exposition format:
/// backslash, double quote, and newline.
fn escape_label_value(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::escape_label_value;

    #[test]
    fn escape_label_value_escapes_backslash() {
        assert_eq!(escape_label_value(r"a\b"), r"a\\b");
    }

    #[test]
    fn escape_label_value_escapes_double_quote() {
        assert_eq!(escape_label_value("a\"b"), "a\\\"b");
    }

    #[test]
    fn escape_label_value_escapes_newline() {
        assert_eq!(escape_label_value("a\nb"), "a\\nb");
    }
}
