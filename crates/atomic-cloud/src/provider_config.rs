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
//! | `llm_model` | the **agentic** LLM — wiki, chat, reports |
//! | `tagging_model` | the **utility** LLM — single-shot tagging (managed: platform-owned) |
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
//! `llm_model` is the account's **agentic** selection (wiki, chat, reports)
//! and `tagging_model` its **utility** selection (single-shot tagging), landing
//! in the two distinct `ProviderConfig` OpenRouter slots. In explicit mode
//! atomic-core pins the per-task settings keys to those slots
//! (`ProviderConfig::apply_to_settings`: `wiki_model`/`chat_model` ← the
//! agentic model, `tagging_model` ← the utility model), so a tenant settings
//! write can never reroute either — which is what makes managed curation
//! ([`crate::curated_models`]) govern every LLM consumer. This is the split
//! that lets agent loops run on an agent-capable model while tagging stays on
//! a cheap one; managed rows seed `tagging_model` platform-owned, so a user
//! only ever chooses the agentic model.

use std::collections::HashMap;

use atomic_core::{ProviderConfig, ProviderType};
use serde_json::Value;
use url::Url;

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
            // `llm_model` is the agentic model (wiki/chat/reports).
            if let Some(model) = string_field("llm_model") {
                config.openrouter_agentic_model = model;
            }
            // `tagging_model` is the platform-owned utility model (single-shot
            // tagging). Absent (e.g. a BYOK row that never set it) leaves the
            // ProviderConfig default in the utility slot.
            if let Some(model) = string_field("tagging_model") {
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

/// The `model_config` keys whose values are tenant-supplied base URLs that
/// drive outbound requests — first at save-time validation, then on every
/// live pipeline call. These must clear [`validate_byok_base_url`] before
/// they can land (SSRF hardening; see [`validate_byok_model_config`]).
const BYOK_BASE_URL_KEYS: &[&str] = &["openrouter_base_url", "openai_compat_base_url"];

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
///
/// It is also the **SSRF gate**: the base-URL keys drive outbound requests,
/// first at save-time validation and then on every live pipeline call, so a
/// tenant could otherwise point them at internal addresses on shared infra
/// (cloud metadata, the control-plane Postgres, east-west services). Every
/// base URL must clear [`validate_byok_base_url`] — https scheme, host not
/// in a private/loopback/link-local/metadata range — before it can land.
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
        } else if let Some(text) = value.as_str() {
            if BYOK_BASE_URL_KEYS.contains(&key.as_str()) {
                validate_byok_base_url(text)
                    .map_err(|reason| format!("model_config.{key} {reason}"))?;
            }
        } else {
            return Err(format!("model_config.{key} must be a string"));
        }
    }

    Ok(())
}

/// SSRF gate for a tenant-supplied provider base URL (the OpenRouter /
/// OpenAI-compatible API base). The same string is fetched at save time and
/// on every live pipeline call, so an unrestricted value would let any
/// authenticated tenant aim our outbound client at internal addresses on
/// shared infrastructure.
///
/// Cloud closes this with a strict **host allowlist**: the URL must be `https`
/// to a known managed-provider host ([`is_allowed_provider_host`] — OpenRouter
/// or OpenAI). Because an arbitrary host can never match, there is nothing to
/// rebind: a public IP, an internal name, or a name that later resolves to a
/// private address is rejected up front, so no resolve-and-pin / egress-proxy
/// plumbing is needed. Custom / self-hosted OpenAI-compatible endpoints are a
/// desktop / self-hosted capability, not a cloud one; the dev/test escape
/// ([`ALLOW_PRIVATE_PROVIDER_URLS_ENV`]) reopens the gate for local mocks.
///
/// The error string is a fixed *reason* — it never echoes upstream response
/// bytes — so it cannot become a read oracle.
pub fn validate_byok_base_url(raw: &str) -> Result<(), &'static str> {
    let url = Url::parse(raw).map_err(|_| "must be a valid URL")?;

    // Dev/test escape hatch: when this env var is set, allow non-https and
    // private/loopback hosts so local mock providers (the integration suite)
    // and a developer's local Ollama / OpenAI-compatible server are reachable.
    // It reopens the SSRF surface this gate closes, so it is NEVER set in
    // production — `serve` warns loudly at boot if it finds it set. (We still
    // require the value to be a parseable URL above.)
    if private_provider_urls_allowed() {
        return Ok(());
    }

    if url.scheme() != "https" {
        return Err("must use the https scheme");
    }

    // Cloud restricts BYOK base URLs to KNOWN provider hosts (OpenRouter /
    // OpenAI). An allowlist is the tightest SSRF closure: an arbitrary
    // tenant-supplied host — a public IP, an internal name, or a name that
    // later rebinds to a private address — simply can't match, so nothing
    // outside the two providers is ever fetched. Custom / self-hosted
    // OpenAI-compatible endpoints are a desktop / self-hosted capability, not
    // a cloud one.
    if is_allowed_provider_host(url.host_str().unwrap_or_default()) {
        Ok(())
    } else {
        Err("must be an OpenRouter or OpenAI endpoint; custom provider hosts aren't supported on cloud")
    }
}

/// The env var that disables the BYOK base-URL SSRF gate
/// ([`validate_byok_base_url`]). Dev/test only — see that function's docs.
pub const ALLOW_PRIVATE_PROVIDER_URLS_ENV: &str = "ATOMIC_CLOUD_ALLOW_PRIVATE_PROVIDER_URLS";

/// Whether the SSRF gate is disabled via [`ALLOW_PRIVATE_PROVIDER_URLS_ENV`]
/// (set to `1` or `true`). Read live so tests can toggle it per process.
pub fn private_provider_urls_allowed() -> bool {
    std::env::var(ALLOW_PRIVATE_PROVIDER_URLS_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Hosts a cloud BYOK base URL may target — the two managed providers, exact
/// apex or a subdomain, and nothing else (see [`validate_byok_base_url`]).
fn is_allowed_provider_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "openrouter.ai"
        || host == "api.openai.com"
        || host.ends_with(".openrouter.ai")
        || host.ends_with(".openai.com")
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
                "llm_model": "anthropic/claude-haiku-4.5",
                "tagging_model": "openai/gpt-5-nano",
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
        // The split: `llm_model` is the agentic model, `tagging_model` the
        // utility model — they land in distinct slots, never collapsed.
        assert_eq!(config.agentic_model(), "anthropic/claude-haiku-4.5");
        assert_eq!(config.llm_model(), "openai/gpt-5-nano");
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
        // Every documented key, well-typed: fine. Base URLs are known
        // provider hosts (the SSRF allowlist is exercised separately below).
        assert_eq!(
            validate_byok_model_config(&json!({
                "embedding_model": "mock-embed",
                "llm_model": "any/model",
                "openrouter_base_url": "https://openrouter.ai/api/v1",
                "openai_compat_base_url": "https://api.openai.com/v1",
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

    #[test]
    fn base_url_gate_accepts_only_known_provider_hosts() {
        // The two managed providers (apex + subdomain) pass.
        assert!(validate_byok_base_url("https://openrouter.ai/api/v1").is_ok());
        assert!(validate_byok_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_byok_base_url("https://gateway.openrouter.ai/v1").is_ok());
        // Any other public host — even a legitimate one — is rejected on cloud:
        // custom endpoints are the SSRF surface the allowlist closes.
        assert!(validate_byok_base_url("https://api.example.com:8443/v1").is_err());
        assert!(validate_byok_base_url("https://8.8.8.8/v1").is_err());
        // Lookalike suffixes must not slip past the allowlist.
        assert!(validate_byok_base_url("https://openrouter.ai.evil.com/v1").is_err());
    }

    #[test]
    fn base_url_gate_requires_https() {
        assert_eq!(
            validate_byok_base_url("http://openrouter.ai/api/v1"),
            Err("must use the https scheme")
        );
        // Non-HTTP schemes that could reach other services are rejected too.
        assert!(validate_byok_base_url("file:///etc/passwd").is_err());
        assert!(validate_byok_base_url("gopher://10.0.0.1/").is_err());
    }

    #[test]
    fn base_url_gate_blocks_private_and_metadata_ipv4() {
        for raw in [
            "https://127.0.0.1/v1",       // loopback
            "https://10.1.2.3/v1",        // 10/8
            "https://172.16.5.6/v1",      // 172.16/12
            "https://192.168.0.1/v1",     // 192.168/16
            "https://169.254.1.1/v1",     // link-local
            "https://169.254.169.254/v1", // cloud metadata
            "https://0.0.0.0/v1",         // unspecified
        ] {
            assert!(
                validate_byok_base_url(raw).is_err(),
                "expected {raw} to be rejected"
            );
        }
    }

    #[test]
    fn base_url_gate_blocks_private_ipv6() {
        for raw in [
            "https://[::1]/v1",                    // loopback
            "https://[::]/v1",                     // unspecified
            "https://[fc00::1]/v1",                // unique-local
            "https://[fd12:3456::1]/v1",           // unique-local
            "https://[fe80::1]/v1",                // link-local
            "https://[::ffff:127.0.0.1]/v1",       // v4-mapped loopback
            "https://[::ffff:169.254.169.254]/v1", // v4-mapped metadata
        ] {
            assert!(
                validate_byok_base_url(raw).is_err(),
                "expected {raw} to be rejected"
            );
        }
        // A public IPv6 literal is not a known provider host → rejected.
        assert!(validate_byok_base_url("https://[2606:4700::1111]/v1").is_err());
    }

    #[test]
    fn base_url_gate_rejects_missing_host_and_garbage() {
        assert!(validate_byok_base_url("https://").is_err());
        assert!(validate_byok_base_url("not a url").is_err());
        assert!(validate_byok_base_url("").is_err());
    }

    #[test]
    fn byok_validation_runs_the_ssrf_gate_on_base_urls() {
        // A non-provider (here, metadata-IP) base URL is rejected, and the
        // message names the field without echoing any upstream bytes.
        let err = validate_byok_model_config(&json!({
            "openrouter_base_url": "https://169.254.169.254/v1",
        }))
        .unwrap_err();
        assert!(err.contains("openrouter_base_url"), "{err}");
        assert!(err.contains("OpenRouter or OpenAI"), "{err}");

        let err = validate_byok_model_config(&json!({
            "openai_compat_base_url": "http://internal.svc/v1",
        }))
        .unwrap_err();
        assert!(err.contains("openai_compat_base_url"), "{err}");

        // Known provider hosts pass.
        assert!(validate_byok_model_config(&json!({
            "openrouter_base_url": "https://openrouter.ai/api/v1",
            "openai_compat_base_url": "https://api.openai.com/v1",
        }))
        .is_ok());
    }
}
