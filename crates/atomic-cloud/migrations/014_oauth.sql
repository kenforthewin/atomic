-- Per-account OAuth storage (docs/plans/atomic-cloud.md, "Auth & tenant
-- routing" → control-plane schema oauth_clients/oauth_codes, and the "OAuth"
-- subsection). These are **per-account in cloud** (vs. server-wide in
-- self-hosted): each subdomain has its own OAuth identity, so every row is
-- scoped by `account_id` and the storage queries always filter on it — a
-- client_id minted under account A must never resolve under account B.
--
-- Self-hosted's equivalent tables live in atomic-core's registry, which cloud
-- (Postgres mode, no registry) does not have; this is why the cloud OAuth flow
-- carries its own control-plane storage rather than extending atomic-server's.
--
-- Secret hygiene (the slice-1/2 rule, see cloud_tokens / magic_links):
--
--   * `oauth_clients.client_id` is an opaque PUBLIC identifier (`occ_<random>`)
--     — stored in plaintext because it is not a secret; the client presents it
--     openly on every request.
--   * `oauth_clients.client_secret_hash` is the SHA-256 hex of the DCR-issued
--     client secret. The plaintext secret is returned to the client exactly
--     once at registration and never persisted.
--   * `oauth_codes.code_hash` is the SHA-256 hex of an opaque authorization
--     code (`oac_<random>`), the same hash-only discipline as magic_links —
--     the plaintext code exists only in the redirect back to the client.
--   * `oauth_codes.code_challenge` is the PKCE challenge. It is already a hash
--     of the client's secret verifier (BASE64URL(SHA256(verifier)) for S256),
--     not a secret at rest, so it is stored as-is and compared at token
--     exchange against the freshly-hashed verifier the client presents.
--
-- Authorization codes are short-lived (default TTL 60s; see
-- src/oauth_store.rs) and single-use: consumption is one atomic UPDATE guarded
-- by `account_id = $1 AND code_hash = $2 AND consumed_at IS NULL AND
-- expires_at > NOW()`, so a replayed or expired code is inert and two
-- concurrent exchanges can never both win. Expired/consumed rows are left for
-- the reaper to purge (purge_expired_oauth_codes); nothing reads them once
-- spent.
--
-- Migration discipline (see 001): ADDITIVE-ONLY — no DROP, ALTER TYPE,
-- RENAME, SET NOT NULL, or validated-at-add constraint.

-- One row per Dynamically-Registered OAuth client, scoped to the account that
-- registered it. `redirect_uris` is the JSON array the client registered; the
-- authorize/token endpoints validate the presented redirect_uri against it.
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id           TEXT PRIMARY KEY,
    account_id          TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    client_secret_hash  TEXT NOT NULL,
    client_name         TEXT NOT NULL,
    redirect_uris       JSONB NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_oauth_clients_account ON oauth_clients(account_id);

-- One row per issued authorization code, scoped to the account. `token_id` is
-- set when the code is exchanged for a token (a forensic link to the
-- cloud_tokens row it minted); `allowed_db_id` carries an optional KB pin
-- through the flow so a db-scoped MCP authorization survives into the token.
CREATE TABLE IF NOT EXISTS oauth_codes (
    code_hash               TEXT PRIMARY KEY,
    account_id              TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    client_id               TEXT NOT NULL,
    code_challenge          TEXT NOT NULL,
    code_challenge_method   TEXT NOT NULL,
    redirect_uri            TEXT NOT NULL,
    scope                   TEXT NOT NULL,
    allowed_db_id           TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at              TIMESTAMPTZ NOT NULL,
    consumed_at             TIMESTAMPTZ,
    token_id                TEXT
);

CREATE INDEX IF NOT EXISTS idx_oauth_codes_account ON oauth_codes(account_id);

INSERT INTO schema_version (version) VALUES (14);
