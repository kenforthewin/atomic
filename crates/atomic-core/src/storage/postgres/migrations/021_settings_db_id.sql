-- Migration 021: scope the settings table by logical database.
--
-- Pre-021, one settings table (key PRIMARY KEY) served two roles at once:
-- the registry role (provider/model config — SQLite keeps these in
-- registry.db) and the per-DB role (task.{id}.* scheduler state, seed
-- flags — SQLite keeps these in each data DB). With no db_id column,
-- per-DB keys collided across logical databases on a shared cluster.
--
-- This migration adds an explicit db_id tier: per-DB rows carry their
-- logical database id, registry-role rows carry the '_global' sentinel
-- (see GLOBAL_SETTINGS_DB_ID in storage/postgres/mod.rs). Existing rows
-- all land in '_global' — correct for the provider/model config they
-- overwhelmingly are. Previously-collided per-DB keys (task.{id}.*,
-- reports.* seed flags, dashboard.featured_report_id) become orphaned
-- global rows invisible to scoped reads — which is NOT benign: invisible
-- seed flags re-seed a duplicate Daily Briefing report, and operator
-- overrides (task.{id}.enabled, GC retention) revert to defaults.
-- Migration 022 repairs this by replicating those orphans into every
-- logical database and removing them from '_global'.
--
-- Statement-level idempotence (IF NOT EXISTS / IF EXISTS) keeps a
-- partially-applied run safe to re-execute.

ALTER TABLE settings ADD COLUMN IF NOT EXISTS db_id TEXT NOT NULL DEFAULT '_global';
ALTER TABLE settings DROP CONSTRAINT IF EXISTS settings_pkey;
ALTER TABLE settings ADD PRIMARY KEY (db_id, key);

INSERT INTO schema_version (version) VALUES (21);
