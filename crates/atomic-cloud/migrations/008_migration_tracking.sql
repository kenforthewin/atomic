-- Per-tenant schema-migration tracking (docs/plans/atomic-cloud.md,
-- "Provisioning lifecycle" → "Schema migration on deploy").
--
-- The boot-time fleet migration runner enumerates account_databases rows
-- whose last_migrated_version lags the binary's compiled tenant schema
-- target, runs atomic-core's migration runner per tenant, and records the
-- outcome here. CloudAuth reads last_migrated_version per request — a
-- lagging tenant gets the structured 503 `account_upgrading` hold message
-- until the runner (or the reaper's failed-migrations arm) brings it
-- current.
--
-- last_migrated_version: the tenant schema version this tenant was last
-- successfully migrated to. 0 = never recorded (the backfill below stamps
-- every pre-existing row, so 0 only ever appears transiently mid-INSERT).
--
-- last_migrated_at: when that success was recorded.
--
-- migration_failed_at / last_migration_error: the most recent failed
-- attempt and its (bounded) error text; both cleared by the next success.
--
-- migration_retry_after / migration_retry_count: the reaper's backoff
-- state for failed migrations — it retries rows whose migration_retry_after
-- has passed and alerts when migration_retry_count exceeds its threshold.
--
-- Backfill: every row that exists before this migration was written by
-- provision_account, which runs the FULL tenant migration set (atomic-core's
-- `DatabaseManager::new_postgres` → `storage.initialize()`) before the row
-- is inserted — so every existing tenant is current with the binary that
-- provisioned it. The literal 22 below is atomic-core's compiled tenant
-- schema target at the time this migration was authored; tenant migration
-- 022 predates the first provision_account in this repo's history, so 22 is
-- provably at-or-below every existing tenant's true version. Stamping
-- at-or-below is the safe direction: if the compiled target has moved past
-- 22 by the time this applies, the boot runner sees these rows as lagging
-- and re-runs initialize() — an idempotent no-op for already-current
-- schemas — then records the true version. Stamping ABOVE a tenant's true
-- version would mask a straggler; 22 cannot be above, per the invariant.
-- (The frozen literal is pinned by a test against
-- PostgresStorage::target_schema_version(); see src/fleet_migration.rs.)
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

ALTER TABLE account_databases ADD COLUMN IF NOT EXISTS last_migrated_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE account_databases ADD COLUMN IF NOT EXISTS last_migrated_at TIMESTAMPTZ;
ALTER TABLE account_databases ADD COLUMN IF NOT EXISTS migration_failed_at TIMESTAMPTZ;
ALTER TABLE account_databases ADD COLUMN IF NOT EXISTS last_migration_error TEXT;
ALTER TABLE account_databases ADD COLUMN IF NOT EXISTS migration_retry_after TIMESTAMPTZ;
ALTER TABLE account_databases ADD COLUMN IF NOT EXISTS migration_retry_count INTEGER NOT NULL DEFAULT 0;

UPDATE account_databases
SET last_migrated_version = 22,
    last_migrated_at = NOW()
WHERE status = 'active' AND last_migrated_version = 0;

INSERT INTO schema_version (version) VALUES (8);
