-- Admin portal foundations (plan: admin portal slice 1).
--
--   * accounts.is_admin — gates the /admin plane. Bootstrapped via the CLI
--     (`account promote`); never settable through a tenant-facing route.
--   * accounts.plan_pinned — an admin-assigned plan holds against the
--     automated writers: the trial-expiry sweep and the Stripe subscription
--     projection skip pinned accounts. Without the pin, a comped account
--     would be clawed back to free within one sweep interval.
--   * admin_actions — the audit ledger every admin mutation writes.
--   * The first comp tier: a catalogue row like any other (future comp
--     tiers are additional rows — the admin UI renders its picker from the
--     plans table, so no code changes). Unlimited atoms/KBs, pro-sized
--     storage, premium models, a real but bounded $5/mo AI allowance.

ALTER TABLE accounts ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE accounts ADD COLUMN plan_pinned BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE IF NOT EXISTS admin_actions (
    id                BIGSERIAL PRIMARY KEY,
    actor             TEXT NOT NULL,          -- account id, or 'cli'
    action            TEXT NOT NULL,          -- 'set_plan' | 'evict' | 'delete_account' | 'promote' | ...
    target_account_id TEXT,
    detail            JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS admin_actions_target_idx
    ON admin_actions (target_account_id, created_at);

INSERT INTO plans
    (id, name, monthly_price_cents, atom_limit, ai_credits_monthly_cents, kb_limit, storage_bytes_limit, feature_flags)
VALUES
    ('comp', 'Comp', 0, NULL, 500, NULL, 10737418240, '{"premium_models": true}'::jsonb)
ON CONFLICT (id) DO NOTHING;

-- Record this migration in the version table (the runner reads MAX(version)).
INSERT INTO schema_version (version) VALUES (20);
