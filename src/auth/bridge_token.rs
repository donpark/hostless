use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::TokenPersistenceMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedToken {
    token: String,
    origin: String,
    app_name: Option<String>,
    expires_at_unix: u64,
    allowed_models: Option<Vec<String>>,
    allowed_providers: Option<Vec<String>>,
    rate_limit_per_hour: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTokenFile {
    version: u8,
    tokens: Vec<PersistedToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedPersistedTokenFile {
    version: u8,
    encrypted: bool,
    data: String,
}

/// Information about an issued bridge token
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BridgeToken {
    /// The token string (e.g., "sk_local_...")
    pub token: String,
    /// The origin this token was issued for
    pub origin: String,
    /// When this token was created
    pub created_at: Instant,
    /// When this token expires
    pub expires_at: Instant,
    /// Optional: allowed model patterns (glob-style)
    pub allowed_models: Option<Vec<String>>,
    /// Optional: allowed provider keys (e.g., ["openai", "anthropic"])
    pub allowed_providers: Option<Vec<String>>,
    /// Optional: rate limit (requests per hour)
    pub rate_limit: Option<RateLimit>,
    /// Optional: human-readable app name for CLI-created tokens
    pub app_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RateLimit {
    pub max_requests: u64,
    pub window: Duration,
    pub current_count: u64,
    pub window_start: Instant,
}

impl RateLimit {
    pub fn new(max_requests: u64, window: Duration) -> Self {
        Self {
            max_requests,
            window,
            current_count: 0,
            window_start: Instant::now(),
        }
    }

    /// Check if a request is allowed and increment the counter.
    /// Returns (allowed, remaining, retry_after_secs)
    pub fn check_and_increment(&mut self) -> (bool, u64, Option<u64>) {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= self.window {
            // Reset window
            self.window_start = now;
            self.current_count = 0;
        }

        if self.current_count >= self.max_requests {
            let retry_after = self
                .window
                .checked_sub(now.duration_since(self.window_start))
                .map(|d| d.as_secs())
                .unwrap_or(0);
            return (false, 0, Some(retry_after));
        }

        self.current_count += 1;
        let remaining = self.max_requests - self.current_count;
        (true, remaining, None)
    }
}

/// Manages bridge tokens in memory
#[allow(dead_code)]
pub struct BridgeTokenManager {
    tokens: RwLock<HashMap<String, BridgeToken>>,
    persistence_mode: TokenPersistenceMode,
}

#[allow(dead_code)]
impl BridgeTokenManager {
    pub fn new() -> Self {
        Self::new_with_persistence(TokenPersistenceMode::Off)
    }

    pub fn new_with_persistence(persistence_mode: TokenPersistenceMode) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            persistence_mode,
        }
    }

    pub fn persistence_mode(&self) -> TokenPersistenceMode {
        self.persistence_mode
    }

    fn persistence_path(&self) -> Result<PathBuf> {
        let dir = crate::config::AppConfig::config_dir()?;
        Ok(dir.join("tokens.json"))
    }

    fn expires_at_to_unix(expires_at: Instant) -> u64 {
        let now_instant = Instant::now();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let remaining = expires_at
            .checked_duration_since(now_instant)
            .unwrap_or_default()
            .as_secs();
        now_unix.saturating_add(remaining)
    }

    fn unix_to_expires_at(expires_at_unix: u64) -> Option<Instant> {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if expires_at_unix <= now_unix {
            return None;
        }
        let remaining = expires_at_unix.saturating_sub(now_unix);
        Some(Instant::now() + Duration::from_secs(remaining))
    }

    fn to_persisted_file(tokens: &HashMap<String, BridgeToken>) -> PersistedTokenFile {
        let mut items = Vec::with_capacity(tokens.len());
        for token in tokens.values() {
            items.push(PersistedToken {
                token: token.token.clone(),
                origin: token.origin.clone(),
                app_name: token.app_name.clone(),
                expires_at_unix: Self::expires_at_to_unix(token.expires_at),
                allowed_models: token.allowed_models.clone(),
                allowed_providers: token.allowed_providers.clone(),
                rate_limit_per_hour: token.rate_limit.as_ref().map(|r| r.max_requests),
            });
        }
        PersistedTokenFile {
            version: 1,
            tokens: items,
        }
    }

    fn write_persisted_file(path: &PathBuf, payload: &str) -> Result<()> {
        use fs2::FileExt;

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .context("Failed to open tokens file")?;
        file.lock_exclusive()
            .context("Failed to lock tokens file")?;
        std::fs::write(path, payload).context("Failed to write tokens file")?;
        file.unlock().context("Failed to unlock tokens file")?;
        Ok(())
    }

    fn persist_tokens_snapshot(&self, tokens: &HashMap<String, BridgeToken>) -> Result<()> {
        match self.persistence_mode {
            TokenPersistenceMode::Off => Ok(()),
            TokenPersistenceMode::File => {
                let path = self.persistence_path()?;
                let payload = serde_json::to_string_pretty(&Self::to_persisted_file(tokens))
                    .context("Failed to serialize tokens")?;
                Self::write_persisted_file(&path, &payload)
            }
            TokenPersistenceMode::Keychain => {
                let path = self.persistence_path()?;
                let plaintext = serde_json::to_vec(&Self::to_persisted_file(tokens))
                    .context("Failed to serialize tokens")?;
                let key = crate::vault::keychain::load_or_create_master_key()
                    .context("Failed to load keychain master key")?;
                let encrypted = crate::vault::encryption::encrypt(&key, &plaintext)
                    .context("Failed to encrypt token store")?;
                let wrapped = EncryptedPersistedTokenFile {
                    version: 1,
                    encrypted: true,
                    data: encrypted,
                };
                let payload = serde_json::to_string_pretty(&wrapped)
                    .context("Failed to serialize encrypted token store")?;
                Self::write_persisted_file(&path, &payload)
            }
        }
    }

    async fn persist_if_enabled(&self) {
        if self.persistence_mode == TokenPersistenceMode::Off {
            return;
        }
        let tokens = self.tokens.read().await;
        if let Err(e) = self.persist_tokens_snapshot(&tokens) {
            warn!(error = %e, "Failed to persist bridge tokens");
        }
    }

    /// Load persisted tokens into memory. Expired tokens are ignored.
    pub async fn load_from_disk(&self) -> Result<usize> {
        if self.persistence_mode == TokenPersistenceMode::Off {
            return Ok(0);
        }

        let path = self.persistence_path()?;
        if !path.exists() {
            return Ok(0);
        }

        let raw = std::fs::read_to_string(&path).context("Failed to read tokens file")?;
        let parsed = match self.persistence_mode {
            TokenPersistenceMode::File => serde_json::from_str::<PersistedTokenFile>(&raw)
                .context("Failed to parse tokens file")?,
            TokenPersistenceMode::Keychain => {
                let wrapped = serde_json::from_str::<EncryptedPersistedTokenFile>(&raw)
                    .context("Failed to parse encrypted token store")?;
                if !wrapped.encrypted {
                    anyhow::bail!("Encrypted token store expected but plaintext data was found");
                }
                let key = crate::vault::keychain::load_or_create_master_key()
                    .context("Failed to load keychain master key")?;
                let decrypted = crate::vault::encryption::decrypt(&key, &wrapped.data)
                    .context("Failed to decrypt token store")?;
                serde_json::from_slice::<PersistedTokenFile>(&decrypted)
                    .context("Failed to parse decrypted token store")?
            }
            TokenPersistenceMode::Off => unreachable!("handled above"),
        };

        let mut loaded = 0usize;
        let mut tokens = self.tokens.write().await;
        tokens.clear();

        for persisted in parsed.tokens {
            let Some(expires_at) = Self::unix_to_expires_at(persisted.expires_at_unix) else {
                continue;
            };
            let now = Instant::now();
            let token = BridgeToken {
                token: persisted.token.clone(),
                origin: persisted.origin,
                created_at: now,
                expires_at,
                allowed_models: persisted.allowed_models,
                allowed_providers: persisted.allowed_providers,
                rate_limit: persisted
                    .rate_limit_per_hour
                    .map(|max| RateLimit::new(max, Duration::from_secs(3600))),
                app_name: persisted.app_name,
            };
            tokens.insert(persisted.token, token);
            loaded += 1;
        }

        Ok(loaded)
    }

    /// Issue a new bridge token for an origin.
    pub async fn issue(
        &self,
        origin: &str,
        ttl: Duration,
        allowed_models: Option<Vec<String>>,
        rate_limit_per_hour: Option<u64>,
    ) -> BridgeToken {
        self.issue_full(origin, ttl, allowed_models, None, rate_limit_per_hour, None)
            .await
    }

    /// Issue a bridge token with full options including provider scope and app name.
    pub async fn issue_full(
        &self,
        origin: &str,
        ttl: Duration,
        allowed_models: Option<Vec<String>>,
        allowed_providers: Option<Vec<String>>,
        rate_limit_per_hour: Option<u64>,
        app_name: Option<String>,
    ) -> BridgeToken {
        let token_string = generate_token();
        let now = Instant::now();

        let rate_limit = rate_limit_per_hour.map(|max| {
            RateLimit::new(max, Duration::from_secs(3600))
        });

        let bridge_token = BridgeToken {
            token: token_string.clone(),
            origin: origin.to_string(),
            created_at: now,
            expires_at: now + ttl,
            allowed_models,
            allowed_providers,
            rate_limit,
            app_name,
        };

        let mut tokens = self.tokens.write().await;
        tokens.insert(token_string.clone(), bridge_token.clone());
        drop(tokens);

        self.persist_if_enabled().await;

        info!(origin = origin, "Issued bridge token (expires in {}s)", ttl.as_secs());
        bridge_token
    }

    /// Validate a bridge token against an origin.
    /// Returns Ok(()) if valid, Err with a reason if not.
    pub async fn validate(&self, token: &str, origin: &str) -> Result<(), TokenError> {
        let tokens = self.tokens.read().await;

        let bridge_token = tokens.get(token).ok_or(TokenError::NotFound)?;

        // Check expiry
        if Instant::now() >= bridge_token.expires_at {
            return Err(TokenError::Expired);
        }

        // Check origin (wildcard "*" matches any origin, including empty)
        if bridge_token.origin != "*" && bridge_token.origin != origin {
            warn!(
                expected = bridge_token.origin.as_str(),
                got = origin,
                "Origin mismatch for bridge token"
            );
            return Err(TokenError::OriginMismatch);
        }

        Ok(())
    }

    /// Validate a token and check if the requested model is allowed.
    pub async fn validate_with_model(
        &self,
        token: &str,
        origin: &str,
        model: &str,
    ) -> Result<(), TokenError> {
        self.validate(token, origin).await?;

        let tokens = self.tokens.read().await;
        if let Some(bridge_token) = tokens.get(token) {
            // Check model scope
            if let Some(ref allowed) = bridge_token.allowed_models {
                let model_allowed = allowed.iter().any(|pattern| {
                    if pattern.ends_with('*') {
                        let prefix = &pattern[..pattern.len() - 1];
                        model.starts_with(prefix)
                    } else {
                        model == pattern
                    }
                });

                if !model_allowed {
                    return Err(TokenError::ModelNotAllowed(model.to_string()));
                }
            }
        }

        Ok(())
    }

    /// Check if the resolved provider is allowed by this token.
    pub async fn validate_provider(
        &self,
        token: &str,
        provider: &str,
    ) -> Result<(), TokenError> {
        let tokens = self.tokens.read().await;
        if let Some(bridge_token) = tokens.get(token) {
            if let Some(ref allowed) = bridge_token.allowed_providers {
                if !allowed.iter().any(|p| p == provider) {
                    return Err(TokenError::ProviderNotAllowed(provider.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Check if the requested model is allowed by this token (without re-validating origin/expiry).
    pub async fn validate_model(
        &self,
        token: &str,
        model: &str,
    ) -> Result<(), TokenError> {
        let tokens = self.tokens.read().await;
        if let Some(bridge_token) = tokens.get(token) {
            if let Some(ref allowed) = bridge_token.allowed_models {
                let model_allowed = allowed.iter().any(|pattern| {
                    if pattern.ends_with('*') {
                        let prefix = &pattern[..pattern.len() - 1];
                        model.starts_with(prefix)
                    } else {
                        model == pattern
                    }
                });
                if !model_allowed {
                    return Err(TokenError::ModelNotAllowed(model.to_string()));
                }
            }
        }
        Ok(())
    }

    /// List all active (non-expired) tokens with summary info.
    pub async fn list_tokens(&self) -> Vec<TokenInfo> {
        let tokens = self.tokens.read().await;
        let now = Instant::now();
        tokens
            .values()
            .filter(|t| t.expires_at > now)
            .map(|t| TokenInfo {
                token_prefix: format!("{}...", &t.token[..t.token.len().min(20)]),
                origin: t.origin.clone(),
                app_name: t.app_name.clone(),
                expires_in_secs: t.expires_at.duration_since(now).as_secs(),
                allowed_models: t.allowed_models.clone(),
                allowed_providers: t.allowed_providers.clone(),
            })
            .collect()
    }

    /// Check and increment rate limit for a token.
    /// Returns Ok(remaining) or Err with retry-after seconds.
    pub async fn check_rate_limit(&self, token: &str) -> Result<u64, (u64, u64)> {
        let mut tokens = self.tokens.write().await;

        if let Some(bridge_token) = tokens.get_mut(token) {
            if let Some(ref mut rate_limit) = bridge_token.rate_limit {
                let (allowed, remaining, retry_after) = rate_limit.check_and_increment();
                if !allowed {
                    return Err((0, retry_after.unwrap_or(60)));
                }
                return Ok(remaining);
            }
        }

        // No rate limit configured
        Ok(u64::MAX)
    }

    /// Refresh a token's expiry
    pub async fn refresh(&self, token: &str, new_ttl: Duration) -> Result<(), TokenError> {
        let mut tokens = self.tokens.write().await;
        let bridge_token = tokens.get_mut(token).ok_or(TokenError::NotFound)?;

        bridge_token.expires_at = Instant::now() + new_ttl;
        drop(tokens);

        self.persist_if_enabled().await;
        info!("Refreshed bridge token (new TTL: {}s)", new_ttl.as_secs());
        Ok(())
    }

    /// Revoke a token
    pub async fn revoke(&self, token: &str) {
        let mut tokens = self.tokens.write().await;
        let removed = tokens.remove(token);
        drop(tokens);

        if removed.is_some() {
            self.persist_if_enabled().await;
        }
        info!("Revoked bridge token");
    }

    /// Clean up expired tokens (call periodically)
    pub async fn cleanup_expired(&self) -> usize {
        let mut tokens = self.tokens.write().await;
        let before = tokens.len();
        let now = Instant::now();
        tokens.retain(|_, t| t.expires_at > now);
        let removed = before - tokens.len();
        drop(tokens);

        if removed > 0 {
            self.persist_if_enabled().await;
        }

        removed
    }
}

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum TokenError {
    #[error("Token not found")]
    NotFound,
    #[error("Token has expired")]
    Expired,
    #[error("Origin mismatch")]
    OriginMismatch,
    #[error("Model '{0}' not allowed by this token's scope")]
    ModelNotAllowed(String),
    #[error("Provider '{0}' not allowed by this token's scope")]
    ProviderNotAllowed(String),
}

/// Summary info for listing tokens (no secrets exposed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub token_prefix: String,
    pub origin: String,
    pub app_name: Option<String>,
    pub expires_in_secs: u64,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_providers: Option<Vec<String>>,
}

/// Generate a high-entropy token string
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        &bytes,
    );
    format!("sk_local_{}", encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_issue_and_validate() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue("https://myapp.com", Duration::from_secs(3600), None, None)
            .await;

        assert!(manager
            .validate(&token.token, "https://myapp.com")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_origin_mismatch() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue("https://myapp.com", Duration::from_secs(3600), None, None)
            .await;

        assert!(matches!(
            manager.validate(&token.token, "https://evil.com").await,
            Err(TokenError::OriginMismatch)
        ));
    }

    #[tokio::test]
    async fn test_expired_token() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue("https://myapp.com", Duration::from_millis(1), None, None)
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(matches!(
            manager.validate(&token.token, "https://myapp.com").await,
            Err(TokenError::Expired)
        ));
    }

    #[tokio::test]
    async fn test_model_scope() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue(
                "https://myapp.com",
                Duration::from_secs(3600),
                Some(vec!["gpt-4o-mini".to_string(), "claude-3-haiku*".to_string()]),
                None,
            )
            .await;

        assert!(manager
            .validate_with_model(&token.token, "https://myapp.com", "gpt-4o-mini")
            .await
            .is_ok());

        assert!(manager
            .validate_with_model(&token.token, "https://myapp.com", "claude-3-haiku-20240307")
            .await
            .is_ok());

        assert!(matches!(
            manager
                .validate_with_model(&token.token, "https://myapp.com", "gpt-4o")
                .await,
            Err(TokenError::ModelNotAllowed(_))
        ));
    }

    #[tokio::test]
    async fn test_rate_limiting() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue(
                "https://myapp.com",
                Duration::from_secs(3600),
                None,
                Some(2), // 2 requests per hour
            )
            .await;

        assert!(manager.check_rate_limit(&token.token).await.is_ok());
        assert!(manager.check_rate_limit(&token.token).await.is_ok());
        assert!(manager.check_rate_limit(&token.token).await.is_err());
    }

    #[tokio::test]
    async fn test_provider_scope() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue_full(
                "https://myapp.com",
                Duration::from_secs(3600),
                None,
                Some(vec!["openai".to_string(), "anthropic".to_string()]),
                None,
                None,
            )
            .await;

        // Allowed providers
        assert!(manager
            .validate_provider(&token.token, "openai")
            .await
            .is_ok());
        assert!(manager
            .validate_provider(&token.token, "anthropic")
            .await
            .is_ok());

        // Disallowed provider
        assert!(matches!(
            manager.validate_provider(&token.token, "google").await,
            Err(TokenError::ProviderNotAllowed(_))
        ));
    }

    #[tokio::test]
    async fn test_no_provider_scope_allows_all() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue("https://myapp.com", Duration::from_secs(3600), None, None)
            .await;

        // No provider scope → all providers allowed
        assert!(manager.validate_provider(&token.token, "openai").await.is_ok());
        assert!(manager.validate_provider(&token.token, "anthropic").await.is_ok());
        assert!(manager.validate_provider(&token.token, "google").await.is_ok());
    }

    #[tokio::test]
    async fn test_wildcard_origin() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue("*", Duration::from_secs(3600), None, None)
            .await;

        // Wildcard origin should match any origin
        assert!(manager.validate(&token.token, "https://anything.com").await.is_ok());
        assert!(manager.validate(&token.token, "http://localhost:3000").await.is_ok());
        assert!(manager.validate(&token.token, "").await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_model_standalone() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue_full(
                "https://myapp.com",
                Duration::from_secs(3600),
                Some(vec!["gpt-4o*".to_string()]),
                None,
                None,
                None,
            )
            .await;

        assert!(manager.validate_model(&token.token, "gpt-4o").await.is_ok());
        assert!(manager.validate_model(&token.token, "gpt-4o-mini").await.is_ok());
        assert!(matches!(
            manager.validate_model(&token.token, "claude-3-opus").await,
            Err(TokenError::ModelNotAllowed(_))
        ));
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let manager = BridgeTokenManager::new();
        let _t1 = manager
            .issue("https://a.com", Duration::from_millis(1), None, None)
            .await;
        let _t2 = manager
            .issue("https://b.com", Duration::from_secs(3600), None, None)
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        let removed = manager.cleanup_expired().await;
        assert_eq!(removed, 1);

        let remaining = manager.list_tokens().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].origin, "https://b.com");
    }

    #[tokio::test]
    async fn test_list_tokens() {
        let manager = BridgeTokenManager::new();
        let _t1 = manager
            .issue_full(
                "http://myapp.localhost:1355",
                Duration::from_secs(3600),
                None,
                Some(vec!["openai".to_string()]),
                None,
                Some("myapp".to_string()),
            )
            .await;

        let tokens = manager.list_tokens().await;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].origin, "http://myapp.localhost:1355");
        assert_eq!(tokens[0].app_name.as_deref(), Some("myapp"));
        assert_eq!(
            tokens[0].allowed_providers.as_ref().unwrap(),
            &vec!["openai".to_string()]
        );
    }

    #[tokio::test]
    async fn test_revoke() {
        let manager = BridgeTokenManager::new();
        let token = manager
            .issue("https://myapp.com", Duration::from_secs(3600), None, None)
            .await;

        assert!(manager.validate(&token.token, "https://myapp.com").await.is_ok());

        manager.revoke(&token.token).await;

        assert!(matches!(
            manager.validate(&token.token, "https://myapp.com").await,
            Err(TokenError::NotFound)
        ));
    }
}
