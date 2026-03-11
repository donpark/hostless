use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, Uri},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::copy_bidirectional;
use tracing::{error, info, warn};

use super::streaming;
use super::AppState;
use crate::auth::middleware::ValidatedToken;
use crate::providers::{self, google::GoogleProvider};

// ─── Health ──────────────────────────────────────────────

pub async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "service": "hostless",
    }))
}

fn is_bare_localhost_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };

    matches!(url.host_str(), Some("localhost") | Some("127.0.0.1"))
}

fn is_bare_localhost_host(host: &str) -> bool {
    let hostname = if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            &host[..=end + 1]
        } else {
            host
        }
    } else if host.matches(':').count() == 1 {
        host.split_once(':').map(|(h, _)| h).unwrap_or(host)
    } else {
        host
    };

    matches!(hostname, "localhost" | "127.0.0.1" | "[::1]")
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    let max_len = left_bytes.len().max(right_bytes.len());

    let mut diff = left_bytes.len() ^ right_bytes.len();
    for i in 0..max_len {
        let a = *left_bytes.get(i).unwrap_or(&0);
        let b = *right_bytes.get(i).unwrap_or(&0);
        diff |= (a ^ b) as usize;
    }

    diff == 0
}

fn query_param(uri: &Uri, name: &str) -> Option<String> {
    let query = uri.query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn realtime_upstream_base(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("https://{}", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("http://{}", rest)
    } else if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{}", rest)
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{}", rest)
    } else {
        format!("https://{}", trimmed)
    }
}

fn openai_upstream_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

async fn proxy_openai_media(
    state: Arc<AppState>,
    req: axum::extract::Request,
    upstream_path: &str,
    enforce_json_model_scope: bool,
) -> Response {
    let validated_token = req.extensions().get::<ValidatedToken>().cloned();
    let headers = req.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let body_bytes = match axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!("Failed to read request body: {}", e),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let mut scoped_model: Option<String> = None;
    if enforce_json_model_scope {
        if let Ok(v) = serde_json::from_slice::<Value>(&body_bytes) {
            if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                let (provider_key, _) = providers::resolve_provider(model);
                if provider_key != "openai" {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": {
                                "message": format!("'{}' currently supports OpenAI-compatible models only.", upstream_path),
                                "type": "invalid_request_error",
                            }
                        })),
                    )
                        .into_response();
                }
                scoped_model = Some(model.to_string());
            }
        }
    }

    if let Some(ref vt) = validated_token {
        if let Err(e) = state.token_manager.validate_provider(&vt.0, "openai").await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }

        if let Some(ref model) = scoped_model {
            if let Err(e) = state.token_manager.validate_model(&vt.0, model).await {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": {
                            "message": format!("{}", e),
                            "type": "scope_error",
                        }
                    })),
                )
                    .into_response();
            }
        }
    }

    let (api_key, custom_base_url) = match state.vault.get_key("openai").await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "No API key configured for provider 'openai'. Use 'hostless keys add openai <key>' to add one.",
                        "type": "configuration_error",
                    }
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to retrieve API key: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": "Failed to retrieve API key from vault",
                        "type": "internal_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let base_url = custom_base_url.as_deref().unwrap_or("https://api.openai.com");
    let url = openai_upstream_url(base_url, upstream_path);

    let mut req_builder = state
        .http_client
        .post(&url)
        .header(header::AUTHORIZATION.as_str(), format!("Bearer {}", api_key));

    if let Some(ct) = content_type {
        req_builder = req_builder.header(header::CONTENT_TYPE.as_str(), ct);
    }

    if let Some(beta) = headers
        .get("openai-beta")
        .and_then(|v| v.to_str().ok())
    {
        req_builder = req_builder.header("openai-beta", beta);
    }

    let upstream_response = match req_builder.body(body_bytes).send().await {
        Ok(resp) => resp,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("Failed to reach upstream provider: {}", e),
                        "type": "upstream_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_content_type = upstream_response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let response_body = match upstream_response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("Failed to read upstream response: {}", e),
                        "type": "upstream_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let mut response = Response::builder().status(status);
    if let Some(ct) = upstream_content_type {
        response = response.header(header::CONTENT_TYPE, ct);
    }

    response
        .body(Body::from(response_body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn ensure_local_management_access(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let admin_header = headers
        .get(crate::auth::admin::ADMIN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if admin_header.is_empty() || !constant_time_eq(admin_header, &state.admin_token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "Missing or invalid management authentication"
            })),
        )
            .into_response());
    }

    let origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if origin.is_empty() || is_bare_localhost_origin(origin) {
        return Ok(());
    }

    Err((
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "Management endpoint is only available from localhost"
        })),
    )
        .into_response())
}

async fn parse_json_request_body(req: axum::extract::Request) -> Result<Value, Response> {
    match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(v) => Ok(v),
            Err(e) => Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!("Invalid JSON body: {}", e),
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response()),
        },
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": format!("Failed to read request body: {}", e),
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response()),
    }
}

// ─── Chat Completions (main proxy endpoint) ──────────────

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    // Extract validated token from middleware (if present)
    let validated_token = req.extensions().get::<ValidatedToken>().cloned();

    // Parse body
    let body: Value = match parse_json_request_body(req).await {
        Ok(v) => v,
        Err(resp) => {
            return resp;
        }
    };

    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("gpt-4o");

    let (provider_key, resolved_model) = providers::resolve_provider(model);

    // Enforce provider scope if a token was validated
    if let Some(ref vt) = validated_token {
        if let Err(e) = state.token_manager.validate_provider(&vt.0, provider_key).await {
            warn!(provider = provider_key, "Provider not allowed by token scope");
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }

        // Enforce model scope
        if let Err(e) = state
            .token_manager
            .validate_model(&vt.0, model)
            .await
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }
    }

    // Look up the API key from the vault
    let (api_key, custom_base_url) = match state.vault.get_key(provider_key).await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            warn!("No API key stored for provider '{}'", provider_key);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": format!("No API key configured for provider '{}'. Use 'hostless keys add {} <key>' to add one.", provider_key, provider_key),
                        "type": "configuration_error",
                    }
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to retrieve API key: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": "Failed to retrieve API key from vault",
                        "type": "internal_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let provider = providers::get_provider(provider_key);

    // Update model name in body to use the resolved (unprefixed) name
    let mut request_body = body.clone();
    request_body["model"] = json!(resolved_model);

    let base_url = custom_base_url
        .as_deref()
        .unwrap_or(provider.default_base_url());

    // Transform the request
    let (mut url, transformed_body, extra_headers) =
        match provider.transform_request(base_url, &request_body) {
            Ok(result) => result,
            Err(e) => {
                error!("Request transformation failed: {}", e);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": format!("Invalid request: {}", e),
                            "type": "invalid_request_error",
                        }
                    })),
                )
                    .into_response();
            }
        };

    // Google uses query param for API key
    if provider_key == "google" {
        url = GoogleProvider::append_api_key_to_url(&url, &api_key);
    }

    // Build the upstream request
    let req_builder = state
        .http_client
        .post(&url)
        .headers(provider.auth_headers(&api_key))
        .headers(extra_headers)
        .header("Content-Type", "application/json")
        .json(&transformed_body);

    let is_stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    info!(
        provider = provider_key,
        model = resolved_model.as_str(),
        stream = is_stream,
        "Proxying request"
    );

    // Send the request
    let upstream_response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Upstream request failed: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("Failed to reach upstream provider: {}", e),
                        "type": "upstream_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let upstream_status = upstream_response.status();

    if !upstream_status.is_success() {
        let error_body = upstream_response.text().await.unwrap_or_default();
        warn!(
            status = upstream_status.as_u16(),
            "Upstream returned error: {}",
            &error_body[..error_body.len().min(500)]
        );
        return (
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(
                serde_json::from_str::<Value>(&error_body).unwrap_or(json!({
                    "error": {
                        "message": error_body,
                        "type": "upstream_error",
                    }
                })),
            ),
        )
            .into_response();
    }

    if is_stream {
        streaming::stream_response(upstream_response, provider).await
    } else {
        // Non-streaming: read full response, transform, return
        match upstream_response.json::<Value>().await {
            Ok(resp_body) => match provider.transform_response(resp_body) {
                Ok(transformed) => Json(transformed).into_response(),
                Err(e) => {
                    error!("Response transformation failed: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": {
                                "message": "Failed to transform upstream response",
                                "type": "internal_error",
                            }
                        })),
                    )
                        .into_response()
                }
            },
            Err(e) => {
                error!("Failed to parse upstream response: {}", e);
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": {
                            "message": "Failed to parse upstream response",
                            "type": "upstream_error",
                        }
                    })),
                )
                    .into_response()
            }
        }
    }
}

// ─── Responses API (OpenAI-compatible passthrough) ───────

pub async fn responses(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    let validated_token = req.extensions().get::<ValidatedToken>().cloned();

    let body = match parse_json_request_body(req).await {
        Ok(v) => v,
        Err(resp) => {
            return resp;
        }
    };

    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("gpt-4o");

    let (provider_key, _resolved_model) = providers::resolve_provider(model);
    if provider_key != "openai" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "'/v1/responses' currently supports OpenAI-compatible models only. Use '/v1/chat/completions' for anthropic/* and google/* routed requests.",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    }

    if let Some(ref vt) = validated_token {
        if let Err(e) = state.token_manager.validate_provider(&vt.0, provider_key).await {
            warn!(provider = provider_key, "Provider not allowed by token scope");
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }

        if let Err(e) = state.token_manager.validate_model(&vt.0, model).await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }
    }

    let (api_key, custom_base_url) = match state.vault.get_key("openai").await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            warn!("No API key stored for provider 'openai'");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "No API key configured for provider 'openai'. Use 'hostless keys add openai <key>' to add one.",
                        "type": "configuration_error",
                    }
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to retrieve API key: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": "Failed to retrieve API key from vault",
                        "type": "internal_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let openai_provider = providers::get_provider("openai");
    let base_url = custom_base_url
        .as_deref()
        .unwrap_or(openai_provider.default_base_url());
    let url = format!("{}/v1/responses", base_url.trim_end_matches('/'));

    let is_stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    info!(
        endpoint = "/v1/responses",
        model = model,
        stream = is_stream,
        "Proxying request"
    );

    let upstream_response = match state
        .http_client
        .post(&url)
        .headers(openai_provider.auth_headers(&api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!("Upstream request failed: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": format!("Failed to reach upstream provider: {}", e),
                        "type": "upstream_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let upstream_status = upstream_response.status();
    if !upstream_status.is_success() {
        let error_body = upstream_response.text().await.unwrap_or_default();
        warn!(
            status = upstream_status.as_u16(),
            "Upstream returned error: {}",
            &error_body[..error_body.len().min(500)]
        );
        return (
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(
                serde_json::from_str::<Value>(&error_body).unwrap_or(json!({
                    "error": {
                        "message": error_body,
                        "type": "upstream_error",
                    }
                })),
            ),
        )
            .into_response();
    }

    if is_stream {
        streaming::stream_passthrough_response(upstream_response).await
    } else {
        match upstream_response.json::<Value>().await {
            Ok(resp_body) => Json(resp_body).into_response(),
            Err(e) => {
                error!("Failed to parse upstream response: {}", e);
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": {
                            "message": "Failed to parse upstream response",
                            "type": "upstream_error",
                        }
                    })),
                )
                    .into_response()
            }
        }
    }
}

// ─── Responses API WebSocket Mode ────────────────────────

pub async fn responses_websocket(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
) -> Response {
    let is_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if !is_upgrade {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "'/v1/responses' websocket mode requires a WebSocket upgrade request",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    }

    let validated_token = req.extensions().get::<ValidatedToken>().cloned();
    let model = match query_param(req.uri(), "model") {
        Some(m) => m,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "'/v1/responses' websocket mode requires a '?model=...' query parameter for pre-upgrade scope enforcement.",
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let (provider_key, _) = providers::resolve_provider(&model);
    if provider_key != "openai" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "'/v1/responses' websocket mode currently supports OpenAI-compatible models only.",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    }

    if let Some(ref vt) = validated_token {
        if let Err(e) = state.token_manager.validate_provider(&vt.0, "openai").await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }

        if let Err(e) = state.token_manager.validate_model(&vt.0, &model).await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }
    }

    let (api_key, custom_base_url) = match state.vault.get_key("openai").await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "No API key configured for provider 'openai'. Use 'hostless keys add openai <key>' to add one.",
                        "type": "configuration_error",
                    }
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to retrieve API key: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": "Failed to retrieve API key from vault",
                        "type": "internal_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let base_url = custom_base_url
        .as_deref()
        .unwrap_or("https://api.openai.com");
    let upstream_base = realtime_upstream_base(base_url);
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/v1/responses");

    let upstream_uri: Uri = match format!("{}{}", upstream_base, path_and_query).parse() {
        Ok(uri) => uri,
        Err(e) => {
            warn!(error = %e, "Rejected malformed responses websocket upstream URI");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "Malformed responses websocket request URI",
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let client_on_upgrade = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();
    let mut headers = parts.headers.clone();

    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", api_key))
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );

    if let Some(authority) = upstream_uri.authority() {
        if let Ok(host) = HeaderValue::from_str(authority.as_str()) {
            headers.insert(header::HOST, host);
        }
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let https = match hyper_rustls::HttpsConnectorBuilder::new().with_native_roots() {
        Ok(builder) => builder
            .https_or_http()
            .enable_http1()
            .build(),
        Err(e) => {
            error!(error = %e, "Failed to initialize TLS roots for responses websocket proxy");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let client: Client<_, Body> = Client::builder(TokioExecutor::new()).build(https);

    let mut upstream_req = axum::http::Request::builder()
        .method(parts.method)
        .uri(upstream_uri);
    if let Some(h) = upstream_req.headers_mut() {
        *h = headers;
    }

    let upstream_req = match upstream_req.body(body) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to build responses websocket upstream request");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut upstream_resp = match client.request(upstream_req).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, "Responses websocket upstream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": "Failed to reach upstream responses websocket endpoint",
                        "type": "upstream_error",
                    }
                })),
            )
                .into_response();
        }
    };

    if upstream_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        warn!(status = upstream_resp.status().as_u16(), "Responses websocket upstream rejected upgrade");
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": {
                    "message": "Upstream rejected responses websocket upgrade",
                    "type": "upstream_error",
                }
            })),
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
                warn!(error = %e, "Responses websocket upgrade failed");
                return;
            }
        };

        let mut client_ws = TokioIo::new(client_ws);
        let mut upstream_ws = TokioIo::new(upstream_ws);

        if let Err(e) = copy_bidirectional(&mut client_ws, &mut upstream_ws).await {
            warn!(error = %e, "Responses websocket proxy stream closed with error");
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

// ─── Realtime API (WebSocket upgrade passthrough) ─────────

pub async fn realtime(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
) -> Response {
    let is_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if !is_upgrade {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "'/v1/realtime' requires a WebSocket upgrade request",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    }

    let validated_token = req.extensions().get::<ValidatedToken>().cloned();
    let model = query_param(req.uri(), "model")
        .unwrap_or_else(|| "gpt-4o-realtime-preview".to_string());
    let (provider_key, _resolved_model) = providers::resolve_provider(&model);

    if provider_key != "openai" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "'/v1/realtime' currently supports OpenAI-compatible models only.",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    }

    if let Some(ref vt) = validated_token {
        if let Err(e) = state.token_manager.validate_provider(&vt.0, provider_key).await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }

        if let Err(e) = state.token_manager.validate_model(&vt.0, &model).await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "message": format!("{}", e),
                        "type": "scope_error",
                    }
                })),
            )
                .into_response();
        }
    }

    let (api_key, custom_base_url) = match state.vault.get_key("openai").await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "No API key configured for provider 'openai'. Use 'hostless keys add openai <key>' to add one.",
                        "type": "configuration_error",
                    }
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to retrieve API key: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": "Failed to retrieve API key from vault",
                        "type": "internal_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let base_url = custom_base_url
        .as_deref()
        .unwrap_or("https://api.openai.com");
    let upstream_base = realtime_upstream_base(base_url);
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/v1/realtime");

    let upstream_uri: Uri = match format!("{}{}", upstream_base, path_and_query).parse() {
        Ok(uri) => uri,
        Err(e) => {
            warn!(error = %e, "Rejected malformed realtime upstream URI");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "Malformed realtime request URI",
                        "type": "invalid_request_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let client_on_upgrade = hyper::upgrade::on(&mut req);
    let (parts, body) = req.into_parts();
    let mut headers = parts.headers.clone();

    // Replace client auth with upstream provider key.
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", api_key))
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        header::HeaderName::from_static("openai-beta"),
        HeaderValue::from_static("realtime=v1"),
    );

    if let Some(authority) = upstream_uri.authority() {
        if let Ok(host) = HeaderValue::from_str(authority.as_str()) {
            headers.insert(header::HOST, host);
        }
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let https = match hyper_rustls::HttpsConnectorBuilder::new().with_native_roots() {
        Ok(builder) => builder
            .https_or_http()
            .enable_http1()
            .build(),
        Err(e) => {
            error!(error = %e, "Failed to initialize TLS roots for realtime proxy");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let client: Client<_, Body> = Client::builder(TokioExecutor::new()).build(https);

    let mut upstream_req = axum::http::Request::builder()
        .method(parts.method)
        .uri(upstream_uri);
    if let Some(h) = upstream_req.headers_mut() {
        *h = headers;
    }

    let upstream_req = match upstream_req.body(body) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to build realtime upstream request");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut upstream_resp = match client.request(upstream_req).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(error = %e, "Realtime upstream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "message": "Failed to reach upstream realtime endpoint",
                        "type": "upstream_error",
                    }
                })),
            )
                .into_response();
        }
    };

    if upstream_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        warn!(status = upstream_resp.status().as_u16(), "Realtime upstream rejected websocket upgrade");
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": {
                    "message": "Upstream rejected realtime websocket upgrade",
                    "type": "upstream_error",
                }
            })),
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
                warn!(error = %e, "Realtime websocket upgrade failed");
                return;
            }
        };

        let mut client_ws = TokioIo::new(client_ws);
        let mut upstream_ws = TokioIo::new(upstream_ws);

        if let Err(e) = copy_bidirectional(&mut client_ws, &mut upstream_ws).await {
            warn!(error = %e, "Realtime websocket proxy stream closed with error");
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

// ─── Media APIs (OpenAI-compatible passthrough) ──────────

pub async fn audio_speech(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    proxy_openai_media(state, req, "/v1/audio/speech", true).await
}

pub async fn audio_transcriptions(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    proxy_openai_media(state, req, "/v1/audio/transcriptions", false).await
}

pub async fn audio_translations(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    proxy_openai_media(state, req, "/v1/audio/translations", false).await
}

pub async fn images_generations(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    proxy_openai_media(state, req, "/v1/images/generations", true).await
}

pub async fn files_upload(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    proxy_openai_media(state, req, "/v1/files", false).await
}

// ─── Embeddings ──────────────────────────────────────────

pub async fn embeddings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    // Embeddings are only supported for OpenAI-compatible providers
    let (api_key, custom_base_url) = match state.vault.get_key("openai").await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "No API key configured for OpenAI. Embeddings require an OpenAI-compatible provider.",
                        "type": "configuration_error",
                    }
                })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to retrieve API key: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": {
                        "message": "Failed to retrieve API key",
                        "type": "internal_error",
                    }
                })),
            )
                .into_response();
        }
    };

    let base_url = custom_base_url.as_deref().unwrap_or("https://api.openai.com");
    let url = format!("{}/v1/embeddings", base_url.trim_end_matches('/'));

    let resp = state
        .http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body = r.json::<Value>().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            (
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(serde_json::from_str::<Value>(&body).unwrap_or(json!({"error": body}))),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"message": format!("Upstream error: {}", e)}})),
        )
            .into_response(),
    }
}

// ─── OAuth Callback ──────────────────────────────────────

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct OAuthCallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn oauth_callback(
    State(_state): State<Arc<AppState>>,
    Query(params): Query<OAuthCallbackParams>,
) -> Response {
    if let Some(error) = params.error {
        return (
            StatusCode::BAD_REQUEST,
            format!("OAuth error: {}", error),
        )
            .into_response();
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            return (StatusCode::BAD_REQUEST, "Missing 'code' parameter").into_response();
        }
    };

    info!("Received OAuth callback with code");

    // The actual token exchange is handled by the OAuth module
    // For now, return a success page
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><title>Hostless - Authorization</title></head>
<body style="font-family: system-ui; text-align: center; padding: 2em;">
    <h1>✓ Authorization Successful</h1>
    <p>The OAuth code has been received. You can close this window.</p>
    <p style="color: #666; font-size: 0.9em;">Code: {}...{}</p>
</body>
</html>"#,
        &code[..code.len().min(8)],
        &code[code.len().saturating_sub(4)..]
    );

    (
        StatusCode::OK,
        [("Content-Type", "text/html")],
        html,
    )
        .into_response()
}

// ─── Register Origin (handshake alternative) ─────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    origin: String,
    callback: Option<String>,
    state: Option<String>,
    /// Optional: restrict which providers this token can access
    allowed_providers: Option<Vec<String>>,
    /// Optional: restrict which models this token can access (glob patterns)
    allowed_models: Option<Vec<String>>,
    /// Optional: rate limit in requests per hour
    rate_limit: Option<u64>,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    port: u16,
    local_url: String,
    token: String,
    state: Option<String>,
    expires_in: u64,
}

pub async fn register_origin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Response {
    info!("Origin registration request from: {}", req.origin);

    // Build a description showing what access is being requested
    let mut scope_desc = String::new();
    if let Some(ref providers) = req.allowed_providers {
        scope_desc.push_str(&format!("\nProviders: {}", providers.join(", ")));
    } else {
        scope_desc.push_str("\nProviders: all configured");
    }
    if let Some(ref models) = req.allowed_models {
        scope_desc.push_str(&format!("\nModels: {}", models.join(", ")));
    } else {
        scope_desc.push_str("\nModels: all");
    }
    if let Some(rl) = req.rate_limit {
        scope_desc.push_str(&format!("\nRate limit: {} requests/hour", rl));
    }

    // Trusted desktop app requests can pre-authorize via admin token to avoid double prompts.
    let approved = if ensure_local_management_access(&state, &headers).is_ok() {
        info!(
            "Origin registration for '{}' approved via local admin-authenticated request",
            req.origin
        );
        true
    } else {
        let approval_result = tokio::task::spawn_blocking({
            let origin = req.origin.clone();
            move || {
                rfd::MessageDialog::new()
                    .set_title("Hostless - Access Request")
                    .set_description(&format!(
                        "Allow '{}' to use your AI API credits?\n{}\n\nThis will grant the app access to make LLM requests through your local proxy.",
                        origin,
                        scope_desc,
                    ))
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
            }
        })
        .await;

        match approval_result {
            Ok(result) => {
                info!(
                    "Native auth dialog result for origin '{}': {:?}",
                    req.origin, result
                );
                matches!(
                    result,
                    rfd::MessageDialogResult::Yes | rfd::MessageDialogResult::Ok
                )
            }
            Err(e) => {
                error!(
                    "Failed to join native auth dialog task for origin '{}': {}",
                    req.origin, e
                );
                false
            }
        }
    };

    if !approved {
        warn!(
            "Origin registration denied for '{}': approval was not granted",
            req.origin
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "User denied access"})),
        )
            .into_response();
    }

    // Add origin to allowlist
    {
        let mut config = state.config.write().await;
        if let Err(e) = config.add_origin(req.origin.clone()) {
            error!("Failed to save origin: {}", e);
        }
    }

    // Issue a bridge token with requested scoping
    let ttl = std::time::Duration::from_secs(3600); // 1 hour
    let token = state
        .token_manager
        .issue_full(
            &req.origin,
            ttl,
            req.allowed_models,
            req.allowed_providers,
            req.rate_limit,
            None,
        )
        .await;

    let response = RegisterResponse {
        port: state.port,
        local_url: format!("http://localhost:{}", state.port),
        token: token.token,
        state: req.state,
        expires_in: 3600,
    };

    // If a callback URL is provided, redirect to it
    if let Some(callback) = req.callback {
        let redirect_url = format!(
            "{}?port={}&local_url={}&state={}&expires_in=3600#token={}",
            callback,
            state.port,
            urlencoding::encode(&format!("http://localhost:{}", state.port)),
            urlencoding::encode(response.state.as_deref().unwrap_or("")),
            urlencoding::encode(&response.token),
        );
        return (
            StatusCode::SEE_OTHER,
            [("Location", redirect_url.as_str())],
            "",
        )
            .into_response();
    }

    Json(response).into_response()
}

// ─── Token Refresh ───────────────────────────────────────

#[derive(Deserialize)]
pub struct RefreshRequest {
    token: String,
}

pub async fn auth_refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RefreshRequest>,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, &headers) {
        return resp;
    }

    let new_ttl = std::time::Duration::from_secs(3600); // 1 hour

    match state.token_manager.refresh(&req.token, new_ttl).await {
        Ok(()) => {
            info!("Token refreshed");
            Json(json!({
                "status": "refreshed",
                "expires_in": 3600,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": format!("{}", e),
                    "type": "authentication_error",
                }
            })),
        )
            .into_response(),
    }
}

// ─── Token Revocation ────────────────────────────────────

#[derive(Deserialize)]
pub struct RevokeRequest {
    token: String,
}

pub async fn auth_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RevokeRequest>,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, &headers) {
        return resp;
    }

    match state.token_manager.revoke(&req.token).await {
        Ok(()) => {
            info!("Token revoked");
            Json(json!({ "status": "revoked" })).into_response()
        }
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": format!("{}", e),
                    "type": "authentication_error",
                }
            })),
        )
            .into_response(),
    }
}

// ─── List Active Tokens ──────────────────────────────────

pub async fn auth_list_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, &headers) {
        return resp;
    }

    let tokens = state.token_manager.list_tokens().await;
    Json(json!({ "tokens": tokens })).into_response()
}

// ─── Direct Token Creation (CLI only, no dialog) ────────

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    /// Origin to bind this token to. Use "*" for CLI tokens.
    origin: String,
    /// Human-readable app or client name
    name: Option<String>,
    /// Optional: restrict which providers this token can access
    allowed_providers: Option<Vec<String>>,
    /// Optional: restrict which models this token can access (glob patterns)
    allowed_models: Option<Vec<String>>,
    /// Optional: rate limit in requests per hour
    rate_limit: Option<u64>,
    /// Token time-to-live in seconds (default: 86400 = 24 hours)
    ttl: Option<u64>,
}

/// Create a bridge token directly, intended for CLI use.
/// Only accessible from localhost (no Origin header) — the caller is the
/// machine owner at the terminal, so no confirmation dialog is needed.
pub async fn create_token(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, req.headers()) {
        return resp;
    }

    // Security gate: only allow this from requests with no Origin header
    // (i.e., CLI tools like curl or `hostless token create` on the local machine).
    // Browser-based apps must use /auth/register which shows a consent dialog.
    let origin_header = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !origin_header.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": {
                    "message": "Direct token creation is only available from CLI (no Origin header). Browser apps must use /auth/register.",
                    "type": "forbidden",
                }
            })),
        )
            .into_response();
    }

    let host_header = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !is_bare_localhost_host(host_header) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": {
                    "message": "Direct token creation is restricted to localhost hostnames.",
                    "type": "forbidden",
                }
            })),
        )
            .into_response();
    }

    // Parse body
    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Failed to read body: {}", e) })),
            )
                .into_response();
        }
    };

    let create_req: CreateTokenRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid JSON: {}", e) })),
            )
                .into_response();
        }
    };

    let ttl_secs = create_req.ttl.unwrap_or(86400);
    let ttl = std::time::Duration::from_secs(ttl_secs);

    // Add origin to allowlist if it's not a wildcard
    if create_req.origin != "*" {
        let mut config = state.config.write().await;
        if let Err(e) = config.add_origin(create_req.origin.clone()) {
            error!("Failed to save origin: {}", e);
        }
    }

    let token = state
        .token_manager
        .issue_full(
            &create_req.origin,
            ttl,
            create_req.allowed_models.clone(),
            create_req.allowed_providers.clone(),
            create_req.rate_limit,
            create_req.name.clone(),
        )
        .await;

    info!(
        origin = create_req.origin.as_str(),
        name = create_req.name.as_deref().unwrap_or("(none)"),
        "CLI token created (TTL: {}s)",
        ttl_secs
    );

    Json(json!({
        "token": token.token,
        "origin": create_req.origin,
        "name": create_req.name,
        "allowed_providers": create_req.allowed_providers,
        "allowed_models": create_req.allowed_models,
        "rate_limit": create_req.rate_limit,
        "expires_in": ttl_secs,
    }))
    .into_response()
}

// ─── Route Management ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterRouteRequest {
    /// App name (becomes <name>.localhost)
    pub name: String,
    /// Target port on 127.0.0.1
    pub port: u16,
    /// PID of the app process (if managed)
    pub pid: Option<u32>,
    /// Auto-provision a bridge token for this app
    #[serde(default = "default_true")]
    pub auto_token: bool,
    /// Provider scope for auto-provisioned token
    pub allowed_providers: Option<Vec<String>>,
    /// Model scope for auto-provisioned token
    pub allowed_models: Option<Vec<String>>,
    /// Rate limit for auto-provisioned token
    pub rate_limit: Option<u64>,
    /// TTL in seconds for auto-provisioned token (default: 86400)
    pub ttl: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct DeregisterRouteRequest {
    /// App name or full hostname (e.g., "myapp" or "myapp.localhost")
    pub name: String,
}

/// POST /routes/register — Register a route mapping a .localhost subdomain to a port.
/// Bare-localhost only (rejects requests with non-localhost Origin).
pub async fn register_route(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, req.headers()) {
        return resp;
    }

    // Parse body
    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Failed to read body: {}", e) })),
            )
                .into_response();
        }
    };

    let register_req: RegisterRouteRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid JSON: {}", e) })),
            )
                .into_response();
        }
    };

    // Register the route
    let route = match state
        .route_table
        .register(&register_req.name, register_req.port, register_req.pid)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("{}", e) })),
            )
                .into_response();
        }
    };

    let hostname = route.hostname.clone();
    let app_url = format!("http://{}:{}", hostname, state.port);

    // Auto-provision a bridge token scoped to this app's origin
    let token_info = if register_req.auto_token {
        let token_origin = app_url.clone();
        let ttl = std::time::Duration::from_secs(register_req.ttl.unwrap_or(86400));

        let token = state
            .token_manager
            .issue_full(
                &token_origin,
                ttl,
                register_req.allowed_models.clone(),
                register_req.allowed_providers.clone(),
                register_req.rate_limit,
                Some(register_req.name.clone()),
            )
            .await;

        // Store token reference in route table
        state.route_table.set_token(&hostname, token.token.clone()).await;

        info!(
            app = register_req.name.as_str(),
            hostname = hostname.as_str(),
            "Auto-provisioned bridge token for app"
        );

        Some(json!({
            "token": token.token,
            "origin": token_origin,
            "expires_in": register_req.ttl.unwrap_or(86400),
        }))
    } else {
        None
    };

    info!(
        app = register_req.name.as_str(),
        hostname = hostname.as_str(),
        target_port = register_req.port,
        "Route registered"
    );

    Json(json!({
        "hostname": hostname,
        "url": app_url,
        "target_port": register_req.port,
        "pid": register_req.pid,
        "token": token_info,
    }))
    .into_response()
}

/// POST /routes/deregister — Remove a route and revoke its associated token.
pub async fn deregister_route(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, req.headers()) {
        return resp;
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Failed to read body: {}", e) })),
            )
                .into_response();
        }
    };

    let deregister_req: DeregisterRouteRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid JSON: {}", e) })),
            )
                .into_response();
        }
    };

    let removed = state.route_table.remove(&deregister_req.name).await;

    match removed {
        Some(route) => {
            // Revoke associated token if any
            if let Some(token) = &route.token {
                if let Err(error) = state.token_manager.revoke(token).await {
                    warn!(
                        app = route.app_name.as_str(),
                        error = %error,
                        "Failed to revoke associated bridge token"
                    );
                } else {
                    info!(app = route.app_name.as_str(), "Revoked associated bridge token");
                }
            }

            Json(json!({
                "removed": true,
                "hostname": route.hostname,
                "app_name": route.app_name,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("No route found for '{}'", deregister_req.name) })),
        )
            .into_response(),
    }
}

/// GET /routes — List all active routes.
pub async fn list_routes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = ensure_local_management_access(&state, &headers) {
        return resp;
    }

    let routes = state.route_table.list().await;
    Json(json!({ "routes": routes })).into_response()
}
