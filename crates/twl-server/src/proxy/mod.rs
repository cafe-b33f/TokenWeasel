//! Reverse proxy to the upstream LLM server.
//!
//! Receives non-management requests, forwards them verbatim to the upstream,
//! and streams the response back while extracting token usage and energy data.
//! Bytes are always forwarded verbatim; accounting is best-effort. All parse
//! buffers are bounded (64 MiB request body, 8 MiB accounting buffer, 1 MiB line).

pub(crate) mod body;
pub(crate) mod headers;
mod stream;

use std::{future::Future, sync::Arc};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderName, Method, StatusCode, Uri},
    response::Response,
};
use bytes::Bytes;

use crate::state::AppState;

use body::{prepare_body, MAX_REQUEST_BODY};
use stream::{
    buffered_accounting_stream, ndjson_accounting_stream, sse_accounting_stream, AccountingContext,
};
use twl_auth::apikey::{extract_and_validate_client_key, extract_client_key, ClientKeyResult};
use twl_backends::energy::MeterGuard;
use twl_backends::RouteRequest;

/// Run validation under an owned permit held by a detached task. Dropping the
/// caller's future detaches the task instead of releasing the permit early.
async fn run_with_validation_slot<T>(
    slots: Arc<tokio::sync::Semaphore>,
    validation: impl Future<Output = T> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let permit = slots
        .acquire_owned()
        .await
        .map_err(|_| "API-key validation semaphore was closed".to_string())?;

    tokio::spawn(async move {
        let _permit = permit;
        validation.await
    })
    .await
    .map_err(|error| format!("API-key validation task failed: {error}"))
}

/// Validate an `twl_` key without allowing either queued or cancelled
/// lookups to consume unbounded blocking-pool capacity.
async fn validate_client_key_bounded(
    headers: &axum::http::HeaderMap,
    state: &Arc<AppState>,
) -> ClientKeyResult {
    // The validator performs no database work when there is no client key, so
    // keyless requests do not need a validation slot.
    if extract_client_key(headers).is_none() {
        return ClientKeyResult::NotPresent;
    }

    let headers = headers.clone();
    let identity = Arc::clone(&state.identity);

    // `spawn_blocking` jobs cannot be cancelled once running. Keep both the
    // validator future and its owned permit in a detached Tokio task so an
    // HTTP disconnect cannot release capacity while the lookup still runs.
    match run_with_validation_slot(Arc::clone(&state.api_key_validation_slots), async move {
        extract_and_validate_client_key(&headers, &identity).await
    })
    .await
    {
        Ok(result) => result,
        Err(error) => ClientKeyResult::Error(error),
    }
}

/// Catch-all axum handler that dispatches every non-management request to the
/// upstream. Buffers the request body, selects a backend, forwards with
/// filtered headers, classifies the response (SSE/NDJSON/buffered), and wraps
/// the byte stream in an accounting stream that extracts usage on Drop.
pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());
    let endpoint = uri.path().to_string();

    // This stage has a limit independent from proxy execution. Its permit is
    // released before fair-queue admission, so waiting proxy requests cannot
    // retain validation capacity or obscure the fair queue's ordering.
    let key_result = validate_client_key_bounded(&headers, &state).await;

    // API key validation
    let (api_key_info, key_consumed) = match key_result {
        ClientKeyResult::Valid(k) => (Some(k), true),
        ClientKeyResult::Invalid => {
            return unauthorized_response();
        }
        ClientKeyResult::Error(msg) => {
            tracing::error!(error = %msg, "API key validation failed due to internal error");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            );
        }
        ClientKeyResult::NotPresent => {
            if state.require_api_key {
                return unauthorized_response();
            }
            (None, false)
        }
    };

    // Admission happens after authentication so all keys owned by the same
    // user share one rolling usage history. The guard lives through response
    // streaming because it is moved into the accounting body below.
    let scheduling_user = api_key_info.as_ref().map(|key| key.user_id);
    let fair_queue_guard = state.fair_queue.acquire(scheduling_user).await;

    // Buffer and inspect request body.
    let body_bytes = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("request body too large (max {MAX_REQUEST_BODY} bytes) or unreadable: {e}"),
            )
        }
    };

    let (out_body, req_model, prefix_hash) = prepare_body(&endpoint, body_bytes);

    // Select and iterate over ordered backend candidates with transport-level
    // failover. Unknown models rejected before the meter guard starts.
    let candidates = match state.registry.route_candidates(RouteRequest {
        model: req_model.as_deref(),
        prefix_hash,
    }) {
        Ok(c) => c,
        Err(twl_backends::RouteError::UnknownModel(m)) => return unknown_model_error(&m),
        Err(twl_backends::RouteError::MissingModel) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "request must specify a model".to_string(),
            );
        }
    };

    // out_body is Bytes – cheap to clone for each attempt.

    struct Chosen {
        idx: usize,
        resp: reqwest::Response,
        load: twl_backends::LoadGuard,
        meter: MeterGuard,
    }

    let mut chosen: Option<Chosen> = None;

    for (pos, &candidate_idx) in candidates.iter().enumerate() {
        let backend = state.registry.backend(candidate_idx);
        let load = state.registry.begin_load(candidate_idx);
        // Start the meter before the upstream call so the prefill/header-wait
        // interval counts as GPU-busy time, and so a terminal transport failure
        // still records one energy row (its guard drops on the failover path).
        let meter = backend.meter.begin();
        let target = format!("{}{}", backend.upstream, path_and_query);

        let req = state
            .client
            .request(method.clone(), &target)
            .body(out_body.clone());
        let req = headers::forward_request_headers(req, &headers, key_consumed);

        // Inject backend auth headers (overrides any client header of the same name).
        let mut req = match req.build() {
            Ok(r) => r,
            Err(e) => return upstream_unavailable_response(e),
        };
        for (name, value) in &backend.auth_headers {
            match (
                HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(value),
            ) {
                (Ok(n), Ok(v)) => {
                    req.headers_mut().insert(n, v);
                }
                _ => tracing::warn!(header = %name, "invalid backend header value, skipped"),
            }
        }

        // Only connection-phase failures are retried on the next candidate.
        // A connect failure means the upstream never received the request, so
        // re-issuing cannot double-execute the completion. Any other error
        // (read timeout, reset, etc.) may have occurred after the upstream
        // began processing and is therefore surfaced to the client as a 502
        // rather than retried on another backend. A completed HTTP exchange
        // including an upstream 5xx is still treated as success and returned
        // as-is.
        match state.client.execute(req).await {
            Ok(resp) => {
                chosen = Some(Chosen {
                    idx: candidate_idx,
                    resp,
                    load,
                    meter,
                });
                break;
            }
            Err(e) => {
                let is_last = pos + 1 == candidates.len();
                if e.is_connect() && !is_last {
                    tracing::warn!(
                        backend = candidate_idx,
                        error = %e,
                        "upstream connect failure, retrying next candidate"
                    );
                    // load and meter guards drop here; on a non-terminal
                    // candidate the meter's tiny interval is recorded,
                    // consistent with how a terminal transport failure is
                    // accounted. Continue to next.
                    continue;
                }
                return upstream_unavailable_response(e);
            }
        }
    }

    let Chosen {
        idx: backend_idx,
        resp: upstream_resp,
        load,
        meter,
    } = chosen.expect("loop always returns or captures on success");

    let status = upstream_resp.status();
    let kind = StreamKind::classify(&upstream_resp, status);

    // One log line per proxied request, once the upstream status is known.
    tracing::info!(
        path = %endpoint,
        backend = state.registry.backend(backend_idx).provider.name(),
        backend_index = backend_idx,
        model = req_model.as_deref().unwrap_or("-"),
        status = status.as_u16(),
        stream = kind.is_streaming(),
        strategy = ?state.registry.strategy(),
        "Serving"
    );

    // Build the response with safe headers copied from the upstream.
    let builder = headers::forward_response_headers(
        Response::builder().status(status),
        upstream_resp.headers(),
    );

    let api_key_id = api_key_info.map(|k| k.key_id);
    let ctx = AccountingContext::new(
        state.accounting.clone(),
        endpoint,
        Arc::new(req_model),
        api_key_id,
    );

    let body = accounting_body(
        kind,
        upstream_resp.bytes_stream(),
        ctx,
        meter,
        load,
        fair_queue_guard,
    );
    builder.body(body).unwrap_or_else(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("build resp: {e}"),
        )
    })
}

/// How an upstream response body should be forwarded and accounted for.
/// All variants forward bytes verbatim; they differ only in usage parsing.
enum StreamKind {
    /// Server-Sent Events (`text/event-stream`).
    Sse,
    /// Newline-delimited JSON (Ollama's `application/x-ndjson`).
    Ndjson,
    /// A single buffered body. `parse` is false for error responses.
    Buffered { parse: bool },
}

impl StreamKind {
    /// Classify upstream response body format (SSE, NDJSON, or buffered).
    /// A buffered body is only inspected when the status is successful.
    fn classify(resp: &reqwest::Response, status: StatusCode) -> Self {
        if is_event_stream(resp) {
            StreamKind::Sse
        } else if is_ndjson_stream(resp) {
            // NDJSON is line-delimited without the `data:` framing.
            StreamKind::Ndjson
        } else {
            StreamKind::Buffered {
                parse: status.is_success(),
            }
        }
    }

    /// Whether this variant is a streamed response (SSE or NDJSON).
    fn is_streaming(&self) -> bool {
        matches!(self, StreamKind::Sse | StreamKind::Ndjson)
    }
}

/// Wrap the upstream byte stream in an accounting stream. Forwards bytes
/// verbatim; usage is recorded on `UsageRecorder::drop`.
fn accounting_body(
    kind: StreamKind,
    stream: impl futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
    ctx: AccountingContext,
    guard: MeterGuard,
    load: twl_backends::LoadGuard,
    fair_queue: crate::fair_queue::FairQueueGuard,
) -> Body {
    match kind {
        StreamKind::Sse => {
            Body::from_stream(sse_accounting_stream(stream, ctx, guard, load, fair_queue))
        }
        StreamKind::Ndjson => Body::from_stream(ndjson_accounting_stream(
            stream, ctx, guard, load, fair_queue,
        )),
        StreamKind::Buffered { parse } => Body::from_stream(buffered_accounting_stream(
            stream, ctx, guard, load, fair_queue, parse,
        )),
    }
}

/// Whether the upstream response is a Server-Sent Events stream.
fn is_event_stream(resp: &reqwest::Response) -> bool {
    content_type_matches(resp, "text/event-stream")
}

/// Whether the upstream response is a newline-delimited JSON stream.
fn is_ndjson_stream(resp: &reqwest::Response) -> bool {
    content_type_matches(resp, "application/x-ndjson")
        || content_type_matches(resp, "application/jsonl")
        || content_type_matches(resp, "application/x-jsonlines")
}

/// Case-insensitive exact match of Content-Type against a media type,
/// stripping `;` parameters. `text/event-streaming` does not match
/// `text/event-stream`; a needle in a parameter value does not match.
pub(crate) fn content_type_matches(resp: &reqwest::Response, media_type: &str) -> bool {
    let lower = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    match lower {
        Some(mut s) => {
            if let Some(semi) = s.find(';') {
                s.truncate(semi);
            }
            s.trim() == media_type
        }
        None => false,
    }
}

/// 401 JSON response for missing or invalid API key.
fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(HeaderName::from_static("content-type"), "application/json")
        .header(HeaderName::from_static("www-authenticate"), "Bearer")
        .body(Body::from(
            serde_json::json!({"error": "invalid or missing API key"}).to_string(),
        ))
        .expect("unauthorized response")
}

/// 404 response for unknown model, in OpenAI error shape.
fn unknown_model_error(model: &str) -> Response {
    tracing::warn!(model = %model, "rejected request for unknown model");
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(HeaderName::from_static("content-type"), "application/json")
        .body(Body::from(
            serde_json::json!({
                "error": {
                    "message": format!("model '{model}' not found"),
                    "type": "invalid_request_error",
                    "code": "model_not_found"
                }
            })
            .to_string(),
        ))
        .expect("error response")
}

/// Build a JSON error response and log the message at WARN level.
fn error_response(status: StatusCode, msg: String) -> Response {
    tracing::warn!("{msg}");
    Response::builder()
        .status(status)
        .header(HeaderName::from_static("content-type"), "application/json")
        .body(Body::from(
            serde_json::json!({ "error": { "message": msg } }).to_string(),
        ))
        .expect("error response")
}

/// Log an upstream failure in detail without exposing its URL or transport
/// diagnostics in the client response.
fn upstream_unavailable_response(error: impl std::fmt::Display) -> Response {
    tracing::warn!(%error, "upstream request failed");
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(HeaderName::from_static("content-type"), "application/json")
        .body(Body::from(
            serde_json::json!({ "error": { "message": "upstream unavailable" } }).to_string(),
        ))
        .expect("error response")
}

#[cfg(test)]
mod validation_limit_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{oneshot, Semaphore};

    use super::{run_with_validation_slot, upstream_unavailable_response};

    #[tokio::test]
    async fn upstream_error_response_does_not_expose_internal_url() {
        const INTERNAL: &str = "http://10.23.45.67:8080/private/completions";
        let response = upstream_unavailable_response(format!("request failed for {INTERNAL}"));
        assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("response body");
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(INTERNAL), "internal URL leaked in {body}");
        assert!(body.contains("upstream unavailable"));
    }

    #[tokio::test]
    async fn cancellation_keeps_slot_until_detached_validation_finishes() {
        let slots = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();

        let validation = tokio::spawn(run_with_validation_slot(Arc::clone(&slots), async move {
            let _ = started_tx.send(());
            let _ = finish_rx.await;
        }));

        started_rx.await.expect("validation started");
        validation.abort();
        assert!(validation
            .await
            .expect_err("caller task was cancelled")
            .is_cancelled());
        assert_eq!(slots.available_permits(), 0);

        finish_tx.send(()).expect("release detached validation");
        let permit = tokio::time::timeout(Duration::from_secs(1), slots.acquire())
            .await
            .expect("validation slot was eventually released")
            .expect("validation semaphore remains open");
        drop(permit);
    }
}
