-- Reaper support (docs/plans/atomic-cloud.md, "Failure recovery & the
-- reaper"). `created_at` records when a reservation was (last) written, so
-- the reaper can tell a crashed deletion's residue — an ACTIVE account whose
-- own subdomain holds an old reservation (the slice-1 Implementation-log
-- follow-up) — from a deletion in flight *right now* between its
-- reserve-subdomain and delete-accounts-row steps. The reaper only clears
-- self-reservations older than a settle grace; `delete_account` re-ups this
-- column on its upsert so a retried deletion is "fresh" again. Backfilled
-- rows get NOW(), which at worst delays their cleanup by one grace period.
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

ALTER TABLE subdomains_reserved
    ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

INSERT INTO schema_version (version) VALUES (3);
