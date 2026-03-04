//! End-to-end test that proxies a real OpenAI chat completion through hostless.
//!
//! Requires `OPENAI_API_KEY` env var to be set. Skips gracefully if missing.
//!
//! Run with:
//!   OPENAI_API_KEY=sk-... cargo test --test openai_e2e -- --ignored --nocapture

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

/// Start the hostless server on a random port, returning (addr, state).
/// Uses an ephemeral in-memory vault — no OS keychain access.
async fn start_server(dev_mode: bool) -> (SocketAddr, Arc<hostless::server::AppState>) {
    let state = hostless::server::AppState::new_ephemeral(0, dev_mode);

    let app = hostless::server::create_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give server a moment to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    (addr, state)
}

#[tokio::test]
#[ignore] // requires OPENAI_API_KEY env var and real network access
async fn test_openai_chat_completion_through_proxy() {
    // ── Gate: skip if no API key ──
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("OPENAI_API_KEY not set – skipping e2e test");
            return;
        }
    };

    // ── 1. Start server ──
    let (addr, state) = start_server(true).await;
    let base = format!("http://{}", addr);
    eprintln!("hostless test server running at {}", base);

    // ── 2. Store OpenAI key in vault ──
    state
        .vault
        .add_key("openai", &api_key, None)
        .await
        .expect("Failed to store OpenAI key in vault");
    eprintln!("Stored OpenAI API key in vault");

    // ── 3. Create a wildcard bridge token via POST /auth/token ──
    let client = reqwest::Client::new();
    let token_resp = client
        .post(format!("{}/auth/token", base))
        // No Origin header → CLI-style access
        .json(&json!({
            "origin": "*",
            "name": "e2e-test",
            "ttl": 300
        }))
        .send()
        .await
        .expect("Token creation request failed");

    let token_status = token_resp.status();
    let token_body: Value = token_resp.json().await.expect("Token response not JSON");
    assert_eq!(
        token_status.as_u16(),
        200,
        "Token creation failed: {}",
        token_body
    );

    let bridge_token = token_body["token"]
        .as_str()
        .expect("No token in response");
    assert!(
        bridge_token.starts_with("sk_local_"),
        "Token has wrong prefix: {}",
        bridge_token
    );
    eprintln!("Got bridge token: {}...", &bridge_token[..20]);

    // ── 4. Proxy a chat completion request ──
    let chat_resp = client
        .post(format!("{}/v1/chat/completions", base))
        .header("Authorization", format!("Bearer {}", bridge_token))
        .header("Content-Type", "application/json")
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [
                {"role": "user", "content": "Say exactly: hello from hostless"}
            ],
            "max_tokens": 20,
            "temperature": 0
        }))
        .send()
        .await
        .expect("Chat completion request failed");

    let status = chat_resp.status();
    let body_text = chat_resp.text().await.unwrap_or_default();
    eprintln!("Upstream status: {}", status);
    eprintln!("Response body: {}", &body_text[..body_text.len().min(500)]);

    assert!(
        status.is_success(),
        "Chat completion failed with status {}: {}",
        status,
        body_text
    );

    let body: Value = serde_json::from_str(&body_text).expect("Response is not valid JSON");

    // ── 5. Validate response structure ──
    assert!(
        body["choices"].is_array(),
        "Response missing 'choices' array: {}",
        body
    );
    let choices = body["choices"].as_array().unwrap();
    assert!(!choices.is_empty(), "No choices returned");

    let content = choices[0]["message"]["content"]
        .as_str()
        .expect("No message content in first choice");
    eprintln!("Assistant said: {}", content);

    // The response should contain our expected phrase (case-insensitive)
    assert!(
        content.to_lowercase().contains("hello from hostless"),
        "Unexpected response: {}",
        content
    );

    // Verify it looks like an OpenAI response
    assert!(
        body["model"].as_str().is_some(),
        "Missing 'model' in response"
    );
    assert!(
        body["usage"].is_object(),
        "Missing 'usage' in response"
    );

    eprintln!("✓ End-to-end OpenAI proxy test passed!");
}

#[tokio::test]
#[ignore]
async fn test_openai_streaming_through_proxy() {
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("OPENAI_API_KEY not set – skipping streaming e2e test");
            return;
        }
    };

    let (addr, state) = start_server(true).await;
    let base = format!("http://{}", addr);

    // Store key
    state
        .vault
        .add_key("openai", &api_key, None)
        .await
        .expect("Failed to store OpenAI key");

    // Create token
    let client = reqwest::Client::new();
    let token_body: Value = client
        .post(format!("{}/auth/token", base))
        .json(&json!({
            "origin": "*",
            "name": "e2e-stream-test",
            "ttl": 300
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let bridge_token = token_body["token"].as_str().unwrap();

    // Streaming request
    let resp = client
        .post(format!("{}/v1/chat/completions", base))
        .header("Authorization", format!("Bearer {}", bridge_token))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [
                {"role": "user", "content": "Say exactly one word: test"}
            ],
            "max_tokens": 10,
            "temperature": 0,
            "stream": true
        }))
        .send()
        .await
        .expect("Streaming request failed");

    assert!(
        resp.status().is_success(),
        "Streaming request failed with status {}",
        resp.status()
    );

    // Read the SSE stream
    let body = resp.text().await.unwrap();
    eprintln!("Stream body (first 500 chars): {}", &body[..body.len().min(500)]);

    // SSE format: lines starting with "data: "
    let data_lines: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("data: "))
        .collect();

    assert!(
        !data_lines.is_empty(),
        "No SSE data lines in streaming response"
    );

    // Last meaningful line should be [DONE]
    let last = data_lines.last().unwrap();
    assert!(
        last.contains("[DONE]"),
        "Stream missing [DONE] sentinel: {}",
        last
    );

    // Parse at least one chunk to verify structure
    let first_data = data_lines[0].strip_prefix("data: ").unwrap();
    let chunk: Value = serde_json::from_str(first_data)
        .expect("First SSE chunk is not valid JSON");
    assert!(
        chunk["choices"].is_array(),
        "SSE chunk missing 'choices': {}",
        chunk
    );

    eprintln!("✓ End-to-end OpenAI streaming test passed!");
}

#[tokio::test]
#[ignore]
async fn test_provider_scope_blocks_openai() {
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("OPENAI_API_KEY not set – skipping scope test");
            return;
        }
    };

    let (addr, state) = start_server(false).await;
    let base = format!("http://{}", addr);

    state
        .vault
        .add_key("openai", &api_key, None)
        .await
        .unwrap();

    // Create a token scoped to anthropic only
    let client = reqwest::Client::new();
    let token_body: Value = client
        .post(format!("{}/auth/token", base))
        .json(&json!({
            "origin": "*",
            "name": "anthropic-only",
            "allowed_providers": ["anthropic"],
            "ttl": 300
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let bridge_token = token_body["token"].as_str().unwrap();

    // Try to hit OpenAI through this anthropic-scoped token → should be 403
    let resp = client
        .post(format!("{}/v1/chat/completions", base))
        .header("Authorization", format!("Bearer {}", bridge_token))
        .json(&json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 5
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        403,
        "Expected 403 Forbidden for out-of-scope provider, got {}",
        resp.status()
    );

    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["type"].as_str() == Some("scope_error"),
        "Expected scope_error, got: {}",
        body
    );

    eprintln!("✓ Provider scope enforcement test passed!");
}
