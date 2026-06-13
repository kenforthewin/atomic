-- Deploy-run history for fleet migration at boot (docs/plans/atomic-cloud.md,
-- "Provisioning lifecycle" → "Schema migration on deploy", steps 4-5).
--
-- Every pod boot inserts one row before its fleet migration run and finishes
-- it with the run's counts and the failure-rate policy's verdict, so
-- operators get per-boot history (`atomic-cloud deploy status`) and the
-- awaiting-review acknowledgment has a durable home all pods can see
-- (`atomic-cloud deploy advance`).
--
-- target_version: the tenant schema version the booting binary compiles
-- (PostgresStorage::target_schema_version()), recorded so history stays
-- meaningful across deploys and `deploy advance` can scope its
-- acknowledgment to runs of the version actually under review.
--
-- total / migrated / failed: the run's tenant counts — enumerated lagging
-- tenants, successful migrations, recorded failures. NULL until the run
-- finishes; tenants neither migrated nor failed when a run times out were
-- simply never attempted (total - migrated - failed).
--
-- deploy_status lifecycle (see crate::deploy):
--   'migrating'          inserted at boot; a row stuck here is a pod that
--                        died (or is still running) mid-fleet-migration
--   'ready'              failure rate below the ready threshold; readiness
--                        flipped without operator action
--   'awaiting_review'    1% ≤ rate < 10%: pod holds not-ready until an
--                        operator runs `deploy advance` (→ 'advanced') or
--                        redeploys
--   'rollback_required'  rate ≥ 10%: the migration itself is broken; no
--                        advance override exists — roll the binary back
--   'migration_timeout'  the run exceeded its wall-clock limit
--   'advanced'           an operator acknowledged an awaiting_review run;
--                        every pod holding on it flips ready on its next
--                        readiness probe
--   'abandoned'          a 'migrating' row stale past the run timeout was
--                        finalized by a later boot or `deploy status` (the
--                        pod died mid-run); terminal history that cannot
--                        shadow `deploy advance`. Additive vocabulary
--                        (TEXT column), added post-009 without a migration.
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

CREATE TABLE IF NOT EXISTS deploy_runs (
    id              TEXT PRIMARY KEY,
    target_version  INTEGER NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at     TIMESTAMPTZ,
    total           INTEGER,
    migrated        INTEGER,
    failed          INTEGER,
    deploy_status   TEXT NOT NULL DEFAULT 'migrating',
    advanced_at     TIMESTAMPTZ
);

-- `deploy status` reads the latest run; `deploy advance` scans a target
-- version's awaiting_review rows.
CREATE INDEX IF NOT EXISTS idx_deploy_runs_started ON deploy_runs(started_at);
CREATE INDEX IF NOT EXISTS idx_deploy_runs_target ON deploy_runs(target_version, deploy_status);

INSERT INTO schema_version (version) VALUES (9);
