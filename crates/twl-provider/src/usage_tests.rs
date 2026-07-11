//! Tests for usage extraction from OpenAI, llama.cpp native, Ollama native,
//! SSE/NDJSON line parsing, heuristic scavenger, DeepSeek cached-token
//! fallback, and edge cases (negative counts, overflow, clamping).

use serde_json::json;

use crate::usage::{extract_usage, parse_ndjson_line, parse_sse_line, scavenge_usage};

const EP: &str = "/v1/chat/completions";

#[test]
fn openai_chat_usage() {
    let v = json!({
        "model": "my-model",
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    });
    let u = extract_usage(&v, EP, &None).expect("usage");
    assert_eq!(u.model, "my-model");
    assert_eq!(u.endpoint, EP);
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 5);
    assert_eq!(u.total_tokens, 15);
    assert_eq!(u.cached_tokens, 0);
}

#[test]
fn openai_total_defaults_to_prompt_plus_completion() {
    let v = json!({
        "model": "m",
        "usage": { "prompt_tokens": 7, "completion_tokens": 3 }
    });
    let u = extract_usage(&v, EP, &None).unwrap();
    assert_eq!(u.total_tokens, 10);
}

#[test]
fn total_only_non_generative_usage_is_classified_as_input() {
    for endpoint in ["/v1/rerank", "/score", "/pooling"] {
        let response = json!({
            "model": "embedding-model",
            "usage": { "total_tokens": 37 }
        });
        let usage = extract_usage(&response, endpoint, &None).expect("usage");
        assert_eq!(usage.input_tokens, 37, "endpoint {endpoint}");
        assert_eq!(usage.output_tokens, 0, "endpoint {endpoint}");
        assert_eq!(usage.total_tokens, 37, "endpoint {endpoint}");
        assert_eq!(
            usage.total_tokens,
            usage.input_tokens + usage.output_tokens,
            "endpoint {endpoint}"
        );
    }
}

#[test]
fn openai_cached_tokens_from_details() {
    let v = json!({
        "model": "m",
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 40 }
        }
    });
    let u = extract_usage(&v, EP, &None).unwrap();
    assert_eq!(u.cached_tokens, 40);
}

#[test]
fn anthropic_cache_tokens_are_included_in_non_streaming_input() {
    let v = json!({
        "model": "claude-serving",
        "usage": {
            "input_tokens": 50,
            "output_tokens": 25,
            "cache_creation_input_tokens": 1_000,
            "cache_read_input_tokens": 200_000
        }
    });

    let u = extract_usage(&v, "/v1/messages", &None).expect("usage");
    assert_eq!(u.input_tokens, 201_050);
    assert_eq!(u.output_tokens, 25);
    assert_eq!(u.cached_tokens, 200_000);
    assert_eq!(u.total_tokens, 201_075);
}

#[test]
fn responses_api_nested_and_alt_field_names() {
    // Streamed Responses API: final counts live under `response.usage`, using
    // input/output rather than prompt/completion field names.
    let v = json!({
        "type": "response.completed",
        "response": {
            "model": "resp-model",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 8,
                "input_tokens_details": { "cached_tokens": 5 }
            }
        }
    });
    let u = extract_usage(&v, "/v1/responses", &None).unwrap();
    assert_eq!(u.model, "resp-model");
    assert_eq!(u.input_tokens, 12);
    assert_eq!(u.output_tokens, 8);
    assert_eq!(u.cached_tokens, 5);
    assert_eq!(u.total_tokens, 20);
}

#[test]
fn model_falls_back_to_request_model() {
    let v = json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 } });
    let u = extract_usage(&v, EP, &Some("from-request".to_string())).unwrap();
    assert_eq!(u.model, "from-request");
}

#[test]
fn model_falls_back_to_unknown() {
    let v = json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 } });
    let u = extract_usage(&v, EP, &None).unwrap();
    assert_eq!(u.model, "unknown");
}

#[test]
fn llamacpp_native_terminal_event() {
    let v = json!({
        "stop": true,
        "model": "gguf",
        "tokens_evaluated": 30,
        "tokens_predicted": 12,
        "tokens_cached": 4
    });
    let u = extract_usage(&v, "/completion", &None).unwrap();
    assert_eq!(u.input_tokens, 30);
    assert_eq!(u.output_tokens, 12);
    assert_eq!(u.cached_tokens, 4);
    assert_eq!(u.total_tokens, 42);
}

#[test]
fn llamacpp_non_terminal_chunk_ignored() {
    // Non-terminal streaming chunks carry a running total; ignored to avoid
    // double counting.
    let v = json!({ "stop": false, "tokens_predicted": 3 });
    assert!(extract_usage(&v, "/completion", &None).is_none());
}

#[test]
fn no_usage_block_returns_none() {
    let v = json!({ "model": "m", "choices": [] });
    assert!(extract_usage(&v, EP, &None).is_none());
}

#[test]
fn null_usage_is_not_a_usage_block() {
    let v = json!({ "model": "m", "usage": null });
    assert!(extract_usage(&v, EP, &None).is_none());
}

#[test]
fn malformed_or_unrecognized_usage_is_not_a_zero_token_record() {
    for usage in [
        json!({}),
        json!([]),
        json!("not an accounting object"),
        json!({ "latency_ms": 42 }),
        json!({ "input_tokens": "12" }),
    ] {
        let response = json!({ "model": "m", "usage": usage });
        assert!(
            extract_usage(&response, EP, &None).is_none(),
            "malformed usage must not suppress missing-usage detection: {response}"
        );
    }
}

#[test]
fn sse_line_with_usage() {
    let line = b"data: {\"model\":\"m\",\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n";
    let u = parse_sse_line(line, EP, &None).expect("usage");
    assert_eq!(u.input_tokens, 2);
    assert_eq!(u.output_tokens, 3);
}

#[test]
fn sse_done_sentinel_is_none() {
    assert!(parse_sse_line(b"data: [DONE]\n", EP, &None).is_none());
}

#[test]
fn sse_non_data_line_is_none() {
    assert!(parse_sse_line(b": keep-alive\n", EP, &None).is_none());
}

#[test]
fn sse_data_chunk_without_usage_is_none() {
    let line = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
    assert!(parse_sse_line(line, EP, &None).is_none());
}

// --- Ollama native API -----------------------------------------------------

#[test]
fn ollama_native_terminal_message() {
    // Terminal message of a streamed /api/chat or /api/generate.
    let v = json!({
        "model": "llama3",
        "done": true,
        "prompt_eval_count": 26,
        "eval_count": 290
    });
    let u = extract_usage(&v, "/api/chat", &None).expect("usage");
    assert_eq!(u.model, "llama3");
    assert_eq!(u.input_tokens, 26);
    assert_eq!(u.output_tokens, 290);
    assert_eq!(u.total_tokens, 316);
    assert_eq!(u.cached_tokens, 0);
}

#[test]
fn ollama_embed_without_done_field() {
    // /api/embed reports prompt_eval_count but has no `done` field, so keying
    // on the count fields (not `done`) is what lets us catch it.
    let v = json!({ "model": "nomic", "embeddings": [[0.1, 0.2]], "prompt_eval_count": 8 });
    let u = extract_usage(&v, "/api/embed", &None).expect("usage");
    assert_eq!(u.input_tokens, 8);
    assert_eq!(u.output_tokens, 0);
}

#[test]
fn ollama_non_terminal_chunk_ignored() {
    // Intermediate streaming chunks omit the count fields, so they must not be
    // counted (which would double-count against the terminal message).
    let v = json!({ "model": "llama3", "done": false, "message": { "content": "hi" } });
    assert!(extract_usage(&v, "/api/chat", &None).is_none());
}

#[test]
fn ndjson_line_with_ollama_usage() {
    let line = b"{\"model\":\"llama3\",\"done\":true,\"prompt_eval_count\":3,\"eval_count\":9}\n";
    let u = parse_ndjson_line(line, "/api/generate", &None).expect("usage");
    assert_eq!(u.input_tokens, 3);
    assert_eq!(u.output_tokens, 9);
}

#[test]
fn ndjson_blank_line_is_none() {
    assert!(parse_ndjson_line(b"\n", "/api/generate", &None).is_none());
}

// --- Heuristic scavenger fallback ------------------------------------------

#[test]
fn scavenger_recovers_unknown_nested_shape() {
    // A hypothetical future/unknown provider: token counts nested under a
    // wrapper we have no adapter for, using synonym field names.
    let v = json!({
        "result": {
            "model_name": "mystery-v2",
            "metrics": { "prompt_token_count": 40, "candidates_token_count": 11 }
        }
    });
    let u = scavenge_usage(&v, "/v2/generate", &None).expect("scavenged usage");
    assert_eq!(u.model, "mystery-v2");
    assert_eq!(u.input_tokens, 40);
    assert_eq!(u.output_tokens, 11);
    assert_eq!(u.total_tokens, 51);
}

#[test]
fn scavenger_model_falls_back_to_request_then_unknown() {
    let v = json!({ "stats": { "total_token_count": 12 } });
    let from_req = scavenge_usage(&v, "/x", &Some("req-model".to_string())).unwrap();
    assert_eq!(from_req.model, "req-model");
    assert_eq!(from_req.total_tokens, 12);

    let unknown = scavenge_usage(&v, "/x", &None).unwrap();
    assert_eq!(unknown.model, "unknown");
}

#[test]
fn scavenger_returns_none_without_token_fields() {
    let v = json!({ "model": "m", "choices": [{ "text": "hello" }] });
    assert!(scavenge_usage(&v, EP, &None).is_none());
}

// --- Hardening: negative counts, overflow, cached > prompt ------------------

#[test]
fn openai_negative_counts_normalized() {
    let v = json!({
        "model": "m",
        "usage": { "prompt_tokens": -5, "completion_tokens": -10, "total_tokens": -3 }
    });
    let u = extract_usage(&v, EP, &None).expect("usage");
    assert_eq!(u.input_tokens, 0);
    assert_eq!(u.output_tokens, 0);
    assert_eq!(u.total_tokens, 0);
    assert_eq!(u.cached_tokens, 0);
}

#[test]
fn openai_cached_exceeds_prompt_clamped() {
    let v = json!({
        "model": "m",
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3,
            "prompt_tokens_details": { "cached_tokens": 999 }
        }
    });
    let u = extract_usage(&v, EP, &None).expect("usage");
    assert_eq!(u.cached_tokens, 5); // clamped to prompt
    assert_eq!(u.input_tokens, 5);
    assert!(u.cached_tokens <= u.input_tokens);
}

#[test]
fn i64_max_prompt_completion_no_panic_no_wrap() {
    let v = json!({
        "model": "m",
        "usage": { "prompt_tokens": i64::MAX, "completion_tokens": i64::MAX }
    });
    let u = extract_usage(&v, EP, &None).expect("usage");
    assert_eq!(u.input_tokens, i64::MAX);
    assert_eq!(u.output_tokens, i64::MAX);
    assert_eq!(u.total_tokens, i64::MAX.saturating_add(i64::MAX)); // saturates, no wrap
    assert!(u.cached_tokens >= 0);
}

#[test]
fn openai_explicit_negative_total_normalized() {
    // Explicit negative total_tokens should be derived from prompt+completion,
    // not silently clamped to zero.
    let v = json!({
        "model": "m",
        "usage": { "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": -999 }
    });
    let u = extract_usage(&v, EP, &None).expect("usage");
    assert_eq!(u.total_tokens, 30); // derived from prompt+completion
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 20);
}

#[test]
fn openai_explicit_zero_conflicts_with_positive_components() {
    // Explicit zero total when components are positive should be overridden.
    let v = json!({
        "model": "m",
        "usage": { "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 0 }
    });
    let u = extract_usage(&v, EP, &None).expect("usage");
    assert_eq!(u.total_tokens, 30); // derived from prompt+completion
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 20);
}

#[test]
fn llamacpp_negative_counts_and_cached_exceeds() {
    let v = json!({
        "stop": true,
        "model": "gguf",
        "tokens_evaluated": -1,
        "tokens_predicted": -2,
        "tokens_cached": 100
    });
    let u = extract_usage(&v, "/completion", &None).expect("usage");
    assert_eq!(u.input_tokens, 0);
    assert_eq!(u.output_tokens, 0);
    assert_eq!(u.cached_tokens, 0);
    assert_eq!(u.total_tokens, 0);
    assert!(u.cached_tokens <= u.input_tokens);
}

#[test]
fn llamacpp_i64_max_no_wrap() {
    let v = json!({
        "stop": true,
        "model": "gguf",
        "tokens_evaluated": i64::MAX,
        "tokens_predicted": i64::MAX,
        "tokens_cached": i64::MAX
    });
    let u = extract_usage(&v, "/completion", &None).expect("usage");
    assert_eq!(u.input_tokens, i64::MAX);
    assert_eq!(u.output_tokens, i64::MAX);
    assert_eq!(u.total_tokens, i64::MAX.saturating_add(i64::MAX));
    assert_eq!(u.cached_tokens, i64::MAX); // clamped to prompt == MAX
    assert!(u.cached_tokens <= u.input_tokens);
}

#[test]
fn ollama_negative_counts_normalized() {
    let v = json!({
        "done": true,
        "prompt_eval_count": -100,
        "eval_count": -200
    });
    let u = extract_usage(&v, "/api/chat", &None).expect("usage");
    assert_eq!(u.input_tokens, 0);
    assert_eq!(u.output_tokens, 0);
    assert_eq!(u.total_tokens, 0);
    assert_eq!(u.cached_tokens, 0);
}

#[test]
fn scavenger_negative_and_overflow_safety() {
    let v = json!({
        "stats": {
            "prompt_token_count": -1,
            "candidates_token_count": -2,
            "total_token_count": -3,
            "cached_tokens": 9999
        }
    });
    let u = scavenge_usage(&v, "/x", &None).expect("scavenged");
    assert!(u.input_tokens >= 0);
    assert!(u.output_tokens >= 0);
    assert!(u.total_tokens >= 0);
    assert!(u.cached_tokens >= 0);
    assert!(u.cached_tokens <= u.input_tokens);
}

#[test]
fn scavenger_all_fields_non_negative_i64_max() {
    let v = json!({
        "stats": {
            "prompt_token_count": i64::MAX,
            "candidates_token_count": i64::MAX,
            "total_token_count": i64::MAX,
            "cached_tokens": i64::MAX
        }
    });
    let u = scavenge_usage(&v, "/x", &None).expect("scavenged");
    assert!(u.input_tokens >= 0);
    assert!(u.output_tokens >= 0);
    assert!(u.total_tokens >= 0);
    assert!(u.cached_tokens >= 0);
    assert!(u.cached_tokens <= u.input_tokens);
}

// --- DeepSeek cached-token fallback -----------------------------------------

#[test]
fn deepseek_cached_tokens_from_prompt_cache_hit_tokens() {
    // DeepSeek may omit prompt_tokens_details.cached_tokens but include
    // prompt_cache_hit_tokens at the usage level.
    let v = json!({
        "model": "deepseek-chat",
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 25,
            "total_tokens": 75,
            "prompt_cache_hit_tokens": 30
        }
    });
    let u = extract_usage(&v, "/v1/chat/completions", &None).unwrap();
    assert_eq!(u.input_tokens, 50);
    assert_eq!(u.output_tokens, 25);
    assert_eq!(u.cached_tokens, 30);
}

#[test]
fn deepseek_cached_tokens_details_wins_over_cache_hit_tokens() {
    // When both prompt_tokens_details.cached_tokens and prompt_cache_hit_tokens
    // are present, the nested details field takes precedence.
    let v = json!({
        "model": "deepseek-chat",
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 25,
            "prompt_tokens_details": { "cached_tokens": 40 },
            "prompt_cache_hit_tokens": 10
        }
    });
    let u = extract_usage(&v, "/v1/chat/completions", &None).unwrap();
    assert_eq!(u.cached_tokens, 40); // details wins
}

#[test]
fn deepseek_cached_clamped_to_prompt() {
    // prompt_cache_hit_tokens is still subject to the clamping rule.
    let v = json!({
        "model": "deepseek-chat",
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "prompt_cache_hit_tokens": 999
        }
    });
    let u = extract_usage(&v, "/v1/chat/completions", &None).unwrap();
    assert_eq!(u.cached_tokens, 10); // clamped to prompt
}
