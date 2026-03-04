use super::Provider;
use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};

/// Google Gemini API adapter.
/// Transforms OpenAI-format requests/responses to Google's generateContent format.
pub struct GoogleProvider;

impl Provider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
    }

    fn default_base_url(&self) -> &str {
        "https://generativelanguage.googleapis.com"
    }

    fn transform_request(
        &self,
        base_url: &str,
        body: &Value,
    ) -> Result<(String, Value, HeaderMap)> {
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("gemini-pro");

        let stream = body
            .get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);

        let action = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };

        // API key will be added as query param by the caller
        let url = format!(
            "{}/v1beta/models/{}:{}",
            base_url.trim_end_matches('/'),
            model,
            action
        );

        let messages = body
            .get("messages")
            .and_then(|m| m.as_array())
            .context("Missing 'messages' array")?;

        // Transform messages to Gemini's content format
        let mut system_instruction: Option<Value> = None;
        let mut contents = Vec::new();

        for msg in messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");

            if role == "system" {
                system_instruction = Some(json!({
                    "parts": [{"text": content}]
                }));
            } else {
                let gemini_role = match role {
                    "assistant" => "model",
                    _ => "user",
                };
                contents.push(json!({
                    "role": gemini_role,
                    "parts": [{"text": content}]
                }));
            }
        }

        let mut gemini_body = json!({
            "contents": contents,
        });

        if let Some(sys) = system_instruction {
            gemini_body["systemInstruction"] = sys;
        }

        // Generation config
        let mut generation_config = json!({});
        if let Some(temp) = body.get("temperature") {
            generation_config["temperature"] = temp.clone();
        }
        if let Some(top_p) = body.get("top_p") {
            generation_config["topP"] = top_p.clone();
        }
        if let Some(max_tokens) = body.get("max_tokens") {
            generation_config["maxOutputTokens"] = max_tokens.clone();
        }
        if let Some(stop) = body.get("stop") {
            generation_config["stopSequences"] = stop.clone();
        }
        if generation_config.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            gemini_body["generationConfig"] = generation_config;
        }

        Ok((url, gemini_body, HeaderMap::new()))
    }

    fn transform_response(&self, body: Value) -> Result<Value> {
        // Transform Gemini response → OpenAI format
        let candidates = body
            .get("candidates")
            .and_then(|c| c.as_array())
            .context("Missing 'candidates' in response")?;

        let mut choices = Vec::new();
        for (i, candidate) in candidates.iter().enumerate() {
            let parts = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array());

            let mut text = String::new();
            if let Some(parts) = parts {
                for part in parts {
                    if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
            }

            let finish_reason = candidate
                .get("finishReason")
                .and_then(|r| r.as_str())
                .map(|r| match r {
                    "STOP" => "stop",
                    "MAX_TOKENS" => "length",
                    "SAFETY" => "content_filter",
                    _ => "stop",
                })
                .unwrap_or("stop");

            choices.push(json!({
                "index": i,
                "message": {
                    "role": "assistant",
                    "content": text,
                },
                "finish_reason": finish_reason,
            }));
        }

        let usage = body.get("usageMetadata").cloned().unwrap_or(json!({}));
        let prompt_tokens = usage
            .get("promptTokenCount")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let completion_tokens = usage
            .get("candidatesTokenCount")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);

        Ok(json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": "gemini",
            "choices": choices,
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            }
        }))
    }

    fn transform_stream_chunk(&self, chunk: &str) -> Result<Option<String>> {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let data: Value =
            serde_json::from_str(trimmed).unwrap_or_else(|_| json!({"candidates": []}));

        let candidates = data.get("candidates").and_then(|c| c.as_array());
        if let Some(candidates) = candidates {
            for candidate in candidates {
                let parts = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array());

                if let Some(parts) = parts {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            let finish_reason = candidate
                                .get("finishReason")
                                .and_then(|r| r.as_str())
                                .and_then(|r| match r {
                                    "STOP" => Some("stop"),
                                    "MAX_TOKENS" => Some("length"),
                                    _ => None,
                                });

                            let openai_chunk = json!({
                                "id": "chatcmpl-stream",
                                "object": "chat.completion.chunk",
                                "created": chrono::Utc::now().timestamp(),
                                "model": "gemini",
                                "choices": [{
                                    "index": 0,
                                    "delta": {
                                        "content": text,
                                    },
                                    "finish_reason": finish_reason,
                                }]
                            });

                            return Ok(Some(serde_json::to_string(&openai_chunk)?));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn auth_headers(&self, api_key: &str) -> HeaderMap {
        // Google uses query parameter for API key, but we can also use header
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", api_key)).unwrap(),
        );
        headers
    }
}

impl GoogleProvider {
    /// Google Gemini passes the API key as a query parameter.
    /// This method appends it to the URL.
    pub fn append_api_key_to_url(url: &str, api_key: &str) -> String {
        let separator = if url.contains('?') { "&" } else { "?" };
        format!("{}{}key={}", url, separator, api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_request() {
        let provider = GoogleProvider;
        let body = json!({
            "model": "gemini-pro",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi there!"},
                {"role": "user", "content": "How are you?"}
            ],
            "temperature": 0.7,
            "max_tokens": 1000
        });

        let (url, transformed, _) = provider
            .transform_request("https://generativelanguage.googleapis.com", &body)
            .unwrap();

        assert!(url.contains("gemini-pro:generateContent"));
        assert!(transformed.get("systemInstruction").is_some());

        let contents = transformed["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3); // system excluded from contents
        assert_eq!(contents[1]["role"], "model"); // assistant → model
    }

    #[test]
    fn test_transform_response() {
        let provider = GoogleProvider;
        let gemini_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello!"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        });

        let openai = provider.transform_response(gemini_resp).unwrap();
        assert_eq!(openai["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(openai["choices"][0]["finish_reason"], "stop");
    }
}
