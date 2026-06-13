//! Plan registry and plan-tier resource limits (plan: "Observability,
//! quotas, billing" → "Quotas" → "Plan-tier resource limits").
//!
//! A [`Plan`] is one row of the seeded `plans` table: a name, a monthly
//! price, and the resource ceilings (`atom_limit`, `kb_limit`,
//! `storage_bytes_limit`) plus the managed-key AI allowance and a
//! `feature_flags` bag. The plan catalogue changes rarely — new tiers are a
//! migration, not a runtime write — so [`PlanRegistry`] loads every plan
//! into memory once and refreshes on demand rather than querying the control
//! plane per request. The hot path (the quota guard) does a `HashMap` lookup,
//! never a round-trip.
//!
//! # Which limit is read live, which is stored
//!
//! Resource enforcement reads the *current* count straight from the tenant
//! database at enforcement time:
//!
//! - **atoms** — `AtomicCore::count_atoms()` against the request's resolved
//!   knowledge base.
//! - **knowledge bases** — `DatabaseManager::list_databases()` length.
//!
//! Both are cheap, single-statement reads and strongly consistent — the
//! count a `POST` would push over the limit is the count that exists the
//! instant before it runs, with no skew from a separately-maintained
//! counter. The `quota_usage` table (crate::quota_usage) is reserved for
//! metrics that *aren't* cheaply countable live (storage bytes, daily
//! rollups) and for the **advisory** AI-credits counter — never gated on,
//! because OpenRouter enforces the real AI limit per managed key (plan:
//! "AI spend is the one quota we do not enforce ourselves").
//!
//! # NULL = unlimited
//!
//! A `NULL` `atom_limit` / `kb_limit` means the plan is unlimited on that
//! axis and the guard never blocks it. The free tier is finite on both; the
//! paid placeholder tier is unlimited on both.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;

/// The default plan stamped on a new account when nothing else applies
/// (plan: free-tier default). Provisioning always stamps `free`; signup
/// completion then promotes the account to the paid trial tier via
/// [`start_trial`](crate::billing::dunning::start_trial), and the trial
/// auto-downgrade returns it here when the 14 days lapse.
pub const DEFAULT_PLAN_ID: &str = "free";

/// One plan-tier row. `atom_limit` / `kb_limit` / `storage_bytes_limit` are
/// `None` when the column is `NULL` = unlimited.
#[derive(Debug, Clone)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub monthly_price_cents: i32,
    /// `None` = unlimited atoms.
    pub atom_limit: Option<i32>,
    /// Managed-key monthly AI allowance in cents. Advisory in cloud —
    /// OpenRouter enforces it on the per-account key.
    pub ai_credits_monthly_cents: i32,
    /// `None` = unlimited knowledge bases.
    pub kb_limit: Option<i32>,
    /// `None` = unlimited storage. Advisory (reaper recompute), never gated
    /// at request time.
    pub storage_bytes_limit: Option<i64>,
    pub feature_flags: serde_json::Value,
}

impl Plan {
    /// Whether a feature flag is set truthy in `feature_flags` (e.g.
    /// `frontier_models`). Absent or non-`true` reads as `false`.
    pub fn feature_enabled(&self, flag: &str) -> bool {
        self.feature_flags
            .get(flag)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

/// The raw column tuple a `plans` row decodes to.
type PlanRow = (
    String,
    String,
    i32,
    Option<i32>,
    i32,
    Option<i32>,
    Option<i64>,
    serde_json::Value,
);

/// In-memory plan catalogue, refreshable from the control plane. Cheap to
/// share behind an `Arc`; the `RwLock` is taken for a microsecond on lookup
/// (read) and only on the rare refresh (write).
pub struct PlanRegistry {
    control: ControlPlane,
    plans: RwLock<HashMap<String, Plan>>,
}

impl PlanRegistry {
    /// Build the registry and eagerly load every plan, so a misconfigured
    /// catalogue (no rows — a migration that didn't seed) surfaces at boot
    /// rather than on the first quota check.
    pub async fn load(control: ControlPlane) -> Result<Self, CloudError> {
        let registry = Self {
            control,
            plans: RwLock::new(HashMap::new()),
        };
        registry.refresh().await?;
        Ok(registry)
    }

    /// Re-read the `plans` table into memory. Called at boot and whenever an
    /// operator changes the catalogue (rare; there is no runtime write path
    /// in this slice — a migration seeds plans, this refreshes after a
    /// deploy). Replaces the map wholesale under the write lock.
    pub async fn refresh(&self) -> Result<(), CloudError> {
        let rows: Vec<PlanRow> = sqlx::query_as(
            "SELECT id, name, monthly_price_cents, atom_limit, ai_credits_monthly_cents, \
                    kb_limit, storage_bytes_limit, feature_flags \
             FROM plans",
        )
        .fetch_all(self.control.pool())
        .await
        .map_err(CloudError::db("loading plans"))?;

        let map: HashMap<String, Plan> = rows
            .into_iter()
            .map(|row| {
                let (
                    id,
                    name,
                    monthly_price_cents,
                    atom_limit,
                    ai_credits_monthly_cents,
                    kb_limit,
                    storage_bytes_limit,
                    feature_flags,
                ) = row;
                (
                    id.clone(),
                    Plan {
                        id,
                        name,
                        monthly_price_cents,
                        atom_limit,
                        ai_credits_monthly_cents,
                        kb_limit,
                        storage_bytes_limit,
                        feature_flags,
                    },
                )
            })
            .collect();

        if map.is_empty() {
            return Err(CloudError::Invariant(
                "plans table is empty; migration 010 seeds 'free' and 'pro'".to_string(),
            ));
        }
        *self.plans.write().expect("plan registry lock poisoned") = map;
        Ok(())
    }

    /// The plan with `id`, cloned out of the cache. `None` for an unknown id.
    pub fn get(&self, id: &str) -> Option<Plan> {
        self.plans
            .read()
            .expect("plan registry lock poisoned")
            .get(id)
            .cloned()
    }

    /// The plan an account is on, resolving its `plan_id` (the live FK) and
    /// falling back to [`DEFAULT_PLAN_ID`] if the column is `NULL` (a row
    /// written before migration 010's backfill, or by an old binary). A
    /// resolved `plan_id` that names no cached plan is a fail-closed
    /// invariant error rather than a silent unlimited grant.
    pub async fn for_account(&self, account_id: &str) -> Result<Plan, CloudError> {
        let plan_id: Option<String> =
            sqlx::query_scalar("SELECT plan_id FROM accounts WHERE id = $1")
                .bind(account_id)
                .fetch_optional(self.control.pool())
                .await
                .map_err(CloudError::db("reading account plan_id"))?
                .flatten();
        let plan_id = plan_id.unwrap_or_else(|| DEFAULT_PLAN_ID.to_string());
        self.get(&plan_id).ok_or_else(|| {
            CloudError::Invariant(format!(
                "account {account_id} references unknown plan {plan_id:?}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flag_reads_truthy_only() {
        let plan = Plan {
            id: "pro".into(),
            name: "Pro".into(),
            monthly_price_cents: 1200,
            atom_limit: None,
            ai_credits_monthly_cents: 2000,
            kb_limit: None,
            storage_bytes_limit: Some(10_737_418_240),
            feature_flags: serde_json::json!({ "frontier_models": true, "off": false }),
        };
        assert!(plan.feature_enabled("frontier_models"));
        assert!(!plan.feature_enabled("off"));
        assert!(!plan.feature_enabled("absent"));
    }
}
