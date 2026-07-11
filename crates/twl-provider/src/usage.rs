//! Extracting token-usage records from upstream responses.
//!
//! Parses response bodies (JSON and streaming) from OpenAI, llama.cpp native,
//! Ollama native, and unknown shapes via a heuristic scavenger. The model
//! name is resolved from the response, the request, or defaults to `"unknown"`.

use serde_json::Value;

use twl_types::Usage;

/// Tries four response shapes in priority order:
/// 1. OpenAI `usage` block with `prompt_tokens`/`completion_tokens`.
/// 2. OpenAI Responses API nested `response.usage` with `input_tokens`/`output_tokens`.
/// 3. llama.cpp native `/completion` terminal event (`stop: true`).
/// 4. Ollama native `/api/chat`/`/api/generate`/`/api/embed` with `prompt_eval_count`/`eval_count`.
pub fn extract_usage(v: &Value, endpoint: &str, req_model: &Option<String>) -> Option<Usage> {
    // The container holding `usage`/`model` is either the object itself, a
    // streamed Responses `response`, or an Anthropic `message_start` message.
    let root = v
        .get("response")
        .filter(|r| r.get("usage").is_some())
        .or_else(|| v.get("message").filter(|m| m.get("usage").is_some()))
        .unwrap_or(v);

    let model = || {
        root.get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .or_else(|| req_model.clone())
            .unwrap_or_else(|| "unknown".to_string())
    };

    if let Some(usage) = root.get("usage").filter(|u| !u.is_null()) {
        return usage_from_openai(usage, model(), endpoint);
    }

    // Native llama.cpp `/completion`/`/infill`: only the terminal event
    // (`stop: true`) carries the final token counts; streaming emits many
    // non-terminal chunks whose `tokens_predicted` is only a running total, so
    // we ignore those to avoid double counting.
    if v.get("stop").and_then(|s| s.as_bool()) == Some(true) {
        return usage_from_llamacpp(v, model, endpoint);
    }

    // Native Ollama (`/api/chat`, `/api/generate`, `/api/embed`): token counts
    // are reported as `prompt_eval_count`/`eval_count`. For streamed
    // generations only the terminal (`done: true`) message carries them;
    // non-terminal chunks omit the fields, so keying on their presence (rather
    // than on `done`) both avoids double counting and still catches embeddings,
    // whose response has no `done` field.
    if v.get("prompt_eval_count").is_some() || v.get("eval_count").is_some() {
        return usage_from_ollama(v, model, endpoint);
    }

    None
}

/// Accepts chat (`prompt_tokens`/`completion_tokens`) and Responses API
/// (`input_tokens`/`output_tokens`) field names. Falls back to DeepSeek's
/// `prompt_cache_hit_tokens` when the nested `*_details.cached_tokens` is
/// absent.
fn usage_from_openai(usage: &Value, model: String, endpoint: &str) -> Option<Usage> {
    let num = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(|n| n.as_i64()))
    };
    let nested_cached = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|n| n.as_i64());
    let has_recognized_count = num(&[
        "prompt_tokens",
        "input_tokens",
        "completion_tokens",
        "output_tokens",
        "total_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "prompt_cache_hit_tokens",
    ])
    .is_some()
        || nested_cached.is_some();
    if !has_recognized_count {
        return None;
    }

    let raw_prompt = num(&["prompt_tokens", "input_tokens"]);
    let raw_completion = num(&["completion_tokens", "output_tokens"]);
    let raw_total = num(&["total_tokens"]);
    let is_non_generative = endpoint.ends_with("/rerank")
        || endpoint.ends_with("/score")
        || endpoint.ends_with("/pooling");
    let total_only_input = if is_non_generative && raw_prompt.is_none() && raw_completion.is_none()
    {
        raw_total.unwrap_or(0).max(0)
    } else {
        0
    };
    let base_prompt = raw_prompt.unwrap_or(total_only_input).max(0);
    let anthropic_cache_creation = num(&["cache_creation_input_tokens"]).unwrap_or(0).max(0);
    let anthropic_cache_read = num(&["cache_read_input_tokens"]).unwrap_or(0).max(0);
    let is_anthropic_usage = usage.get("cache_creation_input_tokens").is_some()
        || usage.get("cache_read_input_tokens").is_some();
    let prompt = base_prompt
        .saturating_add(anthropic_cache_creation)
        .saturating_add(anthropic_cache_read);
    let completion = raw_completion.unwrap_or(0).max(0);
    let total = match raw_total {
        Some(t) if t > 0 => t,
        Some(t) if t == 0 && prompt == 0 && completion == 0 => 0,
        _ => prompt.saturating_add(completion),
    };
    // Try nested prompt_tokens_details / input_tokens_details.cached_tokens
    // first, then fall back to DeepSeek's prompt_cache_hit_tokens.
    let cached = if is_anthropic_usage {
        anthropic_cache_read
    } else {
        match nested_cached {
            Some(v) => v,
            None => usage
                .get("prompt_cache_hit_tokens")
                .and_then(|n| n.as_i64())
                .unwrap_or(0),
        }
    }
    .max(0)
    .min(prompt);
    Some(Usage {
        model,
        endpoint: endpoint.to_string(),
        input_tokens: prompt,
        output_tokens: completion,
        cached_tokens: cached,
        total_tokens: total,
        api_key_id: None,
    })
}

/// Shared logic for Ollama- and llama.cpp-style count fields.
fn usage_from_counts(
    v: &Value,
    prompt_key: &str,
    completion_key: &str,
    cached_key: Option<&str>,
    model: impl FnOnce() -> String,
    endpoint: &str,
) -> Option<Usage> {
    let raw_prompt = v.get(prompt_key).and_then(|n| n.as_i64());
    let raw_completion = v.get(completion_key).and_then(|n| n.as_i64());
    if raw_prompt.is_none() && raw_completion.is_none() {
        return None;
    }
    let prompt = raw_prompt.unwrap_or(0).max(0);
    let completion = raw_completion.unwrap_or(0).max(0);
    let cached = match cached_key {
        Some(k) => v
            .get(k)
            .and_then(|n| n.as_i64())
            .unwrap_or(0)
            .max(0)
            .min(prompt),
        None => 0,
    };
    Some(Usage {
        model: model(),
        endpoint: endpoint.to_string(),
        input_tokens: prompt,
        output_tokens: completion,
        cached_tokens: cached,
        total_tokens: prompt.saturating_add(completion),
        api_key_id: None,
    })
}

/// Ollama has no cached-prompt concept, so `cached_tokens` is always 0.
fn usage_from_ollama(v: &Value, model: impl FnOnce() -> String, endpoint: &str) -> Option<Usage> {
    usage_from_counts(v, "prompt_eval_count", "eval_count", None, model, endpoint)
}

/// Only called once `stop: true` has been verified.
fn usage_from_llamacpp(v: &Value, model: impl FnOnce() -> String, endpoint: &str) -> Option<Usage> {
    usage_from_counts(
        v,
        "tokens_evaluated",
        "tokens_predicted",
        Some("tokens_cached"),
        model,
        endpoint,
    )
}

/// Whether a successful response on this endpoint is expected to carry a
/// usage block. Lets the caller distinguish genuine upstream format drift
/// from endpoints that simply never report usage (e.g. `/v1/models`).
pub fn expects_usage(endpoint: &str) -> bool {
    // Ollama's legacy embeddings endpoint returns no token counts (unlike its
    // replacement, `/api/embed`), so exclude it before the `/embeddings` rule
    // below would otherwise match it.
    if endpoint.ends_with("/api/embeddings") {
        return false;
    }
    endpoint.contains("chat/completions")
        || endpoint.ends_with("/completions")
        || endpoint.ends_with("/completion")
        || endpoint.ends_with("/infill")
        || endpoint.ends_with("/embeddings")
        || endpoint.ends_with("/responses")
        || endpoint.ends_with("/messages")
        || endpoint.ends_with("/rerank")
        || endpoint.ends_with("/score")
        || endpoint.ends_with("/pooling")
        || endpoint.ends_with("/api/chat")
        || endpoint.ends_with("/api/generate")
        || endpoint.ends_with("/api/embed")
}

/// Parses one Server-Sent Events line and extracts its token usage, if any.
///
/// Non-data lines, the `[DONE]` sentinel, malformed JSON, and events without
/// a recognized usage shape return `None`.
pub fn parse_sse_line(line: &[u8], endpoint: &str, req_model: &Option<String>) -> Option<Usage> {
    let line = String::from_utf8_lossy(line);
    let data = line.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    extract_usage(&v, endpoint, req_model)
}

/// Parses one newline-delimited JSON record and extracts its token usage.
///
/// Empty lines, malformed JSON, and records without a recognized usage shape
/// return `None`.
pub fn parse_ndjson_line(line: &[u8], endpoint: &str, req_model: &Option<String>) -> Option<Usage> {
    let line = String::from_utf8_lossy(line);
    let data = line.trim();
    if data.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    extract_usage(&v, endpoint, req_model)
}

/// Last-resort heuristic used when no known response shape matches:
/// recursively searches the JSON for the first object carrying a
/// recognizable token-count field. Only run on complete buffered responses
/// to avoid counting running totals.
pub fn scavenge_usage(v: &Value, endpoint: &str, req_model: &Option<String>) -> Option<Usage> {
    const PROMPT_KEYS: &[&str] = &[
        "prompt_tokens",
        "input_tokens",
        "tokens_evaluated",
        "prompt_eval_count",
        "prompt_token_count",
    ];
    const COMPLETION_KEYS: &[&str] = &[
        "completion_tokens",
        "output_tokens",
        "tokens_predicted",
        "eval_count",
        "completion_token_count",
        "candidates_token_count",
    ];
    const TOTAL_KEYS: &[&str] = &["total_tokens", "total_token_count"];
    const CACHED_KEYS: &[&str] = &["cached_tokens", "tokens_cached", "cache_read_input_tokens"];

    fn find_i64(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<i64> {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(|n| n.as_i64()))
    }

    fn scavenge_counts(v: &Value) -> Option<(i64, i64, i64, i64)> {
        match v {
            Value::Object(map) => {
                let prompt = find_i64(map, PROMPT_KEYS);
                let completion = find_i64(map, COMPLETION_KEYS);
                let total = find_i64(map, TOTAL_KEYS);
                if prompt.is_some() || completion.is_some() || total.is_some() {
                    let cached = find_i64(map, CACHED_KEYS)
                        .or_else(|| {
                            map.get("prompt_tokens_details")
                                .or_else(|| map.get("input_tokens_details"))
                                .and_then(|d| d.as_object())
                                .and_then(|d| find_i64(d, CACHED_KEYS))
                        })
                        .unwrap_or(0);
                    return Some((
                        prompt.unwrap_or(0),
                        completion.unwrap_or(0),
                        total.unwrap_or(0),
                        cached,
                    ));
                }
                map.values().find_map(scavenge_counts)
            }
            Value::Array(arr) => arr.iter().find_map(scavenge_counts),
            _ => None,
        }
    }

    fn scavenge_model(v: &Value) -> Option<String> {
        match v {
            Value::Object(map) => {
                for key in ["model", "model_name", "model_id"] {
                    if let Some(s) = map.get(key).and_then(|m| m.as_str()) {
                        return Some(s.to_string());
                    }
                }
                map.values().find_map(scavenge_model)
            }
            Value::Array(arr) => arr.iter().find_map(scavenge_model),
            _ => None,
        }
    }

    let (prompt, completion, total, cached) = scavenge_counts(v)?;
    let prompt = prompt.max(0);
    let completion = completion.max(0);
    let total = total.max(0);
    let cached = cached.max(0).min(prompt);
    let model = scavenge_model(v)
        .or_else(|| req_model.clone())
        .unwrap_or_else(|| "unknown".to_string());
    Some(Usage {
        model,
        endpoint: endpoint.to_string(),
        input_tokens: prompt,
        output_tokens: completion,
        cached_tokens: cached,
        total_tokens: if total > 0 {
            total
        } else {
            prompt.saturating_add(completion)
        },
        api_key_id: None,
    })
}
