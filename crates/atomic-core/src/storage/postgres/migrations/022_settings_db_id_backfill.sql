-- Migration 022: backfill per-DB-role settings orphaned in '_global' by 021.
--
-- 021 landed every pre-existing settings row in the '_global' tier. That
-- is correct for registry-role config (provider, models), but per-DB-role
-- keys — task.{id}.* scheduler state, reports.* seed flags, the dashboard
-- featured-report pointer — became orphans: scoped reads no longer see
-- them. On an upgraded deployment that is NOT benign:
--
--   * `reports.default_briefing_seeded` going invisible makes
--     `seed_default_briefing_report` re-seed at boot, creating a DUPLICATE
--     Daily Briefing report and resurrecting seeds the user deleted.
--   * Operator overrides (`task.{id}.enabled = false`,
--     `task.task_runs_gc.retain_*`) silently revert to defaults —
--     disabled tasks re-enable, retention loosens.
--
-- Replicating the orphans into EVERY logical database is the faithful
-- interpretation of the pre-021 state: with one shared, unscoped table,
-- these keys observably applied to all logical DBs at once. Copying them
-- to each `databases` row preserves that observable behavior exactly — no
-- re-seeds, no re-enabled tasks, no loosened retention — and from here on
-- each DB's copy evolves independently, as 021 intended.
--
-- Statement-level idempotence: the INSERT skips conflicts and the DELETE
-- finds nothing on a re-run, so a partially-applied run is safe to
-- re-execute.

INSERT INTO settings (db_id, key, value)
SELECT d.id, s.key, s.value
FROM settings s
CROSS JOIN databases d
WHERE s.db_id = '_global'
  AND (s.key LIKE 'task.%'
       OR s.key LIKE 'reports.%'
       OR s.key = 'dashboard.featured_report_id')
ON CONFLICT (db_id, key) DO NOTHING;

DELETE FROM settings
WHERE db_id = '_global'
  AND (key LIKE 'task.%'
       OR key LIKE 'reports.%'
       OR key = 'dashboard.featured_report_id');

INSERT INTO schema_version (version) VALUES (22);
