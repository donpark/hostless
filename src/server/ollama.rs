use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{AppConfig, OllamaApiModelConfig};
use crate::providers;
use crate::server::AppState;
use crate::vault::VaultStore;

#[derive(Clone, Debug, Serialize)]
pub struct OllamaModelDetails {
    pub parent_model: String,
    pub format: String,
    pub family: String,
    pub families: Vec<String>,
    pub parameter_size: String,
    pub quantization_level: String,
}

#[derive(Clone, Debug)]
pub struct OllamaModelSpec {
    pub provider_key: String,
    pub provider_name: String,
    pub id: String,
    pub modified_at: String,
    pub size: u64,
    pub digest: String,
    pub details: OllamaModelDetails,
    pub capabilities: Vec<String>,
    pub context_window: u64,
}

impl OllamaModelSpec {
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.provider_name, self.id)
    }

    pub fn matches_name(&self, model_name: &str) -> bool {
        self.id == model_name
            || self.qualified_name() == model_name
            || format!("{}/{}", self.provider_key, self.id) == model_name
    }
}

pub struct OllamaModelRegistry {
    cache_ttl: Duration,
    cache: RwLock<HashMap<String, CachedProviderCatalog>>,
}

impl OllamaModelRegistry {
    pub fn new() -> Self {
        Self {
            cache_ttl: Duration::from_secs(300),
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn list_for_providers(
        &self,
        state: &AppState,
        configured_providers: &[String],
    ) -> Vec<OllamaModelSpec> {
        let mut models = Vec::new();
        for provider in configured_providers {
            models.extend(self.models_for_provider(state, provider).await);
        }
        models.sort_by(|left, right| left.qualified_name().cmp(&right.qualified_name()));
        models
    }

    pub async fn find_for_providers(
        &self,
        state: &AppState,
        configured_providers: &[String],
        model_name: &str,
    ) -> Option<OllamaModelSpec> {
        let configured: HashSet<&str> = configured_providers.iter().map(String::as_str).collect();
        let (resolved_provider, _) = resolve_catalog_provider(model_name);

        for provider in configured_providers {
            if !configured.contains(provider.as_str()) {
                continue;
            }
            if !resolved_provider.is_empty() && resolved_provider != provider {
                continue;
            }

            if let Some(spec) = self
                .models_for_provider(state, provider)
                .await
                .into_iter()
                .find(|spec| spec.matches_name(model_name))
            {
                return Some(spec);
            }
        }

        None
    }

    async fn models_for_provider(
        &self,
        state: &AppState,
        provider_key: &str,
    ) -> Vec<OllamaModelSpec> {
        let cached = {
            let cache = self.cache.read().await;
            cache.get(provider_key).cloned()
        };

        if let Some(entry) = &cached {
            if entry.fetched_at.elapsed() <= self.cache_ttl {
                return entry.models.clone();
            }
        }

        let config = state.config.read().await.clone();
        match refresh_provider_catalog(&state.http_client, &state.vault, &config, provider_key).await {
            Ok(models) => {
                let mut cache = self.cache.write().await;
                cache.insert(
                    provider_key.to_string(),
                    CachedProviderCatalog {
                        fetched_at: Instant::now(),
                        models: models.clone(),
                    },
                );
                models
            }
            Err(error) => {
                tracing::warn!(provider = provider_key, error = %error, "Failed to refresh Ollama-compatible model catalog");
                cached.map(|entry| entry.models).unwrap_or_default()
            }
        }
    }
}

#[derive(Clone)]
struct CachedProviderCatalog {
    fetched_at: Instant,
    models: Vec<OllamaModelSpec>,
}

async fn refresh_provider_catalog(
    http_client: &reqwest::Client,
    vault: &VaultStore,
    config: &AppConfig,
    provider_key: &str,
) -> Result<Vec<OllamaModelSpec>> {
    let configured = config_models_for_provider(config, provider_key);
    let discovered = discover_provider_models(http_client, vault, config, provider_key)
        .await
        .unwrap_or_default();

    Ok(merge_catalogs(provider_key, discovered, configured))
}

fn merge_catalogs(
    provider_key: &str,
    discovered: Vec<OllamaModelSpec>,
    configured: Vec<OllamaApiModelConfig>,
) -> Vec<OllamaModelSpec> {
    let mut merged: HashMap<String, OllamaModelSpec> = discovered
        .into_iter()
        .map(|spec| (spec.id.clone(), spec))
        .collect();

    for config_model in configured {
        let entry = merged
            .entry(config_model.id.clone())
            .or_insert_with(|| spec_from_config(provider_key, &config_model));
        apply_config_overrides(entry, &config_model);
    }

    let mut models = merged.into_values().collect::<Vec<_>>();
    models.sort_by(|left, right| left.qualified_name().cmp(&right.qualified_name()));
    models
}

fn config_models_for_provider(config: &AppConfig, provider_key: &str) -> Vec<OllamaApiModelConfig> {
    provider_config_keys(provider_key)
        .into_iter()
    .filter_map(|key| config.ollama_api_models.get(key))
        .flat_map(|models| models.iter().cloned())
        .collect()
}

fn apply_config_overrides(spec: &mut OllamaModelSpec, config_model: &OllamaApiModelConfig) {
    if let Some(modified_at) = &config_model.modified_at {
        spec.modified_at = modified_at.clone();
    }
    if let Some(size) = config_model.size {
        spec.size = size;
    }
    if let Some(context_window) = config_model.context_window {
        spec.context_window = context_window;
    }
    if !config_model.capabilities.is_empty() {
        spec.capabilities = config_model.capabilities.clone();
    }
    if let Some(family) = &config_model.family {
        spec.details.family = family.clone();
    }
    if !config_model.families.is_empty() {
        spec.details.families = config_model.families.clone();
    }
    if let Some(parameter_size) = &config_model.parameter_size {
        spec.details.parameter_size = parameter_size.clone();
    }
    if let Some(quantization_level) = &config_model.quantization_level {
        spec.details.quantization_level = quantization_level.clone();
    }
    spec.digest = format!("hostless:{}/{}", spec.provider_name, spec.id);
}

fn spec_from_config(provider_key: &str, config_model: &OllamaApiModelConfig) -> OllamaModelSpec {
    let mut spec = base_spec(provider_key, &config_model.id);
    apply_config_overrides(&mut spec, config_model);
    spec
}

async fn discover_provider_models(
    http_client: &reqwest::Client,
    vault: &VaultStore,
    config: &AppConfig,
    provider_key: &str,
) -> Result<Vec<OllamaModelSpec>> {
    let (api_key, base_url_override) = vault
        .get_key(provider_key)
        .await?
        .with_context(|| format!("Missing API key for provider '{provider_key}'"))?;

    let provider = providers::get_provider(provider_key);
    let base_url = base_url_override
        .or_else(|| config.provider_urls.get(provider_key).cloned())
        .unwrap_or_else(|| provider.default_base_url().to_string());

    match provider_key {
        "openai" => discover_openai_models(http_client, &base_url, &api_key).await,
        "anthropic" => discover_anthropic_models(http_client, &base_url, &api_key).await,
        "google" => discover_google_models(http_client, &base_url, &api_key).await,
        _ => Ok(Vec::new()),
    }
}

async fn discover_openai_models(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<OllamaModelSpec>> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let body = http_client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?
        .json::<OpenAIModelList>()
        .await?;

    Ok(body
        .data
        .into_iter()
        .map(|model| {
            let mut spec = base_spec("openai", &model.id);
            if let Some(created) = chrono::DateTime::<Utc>::from_timestamp(model.created, 0) {
                spec.modified_at = created.to_rfc3339();
            }
            spec
        })
        .collect())
}

async fn discover_anthropic_models(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<OllamaModelSpec>> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let body = http_client
        .get(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?
        .error_for_status()?
        .json::<AnthropicModelList>()
        .await?;

    Ok(body
        .data
        .into_iter()
        .map(|model| {
            let mut spec = base_spec("anthropic", &model.id);
            spec.modified_at = model.created_at;
            spec
        })
        .collect())
}

async fn discover_google_models(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<OllamaModelSpec>> {
    let url = crate::providers::google::GoogleProvider::append_api_key_to_url(
        &format!("{}/v1beta/models", base_url.trim_end_matches('/')),
        api_key,
    );
    let body = http_client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json::<GoogleModelList>()
        .await?;

    Ok(body
        .models
        .into_iter()
        .map(|model| google_model_to_spec(model))
        .collect())
}

fn google_model_to_spec(model: GoogleModel) -> OllamaModelSpec {
    let id = model
        .base_model_id
        .or_else(|| model.name.strip_prefix("models/").map(str::to_string))
        .unwrap_or(model.name);
    let mut spec = base_spec("google", &id);
    let capabilities = google_capabilities(&model.supported_generation_methods);
    if !capabilities.is_empty() {
        spec.capabilities = capabilities;
    }
    if let Some(limit) = model.input_token_limit {
        spec.context_window = limit;
        spec.size = limit;
    }
    spec
}

fn google_capabilities(methods: &[String]) -> Vec<String> {
    let mut capabilities = Vec::new();
    for method in methods {
        match method.as_str() {
            "generateContent" => push_unique(&mut capabilities, "chat"),
            "streamGenerateContent" => push_unique(&mut capabilities, "streaming"),
            "embedContent" => push_unique(&mut capabilities, "embeddings"),
            _ => {}
        }
    }
    capabilities
}

pub fn canonical_model_name(model: &str) -> String {
    let (provider_key, resolved_model) = providers::resolve_provider(model);
    format!("{}/{}", display_provider_name(provider_key), resolved_model)
}

fn resolve_catalog_provider(model_name: &str) -> (&str, &str) {
    if let Some(rest) = model_name.strip_prefix("openai/") {
        ("openai", rest)
    } else if let Some(rest) = model_name.strip_prefix("anthropic/") {
        ("anthropic", rest)
    } else if let Some(rest) = model_name.strip_prefix("google/") {
        ("google", rest)
    } else if let Some(rest) = model_name.strip_prefix("gemini/") {
        ("google", rest)
    } else {
        ("", model_name)
    }
}

fn provider_config_keys(provider_key: &str) -> [&str; 2] {
    match provider_key {
        "google" => ["google", "gemini"],
        _ => [provider_key, provider_key],
    }
}

fn display_provider_name(provider_key: &str) -> &str {
    match provider_key {
        "google" => "gemini",
        other => other,
    }
}

fn base_spec(provider_key: &str, id: &str) -> OllamaModelSpec {
    let provider_name = display_provider_name(provider_key).to_string();
    let family = infer_family(provider_key, id);
    let capabilities = infer_capabilities(provider_key, id);
    let context_window = infer_context_window(provider_key, id);

    OllamaModelSpec {
        provider_key: provider_key.to_string(),
        provider_name: provider_name.clone(),
        id: id.to_string(),
        modified_at: "1970-01-01T00:00:00Z".to_string(),
        size: context_window,
        digest: format!("hostless:{provider_name}/{id}"),
        details: OllamaModelDetails {
            parent_model: "".to_string(),
            format: "api".to_string(),
            family: family.clone(),
            families: vec![family],
            parameter_size: "remote".to_string(),
            quantization_level: "remote".to_string(),
        },
        capabilities,
        context_window,
    }
}

fn infer_family(provider_key: &str, id: &str) -> String {
    match provider_key {
        "openai" if id.starts_with('o') => "o-series".to_string(),
        "openai" if id.contains("image") => "gpt-image".to_string(),
        "openai" if id.contains("tts") || id.contains("transcribe") => "gpt-audio".to_string(),
        "openai" => "gpt".to_string(),
        "anthropic" => "claude".to_string(),
        "google" => "gemini".to_string(),
        _ => provider_key.to_string(),
    }
}

fn infer_capabilities(provider_key: &str, id: &str) -> Vec<String> {
    let mut capabilities = vec!["chat".to_string()];
    if provider_key == "anthropic" || provider_key == "google" || id.contains("4o") {
        push_unique(&mut capabilities, "vision");
    }
    if id.contains("realtime") || id.contains("live") {
        push_unique(&mut capabilities, "realtime");
        push_unique(&mut capabilities, "audio");
    }
    if id.contains("tts") {
        capabilities = vec!["audio".to_string(), "speech".to_string()];
    }
    if id.contains("transcribe") {
        capabilities = vec!["audio".to_string(), "transcription".to_string()];
    }
    if id.contains("image") || id.starts_with("imagen") || id.starts_with("veo") {
        capabilities = vec!["image".to_string()];
    }
    if id.starts_with('o') || id.contains("thinking") || id.contains("reason") || id.contains("sonnet-4") {
        push_unique(&mut capabilities, "reasoning");
    }
    if !id.contains("tts") && !id.contains("transcribe") && !id.contains("image") && !id.starts_with("imagen") {
        push_unique(&mut capabilities, "tools");
    }
    capabilities
}

fn infer_context_window(provider_key: &str, id: &str) -> u64 {
    if provider_key == "google" {
        return 1_000_000;
    }
    if provider_key == "anthropic" {
        return 200_000;
    }
    if id.starts_with('o') {
        return 200_000;
    }
    if id.contains("image") {
        return 32_000;
    }
    128_000
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

#[derive(Debug, Deserialize)]
struct OpenAIModelList {
    data: Vec<OpenAIModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAIModel {
    id: String,
    created: i64,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelList {
    data: Vec<AnthropicModel>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModel {
    id: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct GoogleModelList {
    #[serde(default)]
    models: Vec<GoogleModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleModel {
    name: String,
    base_model_id: Option<String>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

pub struct ModelActivityTracker {
    ttl: Duration,
    entries: RwLock<HashMap<String, Instant>>,
}

impl ModelActivityTracker {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub async fn mark_active(&self, model_name: impl Into<String>) {
        let mut entries = self.entries.write().await;
        entries.insert(model_name.into(), Instant::now());
    }

    pub async fn active_models(&self) -> Vec<ActiveModel> {
        let now = Instant::now();
        let mut entries = self.entries.write().await;
        entries.retain(|_, last_seen| now.duration_since(*last_seen) <= self.ttl);

        entries
            .iter()
            .map(|(model_name, last_seen)| ActiveModel {
                model_name: model_name.clone(),
                expires_at: DateTime::<Utc>::from(std::time::SystemTime::now() + self.ttl.saturating_sub(now.duration_since(*last_seen))),
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ActiveModel {
    pub model_name: String,
    pub expires_at: DateTime<Utc>,
}