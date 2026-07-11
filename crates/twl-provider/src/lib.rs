//! Provider abstraction for multi-backend LLM routing.
//!
//! Handles authentication headers, models-listing endpoint selection, and
//! response-body parsing for each supported provider.
//!
//! OpenAI-shape providers return `{"object":"list","data":[{"id":"...",...}]}`;
//! Ollama returns `{"models":[{"name":...}]}`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Provider dialect controlling authentication, model discovery, and usage parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// A llama.cpp-compatible backend.
    #[default]
    LlamaCpp,
    /// An Ollama backend.
    Ollama,
    /// An LM Studio backend.
    LmStudio,
    /// The DeepSeek API.
    DeepSeek,
    /// The OpenRouter API.
    OpenRouter,
    /// A vLLM-compatible backend.
    Vllm,
}

/// Surfaced as a scalar `"type"` field on model objects in the aggregated
/// `/v1/models` listing. Only embeddings models are distinguished; every
/// other model carries no `type` field, so the listing stays byte-identical
/// for untagged models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    /// A model that produces vector embeddings.
    Embedding,
}

impl ModelKind {
    /// Returns the lowercase wire representation of this model kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
        }
    }
}

/// Errors raised when a provider payload cannot satisfy the normalized usage contract.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The provider response was not valid JSON.
    #[error("failed to parse JSON: {0}")]
    ParseError(#[source] serde_json::Error),

    /// A required top-level field was absent.
    #[error("missing '{0}' field")]
    MissingField(String),

    /// A provider response field had an unexpected JSON type.
    #[error("'{0}' is not an array")]
    WrongType(String),
}

/// A single model entry from a provider's models-listing endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    /// Provider-supplied model identifier.
    pub id: String,
    /// Normalized OpenAI-style model object served from `/v1/models`.
    /// Provider-specific fields (pricing, architecture, descriptions, ...)
    /// are stripped so the aggregated listing stays uniform.
    pub raw: serde_json::Value,
}

/// Optional fields copied verbatim from the upstream entry when present.
const PRESERVED_FIELDS: [&str; 5] = [
    "aliases",
    "context_length",
    "created",
    "knowledge_cutoff",
    "reasoning",
];

/// Always sets `id`, `object` and `owned_by`; copies [`PRESERVED_FIELDS`]
/// when present. `context_length` falls back to `meta.n_ctx` (llama.cpp).
fn normalized_model(id: &str, source: &serde_json::Value, owned_by: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_string(), serde_json::Value::String(id.to_string()));
    map.insert(
        "object".to_string(),
        serde_json::Value::String("model".to_string()),
    );
    map.insert(
        "owned_by".to_string(),
        serde_json::Value::String(owned_by.to_string()),
    );
    for field in PRESERVED_FIELDS {
        if let Some(value) = source.get(field).filter(|v| !v.is_null()) {
            map.insert(field.to_string(), value.clone());
        }
    }
    if !map.contains_key("context_length") {
        if let Some(n_ctx) = source
            .get("meta")
            .and_then(|m| m.get("n_ctx"))
            .filter(|v| !v.is_null())
        {
            map.insert("context_length".to_string(), n_ctx.clone());
        }
    }
    if let Some(kind) = detect_model_kind(source) {
        map.insert(
            "type".to_string(),
            serde_json::Value::String(kind.as_str().to_string()),
        );
    }
    serde_json::Value::Object(map)
}

/// Recognizes three upstream shapes: a scalar `"type"` field (LM Studio), a
/// `"capabilities"` array (Ollama `/api/show`), and OpenRouter architecture
/// modalities ending in `->embedding(s)`.
fn detect_model_kind(source: &serde_json::Value) -> Option<ModelKind> {
    // 1. Scalar `type` field (LM Studio style).
    if let Some(kind) = source
        .get("type")
        .and_then(|v| v.as_str())
        .and_then(kind_from_str)
    {
        return Some(kind);
    }

    // 2. `capabilities` array containing "embedding" (Ollama /api/show style).
    if let Some(caps) = source.get("capabilities").and_then(|v| v.as_array()) {
        for cap in caps {
            if let Some(kind) = cap.as_str().and_then(kind_from_str) {
                return Some(kind);
            }
        }
    }

    // 3. OpenRouter architecture modality signal.
    if let Some(arch) = source.get("architecture") {
        // `output_modalities` may be an array of strings; `modality` a single
        // string. Both may carry a `...->embedding(s)` suffix.
        let modalities = arch
            .get("output_modalities")
            .or_else(|| arch.get("modality"));
        if let Some(mods) = modalities {
            let mut strings: Vec<&str> = Vec::new();
            if let Some(arr) = mods.as_array() {
                for m in arr {
                    if let Some(s) = m.as_str() {
                        strings.push(s);
                    }
                }
            } else if let Some(s) = mods.as_str() {
                strings.push(s);
            }
            for m in strings {
                // Take the part after the last "->" so "text+image->embedding"
                // and bare "embedding" both normalize correctly.
                let tail = m.rsplit("->").next().unwrap_or(m);
                if let Some(kind) = kind_from_str(tail) {
                    return Some(kind);
                }
            }
        }
    }

    None
}

fn kind_from_str(s: &str) -> Option<ModelKind> {
    match s {
        "embedding" | "embeddings" => Some(ModelKind::Embedding),
        _ => None,
    }
}

// ProviderKind methods

impl ProviderKind {
    /// Canonical lowercase name matching the serde key.
    pub fn name(&self) -> &'static str {
        match self {
            Self::LlamaCpp => "llamacpp",
            Self::Ollama => "ollama",
            Self::LmStudio => "lmstudio",
            Self::DeepSeek => "deepseek",
            Self::OpenRouter => "openrouter",
            Self::Vllm => "vllm",
        }
    }

    /// Builds the authentication headers for a request to this provider.
    ///
    /// A non-empty key is sent as an HTTP bearer token. No header is emitted
    /// when the key is absent or empty.
    pub fn auth_headers(&self, api_key: Option<&str>) -> Vec<(String, String)> {
        match api_key {
            Some(key) if !key.is_empty() => {
                vec![("authorization".to_string(), format!("Bearer {key}"))]
            }
            _ => vec![],
        }
    }

    /// Path appended to the backend upstream URL for listing models.
    pub fn models_endpoint(&self) -> &'static str {
        match self {
            Self::LlamaCpp => "/v1/models",
            Self::Ollama => "/api/tags",
            Self::LmStudio => "/v1/models",
            Self::DeepSeek => "/models",
            Self::OpenRouter => "/v1/models",
            Self::Vllm => "/v1/models",
        }
    }

    /// Entries missing a usable id are skipped rather than failing the
    /// whole listing.
    pub fn parse_models(&self, body: &[u8]) -> Result<Vec<ModelEntry>, ProviderError> {
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(ProviderError::ParseError)?;

        match self {
            Self::Ollama => self.parse_ollama(value),
            _ => self.parse_openai_list(value),
        }
    }

    fn parse_openai_list(
        &self,
        value: serde_json::Value,
    ) -> Result<Vec<ModelEntry>, ProviderError> {
        let data = value
            .get("data")
            .ok_or_else(|| ProviderError::MissingField("data".to_string()))?;
        let arr = data
            .as_array()
            .ok_or_else(|| ProviderError::WrongType("data".to_string()))?;
        let provider = self.name();
        let mut entries = Vec::with_capacity(arr.len());
        for item in arr {
            let id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let owned_by = item
                .get("owned_by")
                .and_then(|v| v.as_str())
                .unwrap_or(provider);
            let raw = normalized_model(&id, item, owned_by);
            entries.push(ModelEntry { id, raw });
        }
        Ok(entries)
    }

    fn parse_ollama(&self, value: serde_json::Value) -> Result<Vec<ModelEntry>, ProviderError> {
        let provider = self.name();
        let models = value
            .get("models")
            .ok_or_else(|| ProviderError::MissingField("models".to_string()))?;
        let arr = models
            .as_array()
            .ok_or_else(|| ProviderError::WrongType("models".to_string()))?;
        let mut entries = Vec::with_capacity(arr.len());
        for item in arr {
            let id = match item
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("model").and_then(|v| v.as_str()))
            {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let owned_by = item
                .get("owned_by")
                .and_then(|v| v.as_str())
                .unwrap_or(provider);
            let raw = normalized_model(&id, item, owned_by);
            entries.push(ModelEntry { id, raw });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod usage_tests;

/// Utilities for extracting token usage from provider responses.
pub mod usage;
