-- Migration 011 — Stripe webhook idempotency (plan: "Billing" → "The webhook
-- is the source of truth"; Decisions log 2026-05-25 "Stripe via Customer
-- Portal … Webhook at app.<base>/billing/webhook").
--
-- Additive-only (tests/migration_lint.rs): a single CREATE TABLE. Adds the
-- dedup ledger the webhook handler claims against BEFORE applying any side
-- effect, so a redelivered event (Stripe retries until it sees a 2xx, and
-- explicitly does NOT guarantee at-most-once delivery or ordering) is acked
-- without re-running its money/quota/audit effects.
--
-- Why a table and not just convergent UPSERTs: the convergent state writes
-- already make a *verbatim* replay safe for billing_state/plan_id, but the
-- 'checkout' arm of apply_subscription_event records a plan_transitions audit
-- row unconditionally, so a replay would append a duplicate audit row each
-- time. Claiming the event id once collapses every redelivery to a no-op —
-- audit log included — and bounds the work to one INSERT on the hot path.
--
-- event_id is Stripe's `evt_…` id (globally unique per event, stable across
-- that event's redeliveries). PK so the claim is a single conflict-or-insert.
CREATE TABLE IF NOT EXISTS processed_webhook_events (
    event_id     TEXT PRIMARY KEY,
    event_type   TEXT NOT NULL,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Record this migration in the version table (the runner reads MAX(version)).
INSERT INTO schema_version (version) VALUES (11);
