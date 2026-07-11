-- Migration 021 — trial lifecycle emails (decision 2026-07-11: plan-expiry
-- communication is email-only; the shared product frontend gets no
-- cloud-specific expiry popups — the only in-product surface is the cloud
-- dashboard at /account/*, which already shows the trial banner).
--
-- `trial_warning_sent_at` marks that the "your trial ends soon" email went
-- out, so the hourly sweep sends it at most once per trial: the sweep CLAIMS
-- an account by setting this column in the same UPDATE that selects it
-- (cross-pod safe — the first pod's UPDATE wins), and clears it back to NULL
-- if the send fails so the next sweep retries. NULL = not yet warned, which
-- is also correct for every pre-existing row.
--
-- The trial-ended notice needs no column: the trialing→free downgrade UPDATE
-- (guarded on `billing_state = 'trialing'`) is itself the once-only event
-- that triggers it.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS trial_warning_sent_at TIMESTAMPTZ;

INSERT INTO schema_version (version) VALUES (21);
