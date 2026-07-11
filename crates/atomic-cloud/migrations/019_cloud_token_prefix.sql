-- Cloud tokens gain a display prefix (the first 10 chars of the plaintext,
-- e.g. 'atm_4yegax'), stored at mint time: hash-only storage can't recover
-- it, and the tenant token-management UI renders it so users can tell their
-- tokens apart (and detect "you're revoking the token you're using"). Rows
-- minted before this migration stay NULL and render without a prefix.
ALTER TABLE cloud_tokens ADD COLUMN token_prefix TEXT;

-- Record this migration in the version table (the runner reads MAX(version)).
INSERT INTO schema_version (version) VALUES (19);
