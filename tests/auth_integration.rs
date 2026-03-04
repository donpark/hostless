//! Integration tests for the auth/token flow and proxy middleware.
//!
//! These tests exercise the BridgeTokenManager and middleware logic to verify:
//! - Token creation via CLI (wildcard origin)
//! - Token-gated access with provider and model scope enforcement
//! - Origin isolation between .localhost subdomain apps
//! - Origin check safety (localhost.evil.com blocked)

use hostless::auth::bridge_token::{BridgeTokenManager, TokenError};
use std::time::Duration;

/// Simulates the full curl workflow:
/// 1. Create a wildcard-origin token (what `hostless token create --origin "*"` does)
/// 2. Validate it with various origins (simulating curl with no Origin)
/// 3. Check provider/model scoping
#[tokio::test]
async fn test_curl_token_workflow() {
    let manager = BridgeTokenManager::new();

    // Step 1: Create a wildcard token (CLI token create)
    let token = manager
        .issue_full(
            "*",
            Duration::from_secs(3600),
            None,  // all models
            None,  // all providers
            None,  // no rate limit
            Some("my-curl-tool".to_string()),
        )
        .await;

    assert!(token.token.starts_with("sk_local_"));

    // Step 2: Validate with empty origin (curl sends no Origin)
    assert!(manager.validate(&token.token, "").await.is_ok());

    // Step 3: Validate with any origin (wildcard matches all)
    assert!(manager.validate(&token.token, "http://localhost:3000").await.is_ok());
    assert!(manager.validate(&token.token, "http://myapp.localhost:1355").await.is_ok());
    assert!(manager.validate(&token.token, "https://remote.com").await.is_ok());

    // Step 4: No provider restriction → all providers allowed
    assert!(manager.validate_provider(&token.token, "openai").await.is_ok());
    assert!(manager.validate_provider(&token.token, "anthropic").await.is_ok());
    assert!(manager.validate_provider(&token.token, "google").await.is_ok());
}

/// Token scoped to specific providers blocks other providers
#[tokio::test]
async fn test_provider_scoped_token() {
    let manager = BridgeTokenManager::new();

    let token = manager
        .issue_full(
            "http://myapp.localhost:1355",
            Duration::from_secs(3600),
            None,
            Some(vec!["openai".to_string()]),  // only OpenAI
            None,
            Some("openai-only-app".to_string()),
        )
        .await;

    // OpenAI allowed
    assert!(manager.validate_provider(&token.token, "openai").await.is_ok());

    // Anthropic blocked
    assert!(matches!(
        manager.validate_provider(&token.token, "anthropic").await,
        Err(TokenError::ProviderNotAllowed(_))
    ));

    // Google blocked
    assert!(matches!(
        manager.validate_provider(&token.token, "google").await,
        Err(TokenError::ProviderNotAllowed(_))
    ));
}

/// Token scoped to specific models blocks other models
#[tokio::test]
async fn test_model_scoped_token() {
    let manager = BridgeTokenManager::new();

    let token = manager
        .issue_full(
            "*",
            Duration::from_secs(3600),
            Some(vec!["gpt-4o-mini".to_string(), "claude-3-haiku*".to_string()]),
            None,
            None,
            None,
        )
        .await;

    // Exact match
    assert!(manager.validate_model(&token.token, "gpt-4o-mini").await.is_ok());

    // Glob match
    assert!(manager.validate_model(&token.token, "claude-3-haiku-20240307").await.is_ok());

    // Blocked
    assert!(matches!(
        manager.validate_model(&token.token, "gpt-4o").await,
        Err(TokenError::ModelNotAllowed(_))
    ));
    assert!(matches!(
        manager.validate_model(&token.token, "claude-3-opus").await,
        Err(TokenError::ModelNotAllowed(_))
    ));
}

/// Origin-bound token rejects wrong origin
#[tokio::test]
async fn test_origin_bound_token_rejects_other_origin() {
    let manager = BridgeTokenManager::new();

    let token = manager
        .issue("http://myapp.localhost:1355", Duration::from_secs(3600), None, None)
        .await;

    // Correct origin works
    assert!(manager.validate(&token.token, "http://myapp.localhost:1355").await.is_ok());

    // Different .localhost subdomain rejected
    assert!(matches!(
        manager.validate(&token.token, "http://otherapp.localhost:1355").await,
        Err(TokenError::OriginMismatch)
    ));

    // Bare localhost rejected
    assert!(matches!(
        manager.validate(&token.token, "http://localhost:1355").await,
        Err(TokenError::OriginMismatch)
    ));

    // Empty origin rejected (not a wildcard token)
    assert!(matches!(
        manager.validate(&token.token, "").await,
        Err(TokenError::OriginMismatch)
    ));
}

/// Two .localhost apps get isolated tokens
#[tokio::test]
async fn test_localhost_subdomain_isolation() {
    let manager = BridgeTokenManager::new();

    let token_a = manager
        .issue_full(
            "http://frontend.localhost:1355",
            Duration::from_secs(3600),
            None,
            Some(vec!["openai".to_string()]),
            None,
            Some("frontend".to_string()),
        )
        .await;

    let token_b = manager
        .issue_full(
            "http://backend.localhost:1355",
            Duration::from_secs(3600),
            None,
            Some(vec!["anthropic".to_string()]),
            None,
            Some("backend".to_string()),
        )
        .await;

    // Each token only valid for its own origin
    assert!(manager.validate(&token_a.token, "http://frontend.localhost:1355").await.is_ok());
    assert!(matches!(
        manager.validate(&token_a.token, "http://backend.localhost:1355").await,
        Err(TokenError::OriginMismatch)
    ));

    assert!(manager.validate(&token_b.token, "http://backend.localhost:1355").await.is_ok());
    assert!(matches!(
        manager.validate(&token_b.token, "http://frontend.localhost:1355").await,
        Err(TokenError::OriginMismatch)
    ));

    // Provider scope is per-token
    assert!(manager.validate_provider(&token_a.token, "openai").await.is_ok());
    assert!(matches!(
        manager.validate_provider(&token_a.token, "anthropic").await,
        Err(TokenError::ProviderNotAllowed(_))
    ));

    assert!(manager.validate_provider(&token_b.token, "anthropic").await.is_ok());
    assert!(matches!(
        manager.validate_provider(&token_b.token, "openai").await,
        Err(TokenError::ProviderNotAllowed(_))
    ));
}

/// Middleware origin checking: localhost.evil.com is NOT bare localhost
#[test]
fn test_localhost_evil_com_not_bare_localhost() {
    use hostless::auth::middleware::{is_localhost_subdomain};

    // localhost.evil.com is NOT a .localhost subdomain (it's a subdomain of evil.com)
    assert!(!is_localhost_subdomain("http://localhost.evil.com"));
    assert!(!is_localhost_subdomain("http://localhost.evil.com:3000"));

    // But myapp.localhost IS a .localhost subdomain
    assert!(is_localhost_subdomain("http://myapp.localhost"));
    assert!(is_localhost_subdomain("http://myapp.localhost:1355"));
}
