-- Migration 022 — persist the version-stamp short-circuit's skipped count
-- on deploy runs (scaling plan §4, remediation (b)).
--
-- A fully-current fleet finishes its deploy run with total = 0, migrated =
-- 0, failed = 0 — byte-identical, in the durable ledger, to a run whose
-- enumeration saw no active rows at all (control plane pointed at the
-- wrong database, or mapping rows wedged in a non-'active' status; both
-- the lagging list and the skipped count filter on status = 'active', so
-- such rows fall in neither bucket). The boot log line disambiguates, but
-- pods recycle and logs rotate; `deploy status` is the record an incident
-- reads days later, so the disambiguation must live in the row.
--
-- skipped_current is the missing half of the run's arithmetic: active rows
-- whose stamp already met the target at enumeration, skipped without a
-- tenant connection. total + skipped_current is the active fleet the run
-- saw — 0/0/0 with skipped_current = N reads "all current, clean
-- short-circuit"; 0/0/0 with skipped_current = 0 reads "the gate saw no
-- fleet at all", which is worth an operator's suspicion.
--
-- NULL = not recorded: rows finished before this column existed, or by an
-- older binary mid-rolling-deploy. Displayed as unknown, never backfilled
-- to a guessed 0 — a genuine 0 is precisely the suspicious reading.
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

ALTER TABLE deploy_runs ADD COLUMN IF NOT EXISTS skipped_current INTEGER;

INSERT INTO schema_version (version) VALUES (22);
