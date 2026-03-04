use std::sync::Arc;

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
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

    let upstream_uri: Uri = format!("http://127.0.0.1:{}{}", target_port, path_and_query)
        .parse()
        .unwrap();

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
    if let Some(port) = &state.port.checked_add(0) {
        if let Ok(val) = HeaderValue::from_str(&port.to_string()) {
            headers.insert("x-forwarded-port", val);
        }
    }
    // Increment hop counter
    headers.insert(
        "x-hostless-hops",
        HeaderValue::from_str(&(hops + 1).to_string()).unwrap(),
    );

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

/// Handle WebSocket upgrade by establishing a bidirectional TCP pipe.
async fn handle_websocket_upgrade(
    target_port: u16,
    hops: u32,
    req: Request,
) -> Response {
    let (parts, _body) = req.into_parts();

    // Connect to upstream
    let upstream_addr = format!("127.0.0.1:{}", target_port);
    let upstream_stream = match tokio::net::TcpStream::connect(&upstream_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                target_port = target_port,
                error = %e,
                "WebSocket upgrade: failed to connect to upstream"
            );
            let html = pages::render_error_page(
                StatusCode::BAD_GATEWAY,
                "Bad Gateway",
                "Could not connect to upstream for WebSocket upgrade.",
                None,
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("content-type", "text/html; charset=utf-8")
                .body(Body::from(html))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
        }
    };

    // Build the HTTP upgrade request to send to upstream
    let mut upgrade_request = String::new();
    let path = parts.uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    upgrade_request.push_str(&format!(
        "{} {} HTTP/1.1\r\n",
        parts.method, path
    ));

    // Forward relevant headers
    for (name, value) in &parts.headers {
        let name_str = name.as_str();
        if name_str.eq_ignore_ascii_case("host") {
            // Rewrite host to upstream
            upgrade_request.push_str(&format!("Host: 127.0.0.1:{}\r\n", target_port));
        } else {
            if let Ok(v) = value.to_str() {
                upgrade_request.push_str(&format!("{}: {}\r\n", name_str, v));
            }
        }
    }
    upgrade_request.push_str(&format!("X-Hostless-Hops: {}\r\n", hops + 1));
    upgrade_request.push_str("\r\n");

    // Write the upgrade request to upstream
    use tokio::io::AsyncWriteExt;
    let (mut upstream_read, mut upstream_write) = upstream_stream.into_split();
    if let Err(e) = upstream_write.write_all(upgrade_request.as_bytes()).await {
        error!("Failed to send WebSocket upgrade to upstream: {}", e);
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // Read the upgrade response from upstream
    use tokio::io::AsyncReadExt;
    let mut response_buf = vec![0u8; 4096];
    let n = match upstream_read.read(&mut response_buf).await {
        Ok(n) => n,
        Err(e) => {
            error!("Failed to read WebSocket upgrade response: {}", e);
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let response_text = String::from_utf8_lossy(&response_buf[..n]);

    // Check if upgrade was accepted (HTTP 101)
    if !response_text.starts_with("HTTP/1.1 101") {
        warn!("Upstream rejected WebSocket upgrade: {}", response_text.lines().next().unwrap_or(""));
        return (
            StatusCode::BAD_GATEWAY,
            "Upstream rejected WebSocket upgrade",
        )
            .into_response();
    }

    // Parse the 101 response headers to forward back to client
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for line in response_text.lines().skip(1) {
        if line.is_empty() || line == "\r" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if let Ok(header_val) = HeaderValue::from_str(value.trim()) {
                builder = builder.header(name.trim(), header_val);
            }
        }
    }

    // We need to return a 101 response and then pipe the connections.
    // For now, return 501 — full WebSocket upgrade requires hyper's `on_upgrade` API
    // which needs the original connection. This is a placeholder for Phase 1.
    // TODO: Implement proper WebSocket proxying with hyper upgrade API
    warn!("WebSocket proxying not yet fully implemented");
    (
        StatusCode::NOT_IMPLEMENTED,
        "WebSocket proxying is not yet implemented",
    )
        .into_response()
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
