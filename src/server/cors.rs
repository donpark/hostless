use std::sync::Arc;

use axum::http::{header, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::AppState;
use crate::auth::middleware::is_localhost_subdomain;

/// Check if an origin is exactly bare `localhost` or `127.0.0.1` (any port/scheme).
fn is_bare_localhost(origin: &str) -> bool {
    if let Ok(url) = url::Url::parse(origin) {
        match url.host_str() {
            Some("localhost") | Some("127.0.0.1") => true,
            _ => false,
        }
    } else {
        false
    }
}

/// Build a dynamic CORS layer that checks incoming origins against the allowlist.
pub fn build_cors_layer(state: Arc<AppState>) -> CorsLayer {
    let state_clone = state.clone();

    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            let origin_str = origin.to_str().unwrap_or("");

            // Allow bare localhost/127.0.0.1 (any port) for dev convenience
            if is_bare_localhost(origin_str) {
                return true;
            }

            // Allow .localhost subdomains (RFC 6761) — each is a distinct origin
            // e.g., http://myapp.localhost:1355
            if is_localhost_subdomain(origin_str) {
                return true;
            }

            // Check against the persistent allowlist
            let config = state_clone.config.try_read();
            match config {
                Ok(cfg) => cfg.is_origin_allowed(origin_str),
                Err(_) => false,
            }
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::ACCEPT,
            header::HeaderName::from_static("x-provider"),
        ])
        .allow_credentials(true)
        .max_age(std::time::Duration::from_secs(86400))
}
