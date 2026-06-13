-- Per-tenant provider circuit breaker (docs/plans/atomic-cloud.md,
-- "Worker fairness & job queue" → "Provider rate-limit handling";
-- "Provider management" → "Managed key lifecycle" / "Blocked states").
--
-- provider_paused_until: while in the future, the dispatcher skips this
-- tenant wholesale — its ledger work sits durably until the pause lapses.
-- Written by the breaker (crate::backpressure) on repeated rate-limit
-- failures or on a credit-exhaustion (402) failure; cleared by every
-- provider mutation (crate::provider_credentials — a new key deserves a
-- fresh chance, plan: live rotation step 6).
--
-- provider_pause_kind: why the tenant is paused — 'rate_limit' (background
-- dispatch only; interactive routes unaffected) or 'credits' (background
-- dispatch paused AND the AI-interactive routes return the structured
-- out_of_ai_credits error). NULL whenever provider_paused_until is NULL;
-- enforced in code, not by constraint (additive-only discipline).
--
-- provider_pause_streak: consecutive rate-limit trips, the breaker's
-- cooldown-doubling state (60s, doubling per trip, capped). Reset to 0 by a
-- healthy run and by provider mutations. Credits pauses don't touch it.
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

ALTER TABLE accounts ADD COLUMN IF NOT EXISTS provider_paused_until TIMESTAMPTZ;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS provider_pause_kind TEXT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS provider_pause_streak INTEGER NOT NULL DEFAULT 0;

INSERT INTO schema_version (version) VALUES (7);
