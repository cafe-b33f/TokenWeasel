//! Request-body inspection and rewriting.
//!
//! Extracts the model name from JSON request bodies and, for streaming
//! completion endpoints, injects `stream_options.include_usage=true` so
//! the upstream emits a final usage chunk.

use bytes::Bytes;
use rustc_hash::FxHasher;
use serde_json::Value;
use std::hash::{Hash, Hasher};

/// Cap on request body we buffer before rejecting (64 MiB). Exceeding this
/// returns 413 Payload Too Large.
pub const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// Maximum number of prompt-prefix bytes to hash.
const MAX_PREFIX_BYTES: usize = 8192;

/// Compute a stable hash of the prompt prefix for a given endpoint and JSON body.
///
/// For `chat/completions` endpoints, concatenates the first system message
/// content + separator byte `255` + first user message content, then hashes
/// at most `MAX_PREFIX_BYTES` bytes with FxHasher. Returns `None` when neither
/// system nor user string content is found.
///
/// For `/completions` endpoints, hashes the `prompt` string if present.
///
/// For all other endpoints returns `None`.
fn compute_prefix_hash(endpoint: &str, value: &Value) -> Option<u64> {
    if endpoint.contains("chat/completions") {
        let msgs = value.get("messages")?.as_array()?;
        let mut buf = Vec::new();
        let mut found = false;

        // First system message string content.
        if let Some(content) = msgs
            .iter()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
        {
            buf.extend_from_slice(content.as_bytes());
            found = true;
        }

        buf.push(255u8);

        // First user message string content.
        if let Some(content) = msgs
            .iter()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
        {
            buf.extend_from_slice(content.as_bytes());
            found = true;
        }

        if !found {
            return None;
        }

        let hash_len = buf.len().min(MAX_PREFIX_BYTES);
        let mut hasher = FxHasher::default();
        buf[..hash_len].hash(&mut hasher);
        return Some(hasher.finish());
    }

    if endpoint.ends_with("/completions") {
        let prompt = value.get("prompt")?.as_str()?;
        let bytes = prompt.as_bytes();
        let hash_len = bytes.len().min(MAX_PREFIX_BYTES);
        let mut hasher = FxHasher::default();
        bytes[..hash_len].hash(&mut hasher);
        return Some(hasher.finish());
    }

    None
}

/// Parse the request body (if JSON) to extract the model, and for streaming
/// completion requests inject `stream_options.include_usage=true`. Non-JSON
/// and empty bodies are forwarded verbatim.
pub fn prepare_body(endpoint: &str, body: Bytes) -> (Bytes, Option<String>, Option<u64>) {
    if body.is_empty() {
        return (body, None, None);
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, None, None),
    };
    let prefix_hash = compute_prefix_hash(endpoint, &v);

    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    let is_stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let trackable = endpoint.contains("chat/completions") || endpoint.ends_with("/completions");
    if is_stream && trackable {
        let mut v = v;
        inject_include_usage(&mut v);
        let rewritten = serde_json::to_vec(&v)
            .map(Bytes::from)
            .unwrap_or(body.clone());
        return (rewritten, model, prefix_hash);
    }
    (body, model, prefix_hash)
}

/// Force `stream_options.include_usage=true` on a streaming request body,
/// creating or normalizing `stream_options` as needed.
fn inject_include_usage(v: &mut Value) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    let opts = obj
        .entry("stream_options")
        .or_insert_with(|| Value::Object(Default::default()));
    if !opts.is_object() {
        *opts = Value::Object(Default::default());
    }
    if let Some(opts_obj) = opts.as_object_mut() {
        opts_obj.insert("include_usage".to_string(), Value::Bool(true));
    }
}
