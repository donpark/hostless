pub mod anthropic;
pub mod google;
pub mod openai;

use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// Describes how to transform requests/responses for a specific LLM provider.
#[allow(dead_code)]
pub trait Provider: Send + Sync {
    /// Provider identifier
    fn name(&self) -> &str;

    /// Default base URL for this provider's API
    fn default_base_url(&self) -> &str;

    /// Transform an OpenAI-format request body into the provider's native format.
    /// Returns (full URL path, transformed body, extra headers).
    fn transform_request(
        &self,
        base_url: &str,
        body: &Value,
    ) -> Result<(String, Value, HeaderMap)>;

    /// Transform a provider's response back to OpenAI format (for non-streaming).
    fn transform_response(&self, body: Value) -> Result<Value>;

    /// Transform a single SSE data chunk from the provider's format back to OpenAI SSE format.
    fn transform_stream_chunk(&self, chunk: &str) -> Result<Option<String>>;

    /// Build the authorization header(s) for this provider.
    fn auth_headers(&self, api_key: &str) -> HeaderMap;
}

/// Determine which provider to use based on the model name.
///
/// Convention (matching OpenRouter):
/// - "anthropic/claude-3-..." → Anthropic
/// - "google/gemini-..." → Google
/// - "openai/gpt-..." → OpenAI (explicit prefix)
/// - "gpt-4o", "o1-..." → OpenAI (no prefix, default)
/// - anything else without a known prefix → OpenAI-compatible
///
/// Returns (provider_key, model_name_without_prefix)
pub fn resolve_provider(model: &str) -> (&str, String) {
    if let Some(rest) = model.strip_prefix("anthropic/") {
        ("anthropic", rest.to_string())
    } else if let Some(rest) = model.strip_prefix("google/") {
        ("google", rest.to_string())
    } else if let Some(rest) = model.strip_prefix("openai/") {
        ("openai", rest.to_string())
    } else if model.starts_with("claude") {
        ("anthropic", model.to_string())
    } else if model.starts_with("gemini") {
        ("google", model.to_string())
    } else {
        // Default: OpenAI-compatible
        ("openai", model.to_string())
    }
}

/// Get a boxed provider instance by key
pub fn get_provider(key: &str) -> Box<dyn Provider> {
    match key {
        "anthropic" => Box::new(anthropic::AnthropicProvider),
        "google" => Box::new(google::GoogleProvider),
        _ => Box::new(openai::OpenAIProvider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider() {
        assert_eq!(resolve_provider("anthropic/claude-3-opus"), ("anthropic", "claude-3-opus".to_string()));
        assert_eq!(resolve_provider("google/gemini-pro"), ("google", "gemini-pro".to_string()));
        assert_eq!(resolve_provider("openai/gpt-4o"), ("openai", "gpt-4o".to_string()));
        assert_eq!(resolve_provider("gpt-4o"), ("openai", "gpt-4o".to_string()));
        assert_eq!(resolve_provider("claude-3-haiku"), ("anthropic", "claude-3-haiku".to_string()));
        assert_eq!(resolve_provider("gemini-pro"), ("google", "gemini-pro".to_string()));
        assert_eq!(resolve_provider("llama-3-70b"), ("openai", "llama-3-70b".to_string()));
    }
}
