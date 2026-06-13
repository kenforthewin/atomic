-- Migration 012 — free trials (plan: "Observability, quotas, billing" →
-- "Billing" → "Trials: 14 days of paid tier on signup, no card required.
-- Auto-downgrade to free after."; Decisions log 2026-05-25 "Trials: 14 days
-- of paid tier on signup, no card required. Auto-downgrade to free after.").
--
-- Additive-only (tests/migration_lint.rs): a single ADD COLUMN. No backfill
-- is required — every existing row is a non-trialing account, and a NULL
-- `trial_ends_at` is exactly that. The trial *state* rides the existing
-- `accounts.billing_state` column (migration 010), which gains a 'trialing'
-- value at the application level; no schema change is needed for it because
-- billing_state is free-text TEXT and CloudAuth/the dunning module decode it
-- (an unknown value already degrades to 'active', so an old binary that
-- doesn't know 'trialing' simply serves the account normally — the correct
-- conservative reading for a trial that is, by definition, full access).
--
-- `trial_ends_at` anchors the trial's auto-downgrade exactly the way
-- `past_due_since` anchors dunning: the dunning sweep (crate::billing::dunning)
-- reads it and, once it is in the past, drops the account to the free plan
-- and clears the trialing state (read_only if the now-free account is over
-- the free limits — over-limit data is RETAINED, never deleted). NULL when
-- the account is not on a trial (the default, and every pre-migration row).
ALTER TABLE accounts
    ADD COLUMN IF NOT EXISTS trial_ends_at TIMESTAMPTZ;

-- Partial index over the only rows the trial sweep scans: accounts still in
-- the trialing state with a deadline set. Keeps the periodic sweep's lookup
-- off a full table scan as the account count grows, and stays tiny (a trial
-- is a transient state — most accounts are never in it).
CREATE INDEX IF NOT EXISTS idx_accounts_trialing
    ON accounts (trial_ends_at)
    WHERE billing_state = 'trialing';

-- Record this migration in the version table (the runner reads MAX(version)).
INSERT INTO schema_version (version) VALUES (12);
