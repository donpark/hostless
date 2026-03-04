use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::RwLock;
use tracing::warn;

/// Information about a stored provider (without the actual key)
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub name: String,
    pub base_url: Option<String>,
}

/// A single plaintext key entry
#[derive(Debug, Clone)]
struct VaultEntry {
    api_key: String,
    base_url: Option<String>,
}

/// In-memory vault data keyed by provider name.
#[derive(Debug, Default)]
struct VaultData {
    entries: HashMap<String, VaultEntry>,
}

#[derive(Debug, Deserialize)]
struct LegacyVaultEntry {
    provider: String,
    encrypted_key: String,
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyVaultFile {
    entries: Vec<LegacyVaultEntry>,
}

/// Manages encrypted API key storage
#[allow(dead_code)]
pub struct VaultStore {
    #[allow(dead_code)]
    vault_path: PathBuf,
    data: RwLock<VaultData>,
    /// When true, skip disk writes (in-memory only, no keychain).
    ephemeral: bool,
}

impl VaultStore {
    /// Open the vault, loading the master key and any existing entries
    pub async fn open() -> Result<Self> {
        let config_dir = crate::config::AppConfig::config_dir()?;
        let vault_path = config_dir.join("keys.env");

        let data = if vault_path.exists() {
            let raw =
                tokio::fs::read_to_string(&vault_path)
                    .await
                    .context("Failed to read vault file")?;
            parse_env_vault(&raw)
        } else {
            VaultData::default()
        };

        Ok(Self {
            vault_path,
            data: RwLock::new(data),
            ephemeral: false,
        })
    }

    /// Add or update an API key for a provider
    pub async fn add_key(
        &self,
        provider: &str,
        api_key: &str,
        base_url: Option<&str>,
    ) -> Result<()> {
        self.refresh_from_disk().await?;

        let entry = VaultEntry {
            api_key: api_key.to_string(),
            base_url: base_url.map(|s| s.to_string()),
        };

        let provider_lower = provider.to_lowercase();
        let mut data = self.data.write().await;
        data.entries.insert(provider_lower, entry);

        self.save(&data).await
    }

    /// Get the decrypted API key for a provider
    pub async fn get_key(&self, provider: &str) -> Result<Option<(String, Option<String>)>> {
        self.refresh_from_disk().await?;

        let data = self.data.read().await;
        let provider_lower = provider.to_lowercase();

        if let Some(entry) = data.entries.get(&provider_lower) {
            Ok(Some((entry.api_key.clone(), entry.base_url.clone())))
        } else {
            Ok(None)
        }
    }

    /// Remove a provider's key
    pub async fn remove_key(&self, provider: &str) -> Result<()> {
        self.refresh_from_disk().await?;

        let mut data = self.data.write().await;
        if data.entries.remove(&provider.to_lowercase()).is_none() {
            anyhow::bail!("No key found for provider '{}'", provider);
        }

        self.save(&data).await
    }

    /// List all stored providers (without revealing keys)
    pub async fn list_providers(&self) -> Result<Vec<ProviderInfo>> {
        self.refresh_from_disk().await?;

        let data = self.data.read().await;
        let mut out: Vec<ProviderInfo> = data
            .entries
            .iter()
            .map(|(name, entry)| ProviderInfo {
                name: name.clone(),
                base_url: entry.base_url.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Check if a provider has a stored key
    #[allow(dead_code)]
    pub async fn has_key(&self, provider: &str) -> bool {
        if self.refresh_from_disk().await.is_err() {
            return false;
        }

        let data = self.data.read().await;
        data.entries.contains_key(&provider.to_lowercase())
    }

    /// Create an ephemeral in-memory vault with a random key.
    /// Does NOT touch the OS keychain or any files on disk.
    /// Intended for tests and short-lived processes.
    #[allow(dead_code)]
    pub fn open_ephemeral() -> Self {
        Self {
            vault_path: PathBuf::from("/dev/null"), // never written
            data: RwLock::new(VaultData::default()),
            ephemeral: true,
        }
    }

    /// One-time migration from legacy encrypted JSON vault (~/.hostless/keys.vault)
    /// into dotenv-style plaintext vault (~/.hostless/keys.env).
    pub async fn migrate_legacy_json_vault(&self) -> Result<usize> {
        if self.ephemeral {
            return Ok(0);
        }

        let config_dir = crate::config::AppConfig::config_dir()?;
        let legacy_path = config_dir.join("keys.vault");
        if !legacy_path.exists() {
            return Ok(0);
        }

        let raw = tokio::fs::read_to_string(&legacy_path)
            .await
            .context("Failed to read legacy keys.vault")?;

        let legacy: LegacyVaultFile = serde_json::from_str(&raw)
            .context("Failed to parse legacy keys.vault JSON")?;

        if legacy.entries.is_empty() {
            return Ok(0);
        }

        let master_key = match super::keychain::try_load_existing_master_key()? {
            Some(key) => key,
            None => {
                anyhow::bail!(
                    "Legacy keychain master key not found; cannot decrypt keys.vault automatically"
                );
            }
        };

        let mut migrated = 0usize;
        let mut data = self.data.write().await;

        for entry in legacy.entries {
            let provider = entry.provider.to_lowercase();
            let decrypted = match super::encryption::decrypt(&master_key, &entry.encrypted_key) {
                Ok(v) => v,
                Err(e) => {
                    warn!(provider = provider.as_str(), error = %e, "Skipping undecryptable legacy key entry");
                    continue;
                }
            };

            let api_key = match String::from_utf8(decrypted) {
                Ok(v) => v,
                Err(e) => {
                    warn!(provider = provider.as_str(), error = %e, "Skipping non-UTF8 legacy key entry");
                    continue;
                }
            };

            data.entries.insert(
                provider,
                VaultEntry {
                    api_key,
                    base_url: entry.base_url,
                },
            );
            migrated += 1;
        }

        self.save(&data).await?;
        Ok(migrated)
    }

    /// Save vault to disk (skipped for ephemeral vaults)
    async fn save(&self, data: &VaultData) -> Result<()> {
        if self.ephemeral {
            return Ok(());
        }
        let contents = render_env_vault(data);
        tokio::fs::write(&self.vault_path, contents)
            .await
            .context("Failed to write vault file")?;
        Ok(())
    }

    /// Refresh in-memory vault from disk to pick up external writes (e.g. CLI key updates).
    async fn refresh_from_disk(&self) -> Result<()> {
        if self.ephemeral {
            return Ok(());
        }

        let data = if self.vault_path.exists() {
            let raw = tokio::fs::read_to_string(&self.vault_path)
                .await
                .context("Failed to read vault file")?;
            parse_env_vault(&raw)
        } else {
            VaultData::default()
        };

        let mut current = self.data.write().await;
        *current = data;
        Ok(())
    }
}

fn parse_env_vault(raw: &str) -> VaultData {
    let mut entries: HashMap<String, VaultEntry> = HashMap::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = parse_env_value(value.trim());

        if let Some(provider) = key.strip_prefix("HOSTLESS_KEY_") {
            let provider = provider.to_lowercase();
            let entry = entries.entry(provider).or_insert(VaultEntry {
                api_key: String::new(),
                base_url: None,
            });
            entry.api_key = value;
            continue;
        }

        if let Some(provider) = key.strip_prefix("HOSTLESS_BASE_URL_") {
            let provider = provider.to_lowercase();
            let entry = entries.entry(provider).or_insert(VaultEntry {
                api_key: String::new(),
                base_url: None,
            });
            if !value.is_empty() {
                entry.base_url = Some(value);
            }
        }
    }

    entries.retain(|_, entry| !entry.api_key.is_empty());
    VaultData { entries }
}

fn render_env_vault(data: &VaultData) -> String {
    let mut providers: Vec<&String> = data.entries.keys().collect();
    providers.sort();

    let mut out = String::from("# Hostless keys (dotenv-style)\n");
    out.push_str("# WARNING: plaintext secrets file\n\n");

    for provider in providers {
        if let Some(entry) = data.entries.get(provider) {
            out.push_str(&format!(
                "HOSTLESS_KEY_{}={}\n",
                provider,
                render_env_value(&entry.api_key)
            ));
            if let Some(base_url) = &entry.base_url {
                out.push_str(&format!(
                    "HOSTLESS_BASE_URL_{}={}\n",
                    provider,
                    render_env_value(base_url)
                ));
            }
            out.push('\n');
        }
    }

    out
}

fn parse_env_value(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let inner = &value[1..value.len() - 1];
        inner
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        value.to_string()
    }
}

fn render_env_value(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '+'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
