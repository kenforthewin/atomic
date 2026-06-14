-- Backup-run status + per-tenant last-attempt timestamp
-- (docs/plans/atomic-cloud.md, "Backups & disaster recovery";
-- adversarial-review issues 6 and 5).
--
-- 1. backup_runs.status — a pod killed mid-pass leaves its row with
--    finished_at = NULL forever, so `backup status` shows a perpetually
--    in-flight pass. Mirroring deploy_runs' lifecycle (009), an explicit status
--    lets a startup/status-time finalizer mark a stale in-flight row
--    'abandoned' (dead pod) — distinct from a row still genuinely running.
--    Existing rows read NULL, treated as "running iff finished_at IS NULL" —
--    identical to pre-016 behavior, so an old binary stays correct.
--
-- 2. account_databases.last_backup_attempt_at — the nightly pass orders tenants
--    "stale-first" so a capped pass reaches the most-overdue. Ordering by
--    last_backup_at ASC NULLS FIRST alone let a tenant whose dump keeps FAILING
--    (last_backup_at never stamped) sort first EVERY pass and, under a small
--    cap, starve healthy-but-due tenants forever. Stamping the attempt time on
--    every pass (success OR failure) lets the pass order by the most recent
--    *attempt*, so a just-failed tenant yields to a healthy-but-due one until
--    its turn comes round again. A never-attempted tenant still has both NULL
--    and correctly sorts first.
--
-- Both additive (see 001): nullable columns, no default, no constraint.

ALTER TABLE backup_runs
    ADD COLUMN IF NOT EXISTS status TEXT;   -- NULL | 'running' | 'completed' | 'abandoned'

ALTER TABLE account_databases
    ADD COLUMN IF NOT EXISTS last_backup_attempt_at TIMESTAMPTZ;

INSERT INTO schema_version (version) VALUES (16);
