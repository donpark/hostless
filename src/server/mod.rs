pub mod cors;
pub mod dispatch;
pub mod reverse_proxy;
pub mod route_table;
pub mod routes;
pub mod streaming;

use std::sync::Arc;

use anyhow::Result;
use axum::{
    middleware as axum_middleware,
    routing::{get, post},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::config::AppConfig;
use crate::vault::VaultStore;

/// Shared application state
pub struct AppState {
    pub vault: VaultStore,
    pub config: tokio::sync::RwLock<AppConfig>,
    pub token_manager: auth::bridge_token::BridgeTokenManager,
    pub route_table: route_table::RouteTable,
    pub http_client: reqwest::Client,
    pub port: u16,
    pub admin_token: String,
    /// When true, bare localhost/127.0.0.1 and empty-origin requests bypass auth.
    /// When false (default), all requests must present a valid bridge token.
    pub dev_mode: bool,
}

impl AppState {
    pub async fn new(port: u16, dev_mode: bool) -> Result<Arc<Self>> {
        let vault = VaultStore::open().await?;
        let config = AppConfig::load()?;
        let token_manager = auth::bridge_token::BridgeTokenManager::new();
        let route_table = route_table::RouteTable::new(port);
        let admin_token = auth::admin::load_or_create_admin_token()?;

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 min for long generations
            .build()?;

        let state = Arc::new(Self {
            vault,
            config: tokio::sync::RwLock::new(config),
            token_manager,
            route_table,
            http_client,
            port,
            admin_token,
            dev_mode,
        });

        // Load persisted routes from disk (stale PID routes are filtered out)
        if let Err(e) = state.route_table.load_from_disk().await {
            tracing::warn!("Failed to load routes from disk: {}", e);
        }

        Ok(state)
    }

    /// Create an AppState backed by an ephemeral in-memory vault.
    /// No OS keychain access, no config files on disk.
    /// Use this in tests to avoid keychain prompts.
    #[allow(dead_code)]
    pub fn new_ephemeral(port: u16, dev_mode: bool) -> Arc<Self> {
        let vault = VaultStore::open_ephemeral();
        let config = AppConfig::default();
        let token_manager = auth::bridge_token::BridgeTokenManager::new();
        let route_table = route_table::RouteTable::new(port);

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build HTTP client");

        Arc::new(Self {
            vault,
            config: tokio::sync::RwLock::new(config),
            token_manager,
            route_table,
            http_client,
            port,
            admin_token: "test-admin-token".to_string(),
            dev_mode,
        })
    }
}

/// Build the Axum router with all routes and middleware
pub fn create_router(state: Arc<AppState>) -> Router {
    let cors_layer = cors::build_cors_layer(state.clone());

    let api_routes = Router::new()
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route("/v1/embeddings", post(routes::embeddings))
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth::middleware::auth_middleware,
        ));

    let public_routes = Router::new()
        .route("/health", get(routes::health))
        .route("/callback", get(routes::oauth_callback))
        .route("/auth/register", post(routes::register_origin))
        .route("/auth/token", post(routes::create_token))
        .route("/auth/refresh", post(routes::auth_refresh))
        .route("/auth/revoke", post(routes::auth_revoke))
        .route("/auth/tokens", get(routes::auth_list_tokens))
        // Route management endpoints (localhost-only, guarded in handler)
        .route("/routes", get(routes::list_routes))
        .route("/routes/register", post(routes::register_route))
        .route("/routes/deregister", post(routes::deregister_route));

    // Spawn background task to periodically clean up expired tokens and stale routes
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let removed = cleanup_state.token_manager.cleanup_expired().await;
            if removed > 0 {
                tracing::info!("Cleaned up {} expired bridge tokens", removed);
            }
            let (stale_routes, revoked_tokens) = cleanup_state.route_table.cleanup_stale().await;
            if stale_routes > 0 {
                tracing::info!("Cleaned up {} stale routes", stale_routes);
                // Revoke tokens associated with stale routes
                for token in &revoked_tokens {
                    cleanup_state.token_manager.revoke(token).await;
                }
            }
        }
    });

    // Host-header dispatch layer wraps the entire router.
    // Requests to <name>.localhost are dispatched to the reverse proxy
    // and can NEVER reach management/LLM proxy endpoints.
    Router::new()
        .merge(api_routes)
        .merge(public_routes)
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            dispatch::host_dispatch,
        ))
        .layer(cors_layer)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
