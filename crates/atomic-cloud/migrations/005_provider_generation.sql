-- Provider-config generation counter (docs/plans/atomic-cloud.md,
-- "Provider management" → "Live rotation").
--
-- Every mutation of an account's provider state — credential upsert/delete,
-- active-pointer flip, model-config write — bumps this counter in the same
-- statement or transaction as the mutation itself. The serving layer
-- (AccountCache) records the generation each cached entry's ProviderConfig
-- was built from, and CloudAuth's per-request account lookup carries the
-- current value, so any pod whose in-memory config lags the control plane
-- detects the mismatch on the next request and refreshes from storage.
-- This is what makes rotation convergence bounded: an in-place live swap is
-- still the fast path on the pod that handled the write, but other pods —
-- and a cache rebuild that raced the write — heal on their next request
-- instead of serving the old key until eviction.
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

ALTER TABLE accounts ADD COLUMN IF NOT EXISTS provider_generation BIGINT NOT NULL DEFAULT 0;

INSERT INTO schema_version (version) VALUES (5);
