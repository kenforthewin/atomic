-- Control-plane schema, first cut (docs/plans/atomic-cloud.md, "Auth & tenant
-- routing"). Slice-1 tables only: accounts, account_databases, cloud_tokens,
-- sessions, subdomains_reserved. The oauth_* and provider_credentials tables
-- belong to later slices and arrive as additive migrations.
--
-- Migration discipline (plan: "Schema migration on deploy"): control-plane
-- migrations are ADDITIVE-ONLY — ADD COLUMN, CREATE TABLE, CREATE INDEX,
-- deferred/not-validated constraints. No DROP COLUMN, no ALTER COLUMN TYPE,
-- no renames. Drops happen N+1 deploys later, after all referring code is
-- out of the fleet.

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One row per account. `subdomain` is the primary tenant-routing input
-- (Host header → subdomain → account). `status` lifecycle:
-- 'provisioning' → 'active' (→ 'failed' when signup crashes and the reaper
-- rolls it back). `last_active_db_id` is the user's last-selected knowledge
-- base *inside* their tenant database — the cloud home of the "active
-- database" concept; updated only on explicit switch.
CREATE TABLE IF NOT EXISTS accounts (
    id                  TEXT PRIMARY KEY,
    subdomain           TEXT NOT NULL UNIQUE,
    email               TEXT NOT NULL,
    status              TEXT NOT NULL,
    plan                TEXT NOT NULL,
    last_active_db_id   TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at          TIMESTAMPTZ
);

-- Maps an account to its tenant Postgres database (`acct_<uuid>` on the
-- shared cluster). `cluster_id` exists from day one so a future shard split
-- is mechanical; v1 runs a single cluster.
CREATE TABLE IF NOT EXISTS account_databases (
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    cluster_id  TEXT NOT NULL,
    db_name     TEXT NOT NULL,
    status      TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (account_id, db_name)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_account_databases_cluster_db
    ON account_databases(cluster_id, db_name);

-- Single source of truth for ALL API tokens in cloud (account-scope,
-- KB-scope, MCP-scope) — there is no per-tenant api_tokens table. `hash` is
-- the SHA-256 of an opaque `atm_<random>` token; the subdomain provides
-- account context so the token itself carries none. `scope` is one of
-- 'account' | 'database' | 'mcp'; `allowed_db_id` pins database-scoped
-- tokens to a single KB inside the tenant database.
CREATE TABLE IF NOT EXISTS cloud_tokens (
    hash            TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    scope           TEXT NOT NULL,
    allowed_db_id   TEXT,
    name            TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at    TIMESTAMPTZ,
    expires_at      TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_cloud_tokens_account ON cloud_tokens(account_id);

-- Server-stored web sessions; the browser cookie holds only the opaque
-- session hash. Separate from cloud_tokens because lifetime and revocation
-- UX differ. `ip_first_seen` / `ua_first_seen` are forensic breadcrumbs,
-- nullable because proxies may strip either.
CREATE TABLE IF NOT EXISTS sessions (
    hash            TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL,
    ip_first_seen   TEXT,
    ua_first_seen   TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_account ON sessions(account_id);

-- Time-boxed subdomain holds. Account deletion parks the freed subdomain
-- here for 90 days so stale external clients (RSS readers, MCP configs)
-- pointing at the old name don't silently hit a stranger's account. Distinct
-- from the static code-level blocklist in src/reserved_subdomains.rs.
CREATE TABLE IF NOT EXISTS subdomains_reserved (
    subdomain   TEXT PRIMARY KEY,
    expires_at  TIMESTAMPTZ NOT NULL
);

INSERT INTO schema_version (version) VALUES (1);
