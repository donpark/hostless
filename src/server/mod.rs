pub mod cors;
pub mod dispatch;
pub mod pages;
pub mod reverse_proxy;
pub mod route_table;
pub mod routes;
pub mod streaming;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    middleware as axum_middleware,
    routing::{get, post},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::config::{AppConfig, TokenPersistenceMode};
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
    /// When true, allow wildcard subdomain routing: tenant.app.localhost -> app.localhost.
    pub enable_wildcard_routes: bool,
}

impl AppState {
    fn env_flag_true(name: &str) -> bool {
        std::env::var(name)
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false)
    }

    pub async fn new(
        port: u16,
        dev_mode: bool,
        token_persistence_override: Option<TokenPersistenceMode>,
    ) -> Result<Arc<Self>> {
        let vault = VaultStore::open().await?;
        let config = AppConfig::load()?;
        let token_persistence = token_persistence_override.unwrap_or(config.token_persistence);
        if token_persistence == TokenPersistenceMode::File {
            tracing::warn!(
                "Bridge token persistence is set to plaintext file mode; use keychain mode for stronger at-rest protection"
            );
        }
        let token_manager = auth::bridge_token::BridgeTokenManager::new_with_persistence(token_persistence);
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
            enable_wildcard_routes: Self::env_flag_true("HOSTLESS_ENABLE_WILDCARD_ROUTES"),
        });

        // Load persisted routes from disk (stale PID routes are filtered out)
        if let Err(e) = state.route_table.load_from_disk().await {
            tracing::warn!("Failed to load routes from disk: {}", e);
        }

        match state.token_manager.load_from_disk().await {
            Ok(loaded) if loaded > 0 => {
                tracing::info!(
                    mode = state.token_manager.persistence_mode().as_str(),
                    "Loaded {} bridge tokens from disk",
                    loaded
                );
            }
            Ok(_) => {}
            Err(e) if state.token_manager.persistence_mode() == TokenPersistenceMode::Keychain => {
                return Err(e).context("Failed to load bridge tokens in keychain persistence mode");
            }
            Err(e) => {
                tracing::warn!("Failed to load bridge tokens from disk: {}", e);
            }
        }

        Ok(state)
    }

    /// Create an AppState backed by an ephemeral in-memory vault.
    /// No OS keychain access, no config files on disk.
    /// Use this in tests to avoid keychain prompts.
    #[allow(dead_code)]
    pub fn new_ephemeral(port: u16, dev_mode: bool) -> Arc<Self> {
        Self::new_ephemeral_with_options(port, dev_mode, false)
    }

    #[allow(dead_code)]
    pub fn new_ephemeral_with_options(
        port: u16,
        dev_mode: bool,
        enable_wildcard_routes: bool,
    ) -> Arc<Self> {
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
            enable_wildcard_routes,
        })
    }
}

/// Build the Axum router with all routes and middleware
pub fn create_router(state: Arc<AppState>) -> Router {
    let cors_layer = cors::build_cors_layer(state.clone());

    let api_routes = Router::new()
        .route("/v1/chat/completions", post(routes::chat_completions))
        .route(
            "/v1/responses",
            post(routes::responses).get(routes::responses_websocket),
        )
        .route("/v1/realtime", get(routes::realtime))
        .route("/v1/audio/speech", post(routes::audio_speech))
        .route("/v1/audio/transcriptions", post(routes::audio_transcriptions))
        .route("/v1/audio/translations", post(routes::audio_translations))
        .route("/v1/images/generations", post(routes::images_generations))
        .route("/v1/files", post(routes::files_upload))
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
                    if let Err(error) = cleanup_state.token_manager.revoke(token).await {
                        tracing::warn!(error = %error, "Failed to revoke stale-route token");
                    }
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
