//! Tests for stream classification by Content-Type (case-insensitive exact match).
//!
//! Covers SSE, NDJSON, mixed-case, parameter stripping, near-match false
//! positives, needle-in-param false positives, and missing headers.

use axum::http::header::{self, HeaderValue};
use axum::http::HeaderMap;
use bytes::Bytes;
use reqwest::{Body as ReqwestBody, Response};

/// Build a mock `reqwest::Response` with the given Content-Type value.
fn mock_response(content_type: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
    let http_resp = http::Response::new(ReqwestBody::from(Bytes::new()));
    let mut resp = http_resp;
    *resp.headers_mut() = headers;
    *resp.status_mut() = axum::http::StatusCode::OK;
    resp.into()
}

// SSE Content-Type (mixed case)

#[test]
fn sse_content_type_uppercase() {
    let resp = mock_response("TEXT/EVENT-STREAM");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

#[test]
fn sse_content_type_mixed_case() {
    let resp = mock_response("Text/Event-Stream");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

#[test]
fn sse_content_type_with_charset() {
    let resp = mock_response("text/event-stream; charset=utf-8");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

// SSE Content-Type with extra parameters

#[test]
fn sse_multiple_params() {
    let resp = mock_response("text/event-stream; charset=utf-8; boundary=something");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

#[test]
fn sse_whitespace_around_params() {
    let resp = mock_response("text/event-stream ; charset=utf-8 ");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

// NDJSON Content-Type (mixed case)

#[test]
fn ndjson_uppercase() {
    let resp = mock_response("APPLICATION/X-NDJSON");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "application/x-ndjson"
    ));
}

#[test]
fn ndjson_mixed_case() {
    let resp = mock_response("Application/X-Ndjson");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "application/x-ndjson"
    ));
}

#[test]
fn jsonl_uppercase() {
    let resp = mock_response("APPLICATION/JSONL");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "application/jsonl"
    ));
}

#[test]
fn jsonlines_uppercase() {
    let resp = mock_response("APPLICATION/X-JSONLINES");
    assert!(crate::proxy::content_type_matches(
        &resp,
        "application/x-jsonlines"
    ));
}

// Non-streaming Content-Type

#[test]
fn unrelated_content_type_not_stream() {
    let resp = mock_response("application/json");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "application/x-ndjson"
    ));
}

// Near-match false positives (should NOT match)

#[test]
fn sse_near_match_streaming() {
    // `text/event-streaming` must not match `text/event-stream`
    let resp = mock_response("text/event-streaming");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

#[test]
fn sse_near_match_extended() {
    let resp = mock_response("text/event-stream-extra");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

#[test]
fn ndjson_near_match_extra() {
    // `application/x-ndjson-extra` must not match `application/x-ndjson`
    let resp = mock_response("application/x-ndjson-extra");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "application/x-ndjson"
    ));
}

#[test]
fn jsonl_near_match_extra() {
    let resp = mock_response("application/jsonl-extra");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "application/jsonl"
    ));
}

// Needle only in a parameter (should NOT match)

#[test]
fn sse_needle_in_param_only() {
    // The string `text/event-stream` appears only in a param value, not as the media type
    let resp = mock_response("application/octet-stream; name=text/event-stream");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}

#[test]
fn ndjson_needle_in_param_only() {
    let resp = mock_response("text/plain; type=application/x-ndjson");
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "application/x-ndjson"
    ));
}

// No Content-Type header

#[test]
fn no_content_type_header() {
    let headers = HeaderMap::new();
    let http_resp = http::Response::new(ReqwestBody::from(Bytes::new()));
    let mut resp = http_resp;
    *resp.headers_mut() = headers;
    *resp.status_mut() = axum::http::StatusCode::OK;
    let resp: Response = resp.into();
    assert!(!crate::proxy::content_type_matches(
        &resp,
        "text/event-stream"
    ));
}
