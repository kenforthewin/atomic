//! Managed provider-key lifecycle (plan: "Provider management" → "Managed
//! key lifecycle"; signup step 9; deletion step 3; decision 2026-06-09).
//!
//! Every account gets a platform-provisioned OpenRouter runtime key at
//! signup, minted via the provisioning API ([`crate::provisioning_api`])
//! with the plan's monthly credit allowance and native monthly reset,
//! encrypted via the [`KeyVault`], and stored as the account's
//! `(openrouter, managed)` credentials row. [`ManagedKeys`] is the handle
//! the provisioning/deletion/reaper paths thread through; its `Disabled`
//! mode (the dev default when no provisioning key is configured) skips the
//! lifecycle entirely — accounts provision with no credentials row and run
//! in the keyless "missing key" state.
//!
//! # The orphan window, and why the row insert is immediate
//!
//! `create_key` is the one non-idempotent provisioning step: every call
//! mints a new key billed against the master account. The credentials row
//! is what makes the key findable by every later step (resume, rotation,
//! deletion, rollback), so it is inserted **immediately** after the create
//! returns — the milliseconds between the provider's create and our insert
//! are the only window in which a process crash strands a key nothing in
//! the control plane references. Accepted residue, by design: the
//! operational fallback is the master OpenRouter account's dashboard, which
//! lists every runtime key it has minted (see the custody runbook in
//! [`crate::provisioning_api`]). An insert *failure* (as opposed to a
//! crash) closes its own window: the just-created key is deleted
//! best-effort before the error propagates. And the insert is
//! **conditional, never an upsert** — two concurrent resumes that both
//! mint a key race to one row; the loser detects the conflict, deletes its
//! own freshly minted key (loudly), and proceeds on the winner's row
//! instead of silently overwriting the winner's `external_key_id`.
//!
//! # Best-effort deletion, everywhere
//!
//! Every key-deleting path — account deletion step 3, the stuck-provision
//! rollback, the 23503 orphan cleanup, interrupted-deletion recovery — is
//! **best-effort with loud logging**: a provider outage must never wedge an
//! account deletion or a rollback (plan: deletion is hard-delete v1; a
//! deletion blocked on a third party is a worse failure than a leaked
//! $0.50-capped key). The trade-off is real residue: once the credentials
//! row is gone (the accounts-row CASCADE sweeps it), nothing in the control
//! plane can re-derive the external key id, so the reaper cannot retry a
//! failed delete. The loud `tracing::error!` plus the master-account
//! listing is the recovery path.

use std::sync::Arc;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::keyvault::KeyVault;
use crate::provider_credentials::{
    get_credentials, insert_credentials_if_absent, set_active_provider, CredentialOrigin,
    NewCredentials, Provider,
};
use crate::provisioning_api::ProvisioningApi;

/// Default monthly credit allowance for managed keys, in cents — the plan's
/// free-tier placeholder ($0.50/mo; decisions log 2026-06-09). Product-
/// tunable via the serve CLI; the billing slice derives it from
/// `plans.ai_credits_monthly_cents` instead.
pub const DEFAULT_MONTHLY_ALLOWANCE_CENTS: u32 = 50;

/// Shape of a managed key at creation time.
#[derive(Debug, Clone)]
pub struct ManagedKeyConfig {
    /// Per-key credit limit, cents, with native monthly reset — the hard
    /// stop OpenRouter enforces (plan: AI-spend enforcement is delegated to
    /// per-key credit limits).
    pub monthly_allowance_cents: u32,
    /// `model_config` seeded on the managed credentials row. Managed mode
    /// pins the embedding model fleet-wide and curates the LLM list
    /// (decisions log 2026-06-09); this default carries the pinned ids.
    pub model_config: serde_json::Value,
}

impl Default for ManagedKeyConfig {
    fn default() -> Self {
        Self {
            monthly_allowance_cents: DEFAULT_MONTHLY_ALLOWANCE_CENTS,
            model_config: default_managed_model_config(),
        }
    }
}

/// The fleet-wide managed model selection: the pinned embedding model and
/// the curated list's default LLM (see [`crate::curated_models`] — switching
/// the embedding model invalidates every stored vector and triggers a full
/// re-embed billed to the platform, so user writes to a managed row's
/// `model_config` are curation-checked by the provider routes).
pub fn default_managed_model_config() -> serde_json::Value {
    serde_json::json!({
        "embedding_model": crate::curated_models::MANAGED_EMBEDDING_MODEL,
        // The agentic model (wiki/chat/reports) — user-selectable within the
        // plan's tier; seeded to the free-tier default.
        "llm_model": crate::curated_models::DEFAULT_AGENTIC_MODEL,
        // The tagging model — platform-owned and fixed. Seeded here (not
        // user-writable), preserved across model writes by
        // `merge_managed_model_config`, and read back by
        // `build_provider_config` into the utility-model slot.
        "tagging_model": crate::curated_models::MANAGED_TAGGING_MODEL,
    })
}

/// The managed-key lifecycle handle threaded through provisioning,
/// deletion, and the reaper. Cheap to clone.
#[derive(Clone)]
pub enum ManagedKeys {
    /// No managed keys (the config/CLI default for dev, where no
    /// provisioning key is set): signup step 9 is skipped entirely and
    /// accounts run with no credentials row. Key-deleting paths log loudly
    /// if they encounter managed residue they cannot delete.
    Disabled,
    /// Provision a managed key for every new account.
    Enabled {
        api: Arc<dyn ProvisioningApi>,
        /// Encrypts the runtime key at rest ([`crate::keyvault`]).
        vault: Arc<dyn KeyVault>,
        config: ManagedKeyConfig,
    },
}

impl std::fmt::Debug for ManagedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagedKeys::Disabled => f.write_str("ManagedKeys::Disabled"),
            ManagedKeys::Enabled { config, .. } => f
                .debug_struct("ManagedKeys::Enabled")
                .field("config", config)
                .finish_non_exhaustive(),
        }
    }
}

/// The key's display name on the master OpenRouter account — the dashboard
/// is the operational fallback for orphaned keys, so the name must identify
/// the tenant. Keyed on the account UUID (stable), not the subdomain
/// (renameable).
fn managed_key_name(account_id: &str) -> String {
    format!("atomic-cloud/{account_id}")
}

impl ManagedKeys {
    /// Signup step 9: ensure the account has a managed OpenRouter key,
    /// idempotently. Returns the key's `external_key_id` when one exists or
    /// was created; `None` in `Disabled` mode (or for a pre-managed-keys
    /// row missing its id).
    ///
    /// Idempotent resume (plan: "Signup" → "Idempotency"): an existing
    /// `(openrouter, managed)` credentials row short-circuits — key
    /// creation itself is not idempotent, and the row is the record that a
    /// previous run already paid for one. The check reads only
    /// `external_key_id` (no decrypt), so a resume under a rotated master
    /// key can still complete. On the resume path the active-provider
    /// pointer is re-asserted, healing a crash that landed between the row
    /// insert and the pointer flip.
    ///
    /// `allowance_cents` is the per-key monthly credit cap to mint the key
    /// with — the provisioning caller resolves it from the account's plan
    /// (`plans.ai_credits_monthly_cents`; free=50, pro=2000 per migration
    /// 010) so the key is sized to the tier the account is actually on, not
    /// the fleet-wide fallback. `None` falls back to the configured
    /// [`ManagedKeyConfig::monthly_allowance_cents`] (the `--managed-key-
    /// allowance-cents` flag's default), the legacy behavior for callers
    /// that can't resolve a plan.
    pub(crate) async fn ensure_managed_key(
        &self,
        control: &ControlPlane,
        account_id: &str,
        allowance_cents: Option<u32>,
    ) -> Result<Option<String>, CloudError> {
        let ManagedKeys::Enabled { api, vault, config } = self else {
            return Ok(None);
        };
        let allowance_cents = allowance_cents.unwrap_or(config.monthly_allowance_cents);

        let existing: Option<Option<String>> = sqlx::query_scalar(
            "SELECT external_key_id FROM provider_credentials \
             WHERE account_id = $1 AND provider = $2 AND origin = $3",
        )
        .bind(account_id)
        .bind(Provider::OpenRouter.as_str())
        .bind(CredentialOrigin::Managed.as_str())
        .fetch_optional(control.pool())
        .await
        .map_err(CloudError::db("checking for an existing managed key"))?;
        if let Some(external_key_id) = existing {
            // Resume: the key exists; only the pointer flip might be
            // missing. Re-asserting it is an idempotent UPDATE — no
            // competing writer can exist while the account is still
            // `'provisioning'` (BYOK entry requires an active account).
            set_active_provider(
                control,
                account_id,
                Some((Provider::OpenRouter, CredentialOrigin::Managed)),
            )
            .await?;
            if external_key_id.is_none() {
                // Managed rows always carry the id; tolerate the gap (the
                // row may predate this invariant) but make it visible.
                tracing::warn!(
                    account_id,
                    "managed credentials row has no external_key_id; \
                     its runtime key cannot be lifecycle-managed"
                );
            }
            return Ok(external_key_id);
        }

        let created = api
            .create_key(
                &managed_key_name(account_id),
                allowance_cents,
                /* monthly_reset */ true,
            )
            .await?;
        let external_key_id = created.external_key_id.clone();

        // Insert IMMEDIATELY — the row is what every later step finds the
        // key by; the gap between the create above and this insert is the
        // crash-orphan window documented in the module docs. The insert is
        // conditional (`ON CONFLICT DO NOTHING`), never an upsert: two
        // concurrent resumes can both pass the existence check above and
        // both mint a key, and an upsert would silently overwrite the
        // winner's `external_key_id` with the loser's — orphaning the
        // winner's billed key with zero trace. The loser detects the lost
        // race instead, deletes the key *it* just minted (loudly), and
        // proceeds on the winner's row.
        let inserted = insert_credentials_if_absent(
            control,
            vault.as_ref(),
            account_id,
            NewCredentials {
                provider: Provider::OpenRouter,
                origin: CredentialOrigin::Managed,
                api_key: created.plaintext_key,
                external_key_id: Some(external_key_id.clone()),
                model_config: config.model_config.clone(),
            },
        )
        .await;
        let inserted = match inserted {
            Ok(inserted) => inserted,
            Err(e) => {
                // The insert failed (concurrent deletion cascaded the
                // accounts row away, control plane hiccup): without a row,
                // the key just minted would be unreferenced forever. Delete
                // it best-effort before surfacing the error — the provision
                // is failing either way, and the account (if it still
                // exists) stays `'provisioning'` for the reaper.
                self.delete_external_key_best_effort(account_id, &external_key_id)
                    .await;
                return Err(e);
            }
        };
        if !inserted {
            // Lost the double-mint race: a concurrent resume's row landed
            // between our existence check and our insert. Dispose of the
            // key this call minted — loudly, it was billed — and serve the
            // winner's.
            tracing::warn!(
                account_id,
                external_key_id,
                "concurrent managed-key provisioning detected; deleting the \
                 runtime key this call minted and using the winner's row"
            );
            self.delete_external_key_best_effort(account_id, &external_key_id)
                .await;
            let winner: Option<Option<String>> = sqlx::query_scalar(
                "SELECT external_key_id FROM provider_credentials \
                 WHERE account_id = $1 AND provider = $2 AND origin = $3",
            )
            .bind(account_id)
            .bind(Provider::OpenRouter.as_str())
            .bind(CredentialOrigin::Managed.as_str())
            .fetch_optional(control.pool())
            .await
            .map_err(CloudError::db("reading winning managed-key row"))?;
            let Some(winner_key_id) = winner else {
                // The winner's row vanished again (concurrent deletion).
                // Same shape as the insert-error path: the account, if it
                // still exists, stays 'provisioning' for the reaper.
                return Err(CloudError::Invariant(format!(
                    "managed credentials row for account {account_id} \
                     disappeared after losing the provisioning race"
                )));
            };
            set_active_provider(
                control,
                account_id,
                Some((Provider::OpenRouter, CredentialOrigin::Managed)),
            )
            .await?;
            return Ok(winner_key_id);
        }

        // Make the fresh key the account's active provider config. A crash
        // between the insert and this flip is healed by the resume path
        // above; an error here leaves the account `'provisioning'` and
        // reapable, with the row already in place for the retry.
        set_active_provider(
            control,
            account_id,
            Some((Provider::OpenRouter, CredentialOrigin::Managed)),
        )
        .await?;

        tracing::info!(
            account_id,
            external_key_id,
            "provisioned managed runtime key"
        );
        Ok(Some(external_key_id))
    }

    /// The `external_key_id`s of an account's managed credentials rows —
    /// read these **before** any step that deletes the accounts row (the
    /// CASCADE sweeps the credentials rows, and with them the only stored
    /// reference to the keys).
    pub(crate) async fn managed_key_ids(
        &self,
        control: &ControlPlane,
        account_id: &str,
    ) -> Result<Vec<String>, CloudError> {
        sqlx::query_scalar(
            "SELECT external_key_id FROM provider_credentials \
             WHERE account_id = $1 AND origin = $2 AND external_key_id IS NOT NULL",
        )
        .bind(account_id)
        .bind(CredentialOrigin::Managed.as_str())
        .fetch_all(control.pool())
        .await
        .map_err(CloudError::db("listing managed provider keys"))
    }

    /// Deletion step 3 (and the shared rollback primitive): delete the
    /// account's managed runtime keys via the provisioning API,
    /// best-effort. Errors only when the control-plane *read* fails —
    /// provider-side failures are logged loudly and swallowed, because the
    /// callers are deletions and rollbacks that must not wedge on a
    /// provider outage (module docs: "Best-effort deletion").
    pub(crate) async fn delete_managed_keys_for_account(
        &self,
        control: &ControlPlane,
        account_id: &str,
    ) -> Result<(), CloudError> {
        let ids = self.managed_key_ids(control, account_id).await?;
        for external_key_id in &ids {
            self.delete_external_key_best_effort(account_id, external_key_id)
                .await;
        }
        Ok(())
    }

    /// Re-size the account's managed runtime key(s) to `allowance_cents`,
    /// best-effort (plan: plan-change row of the lifecycle table). Called on
    /// every plan transition so the per-key OpenRouter spend cap tracks the
    /// account's current tier (`plans.ai_credits_monthly_cents`) — without
    /// this the key stays pinned at its signup allowance and a paid/trial
    /// account's AI dies once it spends the free tier's $0.50.
    ///
    /// **Best-effort, never fatal.** A plan transition is a billing-state
    /// change that has already committed by the time this runs; a provider
    /// outage on the PATCH must not roll it back or wedge the caller (a
    /// webhook, a trial sweep, a signup). A failure is a loud
    /// `tracing::error!` and the next transition (or an operator) reconciles
    /// it. `Disabled` mode and accounts with no managed key are silent no-ops
    /// (keyless/BYOK accounts have no managed cap to move). Errors only when
    /// the control-plane *read* of the key id fails.
    pub(crate) async fn reconcile_key_limit(
        &self,
        control: &ControlPlane,
        account_id: &str,
        allowance_cents: u32,
    ) -> Result<(), CloudError> {
        let ManagedKeys::Enabled { api, .. } = self else {
            return Ok(());
        };
        for external_key_id in self.managed_key_ids(control, account_id).await? {
            match api
                .update_key_limit(&external_key_id, allowance_cents)
                .await
            {
                Ok(()) => tracing::info!(
                    account_id,
                    external_key_id,
                    allowance_cents,
                    "reconciled managed runtime key limit to plan allowance"
                ),
                Err(e) => tracing::error!(
                    account_id,
                    external_key_id,
                    allowance_cents,
                    error = %e,
                    "failed to reconcile managed runtime key limit; the plan \
                     transition stands and the next transition (or an operator) \
                     will reconcile it"
                ),
            }
        }
        Ok(())
    }

    /// Best-effort delete of one runtime key by its provider id, for paths
    /// holding the id locally after the credentials row is already gone
    /// (the 23503 cleanup, the failed-insert cleanup, post-CASCADE
    /// rollback). Never fails; failures — including `Disabled`-mode residue
    /// from a mode flip — are loud `tracing::error!`s naming the id and the
    /// operational fallback.
    pub(crate) async fn delete_external_key_best_effort(
        &self,
        account_id: &str,
        external_key_id: &str,
    ) {
        match self {
            ManagedKeys::Disabled => {
                tracing::error!(
                    account_id,
                    external_key_id,
                    "managed runtime key cannot be deleted: provisioning is \
                     disabled in this process; delete it manually via the \
                     master OpenRouter account's key listing"
                );
            }
            ManagedKeys::Enabled { api, .. } => match api.delete_key(external_key_id).await {
                Ok(()) => {
                    tracing::info!(account_id, external_key_id, "deleted managed runtime key");
                }
                Err(e) => {
                    tracing::error!(
                        account_id,
                        external_key_id,
                        error = %e,
                        "failed to delete managed runtime key; the deletion \
                         proceeds (must not wedge on a provider outage) — \
                         delete it manually via the master OpenRouter \
                         account's key listing"
                    );
                }
            },
        }
    }

    /// Decrypted managed credentials for an account, when both the row and
    /// `Enabled` mode exist. Exposed for status surfaces; `Disabled` mode
    /// reads nothing (it holds no vault).
    pub async fn managed_credentials(
        &self,
        control: &ControlPlane,
        account_id: &str,
    ) -> Result<Option<crate::provider_credentials::ProviderCredentials>, CloudError> {
        let ManagedKeys::Enabled { vault, .. } = self else {
            return Ok(None);
        };
        get_credentials(
            control,
            vault.as_ref(),
            account_id,
            Provider::OpenRouter,
            CredentialOrigin::Managed,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_name_is_account_keyed() {
        assert_eq!(
            managed_key_name("0e1a2b3c-4d5e-6f70-8192-a3b4c5d6e7f8"),
            "atomic-cloud/0e1a2b3c-4d5e-6f70-8192-a3b4c5d6e7f8"
        );
    }

    #[test]
    fn default_config_matches_the_plan_placeholder() {
        let config = ManagedKeyConfig::default();
        assert_eq!(config.monthly_allowance_cents, 50, "$0.50/mo free tier");
        assert_eq!(
            config.model_config["embedding_model"],
            serde_json::json!("openai/text-embedding-3-small"),
            "pinned fleet-wide embedding model"
        );
    }

    #[test]
    fn debug_shows_mode_without_internals() {
        assert_eq!(
            format!("{:?}", ManagedKeys::Disabled),
            "ManagedKeys::Disabled"
        );
    }
}
