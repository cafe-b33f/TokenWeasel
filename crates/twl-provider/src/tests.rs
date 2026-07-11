//! Tests for [`ProviderKind`] serde, auth headers, endpoint paths, and
//! models-list parsing for OpenAI-shape and Ollama responses.

use crate::{detect_model_kind, ModelEntry, ModelKind, ProviderKind};

// Serde round-trip

#[test]
fn kind_round_trip() {
    for (variant, name) in [
        (ProviderKind::LlamaCpp, "llamacpp"),
        (ProviderKind::Ollama, "ollama"),
        (ProviderKind::LmStudio, "lmstudio"),
        (ProviderKind::DeepSeek, "deepseek"),
        (ProviderKind::OpenRouter, "openrouter"),
        (ProviderKind::Vllm, "vllm"),
    ] {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, format!("\"{name}\""));
        let parsed: ProviderKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }
}

#[test]
fn default_is_llamacpp() {
    assert_eq!(ProviderKind::default(), ProviderKind::LlamaCpp);
}

#[test]
fn name_returns_lowercase_str() {
    assert_eq!(ProviderKind::LlamaCpp.name(), "llamacpp");
    assert_eq!(ProviderKind::Ollama.name(), "ollama");
    assert_eq!(ProviderKind::LmStudio.name(), "lmstudio");
    assert_eq!(ProviderKind::DeepSeek.name(), "deepseek");
    assert_eq!(ProviderKind::OpenRouter.name(), "openrouter");
    assert_eq!(ProviderKind::Vllm.name(), "vllm");
}

// Auth headers

#[test]
fn auth_headers_with_key() {
    let headers = ProviderKind::LlamaCpp.auth_headers(Some("sk-test123"));
    assert_eq!(
        headers,
        vec![("authorization".to_string(), "Bearer sk-test123".to_string())]
    );
}

#[test]
fn auth_headers_without_key() {
    let headers: Vec<(String, String)> = ProviderKind::Ollama.auth_headers(None);
    assert!(headers.is_empty());
}

#[test]
fn auth_headers_empty_key() {
    let headers = ProviderKind::DeepSeek.auth_headers(Some(""));
    assert!(headers.is_empty());
}

// Models endpoint

#[test]
fn endpoint_per_variant() {
    assert_eq!(ProviderKind::LlamaCpp.models_endpoint(), "/v1/models");
    assert_eq!(ProviderKind::Ollama.models_endpoint(), "/api/tags");
    assert_eq!(ProviderKind::LmStudio.models_endpoint(), "/v1/models");
    assert_eq!(ProviderKind::DeepSeek.models_endpoint(), "/models");
    assert_eq!(ProviderKind::OpenRouter.models_endpoint(), "/v1/models");
    assert_eq!(ProviderKind::Vllm.models_endpoint(), "/v1/models");
}

// Parse models - OpenAI list

#[test]
fn parse_openai_list() {
    let body = r#"{
        "object":"list",
        "data":[
            {"id":"meta-llama/Meta-Llama-3-8B","object":"model"},
            {"id":"mistral-7b","object":"model"}
        ]
    }"#;
    let entries = ProviderKind::LlamaCpp
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, "meta-llama/Meta-Llama-3-8B");
    assert_eq!(entries[1].id, "mistral-7b");
    // Raw entries are normalized to the common field whitelist.
    assert_eq!(
        entries[0]
            .raw
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["id", "object", "owned_by"]
    );
    assert_eq!(
        entries[0].raw.get("owned_by").unwrap().as_str().unwrap(),
        "llamacpp"
    );
}

// Parse models - verbose entries are normalized to the field whitelist

#[test]
fn parse_openai_list_strips_non_common_fields() {
    // OpenRouter-style verbose entry.
    let body = r#"{
        "object":"list",
        "data":[{
            "id":"openai/gpt-5.6-luna",
            "object":"model",
            "owned_by":"openrouter",
            "created":1783590864,
            "context_length":1050000,
            "knowledge_cutoff":"2026-02-16",
            "reasoning":{"default_effort":"medium","default_enabled":true},
            "name":"OpenAI: GPT-5.6 Luna",
            "description":"...",
            "pricing":{"prompt":"0.000001","completion":"0.000006"},
            "architecture":{"modality":"text+image+file->text"},
            "supported_parameters":["tools"],
            "top_provider":{"context_length":1050000},
            "per_request_limits":null
        }]
    }"#;
    let entries = ProviderKind::OpenRouter
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 1);
    let raw = entries[0].raw.as_object().unwrap();
    assert_eq!(
        raw.keys().collect::<Vec<_>>(),
        vec![
            "context_length",
            "created",
            "id",
            "knowledge_cutoff",
            "object",
            "owned_by",
            "reasoning"
        ]
    );
    assert_eq!(raw["created"].as_u64(), Some(1_783_590_864));
    assert_eq!(raw["context_length"].as_u64(), Some(1_050_000));
    assert_eq!(raw["knowledge_cutoff"].as_str(), Some("2026-02-16"));
    assert_eq!(raw["reasoning"]["default_effort"].as_str(), Some("medium"));
    assert_eq!(raw["owned_by"].as_str(), Some("openrouter"));
}

#[test]
fn parse_openai_list_context_length_from_meta_n_ctx() {
    // llama.cpp-style entry: context length lives in meta.n_ctx.
    let body = r#"{
        "object":"list",
        "data":[{
            "id":"unsloth/Qwen3.6-35B-A3B-MTP-GGUF:Q3_K_XL",
            "object":"model",
            "owned_by":"llamacpp",
            "created":1783768007,
            "aliases":["unsloth/Qwen3.6-35B-A3B-MTP-GGUF:Q3_K_XL"],
            "meta":{"n_ctx":262144,"n_params":35505251456,"size":17216578048},
            "tags":[]
        }]
    }"#;
    let entries = ProviderKind::LlamaCpp
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 1);
    let raw = entries[0].raw.as_object().unwrap();
    assert_eq!(raw["context_length"].as_u64(), Some(262_144));
    assert_eq!(raw["aliases"][0].as_str(), Some(entries[0].id.as_str()));
    assert!(raw.get("meta").is_none());
    assert!(raw.get("tags").is_none());
}

// Parse models - Ollama tags

#[test]
fn parse_ollama_tags() {
    let body = r#"{
        "models":[
            {"name":"llama3","size":4000000000},
            {"name":"mistral","size":3000000000}
        ]
    }"#;
    let entries = ProviderKind::Ollama.parse_models(body.as_bytes()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, "llama3");
    assert_eq!(
        entries[0].raw.get("id").unwrap().as_str().unwrap(),
        "llama3"
    );
    assert_eq!(
        entries[0].raw.get("object").unwrap().as_str().unwrap(),
        "model"
    );
    assert_eq!(
        entries[0].raw.get("owned_by").unwrap().as_str().unwrap(),
        "ollama"
    );
    // Provider-specific fields are stripped.
    assert!(entries[0].raw.get("size").is_none());
    assert_eq!(entries[1].id, "mistral");
}

// Parse models - Ollama model fallback

#[test]
fn parse_ollama_model_fallback() {
    let body = r#"{
        "models":[
            {"model":"qwen2.5:1.5b","size":1000000000}
        ]
    }"#;
    let entries = ProviderKind::Ollama.parse_models(body.as_bytes()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "qwen2.5:1.5b");
    assert!(entries[0].raw.get("size").is_none());
    assert!(entries[0].raw.get("name").is_none());
}

// Parse models - empty data

#[test]
fn parse_openai_list_empty_data() {
    let body = r#"{
        "object":"list",
        "data":[]
    }"#;
    let entries: Vec<ModelEntry> = ProviderKind::LlamaCpp
        .parse_models(body.as_bytes())
        .unwrap();
    assert!(entries.is_empty());
}

#[test]
fn parse_ollama_empty_models() {
    let body = r#"{"models":[]}"#;
    let entries: Vec<ModelEntry> = ProviderKind::Ollama.parse_models(body.as_bytes()).unwrap();
    assert!(entries.is_empty());
}

// Parse models - malformed JSON

#[test]
fn parse_malformed_json() {
    let body = b"not json at all {{{";
    let result = ProviderKind::LlamaCpp.parse_models(body);
    assert!(result.is_err());
}

// Parse models - entries missing id are skipped

#[test]
fn parse_skip_missing_id() {
    let body = r#"{
        "object":"list",
        "data":[
            {"id":"good-model","object":"model"},
            {},
            {"object":"model"},
            {"id":"also-good","object":"model"}
        ]
    }"#;
    let entries = ProviderKind::OpenRouter
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, "good-model");
    assert_eq!(entries[1].id, "also-good");
}

// Parse models - normalized shape

#[test]
fn parse_ollama_strips_provider_specific_fields() {
    let ollama = br#"{"models":[{"name":"llama3","digest":"sha256:abc","size":42,"details":{"format":"gguf"}}]}"#;
    let entry = ProviderKind::Ollama.parse_models(ollama).unwrap().remove(0);
    assert_eq!(entry.raw["id"], "llama3");
    assert_eq!(entry.raw["object"], "model");
    assert_eq!(entry.raw["owned_by"], "ollama");
    assert!(entry.raw.get("digest").is_none());
    assert!(entry.raw.get("size").is_none());
    assert!(entry.raw.get("details").is_none());
}

#[test]
fn parse_deepseek_owned_by_fallback() {
    // DeepSeek entries that lack owned_by get the provider name
    let body = r#"{
        "object":"list",
        "data":[
            {"id":"deepseek-chat","object":"model"}
        ]
    }"#;
    let entries = ProviderKind::DeepSeek
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].raw.get("owned_by").unwrap().as_str().unwrap(),
        "deepseek"
    );
    assert_eq!(
        entries[0]
            .raw
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["id", "object", "owned_by"]
    );
}

#[test]
fn parse_openai_list_with_created_and_owned_by() {
    let body = r#"{
        "object":"list",
        "data":[
            {
                "id":"gpt-4",
                "object":"model",
                "created":1690000000,
                "owned_by":"openai"
            }
        ]
    }"#;
    let entries = ProviderKind::LmStudio
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 1);
    let raw = &entries[0].raw;
    assert_eq!(raw.get("id").unwrap().as_str().unwrap(), "gpt-4");
    assert_eq!(raw.get("object").unwrap().as_str().unwrap(), "model");
    assert_eq!(raw.get("created").unwrap().as_u64().unwrap(), 1690000000);
    assert_eq!(raw.get("owned_by").unwrap().as_str().unwrap(), "openai");
    assert_eq!(
        raw.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["created", "id", "object", "owned_by"]
    );
}

// Model kind detection

#[test]
fn detect_embedding_lm_studio_scalar_type() {
    let source = serde_json::json!({"id": "text-embedding-3-small", "type": "embedding"});
    assert_eq!(detect_model_kind(&source), Some(ModelKind::Embedding));
}

#[test]
fn detect_embedding_lm_studio_plural_type() {
    let source = serde_json::json!({"id": "e5", "type": "embeddings"});
    assert_eq!(detect_model_kind(&source), Some(ModelKind::Embedding));
}

#[test]
fn detect_embedding_openrouter_modality_suffix() {
    let source = serde_json::json!({
        "id": "openai/text-embedding-3-small",
        "architecture": {"modality": "text->embedding"}
    });
    assert_eq!(detect_model_kind(&source), Some(ModelKind::Embedding));
}

#[test]
fn detect_embedding_openrouter_output_modalities_array() {
    let source = serde_json::json!({
        "id": "m",
        "architecture": {"output_modalities": ["text", "text->embeddings"]}
    });
    assert_eq!(detect_model_kind(&source), Some(ModelKind::Embedding));
}

#[test]
fn detect_embedding_ollama_capabilities() {
    let source = serde_json::json!({
        "id": "nomic-embed-text",
        "capabilities": ["embedding", "tools"]
    });
    assert_eq!(detect_model_kind(&source), Some(ModelKind::Embedding));
}

#[test]
fn detect_no_kind_for_chat_model() {
    let text_to_text = serde_json::json!({
        "id": "gpt-4",
        "architecture": {"modality": "text->text"}
    });
    assert_eq!(detect_model_kind(&text_to_text), None);
    let bare = serde_json::json!({"id": "gpt-4"});
    assert_eq!(detect_model_kind(&bare), None);
    // An unknown task string is not an embeddings model.
    let unknown = serde_json::json!({"id": "x", "type": "classifier"});
    assert_eq!(detect_model_kind(&unknown), None);
}

#[test]
fn normalized_model_carries_type_for_embeddings() {
    let body = r#"{
        "object":"list",
        "data":[
            {"id":"text-embedding-3-small","object":"model","type":"embedding"},
            {"id":"gpt-4","object":"model"}
        ]
    }"#;
    let entries = ProviderKind::LmStudio
        .parse_models(body.as_bytes())
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].raw.get("type").unwrap().as_str().unwrap(),
        "embedding"
    );
    assert!(
        entries[1].raw.get("type").is_none(),
        "chat model must not carry a type field"
    );
}

#[test]
fn model_kind_as_str_and_serde() {
    assert_eq!(ModelKind::Embedding.as_str(), "embedding");
    let json = serde_json::to_string(&ModelKind::Embedding).unwrap();
    assert_eq!(json, "\"embedding\"");
    let parsed: ModelKind = serde_json::from_str("\"embedding\"").unwrap();
    assert_eq!(parsed, ModelKind::Embedding);
}
