use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Bridge token persistence policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TokenPersistenceMode {
    /// Keep bridge tokens in-memory only (default).
    #[default]
    Off,
    /// Persist bridge tokens as plaintext JSON on disk.
    File,
    /// Persist bridge tokens encrypted with a key stored in OS keychain.
    Keychain,
}

impl TokenPersistenceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::File => "file",
            Self::Keychain => "keychain",
        }
    }
}

/// Application configuration persisted to ~/.hostless/config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Origins allowed to make requests to the proxy
    pub allowed_origins: Vec<String>,
    /// OAuth client configurations per provider
    #[serde(default)]
    pub oauth_clients: HashMap<String, OAuthClientConfig>,
    /// Provider-specific base URL overrides
    #[serde(default)]
    pub provider_urls: HashMap<String, String>,
    /// Bridge token persistence policy (off/file/keychain).
    #[serde(default)]
    pub token_persistence: TokenPersistenceMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            oauth_clients: HashMap::new(),
            provider_urls: HashMap::new(),
            token_persistence: TokenPersistenceMode::Off,
        }
    }
}

impl AppConfig {
    /// Get the config directory path (~/.hostless/)
    pub fn config_dir() -> Result<PathBuf> {
        if let Ok(override_dir) = std::env::var("HOSTLESS_CONFIG_DIR") {
            let dir = PathBuf::from(override_dir);
            if !dir.exists() {
                std::fs::create_dir_all(&dir)
                    .context("Failed to create HOSTLESS_CONFIG_DIR directory")?;
            }
            return Ok(dir);
        }

        let home = dirs::home_dir().context("Cannot determine home directory")?;
        let dir = home.join(".hostless");
        if !dir.exists() {
            std::fs::create_dir_all(&dir).context("Failed to create ~/.hostless directory")?;
        }
        Ok(dir)
    }

    /// Get the config file path
    fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.json"))
    }

    /// Load config from disk, or return defaults if not found
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            let data = std::fs::read_to_string(&path).context("Failed to read config file")?;
            let config: Self =
                serde_json::from_str(&data).context("Failed to parse config file")?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to disk
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let data = serde_json::to_string_pretty(self).context("Failed to serialize config")?;
        std::fs::write(&path, data).context("Failed to write config file")?;
        Ok(())
    }

    /// Check if an origin is in the allowlist
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == origin)
    }

    /// Add an origin to the allowlist and save
    pub fn add_origin(&mut self, origin: String) -> Result<()> {
        if !self.allowed_origins.contains(&origin) {
            self.allowed_origins.push(origin);
            self.save()?;
        }
        Ok(())
    }
}
