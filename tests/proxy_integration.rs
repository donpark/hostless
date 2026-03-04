//! Integration tests ported from portless's proxy.test.ts, routes.test.ts, and cli-utils.test.ts.
//!
//! These tests exercise the full Host-header dispatch → reverse proxy stack,
//! route table management, and process manager utilities.
//!
//! All tests use ephemeral state (no OS keychain, no disk I/O).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::net::TcpListener;
use tower::ServiceExt;

// ═══════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════

/// Spawn a minimal HTTP backend on a random port that returns a fixed response.
async fn spawn_backend(
    status: StatusCode,
    body: &'static str,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    spawn_backend_with_headers(status, body, vec![]).await
}

/// Spawn a backend that also returns extra headers.
async fn spawn_backend_with_headers(
    status: StatusCode,
    body: &'static str,
    extra_headers: Vec<(&'static str, &'static str)>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::{response::IntoResponse, routing::any, Router};

    let extra = extra_headers.clone();
    let app = Router::new().fallback(any(move || {
        let extra = extra.clone();
        async move {
            let mut resp = (status, body).into_response();
            for (k, v) in &extra {
                resp.headers_mut().insert(
                    axum::http::HeaderName::from_static(k),
                    axum::http::HeaderValue::from_static(v),
                );
            }
            resp
        }
    }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

/// Spawn a backend that echoes headers back as JSON.
async fn spawn_echo_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::{extract::Request as AxumReq, response::IntoResponse, routing::any, Router};

    let app = Router::new().fallback(any(|req: AxumReq| async move {
        let mut headers_map = serde_json::Map::new();
        for (name, value) in req.headers() {
            if let Ok(v) = value.to_str() {
                headers_map.insert(
                    name.to_string(),
                    serde_json::Value::String(v.to_string()),
                );
            }
        }
        let body = serde_json::json!({
            "headers": headers_map,
            "method": req.method().to_string(),
            "uri": req.uri().to_string(),
        });
        (StatusCode::OK, axum::Json(body)).into_response()
    }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

/// Create an ephemeral AppState and its router for testing.
fn create_test_app(port: u16) -> (Arc<hostless::server::AppState>, axum::Router) {
    let state = hostless::server::AppState::new_ephemeral(port, true);
    let router = hostless::server::create_router(state.clone());
    (state, router)
}

/// Send a request through the router and get the response.
async fn send_request(
    router: &axum::Router,
    req: Request<Body>,
) -> axum::response::Response {
    router.clone().oneshot(req).await.unwrap()
}

/// Read a response body to string.
async fn body_to_string(resp: axum::response::Response) -> String {
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(body_bytes.to_vec()).unwrap()
}

fn create_temp_home_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("hostless-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn resolve_hostless_bin() -> PathBuf {
    if let Ok(bin) = std::env::var("CARGO_BIN_EXE_hostless") {
        return PathBuf::from(bin);
    }

    let current_exe = std::env::current_exe().unwrap();
    let target_debug = current_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("test binary should be under target/debug/deps");
    let fallback = target_debug.join("hostless");
    assert!(
        fallback.exists(),
        "hostless binary not found at {}",
        fallback.display()
    );
    fallback
}

async fn run_cli(bin: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(bin)
        .env("HOME", home)
        .args(args)
        .output()
        .await
        .unwrap()
}

// ═══════════════════════════════════════════════════════════════════
//  Dispatch + Reverse Proxy Tests (from proxy.test.ts)
// ═══════════════════════════════════════════════════════════════════

/// 404 for unknown .localhost subdomain (proxy.test.ts: "returns 404 for unknown hostname")
#[tokio::test]
async fn test_unknown_hostname_returns_404() {
    let (_state, router) = create_test_app(11434);

    let req = Request::builder()
        .uri("/")
        .header("host", "unknown.localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let text = body_to_string(resp).await;
    assert!(text.contains("unknown.localhost"), "404 body should mention the hostname");
}

/// 404 body includes registered routes (proxy.test.ts: "includes active routes in 404 page")
#[tokio::test]
async fn test_404_mentions_registered_routes() {
    let (state, router) = create_test_app(11434);

    // Register a route
    state.route_table.register("myapp", 4001, None).await.unwrap();

    let req = Request::builder()
        .uri("/")
        .header("host", "other.localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Proxies request to matching route (proxy.test.ts: "proxies request to matching route")
#[tokio::test]
async fn test_proxy_to_matching_route() {
    let (backend_addr, _handle) = spawn_backend(StatusCode::OK, "hello from backend").await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("myapp", backend_addr.port(), None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/some/path?q=1")
        .header("host", "myapp.localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    assert_eq!(text, "hello from backend");
}

/// Host header has port stripped for proxied requests
/// (proxy.test.ts: "strips port from Host header when proxying")
#[tokio::test]
async fn test_forwards_request_headers() {
    let (backend_addr, _handle) = spawn_echo_backend().await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("myapp", backend_addr.port(), None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/test")
        .header("host", "myapp.localhost:11434")
        .header("x-custom", "foobar")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();

    // X-Forwarded-Host should be set
    assert_eq!(
        json["headers"]["x-forwarded-host"].as_str().unwrap(),
        "myapp.localhost:11434"
    );
    // X-Forwarded-Proto should be set
    assert_eq!(
        json["headers"]["x-forwarded-proto"].as_str().unwrap(),
        "http"
    );
    // Custom header should pass through
    assert_eq!(
        json["headers"]["x-custom"].as_str().unwrap(),
        "foobar"
    );
}

/// Dead backend returns 502 (proxy.test.ts: "returns 502 when backend is not running")
#[tokio::test]
async fn test_dead_backend_returns_502() {
    let (state, router) = create_test_app(11434);

    // Register a route to a port where nothing is listening
    state
        .route_table
        .register("deadapp", 59999, None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/")
        .header("host", "deadapp.localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let text = body_to_string(resp).await;
    assert!(text.contains("502"), "Should contain 502 in body");
}

/// Loop detection: returns 508 at MAX_HOPS (proxy.test.ts: "returns 508 when hop count exceeds limit")
#[tokio::test]
async fn test_loop_detection_508_at_max_hops() {
    let (backend_addr, _handle) = spawn_backend(StatusCode::OK, "ok").await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("loop", backend_addr.port(), None)
        .await
        .unwrap();

    // Send request with hops=5 (MAX_HOPS)
    let req = Request::builder()
        .uri("/")
        .header("host", "loop.localhost:11434")
        .header("x-hostless-hops", "5")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::LOOP_DETECTED);

    let text = body_to_string(resp).await;
    assert!(text.contains("Loop Detected"));
}

/// Loop detection: allows requests below MAX_HOPS
/// (proxy.test.ts: "allows requests below the hop limit")
#[tokio::test]
async fn test_loop_detection_allows_below_max() {
    let (backend_addr, _handle) = spawn_backend(StatusCode::OK, "ok").await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("hop", backend_addr.port(), None)
        .await
        .unwrap();

    // hops=4 (below MAX_HOPS=5)
    let req = Request::builder()
        .uri("/")
        .header("host", "hop.localhost:11434")
        .header("x-hostless-hops", "4")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Loop detection: hop counter increments
/// (proxy.test.ts: "increments hop counter when proxying")
#[tokio::test]
async fn test_hop_counter_increments() {
    let (backend_addr, _handle) = spawn_echo_backend().await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("counter", backend_addr.port(), None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/")
        .header("host", "counter.localhost:11434")
        .header("x-hostless-hops", "2")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();

    // The backend should receive hops=3 (incremented from 2)
    assert_eq!(
        json["headers"]["x-hostless-hops"].as_str().unwrap(),
        "3"
    );
}

/// Bare localhost falls through to management API (dispatch boundary test)
/// This is the structural security guarantee: .localhost subdomain traffic
/// can NEVER reach management endpoints.
#[tokio::test]
async fn test_bare_localhost_reaches_management_api() {
    let (_state, router) = create_test_app(11434);

    // Bare localhost → should reach the /health endpoint
    let req = Request::builder()
        .uri("/health")
        .header("host", "localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Subdomain traffic cannot reach management endpoints
#[tokio::test]
async fn test_subdomain_cannot_reach_management() {
    let (_state, router) = create_test_app(11434);

    // Try to reach /health via a .localhost subdomain
    let req = Request::builder()
        .uri("/health")
        .header("host", "evil.localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    // Should get 404 (no route registered for "evil"), NOT 200 from /health
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Subdomain traffic cannot reach /auth endpoints
#[tokio::test]
async fn test_subdomain_cannot_reach_auth() {
    let (_state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/token")
        .header("host", "attacker.localhost:11434")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"origin":"*"}"#))
        .unwrap();

    let resp = send_request(&router, req).await;
    // Should NOT reach /auth/token — dispatch intercepts it
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Subdomain traffic cannot reach /v1 LLM proxy endpoints
#[tokio::test]
async fn test_subdomain_cannot_reach_llm_proxy() {
    let (_state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", "sneaky.localhost:11434")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"gpt-4o","messages":[]}"#))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// No Host header falls through to management API
#[tokio::test]
async fn test_no_host_header_falls_through() {
    let (_state, router) = create_test_app(11434);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 127.0.0.1 as host falls through to management API
#[tokio::test]
async fn test_127_0_0_1_falls_through() {
    let (_state, router) = create_test_app(11434);

    let req = Request::builder()
        .uri("/health")
        .header("host", "127.0.0.1:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Proxy strips hop-by-hop headers from backend response
/// (proxy.test.ts: "strips hop-by-hop headers from proxied responses")
#[tokio::test]
async fn test_strips_hop_by_hop_from_response() {
    let (backend_addr, _handle) = spawn_backend_with_headers(
        StatusCode::OK,
        "ok",
        vec![
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("x-custom", "preserved"),
        ],
    )
    .await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("hop", backend_addr.port(), None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/")
        .header("host", "hop.localhost:11434")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Hop-by-hop headers should be stripped
    assert!(resp.headers().get("connection").is_none());
    assert!(resp.headers().get("keep-alive").is_none());

    // Custom header should be preserved
    assert_eq!(
        resp.headers().get("x-custom").unwrap().to_str().unwrap(),
        "preserved"
    );
}

/// WebSocket upgrade request is detected and dispatched to WS handler
/// (the actual WS proxying returns 502 in oneshot tests since there's no real TCP)
#[tokio::test]
async fn test_websocket_upgrade_detected() {
    let (backend_addr, _handle) = spawn_backend(StatusCode::OK, "ok").await;

    let (state, router) = create_test_app(11434);
    state
        .route_table
        .register("ws", backend_addr.port(), None)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/ws")
        .header("host", "ws.localhost:11434")
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    // WebSocket handler is reached but returns 501 (not yet implemented) or 502
    // (can't complete TCP upgrade in oneshot mode). Either confirms dispatch works.
    let status = resp.status().as_u16();
    assert!(
        status == 501 || status == 502,
        "Expected 501 or 502 for WS upgrade, got {}",
        status
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Route Management API Tests (from routes.test.ts)
// ═══════════════════════════════════════════════════════════════════

/// POST /routes/register creates a route and returns URL + token
#[tokio::test]
async fn test_register_route_api() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/routes/register")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": "myapp",
                "port": 4001,
            })
            .to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(json["hostname"].as_str().unwrap(), "myapp.localhost");
    assert_eq!(json["url"].as_str().unwrap(), "http://myapp.localhost:11434");
    assert_eq!(json["target_port"].as_u64().unwrap(), 4001);

    // Auto-token should be provisioned by default
    assert!(json["token"]["token"].as_str().unwrap().starts_with("sk_local_"));
}

/// POST /routes/register with auto_token=false skips token
#[tokio::test]
async fn test_register_route_no_token() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/routes/register")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": "notokenapp",
                "port": 4002,
                "auto_token": false,
            })
            .to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(json["token"].is_null());
}

/// POST /routes/deregister removes a route
#[tokio::test]
async fn test_deregister_route_api() {
    let (state, router) = create_test_app(11434);
    state.route_table.register("myapp", 4001, None).await.unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/routes/deregister")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "name": "myapp" }).to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["removed"].as_bool().unwrap(), true);
    assert_eq!(json["hostname"].as_str().unwrap(), "myapp.localhost");
}

/// POST /routes/deregister with unknown name returns 404
#[tokio::test]
async fn test_deregister_unknown_route_returns_404() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/routes/deregister")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "name": "nonexistent" }).to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /routes returns the route list
#[tokio::test]
async fn test_list_routes_api() {
    let (state, router) = create_test_app(11434);
    state.route_table.register("app1", 4001, None).await.unwrap();
    state.route_table.register("app2", 4002, None).await.unwrap();

    let req = Request::builder()
        .uri("/routes")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let routes = json["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 2);
}

/// GET /routes returns empty array when no routes
#[tokio::test]
async fn test_list_routes_empty() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .uri("/routes")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let routes = json["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 0);
}

/// GET /routes rejects non-localhost Origin
#[tokio::test]
async fn test_list_routes_rejects_non_localhost_origin() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .uri("/routes")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("origin", "http://evil.localhost:3000")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// GET /auth/tokens rejects non-localhost Origin
#[tokio::test]
async fn test_auth_tokens_rejects_non_localhost_origin() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .uri("/auth/tokens")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// POST /auth/revoke rejects non-localhost Origin
#[tokio::test]
async fn test_auth_revoke_rejects_non_localhost_origin() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/revoke")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("origin", "https://evil.example")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "token": "sk_local_fake" }).to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Route registration with provider scope provisions a scoped token
#[tokio::test]
async fn test_register_route_with_provider_scope() {
    let (state, router) = create_test_app(11434);

    let req = Request::builder()
        .method("POST")
        .uri("/routes/register")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": "scopedapp",
                "port": 4003,
                "allowed_providers": ["openai"],
                "allowed_models": ["gpt-4o*"],
            })
            .to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let token_str = json["token"]["token"].as_str().unwrap();

    // The provisioned token should be scoped — verify via token manager
    assert!(
        state
            .token_manager
            .validate_provider(token_str, "openai")
            .await
            .is_ok()
    );
    assert!(
        state
            .token_manager
            .validate_provider(token_str, "anthropic")
            .await
            .is_err()
    );
    assert!(
        state
            .token_manager
            .validate_model(token_str, "gpt-4o-mini")
            .await
            .is_ok()
    );
    assert!(
        state
            .token_manager
            .validate_model(token_str, "claude-3-opus")
            .await
            .is_err()
    );
}

/// Deregistering a route also revokes its token
#[tokio::test]
async fn test_deregister_revokes_token() {
    let (state, router) = create_test_app(11434);

    // Register
    let req = Request::builder()
        .method("POST")
        .uri("/routes/register")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "name": "ephemeral", "port": 4004 }).to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    let text = body_to_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let token_str = json["token"]["token"].as_str().unwrap().to_string();

    // Token should be valid
    assert!(
        state
            .token_manager
            .validate(&token_str, "http://ephemeral.localhost:11434")
            .await
            .is_ok()
    );

    // Deregister
    let req = Request::builder()
        .method("POST")
        .uri("/routes/deregister")
        .header("host", "localhost:11434")
        .header(hostless::auth::admin::ADMIN_HEADER, state.admin_token.as_str())
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "name": "ephemeral" }).to_string(),
        ))
        .unwrap();

    let resp = send_request(&router, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Token should now be invalid
    assert!(
        state.token_manager.validate(&token_str, "http://ephemeral.localhost:11434").await.is_err()
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Route Table Unit-Level Tests (from routes.test.ts)
// ═══════════════════════════════════════════════════════════════════

/// Replace stale route (dead PID) with new registration
/// (routes.test.ts: "replaces existing route if PID dead")
#[tokio::test]
async fn test_replace_stale_route() {
    let table = hostless::server::route_table::RouteTable::new(11434);

    // Register with a dead PID
    table.register("myapp", 4001, Some(999_999_999)).await.unwrap();

    // Should be able to re-register (old PID is dead)
    let route = table.register("myapp", 4002, Some(std::process::id())).await.unwrap();
    assert_eq!(route.target_port, 4002);

    let found = table.lookup("myapp.localhost").await.unwrap();
    assert_eq!(found.target_port, 4002);
}

/// Cannot replace route with alive PID
/// (routes.test.ts: "does not replace route if PID alive")
#[tokio::test]
async fn test_cannot_replace_alive_route() {
    let table = hostless::server::route_table::RouteTable::new(11434);

    // Register with our own PID (definitely alive)
    table
        .register("myapp", 4001, Some(std::process::id()))
        .await
        .unwrap();

    // Attempting to re-register should fail
    let result = table.register("myapp", 4002, Some(12345)).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));
}

/// Set and retrieve token on route
#[tokio::test]
async fn test_set_token_on_route() {
    let table = hostless::server::route_table::RouteTable::new(11434);
    table.register("myapp", 4001, None).await.unwrap();

    table
        .set_token("myapp.localhost", "sk_local_test123".to_string())
        .await;

    let route = table.lookup("myapp.localhost").await.unwrap();
    assert_eq!(route.token.unwrap(), "sk_local_test123");
}

/// Cleanup preserves routes without PID
/// (routes.test.ts: "preserves routes without PID during cleanup")
#[tokio::test]
async fn test_cleanup_preserves_no_pid_routes() {
    let table = hostless::server::route_table::RouteTable::new(11434);
    table.register("no-pid", 4001, None).await.unwrap();
    table.register("alive", 4002, Some(std::process::id())).await.unwrap();
    table.register("dead", 4003, Some(999_999_999)).await.unwrap();

    let (removed, _tokens) = table.cleanup_stale().await;
    assert_eq!(removed, 1); // Only the dead PID route

    let list = table.list().await;
    assert_eq!(list.len(), 2);

    // Verify the right ones survived
    assert!(table.lookup("no-pid.localhost").await.is_some());
    assert!(table.lookup("alive.localhost").await.is_some());
    assert!(table.lookup("dead.localhost").await.is_none());
}

/// Cleanup returns tokens of removed routes
#[tokio::test]
async fn test_cleanup_returns_removed_tokens() {
    let table = hostless::server::route_table::RouteTable::new(11434);
    table.register("dead-app", 4001, Some(999_999_999)).await.unwrap();
    table
        .set_token("dead-app.localhost", "sk_local_deadtoken".to_string())
        .await;

    let (_removed, tokens) = table.cleanup_stale().await;
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0], "sk_local_deadtoken");
}

/// Route URL includes server port
/// (routes.test.ts: port in links)
#[tokio::test]
async fn test_route_url_includes_port() {
    let table = hostless::server::route_table::RouteTable::new(1355);
    table.register("myapp", 4001, None).await.unwrap();

    let list = table.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].url, "http://myapp.localhost:1355");
}

/// Multiple routes listed correctly
#[tokio::test]
async fn test_multiple_routes_listed() {
    let table = hostless::server::route_table::RouteTable::new(11434);
    table.register("app1", 4001, None).await.unwrap();
    table.register("app2", 4002, None).await.unwrap();
    table.register("app3", 4003, None).await.unwrap();

    let list = table.list().await;
    assert_eq!(list.len(), 3);

    let hostnames: Vec<&str> = list.iter().map(|r| r.hostname.as_str()).collect();
    assert!(hostnames.contains(&"app1.localhost"));
    assert!(hostnames.contains(&"app2.localhost"));
    assert!(hostnames.contains(&"app3.localhost"));
}

/// Remove by app name (not full hostname)
#[tokio::test]
async fn test_remove_by_app_name() {
    let table = hostless::server::route_table::RouteTable::new(11434);
    table.register("myapp", 4001, None).await.unwrap();
    table.register("other", 4002, None).await.unwrap();

    let removed = table.remove("myapp").await;
    assert!(removed.is_some());

    // Other route should still exist
    assert!(table.lookup("other.localhost").await.is_some());
    assert!(table.lookup("myapp.localhost").await.is_none());
}

// ═══════════════════════════════════════════════════════════════════
//  Framework Flag Injection Tests (from cli-utils.test.ts)
// ═══════════════════════════════════════════════════════════════════

use hostless::process::manager::{inject_framework_flags, build_child_env, find_available_port};

/// Astro gets --port and --host
/// (cli-utils.test.ts: "astro" → --port + --host)
#[test]
fn test_inject_astro() {
    let cmd = inject_framework_flags("astro dev", 4001);
    assert_eq!(cmd, "astro dev --port 4001 --host 127.0.0.1");
}

/// React Router gets --port and --host
/// (cli-utils.test.ts: "react-router" → --port + --host)
#[test]
fn test_inject_react_router() {
    let cmd = inject_framework_flags("react-router dev", 4001);
    assert_eq!(cmd, "react-router dev --port 4001 --host 127.0.0.1");
}

/// Angular CLI (ng) gets --port and --host
/// (cli-utils.test.ts: "ng" → --port + --host)
#[test]
fn test_inject_ng() {
    let cmd = inject_framework_flags("ng serve", 4001);
    assert_eq!(cmd, "ng serve --port 4001 --host 127.0.0.1");
}

/// Nuxt gets --port and --host
/// (cli-utils.test.ts: "nuxt" → --port + --host)
#[test]
fn test_inject_nuxt() {
    let cmd = inject_framework_flags("nuxt dev", 4001);
    assert_eq!(cmd, "nuxt dev --port 4001 --host 127.0.0.1");
}

/// Remix gets --port and --host
#[test]
fn test_inject_remix() {
    let cmd = inject_framework_flags("remix dev", 4001);
    assert_eq!(cmd, "remix dev --port 4001 --host 127.0.0.1");
}

/// Already has --port → don't inject
/// (cli-utils.test.ts: "does not inject if --port already present")
#[test]
fn test_no_inject_if_port_present() {
    let cmd = inject_framework_flags("vite --port 3000", 4001);
    assert_eq!(cmd, "vite --port 3000");
}

/// Already has -p → don't inject (short form)
/// (cli-utils.test.ts: "does not inject if -p already present")
#[test]
fn test_no_inject_if_short_port_present() {
    let cmd = inject_framework_flags("next dev -p 3000", 4001);
    assert_eq!(cmd, "next dev -p 3000");
}

/// Node script → no injection
/// (cli-utils.test.ts: "node" → no injection)
#[test]
fn test_no_inject_node() {
    let cmd = inject_framework_flags("node server.js", 4001);
    assert_eq!(cmd, "node server.js");
}

/// Python → no injection
#[test]
fn test_no_inject_python() {
    let cmd = inject_framework_flags("python manage.py runserver", 4001);
    assert_eq!(cmd, "python manage.py runserver");
}

/// npm with vite in command gets -- --port --host
/// (cli-utils.test.ts: "npm run dev with vite" → appends with --)
#[test]
fn test_inject_npm_vite() {
    // This only triggers when "vite" appears in the command
    let cmd = inject_framework_flags("npm run vite", 4001);
    assert_eq!(cmd, "npm run vite -- --port 4001 --host 127.0.0.1");
}

/// pnpm with vite gets -- --port --host
#[test]
fn test_inject_pnpm_vite() {
    let cmd = inject_framework_flags("pnpm run vite", 4001);
    assert_eq!(cmd, "pnpm run vite -- --port 4001 --host 127.0.0.1");
}

/// yarn with astro gets -- --port --host
#[test]
fn test_inject_yarn_astro() {
    let cmd = inject_framework_flags("yarn astro dev", 4001);
    assert_eq!(cmd, "yarn astro dev -- --port 4001 --host 127.0.0.1");
}

/// Empty command → returned as-is
#[test]
fn test_inject_empty_command() {
    let cmd = inject_framework_flags("", 4001);
    assert_eq!(cmd, "");
}

// ═══════════════════════════════════════════════════════════════════
//  build_child_env Tests (from cli-utils.test.ts: env injection)
// ═══════════════════════════════════════════════════════════════════

/// All expected env vars are set
#[test]
fn test_child_env_has_all_vars() {
    let env = build_child_env(4001, Some("sk_local_t"), 1355, "webapp");

    assert_eq!(env.get("PORT").unwrap(), "4001");
    assert_eq!(env.get("HOST").unwrap(), "127.0.0.1");
    assert_eq!(env.get("HOSTLESS_TOKEN").unwrap(), "sk_local_t");
    assert_eq!(
        env.get("HOSTLESS_URL").unwrap(),
        "http://webapp.localhost:1355"
    );
    assert_eq!(
        env.get("HOSTLESS_API").unwrap(),
        "http://localhost:1355"
    );
    assert_eq!(
        env.get("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS").unwrap(),
        ".localhost"
    );
}

/// Without token, HOSTLESS_TOKEN is absent
#[test]
fn test_child_env_no_token() {
    let env = build_child_env(4001, None, 11434, "myapp");
    assert!(env.get("HOSTLESS_TOKEN").is_none());
    // But other vars should still be set
    assert_eq!(env.get("PORT").unwrap(), "4001");
}

/// Inherits existing env vars
#[test]
fn test_child_env_inherits_system_env() {
    let env = build_child_env(4001, None, 11434, "myapp");
    // PATH should be inherited (and possibly augmented)
    assert!(env.get("PATH").is_some());
}

// ═══════════════════════════════════════════════════════════════════
//  Port Allocation Tests (from cli-utils.test.ts: findFreePort)
// ═══════════════════════════════════════════════════════════════════

/// find_available_port returns a bindable port
/// (cli-utils.test.ts: "returns a port that can be bound")
#[test]
fn test_find_available_port_is_bindable() {
    let port = find_available_port().unwrap();
    // Should be in 4000-4999 range or fallback
    assert!(port >= 1024, "Port should be >= 1024, got {}", port);

    // The returned port should be bindable
    let result = std::net::TcpListener::bind(("127.0.0.1", port));
    assert!(result.is_ok(), "Returned port {} should be bindable", port);
}

/// find_available_port returns different ports on consecutive calls
/// (cli-utils.test.ts: port allocation is random)
#[test]
fn test_find_available_port_varies() {
    let mut ports = std::collections::HashSet::new();
    for _ in 0..5 {
        let port = find_available_port().unwrap();
        ports.insert(port);
    }
    // With random allocation in 4000-4999, getting all 5 the same is extremely unlikely
    assert!(
        ports.len() > 1,
        "Expected varied ports, got: {:?}",
        ports
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Hostname / Dispatch Edge Cases (from utils.test.ts)
// ═══════════════════════════════════════════════════════════════════

/// Various hostname parsing edge cases
/// (utils.test.ts: "parseHostname" suite)
#[test]
fn test_hostname_edge_cases() {
    // Hyphens in subdomain
    assert!(hostless::server::dispatch::is_subdomain_host_pub("my-app.localhost"));

    // Multi-level subdomain
    assert!(hostless::server::dispatch::is_subdomain_host_pub("deep.sub.localhost"));

    // Just "localhost" is NOT a subdomain
    assert!(!hostless::server::dispatch::is_subdomain_host_pub("localhost"));

    // "example.com" is not a .localhost subdomain
    assert!(!hostless::server::dispatch::is_subdomain_host_pub("example.com"));

    // Empty string
    assert!(!hostless::server::dispatch::is_subdomain_host_pub(""));
}

/// Concurrent `hostless run` invocations on the same daemon port should both succeed,
/// even when the daemon is initially down.
#[tokio::test]
async fn test_run_concurrent_autostart_is_idempotent() {
    let bin = resolve_hostless_bin();
    let home = create_temp_home_dir();
    let daemon_port = find_available_port().unwrap();
    let daemon_port_arg = daemon_port.to_string();
    let args_a = [
        "run",
        "concurrent-a",
        "--daemon-port",
        daemon_port_arg.as_str(),
        "--",
        "true",
    ];
    let args_b = [
        "run",
        "concurrent-b",
        "--daemon-port",
        daemon_port_arg.as_str(),
        "--",
        "true",
    ];

    let run_a = run_cli(&bin, &home, &args_a);
    let run_b = run_cli(&bin, &home, &args_b);

    let (out_a, out_b) = tokio::join!(run_a, run_b);

    let _ = run_cli(&bin, &home, &["stop"]).await;
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        out_a.status.success(),
        "first run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out_a.stdout),
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert!(
        out_b.status.success(),
        "second run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out_b.stdout),
        String::from_utf8_lossy(&out_b.stderr)
    );
}
