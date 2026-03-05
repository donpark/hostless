use super::Provider;
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::warn;

/// OpenAI-compatible provider.
/// Works for OpenAI, OpenRouter, Groq, Together, and any OpenAI-compatible API.
pub struct OpenAIProvider;

impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn default_base_url(&self) -> &str {
        "https://api.openai.com"
    }

    fn transform_request(
        &self,
        base_url: &str,
        body: &Value,
    ) -> Result<(String, Value, HeaderMap)> {
        // Pass through as-is — the body is already in OpenAI format
        let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
        Ok((url, body.clone(), HeaderMap::new()))
    }

    fn transform_response(&self, body: Value) -> Result<Value> {
        // Already in OpenAI format
        Ok(body)
    }

    fn transform_stream_chunk(&self, chunk: &str) -> Result<Option<String>> {
        // Already in OpenAI SSE format
        if chunk.trim().is_empty() || chunk.trim() == "[DONE]" {
            return Ok(Some(chunk.to_string()));
        }
        Ok(Some(chunk.to_string()))
    }

    fn auth_headers(&self, api_key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        match HeaderValue::from_str(&format!("Bearer {}", api_key)) {
            Ok(value) => {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
            Err(e) => {
                warn!(error = %e, "Skipping invalid OpenAI Authorization header value");
            }
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_headers_invalid_key_does_not_panic() {
        let provider = OpenAIProvider;
        let headers = provider.auth_headers("bad\nkey");
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }
}
