//! Control plane → [`ProviderConfig`] plumbing (plan: "Provider management"
//! → "Plumbing — control plane → AtomicCore").
//!
//! Cloud tenants **never** resolve provider config from their tenant
//! database's settings tables — that is atomic-core's settings-fallback
//! path, which the plan explicitly forbids in cloud (a tenant could
//! otherwise reconfigure providers by writing settings rows, bypassing the
//! control plane's curation and key custody entirely). Instead every tenant
//! manager is opened with an **explicit** `Some(ProviderConfig)` built here
//! from the account's active `provider_credentials` row, and live rotations
//! swap that config in place via `AtomicCore::update_provider_config`.
//!
//! [`config_for_credentials`] builds the config from a decrypted row;
//! [`build_provider_config`] is the underlying constructor, also used by the
//! BYOK save route to build the *candidate* config it validates before
//! anything is stored; [`keyless_provider_config`] is the "no credentials
//! row" shape — the right provider type with no key, so provider calls fail
//! with atomic-core's structured missing-key error instead of silently
//! falling back to settings.
//!
//! # `model_config` vocabulary
//!
//! `provider_credentials.model_config` is the account-level model selection
//! (plan: "Storage schema" — model selection lives with the key). Recognized
//! keys, each optional, applied to the row's own provider:
//!
//! | key | meaning |
//! |---|---|
//! | `embedding_model` | embedding model id |
//! | `llm_model` | the LLM for every task — tagging, wiki, chat, reports |
//! | `openrouter_base_url` | OpenRouter API base override (proxies, gateways, test servers) |
//! | `openai_compat_base_url` | OpenAI-compatible API base (required for that provider to function) |
//! | `embedding_dimension` | embedding vector width (OpenAI-compat only; OpenRouter models carry known dimensions) |
//!
//! Unknown keys are ignored here — *write-side* policy lives with the
//! routes: managed rows are curation-checked ([`crate::curated_models`]),
//! and BYOK rows are vocabulary-checked ([`validate_byok_model_config`],
//! below) so a write can never smuggle keys outside this table into the
//! column. Everything not supplied falls back to atomic-core's own
//! defaults, which keeps this builder a thin overlay rather than a second
//! source of default truth.
//!
//! `llm_model` is the account's *entire* LLM selection: in explicit mode
//! atomic-core pins the per-task `wiki_model`/`chat_model` settings keys to
//! the config's LLM (`ProviderConfig::apply_to_settings`), so a tenant
//! settings write can never route wiki/chat/report traffic to a model this
//! column didn't choose — which is what makes managed curation
//! ([`crate::curated_models`]) actually govern every LLM consumer, not just
//! tagging. Distinct per-task models for BYOK accounts would be new
//! vocabulary here *plus* per-task fields on `ProviderConfig`; deferred
//! until someone asks.

use std::collections::HashMap;

use atomic_core::{ProviderConfig, ProviderType};
use serde_json::Value;

use crate::keyvault::SecretKey;
use crate::provider_credentials::{Provider, ProviderCredentials};

/// Build the explicit [`ProviderConfig`] for a `(provider, key,
/// model_config)` triple. The key lands in the slot matching `provider`;
/// the other provider's slot stays `None`, so a later misrouted call cannot
/// quietly authenticate with the wrong credential.
pub fn build_provider_config(
    provider: Provider,
    api_key: Option<&SecretKey>,
    model_config: &Value,
) -> ProviderConfig {
    // Start from atomic-core's defaults (empty settings map) — the single
    // source of default truth — then overlay the row's selections.
    let mut config = ProviderConfig::from_settings(&HashMap::new());
    let key = api_key.map(|k| k.expose().to_string());

    let string_field = |name: &str| {
        model_config
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    match provider {
        Provider::OpenRouter => {
            config.provider_type = ProviderType::OpenRouter;
            config.openrouter_api_key = key;
            if let Some(model) = string_field("embedding_model") {
                config.openrouter_embedding_model = model;
            }
            if let Some(model) = string_field("llm_model") {
                config.openrouter_llm_model = model;
            }
            if let Some(base_url) = string_field("openrouter_base_url") {
                config.openrouter_base_url = base_url;
            }
        }
        Provider::OpenAiCompat => {
            config.provider_type = ProviderType::OpenAICompat;
            config.openai_compat_api_key = key;
            if let Some(model) = string_field("embedding_model") {
                config.openai_compat_embedding_model = model;
            }
            if let Some(model) = string_field("llm_model") {
                config.openai_compat_llm_model = model;
            }
            if let Some(base_url) = string_field("openai_compat_base_url") {
                config.openai_compat_base_url = base_url;
            }
            if let Some(dimension) = model_config
                .get("embedding_dimension")
                .and_then(Value::as_u64)
            {
                config.openai_compat_embedding_dimension = dimension as usize;
            }
        }
    }
    config
}

/// The full `model_config` vocabulary (module docs) — every key a **BYOK**
/// write may carry. Wider than the managed allowlist
/// (`crate::curated_models`): BYOK users own their base URLs and (subject
/// to the platform dimension pin) their embedding setup.
pub const BYOK_ALLOWED_KEYS: &[&str] = &[
    "embedding_model",
    "llm_model",
    "openrouter_base_url",
    "openai_compat_base_url",
    "embedding_dimension",
];

/// Validate a user-submitted BYOK `model_config` against the documented
/// vocabulary. Mirrors `validate_managed_model_config`'s shape (an
/// `Err(message)` written for the 400 body), with the wider
/// [`BYOK_ALLOWED_KEYS`] allowlist and no model curation.
///
/// This is a **secret-hygiene** gate as much as a schema check: the column
/// is stored plaintext (only the API key is encrypted) and echoed verbatim
/// by the status route, so an unknown key — say a client nesting `api_key`
/// inside `model_config` — would persist a secret unencrypted and display
/// it forever. Rejecting everything outside the vocabulary closes that by
/// construction.
pub fn validate_byok_model_config(model_config: &Value) -> Result<(), String> {
    let Some(object) = model_config.as_object() else {
        return Err("model_config must be a JSON object".to_string());
    };

    for (key, value) in object {
        if !BYOK_ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "model_config key {key:?} is not part of the model-config \
                 vocabulary; allowed keys: {BYOK_ALLOWED_KEYS:?}. model_config \
                 is stored and displayed in plain text — API keys belong in \
                 the api_key field, never here."
            ));
        }
        if key == "embedding_dimension" {
            if !value.is_u64() {
                return Err(
                    "model_config.embedding_dimension must be a positive integer".to_string(),
                );
            }
        } else if !value.is_string() {
            return Err(format!("model_config.{key} must be a string"));
        }
    }

    Ok(())
}

/// The explicit config for a decrypted credentials row.
pub fn config_for_credentials(credentials: &ProviderCredentials) -> ProviderConfig {
    build_provider_config(
        credentials.provider,
        Some(&credentials.api_key),
        &credentials.model_config,
    )
}

/// The "no credentials row" config: the platform's default provider type
/// with no key. Cloud always opens tenant managers with `Some(config)` —
/// passing `None` would route provider resolution through the tenant's own
/// settings tables (the forbidden fallback; module docs) — so an account
/// without credentials gets this shape and every provider call fails with
/// atomic-core's structured missing-key error.
pub fn keyless_provider_config() -> ProviderConfig {
    build_provider_config(Provider::OpenRouter, None, &Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openrouter_config_carries_key_models_and_base_url() {
        let key = SecretKey::new("sk-or-v1-secret".to_string());
        let config = build_provider_config(
            Provider::OpenRouter,
            Some(&key),
            &json!({
                "embedding_model": "openai/text-embedding-3-small",
                "llm_model": "openai/gpt-4o-mini",
                "openrouter_base_url": "http://127.0.0.1:9999",
            }),
        );
        assert_eq!(config.provider_type, ProviderType::OpenRouter);
        assert_eq!(
            config.openrouter_api_key.as_deref(),
            Some("sk-or-v1-secret")
        );
        assert_eq!(config.openai_compat_api_key, None, "wrong-slot key");
        assert_eq!(config.embedding_model(), "openai/text-embedding-3-small");
        assert_eq!(config.llm_model(), "openai/gpt-4o-mini");
        assert_eq!(config.openrouter_base_url, "http://127.0.0.1:9999");
    }

    #[test]
    fn openai_compat_config_carries_base_url_and_dimension() {
        let key = SecretKey::new("compat-secret".to_string());
        let config = build_provider_config(
            Provider::OpenAiCompat,
            Some(&key),
            &json!({
                "embedding_model": "mock-embed",
                "llm_model": "mock-llm",
                "openai_compat_base_url": "http://127.0.0.1:8088",
                "embedding_dimension": 1536,
            }),
        );
        assert_eq!(config.provider_type, ProviderType::OpenAICompat);
        assert_eq!(
            config.openai_compat_api_key.as_deref(),
            Some("compat-secret")
        );
        assert_eq!(config.openrouter_api_key, None, "wrong-slot key");
        assert_eq!(config.embedding_model(), "mock-embed");
        assert_eq!(config.llm_model(), "mock-llm");
        assert_eq!(config.openai_compat_base_url, "http://127.0.0.1:8088");
        assert_eq!(config.embedding_dimension(), 1536);
    }

    #[test]
    fn defaults_fill_everything_not_supplied() {
        let config = build_provider_config(Provider::OpenRouter, None, &json!({}));
        let core_defaults = ProviderConfig::from_settings(&HashMap::new());
        assert_eq!(
            config.openrouter_base_url,
            core_defaults.openrouter_base_url
        );
        assert_eq!(config.embedding_model(), core_defaults.embedding_model());
        // Empty strings are treated as absent, not as overrides.
        let config = build_provider_config(
            Provider::OpenRouter,
            None,
            &json!({ "embedding_model": "", "openrouter_base_url": "" }),
        );
        assert_eq!(config.embedding_model(), core_defaults.embedding_model());
        assert_eq!(
            config.openrouter_base_url,
            core_defaults.openrouter_base_url
        );
    }

    #[test]
    fn byok_vocabulary_accepts_documented_keys_only() {
        // Every documented key, well-typed: fine.
        assert_eq!(
            validate_byok_model_config(&json!({
                "embedding_model": "mock-embed",
                "llm_model": "any/model",
                "openrouter_base_url": "http://127.0.0.1:1",
                "openai_compat_base_url": "http://127.0.0.1:2",
                "embedding_dimension": 1536,
            })),
            Ok(())
        );
        assert_eq!(validate_byok_model_config(&json!({})), Ok(()));

        // The secret-hygiene case: a nested api_key must never reach the
        // plaintext column.
        let err = validate_byok_model_config(&json!({ "api_key": "sk-leak" })).unwrap_err();
        assert!(err.contains("api_key"), "{err}");
        assert!(
            !err.contains("sk-leak"),
            "rejection must not echo the value: {err}"
        );

        // Shape checks mirror the managed validator's.
        assert!(validate_byok_model_config(&json!("gpt")).is_err());
        assert!(validate_byok_model_config(&json!({ "llm_model": 42 })).is_err());
        assert!(
            validate_byok_model_config(&json!({ "embedding_dimension": "1536" })).is_err(),
            "dimension must be a number, not a string"
        );
        assert!(validate_byok_model_config(&json!({ "embedding_dimension": -5 })).is_err());
    }

    #[test]
    fn keyless_config_is_openrouter_with_no_key() {
        let config = keyless_provider_config();
        assert_eq!(config.provider_type, ProviderType::OpenRouter);
        assert_eq!(config.openrouter_api_key, None);
        assert_eq!(config.openai_compat_api_key, None);
    }
}
