-- Migration 017 — launch plan values.
--
-- Pre-launch tuning of the seeded plan tiers from migration 010:
--   * free atom ceiling 100 -> 250 (100 is the first friction in a knowledge tool)
--   * pro managed-AI cap $20 -> $10/mo (2000 -> 1000 cents; margin-positive on a $12 plan)
--   * drop the undefined `frontier_models` flag (no frontier feature ships in v1)
--
-- Idempotent UPDATEs keyed by plan id, so any already-seeded control plane
-- (dev/staging) converges to the launch values.

UPDATE plans SET atom_limit = 250 WHERE id = 'free';

UPDATE plans
   SET ai_credits_monthly_cents = 1000,
       feature_flags = '{}'::jsonb
 WHERE id = 'pro';

-- Record this migration in the version table (the runner reads MAX(version)).
INSERT INTO schema_version (version) VALUES (17);
