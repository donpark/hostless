use super::Provider;
use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};

/// Anthropic Messages API adapter.
/// Transforms OpenAI-format requests/responses to Anthropic's native format.
pub struct AnthropicProvider;

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_base_url(&self) -> &str {
        "https://api.anthropic.com"
    }

    fn transform_request(
        &self,
        base_url: &str,
        body: &Value,
    ) -> Result<(String, Value, HeaderMap)> {
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));

        let messages = body
            .get("messages")
            .and_then(|m| m.as_array())
            .context("Missing 'messages' array")?;

        // Extract system message (Anthropic wants it as a top-level field)
        let mut system_text: Option<String> = None;
        let mut filtered_messages = Vec::new();

        for msg in messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "system" {
                let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                system_text = Some(content.to_string());
            } else {
                filtered_messages.push(msg.clone());
            }
        }

        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("claude-sonnet-4-20250514");

        // Build Anthropic request body
        let mut anthropic_body = json!({
            "model": model,
            "messages": filtered_messages,
            "max_tokens": body.get("max_tokens")
                .and_then(|m| m.as_u64())
                .unwrap_or(4096),
        });

        if let Some(sys) = system_text {
            anthropic_body["system"] = json!(sys);
        }

        // Forward optional parameters
        if let Some(temp) = body.get("temperature") {
            anthropic_body["temperature"] = temp.clone();
        }
        if let Some(top_p) = body.get("top_p") {
            anthropic_body["top_p"] = top_p.clone();
        }
        if let Some(stream) = body.get("stream") {
            anthropic_body["stream"] = stream.clone();
        }
        if let Some(stop) = body.get("stop") {
            anthropic_body["stop_sequences"] = stop.clone();
        }

        let mut extra_headers = HeaderMap::new();
        extra_headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        Ok((url, anthropic_body, extra_headers))
    }

    fn transform_response(&self, body: Value) -> Result<Value> {
        // Transform Anthropic response → OpenAI format
        let id = body
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("msg_unknown");
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("claude");

        let content_blocks = body.get("content").and_then(|c| c.as_array());
        let mut full_text = String::new();
        if let Some(blocks) = content_blocks {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        full_text.push_str(text);
                    }
                }
            }
        }

        let stop_reason = body
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .map(|r| match r {
                "end_turn" => "stop",
                "max_tokens" => "length",
                other => other,
            })
            .unwrap_or("stop");

        let usage = body.get("usage").cloned().unwrap_or(json!({}));
        let input_tokens = usage.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        let output_tokens = usage.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);

        Ok(json!({
            "id": format!("chatcmpl-{}", id),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": full_text,
                },
                "finish_reason": stop_reason,
            }],
            "usage": {
                "prompt_tokens": input_tokens,
                "completion_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
            }
        }))
    }

    fn transform_stream_chunk(&self, chunk: &str) -> Result<Option<String>> {
        // Parse Anthropic SSE events and convert to OpenAI SSE format
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let data: Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|_| json!({"type": "unknown"}));

        let event_type = data.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "content_block_delta" => {
                let delta_text = data
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                let openai_chunk = json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "created": chrono::Utc::now().timestamp(),
                    "model": "claude",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "content": delta_text,
                        },
                        "finish_reason": null,
                    }]
                });

                Ok(Some(serde_json::to_string(&openai_chunk)?))
            }
            "message_start" => {
                let model = data
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("claude");

                let openai_chunk = json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "created": chrono::Utc::now().timestamp(),
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                        },
                        "finish_reason": null,
                    }]
                });

                Ok(Some(serde_json::to_string(&openai_chunk)?))
            }
            "message_delta" => {
                let stop_reason = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                    .map(|r| match r {
                        "end_turn" => "stop",
                        "max_tokens" => "length",
                        other => other,
                    });

                let openai_chunk = json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "created": chrono::Utc::now().timestamp(),
                    "model": "claude",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": stop_reason,
                    }]
                });

                Ok(Some(serde_json::to_string(&openai_chunk)?))
            }
            "message_stop" => Ok(Some("[DONE]".to_string())),
            _ => Ok(None),
        }
    }

    fn auth_headers(&self, api_key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(api_key).unwrap(),
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_request_extracts_system() {
        let provider = AnthropicProvider;
        let body = json!({
            "model": "claude-3-opus",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"}
            ],
            "max_tokens": 1000
        });

        let (url, transformed, _headers) = provider
            .transform_request("https://api.anthropic.com", &body)
            .unwrap();

        assert!(url.contains("/v1/messages"));
        assert_eq!(transformed["system"], "You are helpful.");
        let msgs = transformed["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn test_transform_response() {
        let provider = AnthropicProvider;
        let anthropic_resp = json!({
            "id": "msg_123",
            "type": "message",
            "model": "claude-3-opus-20240229",
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let openai = provider.transform_response(anthropic_resp).unwrap();
        assert_eq!(openai["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(openai["choices"][0]["finish_reason"], "stop");
    }
}
