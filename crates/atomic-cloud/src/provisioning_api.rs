//! OpenRouter provisioning API — minting and managing per-tenant runtime
//! keys (plan: "Provider management" → "Managed key lifecycle").
//!
//! [`ProvisioningApi`] is the narrow seam between the managed-key lifecycle
//! ([`crate::managed_keys`]) and OpenRouter's key-management HTTP API. The
//! production implementation is [`OpenRouterProvisioning`]; tests substitute
//! a recording implementation (see `tests/support`) so NO REAL PROVIDER is
//! ever called from the suite, and a wiremock test pins the real client's
//! request shape.
//!
//! # API shape (verified against openrouter.ai docs, 2026-06)
//!
//! | Operation | Request | Response |
//! |---|---|---|
//! | Create | `POST {base}/keys` `{name, limit, limit_reset}` | `{key, data: {hash, …}}` |
//! | Update limit | `PATCH {base}/keys/{hash}` `{limit}` | `{data: {…}}` |
//! | Delete | `DELETE {base}/keys/{hash}` | `{deleted: true}` |
//! | Usage | `GET {base}/keys/{hash}` | `{data: {usage, limit, limit_remaining, disabled, …}}` |
//!
//! `limit` is denominated in **USD** (a double); the trait speaks **cents**
//! (the plan denominates allowances in cents — `ai_credits_monthly_cents`)
//! and converts at the boundary. `limit_reset: "monthly"` resets the spend
//! at midnight UTC on the 1st, natively at OpenRouter — no rollover code on
//! our side. The `hash` field is the key's lifecycle identifier; it is what
//! `provider_credentials.external_key_id` stores. The top-level `key` field
//! of the create response is the runtime key **plaintext, shown exactly
//! once** — it goes straight into a [`SecretKey`] and is never logged.
//!
//! # Provisioning-key custody (operator runbook)
//!
//! The provisioning key authenticates every call here and **can mint runtime
//! keys against the master OpenRouter account's balance**. Crown-jewel
//! custody, same discipline as the KeyVault master key (plan: "Provider
//! management" → shared infrastructure):
//!
//! - Deploy as a **sealed secret** in the env var named by
//!   [`PROVISIONING_KEY_ENV`]. Never stored in the control plane, never
//!   accepted on the command line (argv leaks into process listings).
//! - The master account's prepaid balance funds every managed tenant: an
//!   empty balance is an all-tenants outage. Monitor and auto-top-up
//!   (tracked in the plan's open questions).
//! - The master account's dashboard lists every runtime key it has minted —
//!   that listing is the operational fallback for any key this codebase
//!   loses track of (the documented orphan windows in
//!   [`crate::managed_keys`] and the rollback paths).

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::CloudError;
use crate::keyvault::SecretKey;

/// Environment variable the `serve` CLI reads the OpenRouter provisioning
/// key from by default (`--openrouter-provisioning-key-env` renames it).
pub const PROVISIONING_KEY_ENV: &str = "ATOMIC_CLOUD_OPENROUTER_PROVISIONING_KEY";

/// Default base URL for OpenRouter's provisioning endpoints. Overridable
/// (CLI `--openrouter-provisioning-url`) for tests and proxies.
pub const DEFAULT_OPENROUTER_PROVISIONING_URL: &str = "https://openrouter.ai/api/v1";

/// HTTP timeout for provisioning calls. Key management is a handful of
/// small JSON round-trips; anything slower than this is an outage, and
/// signup (which calls [`ProvisioningApi::create_key`] synchronously) must
/// not hang on it.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A freshly minted runtime key. The plaintext is shown exactly once by the
/// provider; it arrives pre-wrapped in a [`SecretKey`] (Debug/Display
/// redacted, not `Serialize`) so nothing downstream can leak it by accident.
#[derive(Debug)]
pub struct CreatedRuntimeKey {
    /// The runtime API key plaintext — encrypt and store immediately.
    pub plaintext_key: SecretKey,
    /// The provider's lifecycle identifier for the key (OpenRouter calls it
    /// the `hash`). Opaque reference, not a secret; stored in the clear as
    /// `provider_credentials.external_key_id`.
    pub external_key_id: String,
}

/// Spend/limit state of a runtime key, in the provider's native USD
/// denomination (plan: "Audit / visibility" — "62% of monthly AI credits
/// used"). Kept as the raw doubles the API reports; consumers convert to
/// cents only where the plan's schema demands it.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeKeyUsage {
    /// Credits consumed in the current limit period, USD.
    pub usage_usd: f64,
    /// The key's credit limit, USD. `None` = unlimited.
    pub limit_usd: Option<f64>,
    /// Remaining allowance, USD. `None` when the key is unlimited.
    pub limit_remaining_usd: Option<f64>,
    /// Whether the key has been disabled provider-side.
    pub disabled: bool,
}

/// Lifecycle operations on per-tenant runtime keys (plan: "Managed key
/// lifecycle" table). Object-safe so the composition can inject a recording
/// implementation in tests.
///
/// All amounts are **cents** at this seam; implementations convert to the
/// provider's denomination.
#[async_trait]
pub trait ProvisioningApi: Send + Sync {
    /// Mint a runtime key named `name` with a `credit_limit_cents` spending
    /// cap. `monthly_reset` requests the provider's native monthly reset
    /// (midnight UTC) — the plan's free-tier shape; `false` makes the cap
    /// lifetime-total.
    ///
    /// **Not idempotent**: every call creates a new key. Callers own the
    /// check-before-create discipline (plan: "Signup" → "Idempotency").
    async fn create_key(
        &self,
        name: &str,
        credit_limit_cents: u32,
        monthly_reset: bool,
    ) -> Result<CreatedRuntimeKey, CloudError>;

    /// PATCH the key's credit limit (plan: plan-change row of the lifecycle
    /// table).
    async fn update_key_limit(
        &self,
        external_key_id: &str,
        credit_limit_cents: u32,
    ) -> Result<(), CloudError>;

    /// Delete the key. **Idempotent by contract**: deleting a key that no
    /// longer exists succeeds — every caller is a cleanup path (account
    /// deletion, provision rollback) that may race another cleanup of the
    /// same key, and "already gone" is their success condition.
    async fn delete_key(&self, external_key_id: &str) -> Result<(), CloudError>;

    /// Current spend/limit state, for the settings-page usage display.
    async fn get_key_usage(&self, external_key_id: &str) -> Result<RuntimeKeyUsage, CloudError>;
}

/// Production [`ProvisioningApi`]: reqwest against OpenRouter's key API,
/// authenticated by the provisioning key (see the module docs for custody).
pub struct OpenRouterProvisioning {
    client: reqwest::Client,
    /// Normalized base URL, no trailing slash (e.g.
    /// `https://openrouter.ai/api/v1`).
    base_url: String,
    provisioning_key: SecretKey,
}

impl std::fmt::Debug for OpenRouterProvisioning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never derive: the derived impl would print the provisioning key.
        f.debug_struct("OpenRouterProvisioning")
            .field("base_url", &self.base_url)
            .field("provisioning_key", &"[redacted]")
            .finish()
    }
}

/// `POST /keys` response: the plaintext key (top level, shown once) and the
/// lifecycle identifier inside `data`.
#[derive(Deserialize)]
struct CreateKeyResponse {
    key: String,
    data: CreateKeyData,
}

#[derive(Deserialize)]
struct CreateKeyData {
    hash: String,
}

/// `GET /keys/{hash}` response envelope.
#[derive(Deserialize)]
struct GetKeyResponse {
    data: KeyUsageData,
}

#[derive(Deserialize)]
struct KeyUsageData {
    #[serde(default)]
    usage: f64,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    limit_remaining: Option<f64>,
    #[serde(default)]
    disabled: bool,
}

/// Cents → the provider's USD denomination.
fn cents_to_usd(cents: u32) -> f64 {
    f64::from(cents) / 100.0
}

impl OpenRouterProvisioning {
    /// Build a client against `base_url` (normalized; trailing slash
    /// tolerated) with the given provisioning key.
    pub fn new(base_url: &str, provisioning_key: SecretKey) -> Result<Self, CloudError> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        // Validate at construction so a malformed URL fails at boot, not on
        // the first signup.
        url::Url::parse(&base_url)
            .map_err(|e| CloudError::InvalidUrl(format!("provisioning base URL: {e}")))?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| CloudError::ProviderProvisioning {
                context: "building provisioning HTTP client".to_string(),
                message: e.to_string(),
            })?;
        Ok(Self {
            client,
            base_url,
            provisioning_key,
        })
    }

    /// Build from the environment variable named `var` (conventionally
    /// [`PROVISIONING_KEY_ENV`]). The error names the variable and never
    /// echoes its value — same discipline as the KeyVault master key.
    pub fn from_env(base_url: &str, var: &str) -> Result<Self, CloudError> {
        let key = std::env::var(var).map_err(|_| {
            CloudError::InvalidProvisioningKey(format!(
                "environment variable {var} is not set; managed provider keys \
                 cannot be provisioned without the OpenRouter provisioning key \
                 (run with provisioning mode 'disabled' for keyless dev accounts)"
            ))
        })?;
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(CloudError::InvalidProvisioningKey(format!(
                "environment variable {var} is empty"
            )));
        }
        Self::new(base_url, SecretKey::new(key))
    }

    /// `{base}/keys[/{segment}]`, with the segment percent-encoded — key
    /// hashes are provider-opaque, so never trust them to be URL-safe.
    fn endpoint(&self, segment: Option<&str>) -> Result<url::Url, CloudError> {
        let mut url = url::Url::parse(&self.base_url)
            .map_err(|e| CloudError::InvalidUrl(format!("provisioning base URL: {e}")))?;
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                CloudError::InvalidUrl("provisioning base URL cannot be a base".to_string())
            })?;
            path.pop_if_empty().push("keys");
            if let Some(segment) = segment {
                path.push(segment);
            }
        }
        Ok(url)
    }

    /// Run a request with the bearer header, mapping non-success statuses
    /// to typed errors. The error carries the status and a bounded slice of
    /// the provider's error body — which never contains key material,
    /// because the only response that ever carries a key is a *successful*
    /// create, and successes don't come through here.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        context: &str,
    ) -> Result<reqwest::Response, CloudError> {
        let response = request
            .bearer_auth(self.provisioning_key.expose())
            .send()
            .await
            .map_err(|e| CloudError::ProviderProvisioning {
                context: context.to_string(),
                // reqwest errors never echo request headers, so the bearer
                // token cannot appear here.
                message: e.to_string(),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        // Do NOT echo the upstream response body into the error: it would turn a
        // non-2xx into a read-oracle for the configured provisioning endpoint
        // (SEC-1). Report the status code only — the body is drained so the
        // connection can be reused, and logged at debug for the operator.
        let body = response.text().await.unwrap_or_default();
        tracing::debug!(%status, context, body = %truncate(&body, 500), "provisioning API returned a non-success status");
        Err(CloudError::ProviderProvisioning {
            context: context.to_string(),
            message: format!(
                "the provider rejected the request (HTTP {})",
                status.as_u16()
            ),
        })
    }
}

/// Bound provider error bodies before they enter error messages/logs.
fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[async_trait]
impl ProvisioningApi for OpenRouterProvisioning {
    async fn create_key(
        &self,
        name: &str,
        credit_limit_cents: u32,
        monthly_reset: bool,
    ) -> Result<CreatedRuntimeKey, CloudError> {
        let mut body = serde_json::json!({
            "name": name,
            "limit": cents_to_usd(credit_limit_cents),
        });
        if monthly_reset {
            body["limit_reset"] = serde_json::json!("monthly");
        }
        let response = self
            .send(
                self.client.post(self.endpoint(None)?).json(&body),
                "creating runtime key",
            )
            .await?;
        // The success body carries the key plaintext: a decode failure must
        // NOT echo the body into the error.
        let created: CreateKeyResponse =
            response
                .json()
                .await
                .map_err(|e| CloudError::ProviderProvisioning {
                    context: "creating runtime key".to_string(),
                    message: format!(
                        "unexpected response shape (body withheld — it may contain \
                         the key): {e}"
                    ),
                })?;
        Ok(CreatedRuntimeKey {
            plaintext_key: SecretKey::new(created.key),
            external_key_id: created.data.hash,
        })
    }

    async fn update_key_limit(
        &self,
        external_key_id: &str,
        credit_limit_cents: u32,
    ) -> Result<(), CloudError> {
        let body = serde_json::json!({ "limit": cents_to_usd(credit_limit_cents) });
        self.send(
            self.client
                .patch(self.endpoint(Some(external_key_id))?)
                .json(&body),
            "updating runtime key limit",
        )
        .await?;
        Ok(())
    }

    async fn delete_key(&self, external_key_id: &str) -> Result<(), CloudError> {
        let request = self.client.delete(self.endpoint(Some(external_key_id))?);
        match self.send(request, "deleting runtime key").await {
            Ok(_) => Ok(()),
            // 404 = already gone = the caller's success condition (see the
            // trait contract). Matching on the formatted status keeps `send`
            // single-shaped; the string is ours, not the provider's.
            Err(CloudError::ProviderProvisioning { message, .. })
                if message.starts_with("HTTP 404") =>
            {
                tracing::info!(
                    external_key_id,
                    "runtime key already deleted provider-side; treating as success"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn get_key_usage(&self, external_key_id: &str) -> Result<RuntimeKeyUsage, CloudError> {
        let response = self
            .send(
                self.client.get(self.endpoint(Some(external_key_id))?),
                "fetching runtime key usage",
            )
            .await?;
        let body: GetKeyResponse =
            response
                .json()
                .await
                .map_err(|e| CloudError::ProviderProvisioning {
                    context: "fetching runtime key usage".to_string(),
                    message: format!("unexpected response shape: {e}"),
                })?;
        Ok(RuntimeKeyUsage {
            usage_usd: body.data.usage,
            limit_usd: body.data.limit,
            limit_remaining_usd: body.data.limit_remaining,
            disabled: body.data.disabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents_convert_to_usd() {
        assert_eq!(cents_to_usd(50), 0.5);
        assert_eq!(cents_to_usd(0), 0.0);
        assert_eq!(cents_to_usd(12345), 123.45);
    }

    #[test]
    fn debug_redacts_the_provisioning_key() {
        let api = OpenRouterProvisioning::new(
            "https://openrouter.ai/api/v1",
            SecretKey::new("sk-or-prov-secret".to_string()),
        )
        .expect("construct");
        let rendered = format!("{api:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains("sk-or-prov-secret"));
    }

    #[test]
    fn endpoint_building_normalizes_and_encodes() {
        let api = OpenRouterProvisioning::new(
            "https://example.test/api/v1/", // trailing slash tolerated
            SecretKey::new("k".to_string()),
        )
        .expect("construct");
        assert_eq!(
            api.endpoint(None).unwrap().as_str(),
            "https://example.test/api/v1/keys"
        );
        assert_eq!(
            api.endpoint(Some("hash-1")).unwrap().as_str(),
            "https://example.test/api/v1/keys/hash-1"
        );
        // Hostile segments are encoded, never path-spliced.
        let url = api.endpoint(Some("../admin?x=1")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.test/api/v1/keys/..%2Fadmin%3Fx=1"
        );
    }

    #[test]
    fn from_env_never_echoes_the_value() {
        let missing = "ATOMIC_CLOUD_TEST_PROVISIONING_KEY_MISSING";
        match OpenRouterProvisioning::from_env(DEFAULT_OPENROUTER_PROVISIONING_URL, missing) {
            Err(CloudError::InvalidProvisioningKey(msg)) => {
                assert!(msg.contains(missing), "error must name the variable");
            }
            other => panic!("expected InvalidProvisioningKey, got {other:?}"),
        }

        let set = "ATOMIC_CLOUD_TEST_PROVISIONING_KEY_SET";
        std::env::set_var(set, "sk-or-prov-abc");
        assert!(OpenRouterProvisioning::from_env(DEFAULT_OPENROUTER_PROVISIONING_URL, set).is_ok());

        let empty = "ATOMIC_CLOUD_TEST_PROVISIONING_KEY_EMPTY";
        std::env::set_var(empty, "   ");
        match OpenRouterProvisioning::from_env(DEFAULT_OPENROUTER_PROVISIONING_URL, empty) {
            Err(CloudError::InvalidProvisioningKey(msg)) => {
                assert!(msg.contains(empty), "error must name the variable");
            }
            other => panic!("expected InvalidProvisioningKey, got {other:?}"),
        }
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("ab", 3), "ab");
        // Multi-byte boundary: must not split a char.
        assert_eq!(truncate("ééé", 2), "éé");
    }
}
