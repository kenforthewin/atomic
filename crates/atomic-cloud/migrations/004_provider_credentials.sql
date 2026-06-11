-- Encrypted provider credentials (docs/plans/atomic-cloud.md, "Provider
-- management" → "Storage schema" / "Encryption at rest").
--
-- One row per (account, provider, origin). `origin` distinguishes
-- platform-provisioned keys ('managed' — created via OpenRouter's
-- provisioning API at signup) from user-provided BYOK keys ('user'). The
-- composite primary key lets a managed and a BYOK row coexist, so switching
-- between them is a pointer flip on `accounts`, not a re-provision.
--
-- `encrypted_key`/`nonce`/`encryption_version` are the KeyVault triple
-- (src/keyvault.rs): AES-256-GCM ciphertext under the process master key,
-- the fresh 96-bit nonce used for that one encryption, and the master-key
-- generation that produced it. The plaintext key never touches this table.
-- `external_key_id` is the OpenRouter provisioning-API identifier needed to
-- PATCH/DELETE managed keys; it is an opaque reference, not a secret, so it
-- is stored in the clear. `model_config` holds the account-level model
-- selection ({ embedding_model, llm_model, ... }) — provider config is
-- account-level in v1, not per-KB (decisions log 2026-05-25).
--
-- Migration discipline (see 001): ADDITIVE-ONLY.

CREATE TABLE IF NOT EXISTS provider_credentials (
    account_id              TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    provider                TEXT NOT NULL CHECK (provider IN ('openrouter', 'openai_compat')),
    origin                  TEXT NOT NULL CHECK (origin IN ('managed', 'user')),
    external_key_id         TEXT,
    encrypted_key           BYTEA NOT NULL,
    nonce                   BYTEA NOT NULL,
    encryption_version      INT NOT NULL,
    model_config            JSONB NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    rotated_at              TIMESTAMPTZ,
    last_used_at            TIMESTAMPTZ,
    last_validated_at       TIMESTAMPTZ,
    last_validation_error   TEXT,
    PRIMARY KEY (account_id, provider, origin)
);

-- Which provider_credentials row is the account's active config. The plan
-- names a single `accounts.active_provider` column; rows are keyed
-- (provider, origin), so "which row" needs both halves. Encoding choice:
-- TWO nullable columns rather than one encoded text value ('openrouter:
-- managed'), because the pair stays native to SQL — the active-row lookup
-- is a plain two-column join with no parse step, and each half keeps its
-- own CHECK-style vocabulary instead of a stringly cross-product. The flip
-- is still a single UPDATE, so atomicity is unaffected. Both columns NULL
-- (the backfill state for pre-existing accounts) means "no active provider
-- config": the account resolves to a key-less ProviderConfig and provider
-- calls fail with a structured error (plan: "Plumbing").
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS active_provider TEXT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS active_origin   TEXT;

-- Half-set pointers are meaningless; reject them at the schema. NOT VALID
-- per the additive-only discipline (no scan of existing rows — trivially
-- satisfied anyway, since both columns are brand new and all-NULL); new
-- writes are checked.
ALTER TABLE accounts ADD CONSTRAINT accounts_active_provider_paired
    CHECK ((active_provider IS NULL) = (active_origin IS NULL)) NOT VALID;

INSERT INTO schema_version (version) VALUES (4);
