use std::sync::Arc;

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::copy_bidirectional;
use tracing::{debug, error, warn};

use super::AppState;
use super::pages;

/// Maximum number of proxy hops before returning 508 Loop Detected.
const MAX_HOPS: u32 = 5;

/// Hop-by-hop headers that must not be forwarded (RFC 2616 §13.5.1).
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Reverse proxy a request to a local app's target port.
///
/// This is the handler for `.localhost` subdomain requests dispatched by
/// the Host-header dispatch layer. It forwards the request to
/// `127.0.0.1:<target_port>` and pipes the response back.
pub async fn reverse_proxy(
    state: Arc<AppState>,
    target_port: u16,
    req: Request,
) -> Response {
    // --- Loop detection ---
    let hops = req
        .headers()
        .get("x-hostless-hops")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);

    if hops >= MAX_HOPS {
        warn!(hops = hops, "Loop detected in reverse proxy");
        let html = pages::render_error_page(
            StatusCode::LOOP_DETECTED,
            "Loop Detected",
            "This request passed through hostless too many times. This usually means an app proxy loop.",
            Some("Fix: configure your frontend proxy with changeOrigin: true."),
        );
        return Response::builder()
            .status(StatusCode::LOOP_DETECTED)
            .header("content-type", "text/html; charset=utf-8")
            .body(Body::from(html))
            .unwrap_or_else(|_| StatusCode::LOOP_DETECTED.into_response());
    }

    // --- Check for WebSocket upgrade ---
    let is_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_upgrade {
        return handle_websocket_upgrade(target_port, hops, req).await;
    }

    // --- Build upstream URI ---
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let upstream_uri: Uri = match format!("http://127.0.0.1:{}{}", target_port, path_and_query)
        .parse()
    {
        Ok(uri) => uri,
        Err(e) => {
            warn!(target_port = target_port, error = %e, "Rejected malformed proxied URI");
            let html = pages::render_error_page(
                StatusCode::BAD_REQUEST,
                "Bad Request",
                "The request URI could not be forwarded to the local app.",
                None,
            );
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("content-type", "text/html; charset=utf-8")
                .body(Body::from(html))
                .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response());
        }
    };

    // --- Build forwarded headers ---
    let mut headers = strip_hop_by_hop(&parts.headers);

    // X-Forwarded-* headers
    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_str(
            parts
                .headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        )
        .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "x-forwarded-proto",
        HeaderValue::from_static("http"),
    );
    if let Ok(val) = HeaderValue::from_str(&state.port.to_string()) {
        headers.insert("x-forwarded-port", val);
    }
    // Increment hop counter
    if let Ok(val) = HeaderValue::from_str(&(hops + 1).to_string()) {
        headers.insert("x-hostless-hops", val);
    } else {
        warn!(hops = hops, "Failed to set x-hostless-hops header");
    }

    // --- Forward the request ---
    let client = Client::builder(TokioExecutor::new())
        .build_http::<Body>();

    let mut upstream_req = Request::builder()
        .method(parts.method)
        .uri(upstream_uri);

    if let Some(h) = upstream_req.headers_mut() {
        *h = headers;
    }

    let upstream_req = match upstream_req.body(body) {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to build upstream request: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    match client.request(upstream_req).await {
        Ok(resp) => {
            let (parts, incoming_body) = resp.into_parts();
            let mut response_headers = strip_hop_by_hop(&parts.headers);

            // Remove server header to avoid leaking upstream info
            response_headers.remove(header::SERVER);

            let mut response = Response::builder().status(parts.status);
            if let Some(h) = response.headers_mut() {
                *h = response_headers;
            }

            // Convert hyper Incoming body to axum Body
            let body = Body::new(incoming_body);

            response
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            // Connection refused → app is probably not running
            debug!(
                target_port = target_port,
                error = %e,
                "Failed to connect to upstream app"
            );
            let html = pages::render_error_page(
                StatusCode::BAD_GATEWAY,
                "Bad Gateway",
                &format!("Could not connect to app on port {}.", target_port),
                None,
            );
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("content-type", "text/html; charset=utf-8")
                .body(Body::from(html))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
    }
}

/// WebSocket upgrade handling is intentionally disabled until full hyper
/// on_upgrade support is implemented.
async fn handle_websocket_upgrade(
    target_port: u16,
    hops: u32,
    mut req: Request,
) -> Response {
    // Capture client upgrade handle before consuming request parts.
    let client_on_upgrade = hyper::upgrade::on(&mut req);

    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let upstream_uri: Uri = match format!("http://127.0.0.1:{}{}", target_port, path_and_query)
        .parse()
    {
        Ok(uri) => uri,
        Err(e) => {
            warn!(target_port = target_port, error = %e, "Rejected malformed WebSocket upstream URI");
            return (
                StatusCode::BAD_REQUEST,
                "Malformed WebSocket request URI",
            )
                .into_response();
        }
    };

    let mut headers = parts.headers.clone();
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{}", target_port))
            .unwrap_or_else(|_| HeaderValue::from_static("127.0.0.1")),
    );
    if let Ok(val) = HeaderValue::from_str(&(hops + 1).to_string()) {
        headers.insert("x-hostless-hops", val);
    }

    let client = Client::builder(TokioExecutor::new())
        .build_http::<Body>();

    let mut upstream_req = Request::builder()
        .method(parts.method)
        .uri(upstream_uri);
    if let Some(h) = upstream_req.headers_mut() {
        *h = headers;
    }

    let upstream_req = match upstream_req.body(body) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to build WebSocket upstream request");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut upstream_resp = match client.request(upstream_req).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(target_port = target_port, error = %e, "WebSocket upstream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                "Failed to reach upstream WebSocket endpoint",
            )
                .into_response();
        }
    };

    if upstream_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        warn!(
            status = upstream_resp.status().as_u16(),
            target_port = target_port,
            "Upstream did not accept WebSocket upgrade"
        );
        return (
            StatusCode::BAD_GATEWAY,
            "Upstream rejected WebSocket upgrade",
        )
            .into_response();
    }

    let upstream_on_upgrade = hyper::upgrade::on(&mut upstream_resp);
    let upstream_headers = upstream_resp.headers().clone();

    tokio::spawn(async move {
        let upgraded = tokio::try_join!(client_on_upgrade, upstream_on_upgrade);
        let (client_ws, upstream_ws) = match upgraded {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = %e, "WebSocket upgrade failed");
                return;
            }
        };

        let mut client_ws = TokioIo::new(client_ws);
        let mut upstream_ws = TokioIo::new(upstream_ws);

        if let Err(e) = copy_bidirectional(&mut client_ws, &mut upstream_ws).await {
            debug!(error = %e, "WebSocket proxy stream closed with error");
        }
    });

    let mut response = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    if let Some(resp_headers) = response.headers_mut() {
        *resp_headers = upstream_headers;
    }

    response
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Strip hop-by-hop headers from a header map.
fn strip_hop_by_hop(headers: &HeaderMap) -> HeaderMap {
    let mut cleaned = headers.clone();
    for name in HOP_BY_HOP_HEADERS {
        cleaned.remove(*name);
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn test_strip_hop_by_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        headers.insert("x-custom", HeaderValue::from_static("hello"));

        let cleaned = strip_hop_by_hop(&headers);
        assert!(!cleaned.contains_key(header::CONNECTION));
        assert!(!cleaned.contains_key("keep-alive"));
        assert!(cleaned.contains_key(header::CONTENT_TYPE));
        assert!(cleaned.contains_key("x-custom"));
    }

    #[test]
    fn test_max_hops_constant() {
        assert_eq!(MAX_HOPS, 5);
    }
}
