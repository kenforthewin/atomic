-- Dispatch hints (docs/plans/atomic-cloud.md, "Worker fairness & job queue"
-- → "Cross-tenant ledger scan").
--
-- One row per account with possibly-pending ledger work. The tenant plane
-- UPSERTs the row after any mutating data-plane request (the write that may
-- have enqueued `atom_pipeline_jobs` / `task_runs` rows in the tenant
-- database); the dispatcher polls only hinted tenants and clears a hint when
-- the tenant's ledgers come back empty — but only when `last_enqueued_at` is
-- no newer than the value it read when its scan started, so a hint written
-- DURING a scan survives the clear. A lost hint (dual-write failure) is
-- bounded by the dispatcher's slow-path full scan over all active accounts.
--
-- ON DELETE CASCADE: hints are pure derived state — they die with the
-- account, same as every other account-owned table (the deletion sequence
-- deletes the accounts row; the FK sweeps this row with it).
--
-- Migration discipline: additive-only (see 001_control_plane.sql).

CREATE TABLE IF NOT EXISTS dispatch_hints (
    account_id        TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    last_enqueued_at  TIMESTAMPTZ NOT NULL
);

INSERT INTO schema_version (version) VALUES (6);
