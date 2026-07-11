//! Tests for request-body inspection and `stream_options` injection.
//!
//! Covers empty body, non-JSON, model extraction, streaming `include_usage`
//! injection, null stream_options normalization, and endpoint detection.

use bytes::Bytes;
use serde_json::Value;

use crate::proxy::body::prepare_body;

/// Shorthand to parse JSON bytes.
fn parse(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).expect("valid JSON")
}

#[test]
fn empty_body_untouched() {
    let (out, model, _hash) = prepare_body("/v1/chat/completions", Bytes::new());
    assert!(out.is_empty());
    assert!(model.is_none());
}

#[test]
fn non_json_body_forwarded_verbatim() {
    let body = Bytes::from_static(b"not json at all");
    let (out, model, _hash) = prepare_body("/v1/chat/completions", body.clone());
    assert_eq!(out, body);
    assert!(model.is_none());
}

#[test]
fn model_extracted_without_rewrite_for_non_stream() {
    let body = Bytes::from(r#"{"model":"m","stream":false}"#);
    let (out, model, _hash) = prepare_body("/v1/chat/completions", body.clone());
    assert_eq!(model.as_deref(), Some("m"));
    assert_eq!(out, body);
}

#[test]
fn streaming_chat_injects_include_usage() {
    let body = Bytes::from(r#"{"model":"m","stream":true}"#);
    let (out, model, _hash) = prepare_body("/v1/chat/completions", body);
    assert_eq!(model.as_deref(), Some("m"));
    let v = parse(&out);
    assert_eq!(v["stream_options"]["include_usage"], Value::Bool(true));
}

#[test]
fn streaming_normalizes_null_stream_options() {
    let body = Bytes::from(r#"{"model":"m","stream":true,"stream_options":null}"#);
    let (out, _, _hash) = prepare_body("/v1/chat/completions", body);
    let v = parse(&out);
    assert_eq!(v["stream_options"]["include_usage"], Value::Bool(true));
}

#[test]
fn streaming_preserves_existing_stream_options() {
    let body = Bytes::from(r#"{"model":"m","stream":true,"stream_options":{"foo":1}}"#);
    let (out, _, _hash) = prepare_body("/v1/chat/completions", body);
    let v = parse(&out);
    assert_eq!(v["stream_options"]["include_usage"], Value::Bool(true));
    assert_eq!(v["stream_options"]["foo"], 1);
}

#[test]
fn non_completion_endpoint_not_rewritten_even_when_streaming() {
    let body = Bytes::from(r#"{"model":"m","stream":true}"#);
    let (out, model, _hash) = prepare_body("/v1/embeddings", body.clone());
    assert_eq!(model.as_deref(), Some("m"));
    assert_eq!(out, body);
}

#[test]
fn bare_completions_endpoint_is_trackable() {
    let body = Bytes::from(r#"{"model":"m","stream":true}"#);
    let (out, _, _hash) = prepare_body("/v1/completions", body);
    let v = parse(&out);
    assert_eq!(v["stream_options"]["include_usage"], Value::Bool(true));
}

#[test]
fn identical_chat_bodies_yield_equal_prefix_hashes() {
    let body = Bytes::from(
        r#"{"messages":[{"role":"system","content":"you are helpful"},{"role":"user","content":"hello"}]}"#,
    );
    let (_, _, h1) = prepare_body("/v1/chat/completions", body.clone());
    let (_, _, h2) = prepare_body("/v1/chat/completions", body);
    assert_eq!(h1, h2);
    assert!(h1.is_some());
}

#[test]
fn prefix_hash_stable_across_later_turns() {
    // First request: system + first user message.
    let first = Bytes::from(
        r#"{"messages":[{"role":"system","content":"be concise"},{"role":"user","content":"what is rust"}]}"#,
    );
    let (_, _, h_first) = prepare_body("/v1/chat/completions", first);

    // Later request: same system and same first user, plus extra turns.
    let later = Bytes::from(
        r#"{"messages":[{"role":"system","content":"be concise"},{"role":"user","content":"what is rust"},{"role":"assistant","content":"a language"},{"role":"user","content":"tell me more"}]}"#,
    );
    let (_, _, h_later) = prepare_body("/v1/chat/completions", later);

    assert_eq!(h_first, h_later);
}

#[test]
fn different_first_user_message_yields_different_hash() {
    let body_a = Bytes::from(
        r#"{"messages":[{"role":"system","content":"be concise"},{"role":"user","content":"hello"}]}"#,
    );
    let body_b = Bytes::from(
        r#"{"messages":[{"role":"system","content":"be concise"},{"role":"user","content":"goodbye"}]}"#,
    );
    let (_, _, ha) = prepare_body("/v1/chat/completions", body_a);
    let (_, _, hb) = prepare_body("/v1/chat/completions", body_b);
    assert_ne!(ha, hb);
}

#[test]
fn embeddings_endpoint_yields_none_prefix_hash() {
    let body = Bytes::from(r#"{"input":"text"}"#);
    let (_, _, h) = prepare_body("/v1/embeddings", body);
    assert!(h.is_none());
}

#[test]
fn bare_completions_prompt_hash() {
    let body = Bytes::from(r#"{"prompt":"once upon a time"}"#);
    let (_, _, h1) = prepare_body("/v1/completions", body.clone());
    let (_, _, h2) = prepare_body("/v1/completions", body);
    assert_eq!(h1, h2);
    assert!(h1.is_some());
}
