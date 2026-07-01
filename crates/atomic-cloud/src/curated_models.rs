//! Curated model policy for managed provider keys (plan: "Provider
//! management" → "Model curation"; decision 2026-06-09).
//!
//! Managed keys spend the platform's money, so the platform picks the
//! models:
//!
//! - **The embedding model is pinned fleet-wide** ([`MANAGED_EMBEDDING_MODEL`]).
//!   Switching embedding models invalidates every stored vector and forces a
//!   full re-embed billed to the platform, so it is not user-changeable at
//!   all in managed mode.
//! - **Tagging is a fixed cheap utility model** ([`MANAGED_TAGGING_MODEL`]) —
//!   single-shot structured output, seeded platform-owned, not user-selectable.
//! - **Wiki/chat/reports run on an agentic model the user picks**, from a
//!   plan-gated list ([`agentic_models_for_plan`]): the free tier gets
//!   [`FREE_AGENTIC_MODELS`], premium plans unlock [`PRO_AGENTIC_MODELS`] via
//!   the `premium_models` feature flag on `plans.feature_flags`.
//!
//! [`validate_managed_model_config`] is the enforcement point for **user
//! writes** to a managed row's `model_config` (the provider-settings routes
//! in [`crate::tenant_plane`]). Platform-side writes — the seed config in
//! [`crate::managed_keys::ManagedKeyConfig`] — are deliberately not run
//! through it: the composition may legitimately point managed keys at a
//! proxy base URL (tests, gateways), which a *user* must never be able to do
//! (redirecting the platform-funded key to an attacker-controlled endpoint
//! would exfiltrate it; see the key-shaped rule below).
//!
//! BYOK rows are exempt from all of this — their key, their bill, any model
//! they like. The one BYOK guardrail is a loud re-embed warning when an
//! embedding-model change is saved (plan text), produced by the routes, not
//! here.

use serde_json::{Map, Value};

/// The fleet-wide pinned embedding model for managed keys. Matches
/// atomic-core's OpenRouter default so settings-mode (self-hosted) and
/// explicit-mode (cloud) deployments embed identically, and matches the
/// [`PINNED_EMBEDDING_DIMENSION`]-wide column the tenant schema is
/// reconciled to.
pub const MANAGED_EMBEDDING_MODEL: &str = "qwen/qwen3-embedding-8b";

/// The platform-pinned embedding dimension — the width of every tenant's
/// vector column, fixed at provision time and **not changeable in cloud**
/// (v1). [`MANAGED_EMBEDDING_MODEL`] produces vectors of exactly this
/// width.
///
/// This pin governs BYOK as well as managed configs: tenant settings writes
/// are inert for embedding-space keys in explicit mode (atomic-core), so no
/// cloud mechanism exists that could recreate a tenant's vector index at a
/// different width — accepting a config whose effective dimension differs
/// would wedge the account (every embed fails against the mismatched
/// column). The provider routes therefore *reject* such configs with a
/// structured `embedding_dimension_unsupported` error instead of storing
/// them with an unfulfillable re-embed warning. Revisit alongside a real
/// dimension-migration story.
pub const PINNED_EMBEDDING_DIMENSION: usize = 1024;

/// The fleet-wide **tagging** model for managed keys — single-shot structured
/// output, no agent loop. Platform-fixed (not user-selectable): it is seeded
/// as a platform-owned key and every account tags on the same cheap utility
/// model, so tagging cost is predictable and users never have to think about
/// it. Wiki, chat, and reports run on the account's *agentic* model instead
/// (the split atomic-core's `ProviderConfig` preserves).
pub const MANAGED_TAGGING_MODEL: &str = "openai/gpt-5-nano";

/// The **agentic** LLM list for the free tier (wiki, chat, reports). These run
/// multi-step tool loops, so every entry must be agent-capable — the nano
/// utility tier belongs on tagging, never here. Kept cheap enough that the
/// free monthly allowance still buys real usage. The first entry is the signup
/// default ([`crate::managed_keys::default_managed_model_config`]).
pub const FREE_AGENTIC_MODELS: &[&str] = &["openai/gpt-5-mini", "google/gemini-2.5-flash"];

/// The **agentic** LLM list for premium (paid) plans: the free set plus the
/// higher-quality options paid tiers unlock. Gated by the plan's
/// `premium_models` feature flag (see [`agentic_models_for_plan`]).
pub const PRO_AGENTIC_MODELS: &[&str] = &[
    "openai/gpt-5-mini",
    "google/gemini-2.5-flash",
    "anthropic/claude-haiku-4.5",
    "deepseek/deepseek-v3.2",
];

/// The agentic model list a plan may pick from. Premium plans (the
/// `premium_models` feature flag) get [`PRO_AGENTIC_MODELS`]; everyone else
/// gets [`FREE_AGENTIC_MODELS`]. The signup default —
/// `FREE_AGENTIC_MODELS[0]` — is on both, so a downgrade never strands an
/// account on a model it can no longer select.
pub fn agentic_models_for_plan(premium: bool) -> &'static [&'static str] {
    if premium {
        PRO_AGENTIC_MODELS
    } else {
        FREE_AGENTIC_MODELS
    }
}

/// The signup-default agentic model (first free-tier entry). Also the value
/// [`crate::managed_keys::default_managed_model_config`] seeds as `llm_model`.
pub const DEFAULT_AGENTIC_MODEL: &str = FREE_AGENTIC_MODELS[0];

/// The `model_config` keys a **user** may write on a managed row — the
/// model selections, and nothing else. This is the user-writable half of
/// the managed config split; everything outside it is **platform-owned**:
/// seeded at provision time and only ever written platform-side — most
/// importantly the base-URL override keys (`openrouter_base_url` /
/// `openai_compat_base_url`, see [`crate::provider_config`]): a
/// user-supplied base URL on a managed key would route the platform-funded
/// credential to an arbitrary endpoint.
///
/// Reads as well as writes respect the split: [`validate_managed_model_config`]
/// rejects user writes outside this list, and
/// [`merge_managed_model_config`] preserves the platform-owned keys when a
/// validated write lands.
const MANAGED_ALLOWED_KEYS: &[&str] = &["embedding_model", "llm_model"];

/// Validate a user-submitted `model_config` for a managed credentials row.
///
/// Returns `Err(message)` describing the first violation — the message is
/// written for the 400 response body of the models route, naming the pinned
/// embedding model or the curated list so the caller can self-correct. The
/// rules, in check order:
///
/// 1. `model_config` must be a JSON object.
/// 2. Only [`MANAGED_ALLOWED_KEYS`] may appear (module docs: the base-URL
///    exfiltration rule).
/// 3. Present values must be strings.
/// 4. `embedding_model`, when present, must equal
///    [`MANAGED_EMBEDDING_MODEL`] (absent is fine — the config builder's
///    default *is* the pinned model).
/// 5. `llm_model` (the agentic model), when present, must be on the plan's
///    list ([`agentic_models_for_plan`]) — `premium` selects free vs. paid.
pub fn validate_managed_model_config(model_config: &Value, premium: bool) -> Result<(), String> {
    let Some(object) = model_config.as_object() else {
        return Err("model_config must be a JSON object".to_string());
    };

    for (key, value) in object {
        if !MANAGED_ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "model_config key {key:?} is not configurable on the managed provider; \
                 allowed keys: {MANAGED_ALLOWED_KEYS:?}. Bring your own key to configure \
                 providers freely."
            ));
        }
        if !value.is_string() {
            return Err(format!("model_config.{key} must be a string"));
        }
    }

    if let Some(embedding) = object.get("embedding_model").and_then(Value::as_str) {
        if embedding != MANAGED_EMBEDDING_MODEL {
            return Err(format!(
                "the managed embedding model is pinned to {MANAGED_EMBEDDING_MODEL:?} \
                 (changing it would invalidate every stored embedding); bring your own \
                 key to use a different embedding model"
            ));
        }
    }

    if let Some(llm) = object.get("llm_model").and_then(Value::as_str) {
        let allowed = agentic_models_for_plan(premium);
        if !allowed.contains(&llm) {
            return Err(format!(
                "{llm:?} is not available on your plan; choose one of {allowed:?}, \
                 upgrade your plan, or bring your own key to use other models"
            ));
        }
    }

    Ok(())
}

/// Merge a validated user write over a managed row's stored `model_config`,
/// preserving the platform-owned keys (module docs:
/// [`MANAGED_ALLOWED_KEYS`] is the user-writable set; everything else is
/// platform-owned).
///
/// Curation rejects unknown keys, so a user can never *resubmit* a
/// platform-seeded key like `openrouter_base_url` — a wholesale replace
/// would silently drop it, rerouting a proxy deployment's managed traffic
/// to the real endpoint. The merge keeps every stored key and overlays only
/// what the user submitted; callers must run
/// [`validate_managed_model_config`] on `submitted` first, which guarantees
/// the overlay touches user-writable keys only.
pub fn merge_managed_model_config(stored: &Value, submitted: &Value) -> Value {
    let mut merged: Map<String, Value> = stored.as_object().cloned().unwrap_or_default();
    if let Some(submitted) = submitted.as_object() {
        for (key, value) in submitted {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_managed_config_passes_curation() {
        // The signup seed's user-writable choice (the agentic llm_model) must
        // pass free-tier curation. The seed also carries platform-owned keys
        // (tagging_model, and possibly a base-URL override) that user-validate
        // deliberately rejects, so validate the user-writable part — that's the
        // only part a user could ever resubmit.
        let seed = crate::managed_keys::default_managed_model_config();
        assert_eq!(
            validate_managed_model_config(&json!({ "llm_model": DEFAULT_AGENTIC_MODEL }), false),
            Ok(())
        );
        assert_eq!(seed["embedding_model"], json!(MANAGED_EMBEDDING_MODEL));
        assert_eq!(seed["llm_model"], json!(DEFAULT_AGENTIC_MODEL));
        // Tagging is platform-owned and fixed — seeded, not user-selectable.
        assert_eq!(seed["tagging_model"], json!(MANAGED_TAGGING_MODEL));
    }

    #[test]
    fn pinned_model_produces_the_pinned_dimension() {
        // The two pins must agree: the seeded managed config's effective
        // embedding dimension is the width tenant columns are created at.
        let seed = crate::managed_keys::default_managed_model_config();
        let config = crate::provider_config::build_provider_config(
            crate::provider_credentials::Provider::OpenRouter,
            None,
            &seed,
        );
        assert_eq!(config.embedding_dimension(), PINNED_EMBEDDING_DIMENSION);
    }

    #[test]
    fn every_curated_agentic_choice_passes_its_tier() {
        // Premium plans may pick any pro model; free plans any free model.
        for model in PRO_AGENTIC_MODELS {
            let config = json!({ "llm_model": model });
            assert_eq!(
                validate_managed_model_config(&config, true),
                Ok(()),
                "pro {model}"
            );
        }
        for model in FREE_AGENTIC_MODELS {
            let config = json!({ "llm_model": model });
            assert_eq!(
                validate_managed_model_config(&config, false),
                Ok(()),
                "free {model}"
            );
        }
        // Partial and empty objects are fine — absent keys fall back to
        // defaults that are themselves curated.
        assert_eq!(validate_managed_model_config(&json!({}), false), Ok(()));
    }

    #[test]
    fn premium_only_model_is_gated_by_tier() {
        // A model on the pro list but not the free list: rejected for free,
        // accepted for premium. `deepseek/deepseek-v3.2` is exactly that.
        let premium_only = PRO_AGENTIC_MODELS
            .iter()
            .find(|m| !FREE_AGENTIC_MODELS.contains(m))
            .expect("pro list must add at least one model over free");
        let config = json!({ "llm_model": premium_only });
        assert_eq!(validate_managed_model_config(&config, true), Ok(()));
        let err = validate_managed_model_config(&config, false).unwrap_err();
        assert!(err.contains("not available on your plan"), "{err}");
    }

    #[test]
    fn uncurated_llm_is_rejected_on_every_tier() {
        let config = json!({ "llm_model": "openai/o1-pro" });
        for premium in [false, true] {
            let err = validate_managed_model_config(&config, premium).unwrap_err();
            assert!(err.contains("not available on your plan"), "{err}");
        }
    }

    #[test]
    fn embedding_model_is_pinned() {
        let ok = json!({ "embedding_model": MANAGED_EMBEDDING_MODEL });
        assert_eq!(validate_managed_model_config(&ok, false), Ok(()));

        let other = json!({ "embedding_model": "openai/text-embedding-3-large" });
        let err = validate_managed_model_config(&other, false).unwrap_err();
        assert!(err.contains("pinned"), "{err}");
    }

    #[test]
    fn tagging_model_is_not_user_selectable() {
        // Tagging is platform-owned: a user write naming it is rejected as
        // not-configurable (it isn't in MANAGED_ALLOWED_KEYS), the same guard
        // that blocks base-URL overrides.
        let config = json!({ "tagging_model": "openai/gpt-5-mini" });
        let err = validate_managed_model_config(&config, true).unwrap_err();
        assert!(err.contains("not configurable"), "{err}");
    }

    #[test]
    fn base_url_overrides_are_rejected() {
        // The exfiltration rule: a user must never point the managed key at
        // their own endpoint.
        for key in ["openrouter_base_url", "openai_compat_base_url"] {
            let config = json!({ key: "https://attacker.example/api/v1" });
            let err = validate_managed_model_config(&config, false).unwrap_err();
            assert!(err.contains("not configurable"), "{key}: {err}");
        }
    }

    #[test]
    fn non_object_and_non_string_values_are_rejected() {
        assert!(validate_managed_model_config(&json!("gpt"), false).is_err());
        assert!(validate_managed_model_config(&json!(["a"]), false).is_err());
        let err = validate_managed_model_config(&json!({ "llm_model": 42 }), false).unwrap_err();
        assert!(err.contains("must be a string"), "{err}");
    }

    #[test]
    fn merge_preserves_platform_owned_keys() {
        // A user picks a different curated agentic LLM. The platform-seeded
        // base-URL override and the fixed tagging model (both curation forbids
        // them from resubmitting) must survive, and the untouched embedding
        // model stays put.
        let stored = json!({
            "embedding_model": MANAGED_EMBEDDING_MODEL,
            "llm_model": FREE_AGENTIC_MODELS[0],
            "tagging_model": MANAGED_TAGGING_MODEL,
            "openrouter_base_url": "http://proxy.internal/api/v1",
        });
        let submitted = json!({ "llm_model": FREE_AGENTIC_MODELS[1] });
        let merged = merge_managed_model_config(&stored, &submitted);
        assert_eq!(
            merged,
            json!({
                "embedding_model": MANAGED_EMBEDDING_MODEL,
                "llm_model": FREE_AGENTIC_MODELS[1],
                "tagging_model": MANAGED_TAGGING_MODEL,
                "openrouter_base_url": "http://proxy.internal/api/v1",
            })
        );
    }

    #[test]
    fn merge_tolerates_degenerate_shapes() {
        // Empty submission: a no-op. Non-object stored config (never written
        // by us, but the column is JSONB): treated as empty rather than
        // panicking, so the submission alone defines the result.
        let stored = json!({ "llm_model": FREE_AGENTIC_MODELS[0] });
        assert_eq!(merge_managed_model_config(&stored, &json!({})), stored);
        assert_eq!(
            merge_managed_model_config(
                &Value::Null,
                &json!({ "llm_model": PRO_AGENTIC_MODELS[2] })
            ),
            json!({ "llm_model": PRO_AGENTIC_MODELS[2] })
        );
    }
}
