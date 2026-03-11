use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tracing::warn;

use super::bridge_token::TokenError;
use crate::server::AppState;

/// Check if an origin is exactly `localhost` or `127.0.0.1` (with any port).
/// Does NOT match subdomains like `myapp.localhost` or `localhost.evil.com`.
fn is_bare_localhost(origin: &str) -> bool {
    // Parse as URL to get host reliably
    if let Ok(url) = url::Url::parse(origin) {
        match url.host_str() {
            Some("localhost") | Some("127.0.0.1") => true,
            _ => false,
        }
    } else {
        false
    }
}

/// Check if an origin is a `.localhost` subdomain (RFC 6761 reserved).
/// e.g., `http://myapp.localhost:1355` → true
/// e.g., `http://localhost:3000` → false (bare localhost, not a subdomain)
pub fn is_localhost_subdomain(origin: &str) -> bool {
    if let Ok(url) = url::Url::parse(origin) {
        match url.host_str() {
            Some(host) => {
                host.ends_with(".localhost") && host != "localhost"
            }
            None => false,
        }
    } else {
        false
    }
}

/// Authentication middleware for /v1/* routes.
///
/// - `--dev-mode`: bare `localhost`/`127.0.0.1` requests bypass auth (old behavior)
/// - Without `--dev-mode`: ALL requests (including localhost) must present a valid bridge token
/// - `.localhost` subdomains (e.g., `myapp.localhost`) always require a token (app-specific identity)
/// - Requests with no `Origin` header require a token unless `--dev-mode` is enabled
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // In dev-mode, allow bare localhost and empty origin (CLI/curl) without tokens
    if state.dev_mode {
        if origin.is_empty() || is_bare_localhost(&origin) {
            return next.run(req).await;
        }
    }

    // All other requests (including .localhost subdomains, remote origins,
    // and non-dev-mode localhost) must present a valid bearer token.
    if origin.is_empty() {
        // No origin and not in dev-mode → require token
        // (non-browser clients like curl must use `hostless token create`)
    }

    // Extract bearer token
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let token = if let Some(bearer) = auth_header.strip_prefix("Bearer ") {
        bearer.trim().to_string()
    } else {
        let hint = if origin.is_empty() {
            "CLI/non-browser clients require a token. Create one with 'hostless token create'."
        } else if is_bare_localhost(&origin) {
            "Localhost requests require a token. Start the server with --dev-mode for unrestricted local access, or create a token with 'hostless token create'."
        } else {
            "Missing or invalid Authorization header. Expected 'Bearer <token>'."
        };
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": hint,
                    "type": "authentication_error",
                }
            })),
        )
            .into_response();
    };

    // For .localhost subdomains and origin-bearing requests, validate token + origin.
    // For empty-origin (CLI with token), validate token alone (origin stored as empty).
    let effective_origin = &origin;

    // Validate token + origin
    match state.token_manager.validate(&token, effective_origin).await {
        Ok(()) => {}
        Err(TokenError::NotFound) => {
            warn!(origin = origin.as_str(), "Unknown bridge token");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "message": "Invalid or unknown bridge token",
                        "type": "authentication_error",
                    }
                })),
            )
                .into_response();
        }
        Err(TokenError::AmbiguousPrefix) => {
            warn!(origin = origin.as_str(), "Ambiguous bridge token prefix");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "message": "Invalid or ambiguous bridge token",
                        "type": "authentication_error",
                    }
                })),
            )
                .into_response();
        }
        Err(TokenError::Expired) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "message": "Bridge token has expired. Please re-authenticate.",
                        "type": "token_expired",
                    }
                })),
            )
                .into_response();
        }
        Err(TokenError::OriginMismatch) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": "This token is not valid for the requesting origin",
                        "type": "origin_mismatch",
                    }
                })),
            )
                .into_response();
        }
        Err(TokenError::ModelNotAllowed(model)) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("Model '{}' is not allowed by this token's scope", model),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }
        Err(TokenError::ProviderNotAllowed(provider)) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("Provider '{}' is not allowed by this token's scope", provider),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }
    }

    // Check rate limit
    match state.token_manager.check_rate_limit(&token).await {
        Ok(_remaining) => {}
        Err((_remaining, retry_after)) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", retry_after.to_string())],
                Json(json!({
                    "error": {
                        "message": "Rate limit exceeded for this token",
                        "type": "rate_limit_error",
                    }
                })),
            )
                .into_response();
        }
    }

    // Stash the validated token string in request extensions so routes can
    // use it for provider/model scope checks.
    let mut req = req;
    req.extensions_mut().insert(ValidatedToken(token.clone()));

    next.run(req).await
}

/// Inserted into request extensions after successful token validation.
/// Routes can extract this to perform provider/model scope checks.
#[derive(Clone, Debug)]
pub struct ValidatedToken(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_bare_localhost() {
        // Should match bare localhost
        assert!(is_bare_localhost("http://localhost"));
        assert!(is_bare_localhost("http://localhost:3000"));
        assert!(is_bare_localhost("http://localhost:48282"));
        assert!(is_bare_localhost("https://localhost:443"));
        assert!(is_bare_localhost("http://127.0.0.1"));
        assert!(is_bare_localhost("http://127.0.0.1:8080"));

        // Should NOT match subdomains or lookalikes
        assert!(!is_bare_localhost("http://myapp.localhost:1355"));
        assert!(!is_bare_localhost("http://localhost.evil.com"));
        assert!(!is_bare_localhost("http://api.localhost:1355"));
        assert!(!is_bare_localhost("https://example.com"));
        assert!(!is_bare_localhost(""));
    }

    #[test]
    fn test_is_localhost_subdomain() {
        // Should match .localhost subdomains
        assert!(is_localhost_subdomain("http://myapp.localhost"));
        assert!(is_localhost_subdomain("http://myapp.localhost:1355"));
        assert!(is_localhost_subdomain("https://api.myapp.localhost:1355"));
        assert!(is_localhost_subdomain("http://deep.sub.localhost:8080"));

        // Should NOT match bare localhost or non-.localhost
        assert!(!is_localhost_subdomain("http://localhost"));
        assert!(!is_localhost_subdomain("http://localhost:3000"));
        assert!(!is_localhost_subdomain("http://localhost.evil.com"));
        assert!(!is_localhost_subdomain("http://127.0.0.1:3000"));
        assert!(!is_localhost_subdomain("https://example.com"));
        assert!(!is_localhost_subdomain(""));
    }
}
