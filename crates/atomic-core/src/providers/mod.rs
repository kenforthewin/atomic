//! Provider abstraction layer for AI services (embeddings, LLM completion)
//! Enables pluggable providers (OpenRouter, Ollama, etc.)

pub mod error;
pub mod models;
pub mod ollama;
pub mod openai_compat;
pub mod openrouter;
pub mod structured;
pub mod traits;
pub mod types;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

pub use error::ProviderError;
pub use models::{
    fetch_and_return_capabilities, get_cached_capabilities_sync, save_capabilities_cache,
    AvailableModel,
};
pub use ollama::OllamaProvider;
pub use openai_compat::OpenAICompatProvider;
pub use openrouter::OpenRouterProvider;
pub use structured::{
    call_structured, lint_schema, parse_tolerant, StructuredCall, StructuredCallError,
};
pub use traits::{
    EmbeddingConfig, EmbeddingProvider, LlmConfig, LlmProvider, StreamingLlmProvider,
};

/// Provider type enum
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderType {
    OpenRouter,
    Ollama,
    OpenAICompat,
}

impl ProviderType {
    pub fn from_string(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "ollama" => ProviderType::Ollama,
            "openai_compat" => ProviderType::OpenAICompat,
            _ => ProviderType::OpenRouter,
        }
    }
}

/// Default OpenRouter API endpoint. `ProviderConfig::openrouter_base_url`
/// resolves to this unless explicitly overridden (proxies, gateways, or test
/// servers that speak the same API).
pub const OPENROUTER_DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Provider configuration extracted from settings
///
/// `Debug` is implemented manually to redact API keys — configs get logged
/// via `tracing` in pipeline diagnostics and must never leak key material.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub provider_type: ProviderType,
    // OpenRouter settings
    pub openrouter_api_key: Option<String>,
    /// Base URL for the OpenRouter API. Defaults to
    /// [`OPENROUTER_DEFAULT_BASE_URL`]; overridable (settings key
    /// `openrouter_base_url`) for proxies or composing processes that front
    /// the OpenRouter API.
    pub openrouter_base_url: String,
    pub openrouter_embedding_model: String,
    pub openrouter_llm_model: String,
    /// User-specified context length override. None = use model default from API cache.
    pub openrouter_context_length: Option<usize>,
    // Ollama settings
    pub ollama_host: String,
    pub ollama_embedding_model: String,
    pub ollama_llm_model: String,
    pub ollama_context_length: usize,
    pub ollama_timeout_secs: u64,
    // OpenAI-compatible settings
    pub openai_compat_base_url: String,
    pub openai_compat_api_key: Option<String>,
    pub openai_compat_embedding_model: String,
    pub openai_compat_llm_model: String,
    pub openai_compat_embedding_dimension: usize,
    pub openai_compat_context_length: usize,
    pub openai_compat_timeout_secs: u64,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact key material: `Some("[redacted]")` preserves the
        // present/absent signal without the value. Every other field is
        // safe to print verbatim.
        fn redacted(key: &Option<String>) -> Option<&'static str> {
            key.as_ref().map(|_| "[redacted]")
        }
        f.debug_struct("ProviderConfig")
            .field("provider_type", &self.provider_type)
            .field("openrouter_api_key", &redacted(&self.openrouter_api_key))
            .field("openrouter_base_url", &self.openrouter_base_url)
            .field(
                "openrouter_embedding_model",
                &self.openrouter_embedding_model,
            )
            .field("openrouter_llm_model", &self.openrouter_llm_model)
            .field("openrouter_context_length", &self.openrouter_context_length)
            .field("ollama_host", &self.ollama_host)
            .field("ollama_embedding_model", &self.ollama_embedding_model)
            .field("ollama_llm_model", &self.ollama_llm_model)
            .field("ollama_context_length", &self.ollama_context_length)
            .field("ollama_timeout_secs", &self.ollama_timeout_secs)
            .field("openai_compat_base_url", &self.openai_compat_base_url)
            .field(
                "openai_compat_api_key",
                &redacted(&self.openai_compat_api_key),
            )
            .field(
                "openai_compat_embedding_model",
                &self.openai_compat_embedding_model,
            )
            .field("openai_compat_llm_model", &self.openai_compat_llm_model)
            .field(
                "openai_compat_embedding_dimension",
                &self.openai_compat_embedding_dimension,
            )
            .field(
                "openai_compat_context_length",
                &self.openai_compat_context_length,
            )
            .field(
                "openai_compat_timeout_secs",
                &self.openai_compat_timeout_secs,
            )
            .finish()
    }
}

impl ProviderConfig {
    pub fn from_settings(settings: &HashMap<String, String>) -> Self {
        let provider_type = ProviderType::from_string(
            settings
                .get("provider")
                .map(|s| s.as_str())
                .unwrap_or("openrouter"),
        );

        ProviderConfig {
            provider_type,
            openrouter_api_key: settings.get("openrouter_api_key").cloned(),
            openrouter_base_url: settings
                .get("openrouter_base_url")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| OPENROUTER_DEFAULT_BASE_URL.to_string()),
            openrouter_embedding_model: settings
                .get("embedding_model")
                .cloned()
                .unwrap_or_else(|| "openai/text-embedding-3-small".to_string()),
            openrouter_llm_model: settings
                .get("tagging_model")
                .cloned()
                .unwrap_or_else(|| "openai/gpt-4o-mini".to_string()),
            openrouter_context_length: settings.get("openrouter_context_length").and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    s.parse().ok()
                }
            }),
            ollama_host: settings
                .get("ollama_host")
                .cloned()
                .unwrap_or_else(|| "http://127.0.0.1:11434".to_string()),
            ollama_embedding_model: settings
                .get("ollama_embedding_model")
                .cloned()
                .unwrap_or_else(|| "nomic-embed-text".to_string()),
            ollama_llm_model: settings
                .get("ollama_llm_model")
                .cloned()
                .unwrap_or_else(|| "llama3.2".to_string()),
            ollama_context_length: settings
                .get("ollama_context_length")
                .and_then(|s| s.parse().ok())
                .unwrap_or(65536),
            ollama_timeout_secs: settings
                .get("ollama_timeout_secs")
                .and_then(|s| s.parse().ok())
                .unwrap_or(120), // Default 2 minutes
            openai_compat_base_url: settings
                .get("openai_compat_base_url")
                .cloned()
                .unwrap_or_default(),
            openai_compat_api_key: settings
                .get("openai_compat_api_key")
                .cloned()
                .filter(|k| !k.is_empty()),
            openai_compat_embedding_model: settings
                .get("openai_compat_embedding_model")
                .cloned()
                .unwrap_or_default(),
            openai_compat_llm_model: settings
                .get("openai_compat_llm_model")
                .cloned()
                .unwrap_or_default(),
            openai_compat_embedding_dimension: settings
                .get("openai_compat_embedding_dimension")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1536),
            openai_compat_context_length: settings
                .get("openai_compat_context_length")
                .and_then(|s| s.parse().ok())
                .unwrap_or(65536),
            openai_compat_timeout_secs: settings
                .get("openai_compat_timeout_secs")
                .and_then(|s| s.parse().ok())
                .unwrap_or(300), // Default 5 minutes
        }
    }

    /// Overlay this config onto a resolved settings map so that
    /// [`ProviderConfig::from_settings`] on the result reproduces `self`
    /// exactly. Every key `from_settings` reads is either written or removed
    /// (for `None` optionals), so no provider-config residue from the
    /// underlying map can survive.
    ///
    /// This is the bridge used by `AtomicCore`'s explicit provider-config
    /// mode: AI operations resolve their settings map once per operation, and
    /// this overlay makes the active config authoritative over whatever the
    /// settings tables hold, without touching unrelated settings (prompts,
    /// strategies).
    ///
    /// **Model selection is part of provider config.** Beyond the keys
    /// `from_settings` reads back, the overlay also pins the per-task model
    /// keys (`wiki_model`, `chat_model`) to this config's
    /// [`llm_model`](Self::llm_model): wiki generation, the chat agent, and
    /// reports resolve their model from those settings keys (OpenRouter
    /// only; the other providers already use `llm_model()` directly), and an
    /// explicit config that left them settings-resolved would let a settings
    /// write route traffic on the explicitly configured credential to any
    /// model the key can reach — exactly what explicit mode exists to
    /// prevent. The roundtrip property above is unaffected: `from_settings`
    /// ignores both keys.
    pub(crate) fn apply_to_settings(&self, settings: &mut HashMap<String, String>) {
        let provider = match self.provider_type {
            ProviderType::OpenRouter => "openrouter",
            ProviderType::Ollama => "ollama",
            ProviderType::OpenAICompat => "openai_compat",
        };
        settings.insert("provider".to_string(), provider.to_string());

        match &self.openrouter_api_key {
            Some(key) => settings.insert("openrouter_api_key".to_string(), key.clone()),
            None => settings.remove("openrouter_api_key"),
        };
        settings.insert(
            "openrouter_base_url".to_string(),
            self.openrouter_base_url.clone(),
        );
        settings.insert(
            "embedding_model".to_string(),
            self.openrouter_embedding_model.clone(),
        );
        settings.insert(
            "tagging_model".to_string(),
            self.openrouter_llm_model.clone(),
        );
        // The per-task model keys (doc comment above): one explicit LLM
        // selection governs tagging, wiki, chat, and reports alike.
        settings.insert("wiki_model".to_string(), self.llm_model().to_string());
        settings.insert("chat_model".to_string(), self.llm_model().to_string());
        match self.openrouter_context_length {
            Some(len) => settings.insert("openrouter_context_length".to_string(), len.to_string()),
            None => settings.remove("openrouter_context_length"),
        };

        settings.insert("ollama_host".to_string(), self.ollama_host.clone());
        settings.insert(
            "ollama_embedding_model".to_string(),
            self.ollama_embedding_model.clone(),
        );
        settings.insert(
            "ollama_llm_model".to_string(),
            self.ollama_llm_model.clone(),
        );
        settings.insert(
            "ollama_context_length".to_string(),
            self.ollama_context_length.to_string(),
        );
        settings.insert(
            "ollama_timeout_secs".to_string(),
            self.ollama_timeout_secs.to_string(),
        );

        settings.insert(
            "openai_compat_base_url".to_string(),
            self.openai_compat_base_url.clone(),
        );
        match &self.openai_compat_api_key {
            Some(key) => settings.insert("openai_compat_api_key".to_string(), key.clone()),
            None => settings.remove("openai_compat_api_key"),
        };
        settings.insert(
            "openai_compat_embedding_model".to_string(),
            self.openai_compat_embedding_model.clone(),
        );
        settings.insert(
            "openai_compat_llm_model".to_string(),
            self.openai_compat_llm_model.clone(),
        );
        settings.insert(
            "openai_compat_embedding_dimension".to_string(),
            self.openai_compat_embedding_dimension.to_string(),
        );
        settings.insert(
            "openai_compat_context_length".to_string(),
            self.openai_compat_context_length.to_string(),
        );
        settings.insert(
            "openai_compat_timeout_secs".to_string(),
            self.openai_compat_timeout_secs.to_string(),
        );
    }

    /// Get the embedding model for the current provider
    pub fn embedding_model(&self) -> &str {
        match self.provider_type {
            ProviderType::OpenRouter => &self.openrouter_embedding_model,
            ProviderType::Ollama => &self.ollama_embedding_model,
            ProviderType::OpenAICompat => &self.openai_compat_embedding_model,
        }
    }

    /// Get the LLM model for the current provider
    pub fn llm_model(&self) -> &str {
        match self.provider_type {
            ProviderType::OpenRouter => &self.openrouter_llm_model,
            ProviderType::Ollama => &self.ollama_llm_model,
            ProviderType::OpenAICompat => &self.openai_compat_llm_model,
        }
    }

    /// Get the embedding dimension for the current embedding model
    pub fn embedding_dimension(&self) -> usize {
        match self.provider_type {
            ProviderType::OpenRouter => {
                openrouter::models::get_embedding_dimension(&self.openrouter_embedding_model)
                    .unwrap_or(1536) // Fall back to 1536 for unknown models
            }
            ProviderType::Ollama => ollama::get_embedding_dimension(&self.ollama_embedding_model),
            ProviderType::OpenAICompat => self.openai_compat_embedding_dimension,
        }
    }

    /// Get the context length (in tokens) for the current provider's LLM.
    /// For OpenRouter: uses user override if set, otherwise looks up the model's
    /// context length from the in-memory capabilities cache, falling back to None.
    /// For Ollama/OpenAI-compat: uses the user-specified setting.
    pub fn context_length(&self) -> Option<usize> {
        match self.provider_type {
            ProviderType::OpenRouter => {
                if let Some(ctx) = self.openrouter_context_length {
                    return Some(ctx);
                }
                // Fall back to model metadata from capabilities cache
                let cache = CAPABILITIES_CACHE.inner.lock().ok()?;
                cache
                    .as_ref()?
                    .context_lengths
                    .get(&self.openrouter_llm_model)
                    .copied()
            }
            ProviderType::Ollama => Some(self.ollama_context_length),
            ProviderType::OpenAICompat => Some(self.openai_compat_context_length),
        }
    }

    /// Get the context length for a specific model (used when the active model
    /// differs from the default LLM model, e.g. wiki_model or chat_model).
    pub fn context_length_for_model(&self, model: &str) -> Option<usize> {
        match self.provider_type {
            ProviderType::OpenRouter => {
                if let Some(ctx) = self.openrouter_context_length {
                    return Some(ctx);
                }
                let cache = CAPABILITIES_CACHE.inner.lock().ok()?;
                cache.as_ref()?.context_lengths.get(model).copied()
            }
            _ => self.context_length(),
        }
    }
}

/// Create an embedding provider based on configuration
pub fn create_embedding_provider(
    config: &ProviderConfig,
) -> Result<Arc<dyn EmbeddingProvider>, ProviderError> {
    match config.provider_type {
        ProviderType::OpenRouter => {
            let api_key = config.openrouter_api_key.clone().ok_or_else(|| {
                ProviderError::Configuration("OpenRouter API key not configured".to_string())
            })?;
            Ok(Arc::new(OpenRouterProvider::with_base_url(
                api_key,
                config.openrouter_base_url.clone(),
            )))
        }
        ProviderType::Ollama => Ok(Arc::new(OllamaProvider::new(
            Some(config.ollama_host.clone()),
            Some(config.ollama_timeout_secs),
        ))),
        ProviderType::OpenAICompat => {
            if config.openai_compat_base_url.is_empty() {
                return Err(ProviderError::Configuration(
                    "OpenAI Compatible base URL not configured".to_string(),
                ));
            }
            Ok(Arc::new(OpenAICompatProvider::new(
                config.openai_compat_base_url.clone(),
                config.openai_compat_api_key.clone(),
                Some(config.openai_compat_timeout_secs),
            )))
        }
    }
}

/// Create an LLM provider based on configuration
pub fn create_llm_provider(config: &ProviderConfig) -> Result<Arc<dyn LlmProvider>, ProviderError> {
    match config.provider_type {
        ProviderType::OpenRouter => {
            let api_key = config.openrouter_api_key.clone().ok_or_else(|| {
                ProviderError::Configuration("OpenRouter API key not configured".to_string())
            })?;
            Ok(Arc::new(OpenRouterProvider::with_base_url(
                api_key,
                config.openrouter_base_url.clone(),
            )))
        }
        ProviderType::Ollama => Ok(Arc::new(OllamaProvider::new(
            Some(config.ollama_host.clone()),
            Some(config.ollama_timeout_secs),
        ))),
        ProviderType::OpenAICompat => {
            if config.openai_compat_base_url.is_empty() {
                return Err(ProviderError::Configuration(
                    "OpenAI Compatible base URL not configured".to_string(),
                ));
            }
            Ok(Arc::new(OpenAICompatProvider::new(
                config.openai_compat_base_url.clone(),
                config.openai_compat_api_key.clone(),
                Some(config.openai_compat_timeout_secs),
            )))
        }
    }
}

/// Create a streaming LLM provider based on configuration
pub fn create_streaming_llm_provider(
    config: &ProviderConfig,
) -> Result<Arc<dyn StreamingLlmProvider>, ProviderError> {
    match config.provider_type {
        ProviderType::OpenRouter => {
            let api_key = config.openrouter_api_key.clone().ok_or_else(|| {
                ProviderError::Configuration("OpenRouter API key not configured".to_string())
            })?;
            Ok(Arc::new(OpenRouterProvider::with_base_url(
                api_key,
                config.openrouter_base_url.clone(),
            )))
        }
        ProviderType::Ollama => Ok(Arc::new(OllamaProvider::new(
            Some(config.ollama_host.clone()),
            Some(config.ollama_timeout_secs),
        ))),
        ProviderType::OpenAICompat => {
            if config.openai_compat_base_url.is_empty() {
                return Err(ProviderError::Configuration(
                    "OpenAI Compatible base URL not configured".to_string(),
                ));
            }
            Ok(Arc::new(OpenAICompatProvider::new(
                config.openai_compat_base_url.clone(),
                config.openai_compat_api_key.clone(),
                Some(config.openai_compat_timeout_secs),
            )))
        }
    }
}

// ==================== Provider Cache ====================

/// Cached provider instances keyed on ProviderConfig.
/// Avoids creating a new reqwest::Client per API call.
struct ProviderCache {
    embedding: Mutex<Option<(ProviderConfig, Arc<dyn EmbeddingProvider>)>>,
    llm: Mutex<Option<(ProviderConfig, Arc<dyn LlmProvider>)>>,
}

static PROVIDER_CACHE: LazyLock<ProviderCache> = LazyLock::new(|| ProviderCache {
    embedding: Mutex::new(None),
    llm: Mutex::new(None),
});

/// Get or create a cached embedding provider.
/// Returns the same Arc if config hasn't changed.
pub fn get_embedding_provider(
    config: &ProviderConfig,
) -> Result<Arc<dyn EmbeddingProvider>, ProviderError> {
    let mut cache = PROVIDER_CACHE
        .embedding
        .lock()
        .map_err(|_| ProviderError::Configuration("Provider cache lock poisoned".to_string()))?;

    if let Some((ref cached_config, ref provider)) = *cache {
        if cached_config == config {
            return Ok(Arc::clone(provider));
        }
    }

    let provider = create_embedding_provider(config)?;
    *cache = Some((config.clone(), Arc::clone(&provider)));
    Ok(provider)
}

/// Get or create a cached LLM provider.
/// Returns the same Arc if config hasn't changed.
pub fn get_llm_provider(config: &ProviderConfig) -> Result<Arc<dyn LlmProvider>, ProviderError> {
    let mut cache = PROVIDER_CACHE
        .llm
        .lock()
        .map_err(|_| ProviderError::Configuration("Provider cache lock poisoned".to_string()))?;

    if let Some((ref cached_config, ref provider)) = *cache {
        if cached_config == config {
            return Ok(Arc::clone(provider));
        }
    }

    let provider = create_llm_provider(config)?;
    *cache = Some((config.clone(), Arc::clone(&provider)));
    Ok(provider)
}

// ==================== Model Capabilities Cache ====================

/// In-memory cache for model capabilities to avoid repeated DB reads + API calls.
struct CapabilitiesCache {
    inner: Mutex<Option<models::ModelCapabilitiesCache>>,
}

static CAPABILITIES_CACHE: LazyLock<CapabilitiesCache> = LazyLock::new(|| CapabilitiesCache {
    inner: Mutex::new(None),
});

/// Get cached model capabilities from memory, falling back to DB, then API.
/// This avoids each concurrent task independently fetching capabilities.
pub async fn get_model_capabilities(
    db_conn_fn: impl Fn() -> Result<rusqlite::Connection, String>,
) -> Option<models::ModelCapabilitiesCache> {
    // Check in-memory cache first
    {
        let cache = CAPABILITIES_CACHE.inner.lock().ok()?;
        if let Some(ref caps) = *cache {
            if !caps.is_stale() {
                return Some(caps.clone());
            }
        }
    }

    // Try DB cache
    let db_cache = {
        let conn = db_conn_fn().ok()?;
        get_cached_capabilities_sync(&conn).ok().flatten()
    };

    let (cached, is_stale) = match db_cache {
        Some(ref cache) => (Some(cache.clone()), cache.is_stale()),
        None => (None, true),
    };

    let result = if is_stale {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        match fetch_and_return_capabilities(&client).await {
            Ok(fresh) => {
                // Save to DB
                if let Ok(conn) = db_conn_fn() {
                    let _ = save_capabilities_cache(&conn, &fresh);
                }
                fresh
            }
            Err(_) => cached.unwrap_or_default(),
        }
    } else {
        cached.unwrap_or_default()
    };

    // Store in memory
    if let Ok(mut cache) = CAPABILITIES_CACHE.inner.lock() {
        *cache = Some(result.clone());
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_error_is_retryable() {
        // Retryable errors
        let rate_limited = ProviderError::RateLimited {
            retry_after_secs: Some(30),
        };
        assert!(
            rate_limited.is_retryable(),
            "Rate limited should be retryable"
        );

        let network_error = ProviderError::Network("connection refused".to_string());
        assert!(
            network_error.is_retryable(),
            "Network errors should be retryable"
        );

        // Non-retryable errors
        let config_error = ProviderError::Configuration("missing API key".to_string());
        assert!(
            !config_error.is_retryable(),
            "Config errors should not be retryable"
        );

        let api_error = ProviderError::Api {
            status: 400,
            message: "bad request".to_string(),
        };
        assert!(
            !api_error.is_retryable(),
            "API errors should not be retryable"
        );

        let model_error = ProviderError::ModelNotFound("gpt-5".to_string());
        assert!(
            !model_error.is_retryable(),
            "Model not found should not be retryable"
        );
    }

    #[test]
    fn test_provider_config_from_settings_openrouter() {
        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert("provider".to_string(), "openrouter".to_string());
        settings.insert("openrouter_api_key".to_string(), "test-key".to_string());
        settings.insert(
            "embedding_model".to_string(),
            "openai/text-embedding-3-large".to_string(),
        );

        let config = ProviderConfig::from_settings(&settings);

        assert_eq!(config.provider_type, ProviderType::OpenRouter);
        assert_eq!(config.openrouter_api_key, Some("test-key".to_string()));
        assert_eq!(config.embedding_model(), "openai/text-embedding-3-large");
        assert_eq!(config.embedding_dimension(), 3072); // text-embedding-3-large
    }

    #[test]
    fn test_provider_config_from_settings_ollama() {
        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert("provider".to_string(), "ollama".to_string());
        settings.insert(
            "ollama_host".to_string(),
            "http://localhost:11434".to_string(),
        );
        settings.insert(
            "ollama_embedding_model".to_string(),
            "nomic-embed-text".to_string(),
        );
        settings.insert("ollama_llm_model".to_string(), "llama3.2".to_string());

        let config = ProviderConfig::from_settings(&settings);

        assert_eq!(config.provider_type, ProviderType::Ollama);
        assert_eq!(config.ollama_host, "http://localhost:11434");
        assert_eq!(config.embedding_model(), "nomic-embed-text");
        assert_eq!(config.llm_model(), "llama3.2");
    }

    #[test]
    fn test_openrouter_base_url_default_and_override() {
        let config = ProviderConfig::from_settings(&HashMap::new());
        assert_eq!(config.openrouter_base_url, OPENROUTER_DEFAULT_BASE_URL);

        // Empty value falls back to the default, same as a missing key.
        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert("openrouter_base_url".to_string(), "".to_string());
        let config = ProviderConfig::from_settings(&settings);
        assert_eq!(config.openrouter_base_url, OPENROUTER_DEFAULT_BASE_URL);

        settings.insert(
            "openrouter_base_url".to_string(),
            "http://127.0.0.1:9999".to_string(),
        );
        let config = ProviderConfig::from_settings(&settings);
        assert_eq!(config.openrouter_base_url, "http://127.0.0.1:9999");
    }

    #[test]
    fn test_openrouter_provider_base_url_normalization() {
        // Default endpoint already ends with /v1 — kept verbatim.
        let provider = OpenRouterProvider::new("k".to_string());
        assert_eq!(provider.base_url(), OPENROUTER_DEFAULT_BASE_URL);

        // Bare host gets /v1 appended; trailing slashes are trimmed first.
        let provider = OpenRouterProvider::with_base_url(
            "k".to_string(),
            "http://localhost:8080/".to_string(),
        );
        assert_eq!(provider.base_url(), "http://localhost:8080/v1");

        // An explicit /v1 suffix is preserved, not doubled.
        let provider = OpenRouterProvider::with_base_url(
            "k".to_string(),
            "http://proxy.internal/api/v1".to_string(),
        );
        assert_eq!(provider.base_url(), "http://proxy.internal/api/v1");
    }

    #[test]
    fn test_provider_config_debug_redacts_keys() {
        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert(
            "openrouter_api_key".to_string(),
            "sk-or-super-secret".to_string(),
        );
        settings.insert(
            "openai_compat_api_key".to_string(),
            "compat-super-secret".to_string(),
        );
        settings.insert(
            "embedding_model".to_string(),
            "openai/text-embedding-3-small".to_string(),
        );
        let config = ProviderConfig::from_settings(&settings);

        let debug = format!("{:?}", config);
        let debug_alt = format!("{:#?}", config);
        for rendered in [&debug, &debug_alt] {
            assert!(
                !rendered.contains("sk-or-super-secret"),
                "Debug output leaked the OpenRouter key: {rendered}"
            );
            assert!(
                !rendered.contains("compat-super-secret"),
                "Debug output leaked the OpenAI-compat key: {rendered}"
            );
            assert!(
                rendered.contains("[redacted]"),
                "Debug output should mark present keys as redacted: {rendered}"
            );
            // Non-secret fields stay visible.
            assert!(
                rendered.contains("openai/text-embedding-3-small"),
                "Debug output should keep non-secret fields: {rendered}"
            );
        }

        // Absent keys render as None, preserving the present/absent signal.
        let bare = ProviderConfig::from_settings(&HashMap::new());
        let rendered = format!("{:?}", bare);
        assert!(rendered.contains("openrouter_api_key: None"), "{rendered}");
    }

    #[test]
    fn test_apply_to_settings_roundtrip() {
        // A config with every optional populated and nothing left at its
        // default, overlaid onto a map full of conflicting provider keys:
        // from_settings must reproduce the config exactly and unrelated
        // settings must survive untouched.
        let mut config = ProviderConfig::from_settings(&HashMap::new());
        config.provider_type = ProviderType::OpenRouter;
        config.openrouter_api_key = Some("key-a".to_string());
        config.openrouter_base_url = "http://proxy.internal/api/v1".to_string();
        config.openrouter_embedding_model = "mock/embed".to_string();
        config.openrouter_llm_model = "mock/llm".to_string();
        config.openrouter_context_length = Some(8192);
        config.ollama_host = "http://ollama:11434".to_string();
        config.ollama_embedding_model = "embed-x".to_string();
        config.ollama_llm_model = "llm-x".to_string();
        config.ollama_context_length = 1234;
        config.ollama_timeout_secs = 77;
        config.openai_compat_base_url = "http://compat:9000".to_string();
        config.openai_compat_api_key = Some("key-b".to_string());
        config.openai_compat_embedding_model = "ce".to_string();
        config.openai_compat_llm_model = "cl".to_string();
        config.openai_compat_embedding_dimension = 768;
        config.openai_compat_context_length = 4321;
        config.openai_compat_timeout_secs = 99;

        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert("provider".to_string(), "ollama".to_string());
        settings.insert("openrouter_api_key".to_string(), "stale-key".to_string());
        settings.insert(
            "openrouter_base_url".to_string(),
            "http://stale".to_string(),
        );
        settings.insert("embedding_model".to_string(), "stale/embed".to_string());
        settings.insert("wiki_model".to_string(), "frontier/expensive".to_string());
        settings.insert("chat_model".to_string(), "frontier/expensive".to_string());
        settings.insert(
            "wiki_generation_prompt".to_string(),
            "keep this prompt".to_string(),
        );

        config.apply_to_settings(&mut settings);
        assert_eq!(ProviderConfig::from_settings(&settings), config);
        // Model selection is provider config: the per-task model keys are
        // pinned to the explicit config's LLM, so a settings write can never
        // route wiki/chat/report traffic on the configured credential to a
        // model the config didn't choose.
        for key in ["wiki_model", "chat_model"] {
            assert_eq!(
                settings.get(key).map(|s| s.as_str()),
                Some(config.llm_model()),
                "{key} must be pinned to the explicit config's LLM"
            );
        }
        assert_eq!(
            settings.get("wiki_generation_prompt").map(|s| s.as_str()),
            Some("keep this prompt"),
            "non-provider settings must survive the overlay"
        );

        // None-valued optionals must clear stale residue, not inherit it.
        config.openrouter_api_key = None;
        config.openrouter_context_length = None;
        config.openai_compat_api_key = None;
        let mut settings: HashMap<String, String> = HashMap::new();
        settings.insert("openrouter_api_key".to_string(), "stale-key".to_string());
        settings.insert("openrouter_context_length".to_string(), "999".to_string());
        settings.insert("openai_compat_api_key".to_string(), "stale-b".to_string());
        config.apply_to_settings(&mut settings);
        assert_eq!(ProviderConfig::from_settings(&settings), config);
        assert!(!settings.contains_key("openrouter_api_key"));
    }

    #[test]
    fn test_provider_config_defaults() {
        // Empty settings should use defaults
        let settings: HashMap<String, String> = HashMap::new();
        let config = ProviderConfig::from_settings(&settings);

        assert_eq!(config.provider_type, ProviderType::OpenRouter); // Default
        assert_eq!(
            config.openrouter_embedding_model,
            "openai/text-embedding-3-small"
        );
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
    }
}
