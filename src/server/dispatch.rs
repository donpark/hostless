//! Host-header dispatch layer.
//!
//! This is the structural security boundary between the reverse proxy (browser→app)
//! and the management/LLM proxy plane. Requests to `<name>.localhost` are dispatched
//! to the reverse proxy handler and can NEVER reach management endpoints like `/auth/*`
//! or LLM proxy endpoints like `/v1/*`.
//!
//! Only bare `localhost` / `127.0.0.1` requests fall through to the Axum router.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use tracing::debug;

use super::reverse_proxy;
use super::AppState;

/// Extract the hostname from the `Host` header, stripping any port.
/// e.g., "myapp.localhost:11434" → "myapp.localhost"
fn extract_hostname(req: &Request) -> Option<String> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|host| {
            // Strip port if present
            if let Some((hostname, _port)) = host.rsplit_once(':') {
                hostname.to_string()
            } else {
                host.to_string()
            }
        })
}

/// Check if a hostname is a `.localhost` subdomain (not bare `localhost`).
fn is_subdomain_host(hostname: &str) -> bool {
    hostname.ends_with(".localhost") && hostname != ".localhost"
}

/// Public wrapper for integration tests.
#[allow(dead_code)]
pub fn is_subdomain_host_pub(hostname: &str) -> bool {
    is_subdomain_host(hostname)
}

/// The Host-header dispatch function.
///
/// This is called as Axum middleware (via `from_fn_with_state`). It inspects the
/// `Host` header and decides whether to dispatch to the reverse proxy or fall
/// through to the normal Axum router.
///
/// Security guarantee: requests to `<name>.localhost` can ONLY reach the
/// reverse-proxied app. They structurally cannot reach `/auth/*`, `/health`,
/// `/v1/*`, or any other hostless management/proxy endpoint.
pub async fn host_dispatch(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let hostname = match extract_hostname(&req) {
        Some(h) => h,
        None => {
            // No Host header — fall through to normal router
            return next.run(req).await;
        }
    };

    if !is_subdomain_host(&hostname) {
        // Bare localhost or 127.0.0.1 — fall through to management + LLM proxy
        return next.run(req).await;
    }

    // This is a .localhost subdomain request → dispatch to reverse proxy
    debug!(hostname = hostname.as_str(), "Host dispatch → reverse proxy");

    // Look up the route
    let route = state.route_table.lookup(&hostname).await;

    match route {
        Some(route) => {
            reverse_proxy::reverse_proxy(state, route.target_port, req).await
        }
        None => {
            // No route registered for this subdomain
            (
                StatusCode::NOT_FOUND,
                format!(
                    "No app registered for '{}'. Use 'hostless run {} <command>' to start one.",
                    hostname,
                    hostname.strip_suffix(".localhost").unwrap_or(&hostname)
                ),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn test_extract_hostname() {
        let req = Request::builder()
            .header("host", "myapp.localhost:11434")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_hostname(&req).unwrap(), "myapp.localhost");

        let req = Request::builder()
            .header("host", "localhost:11434")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_hostname(&req).unwrap(), "localhost");

        let req = Request::builder()
            .header("host", "myapp.localhost")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_hostname(&req).unwrap(), "myapp.localhost");
    }

    #[test]
    fn test_is_subdomain_host() {
        assert!(is_subdomain_host("myapp.localhost"));
        assert!(is_subdomain_host("api.myapp.localhost"));
        assert!(is_subdomain_host("deep.sub.localhost"));

        assert!(!is_subdomain_host("localhost"));
        assert!(!is_subdomain_host(".localhost"));
        assert!(!is_subdomain_host("127.0.0.1"));
        assert!(!is_subdomain_host("example.com"));
    }
}
