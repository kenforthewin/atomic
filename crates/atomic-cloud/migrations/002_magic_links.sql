-- Magic links (docs/plans/atomic-cloud.md, "Provisioning lifecycle" →
-- "Signup" step 2; decisions log 2026-05-25: authentication is magic-link
-- only). One row per requested link, for both purposes:
--
--   'signup' — carries the subdomain the user asked for in
--              `requested_subdomain`; the authoritative subdomain claim
--              still happens at consume time via the accounts UNIQUE
--              constraint, so this column is a request, not a reservation.
--   'login'  — `requested_subdomain` is NULL; the account is found by email
--              when the link is consumed.
--
-- `token_hash` is the SHA-256 hex of an opaque `aml_<random>` plaintext —
-- the same hash-only discipline as cloud_tokens and sessions; the plaintext
-- exists only in the emailed link. (The pre-rewrite prototype stored raw
-- tokens; that is exactly what this schema must never do.)
--
-- Single use: consumption is an atomic UPDATE guarded by
-- `consumed_at IS NULL AND expires_at > NOW()`; expired or already-consumed
-- rows are inert. `request_ip` is a forensic breadcrumb, nullable because
-- proxies may strip it.
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

CREATE TABLE IF NOT EXISTS magic_links (
    token_hash          TEXT PRIMARY KEY,
    email               TEXT NOT NULL,
    purpose             TEXT NOT NULL,
    requested_subdomain TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at          TIMESTAMPTZ NOT NULL,
    consumed_at         TIMESTAMPTZ,
    request_ip          TEXT
);

INSERT INTO schema_version (version) VALUES (2);
