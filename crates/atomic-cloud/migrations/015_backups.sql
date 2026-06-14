-- Backups & disaster recovery v1 (docs/plans/atomic-cloud.md, "Backups &
-- disaster recovery" → "v1: nightly logical dumps").
--
-- Two additions, both additive (see 001 for the discipline):
--
-- 1. Per-(account, db) backup status on account_databases — the freshness
--    signal the staleness monitor reads ("alert when any tenant's last
--    successful backup is >36h old") and the per-tenant error a failed dump
--    records. NULL last_backup_at means "never backed up yet" (a tenant
--    provisioned since the last nightly pass), which the monitor treats as
--    stale once the tenant is older than the alert horizon.
--
-- 2. A backup_runs ledger: one row per nightly/final pass, with the same
--    deploy/run shape as deploy_runs (009) — started/finished timestamps and
--    total/succeeded/failed counts — so operators get backup history and the
--    nightly job is observable across pods. `kind` is 'nightly' (the
--    fleet-wide pass) or 'final' (the single dump taken before an account
--    deletion). Counts are NULL until the run finishes.
--
-- Retention (14 daily + 8 weekly tenant dumps; 30 days for finals) is bucket
-- lifecycle policy, NOT rows here — this ledger is history, not a GC target.

ALTER TABLE account_databases
    ADD COLUMN IF NOT EXISTS last_backup_at    TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS last_backup_error TEXT;

CREATE TABLE IF NOT EXISTS backup_runs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,            -- 'nightly' | 'final'
    started_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at TIMESTAMPTZ,
    total       INTEGER,                  -- databases the pass attempted
    succeeded   INTEGER,
    failed      INTEGER
);

-- `backup status` / history reads the latest runs newest-first.
CREATE INDEX IF NOT EXISTS idx_backup_runs_started ON backup_runs(started_at DESC);

INSERT INTO schema_version (version) VALUES (15);
