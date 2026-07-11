//! The dunning state machine and billing serving-state transitions (plan:
//! "Billing" → "Plan transitions"; Decisions log 2026-05-25 "Never
//! auto-delete data for payment failure. Read-only after 3 days past_due,
//! suspended after 14 days, data retained").
//!
//! `accounts.billing_state` is the serving restriction a payment problem
//! imposes; it is **orthogonal** to `accounts.status` (CloudAuth's
//! provisioning/active gate). A delinquent account stays `status='active'`.
//! The four states and what each permits on the data plane:
//!
//! | State       | Reads | Writes | Login | Notes                          |
//! |-------------|-------|--------|-------|--------------------------------|
//! | `active`    | ✓     | ✓      | ✓     | Normal.                        |
//! | `past_due`  | ✓     | ✓      | ✓     | Grace; full access.            |
//! | `read_only` | ✓     | ✗      | ✓     | 3 days past_due; writes 402.   |
//! | `suspended` | ✗     | ✗      | ✗     | 14 days past_due; data kept.   |
//!
//! Transitions are driven from two places:
//!
//! - **Webhooks** ([`apply_subscription_event`], [`apply_payment_outcome`]):
//!   a failed payment moves `active → past_due` and stamps `past_due_since`;
//!   a succeeded payment (or a checkout/upgrade) clears back to `active`; a
//!   deleted subscription drops the plan to `free` (keeping any over-limit
//!   data, writes blocked until under — enforced by the quota guard, not
//!   here).
//! - **Time** ([`advance_dunning`]): a reaper-style sweep that reads
//!   `past_due_since` and advances `past_due → read_only` at 3 days and
//!   `read_only → suspended` at 14 days. It takes an explicit `now` so the
//!   thresholds are testable by manufacturing a past `past_due_since` via
//!   SQL — no real waits.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::billing::SubscriptionState;
use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::managed_keys::ManagedKeys;
use crate::plans::DEFAULT_PLAN_ID;

/// Days past_due before writes are blocked (`past_due → read_only`).
pub const READ_ONLY_AFTER_DAYS: i64 = 7;

/// Days past_due before serving is blocked (`read_only → suspended`).
pub const SUSPENDED_AFTER_DAYS: i64 = 21;

/// The two day-count thresholds the time-driven dunning ladder advances on
/// (plan: 3 days → read_only, 14 days → suspended). A config struct rather
/// than two bare `const`s so `serve` can expose them as `--dunning-*` flags
/// (with the plan's defaults) and a test can shrink them; [`Default`] is the
/// plan's `READ_ONLY_AFTER_DAYS`/`SUSPENDED_AFTER_DAYS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DunningThresholds {
    /// Days past_due before `past_due → read_only`.
    pub read_only_after_days: i64,
    /// Days past_due before `read_only → suspended`.
    pub suspended_after_days: i64,
}

impl Default for DunningThresholds {
    fn default() -> Self {
        Self {
            read_only_after_days: READ_ONLY_AFTER_DAYS,
            suspended_after_days: SUSPENDED_AFTER_DAYS,
        }
    }
}

/// The serving restriction a billing problem imposes. Stored as text in
/// `accounts.billing_state`; read by CloudAuth (suspended → block) and the
/// billing write-guard (read_only → block writes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingState {
    /// Normal — no restriction.
    Active,
    /// A free trial is running (14 days of the paid tier on signup, no card
    /// — plan: "Trials"). Serving-wise identical to [`Active`](Self::Active):
    /// full read+write access. Distinct only so the trial auto-downgrade can
    /// find these accounts and the frontend can show a trial banner. The
    /// [`crate::billing::dunning::advance_dunning`] sweep ends it once
    /// `trial_ends_at` passes.
    Trialing,
    /// A payment failed; full access continues during the grace window.
    PastDue,
    /// 3 days past_due: reads and exports allowed, writes blocked.
    ReadOnly,
    /// 14 days past_due: serving (and login) blocked; data retained.
    Suspended,
}

impl BillingState {
    /// The text stored in `accounts.billing_state`.
    pub fn as_str(self) -> &'static str {
        match self {
            BillingState::Active => "active",
            BillingState::Trialing => "trialing",
            BillingState::PastDue => "past_due",
            BillingState::ReadOnly => "read_only",
            BillingState::Suspended => "suspended",
        }
    }

    /// Whether serving is blocked entirely (the request never reaches a
    /// handler — CloudAuth's gate).
    pub fn blocks_serving(self) -> bool {
        self == BillingState::Suspended
    }

    /// Whether *mutating* requests are blocked while reads still pass (the
    /// read-only gate). Suspended blocks everything via [`blocks_serving`],
    /// so it is excluded here.
    pub fn blocks_writes(self) -> bool {
        self == BillingState::ReadOnly
    }
}

impl FromStr for BillingState {
    type Err = CloudError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(BillingState::Active),
            "trialing" => Ok(BillingState::Trialing),
            "past_due" => Ok(BillingState::PastDue),
            "read_only" => Ok(BillingState::ReadOnly),
            "suspended" => Ok(BillingState::Suspended),
            other => Err(CloudError::Invariant(format!(
                "unknown billing_state {other:?}"
            ))),
        }
    }
}

/// Reconstruct the billing state from the raw column, defaulting an unknown
/// or absent value to [`BillingState::Active`] with a loud log — the
/// conservative reading (never block a paying user over one corrupt column),
/// mirroring [`crate::backpressure::ProviderPause::from_columns`].
pub fn billing_state_from_column(value: &str) -> BillingState {
    match BillingState::from_str(value) {
        Ok(state) => state,
        Err(_) => {
            tracing::warn!(value, "unknown billing_state; treating as active");
            BillingState::Active
        }
    }
}

/// Record an entry in the `plan_transitions` audit log. Best-effort context;
/// callers thread the trigger and an optional detail. A foreign-key
/// violation (the account vanished) is success — there is nothing to audit
/// for a deleted account.
///
/// Runs on whatever connection the caller threads — the webhook's apply path
/// passes the same transaction the claim rode, so the audit row commits (or
/// rolls back) atomically with the rest of the event's effects.
pub(crate) async fn record_transition(
    conn: &mut PgConnection,
    account_id: &str,
    from_plan: Option<&str>,
    to_plan: Option<&str>,
    trigger: &str,
    detail: Option<&str>,
) -> Result<(), CloudError> {
    let result = sqlx::query(
        "INSERT INTO plan_transitions \
             (account_id, from_plan_id, to_plan_id, trigger, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(account_id)
    .bind(from_plan)
    .bind(to_plan)
    .bind(trigger)
    .bind(detail)
    .execute(&mut *conn)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => Ok(()),
        Err(e) => Err(CloudError::db("recording plan transition")(e)),
    }
}

/// Claim a Stripe webhook event id for one-time processing on the caller's
/// connection. Inserts the id into `processed_webhook_events`; returns `true`
/// when THIS call won the insert (a first delivery, so the caller should apply
/// the event) and `false` when the id was already present (a redelivery —
/// Stripe retries until it sees a 2xx and does not guarantee at-most-once
/// delivery, so the caller acks without re-running the side effects).
///
/// This is the idempotency boundary: claiming before applying collapses every
/// redelivery of the same event to a no-op, including the unconditional
/// `checkout`-arm audit row that would otherwise duplicate in
/// `plan_transitions` (plan: "The webhook is the source of truth").
///
/// **Atomicity with apply.** The webhook claims and applies inside ONE
/// transaction (see [`crate::billing_routes`]): the claim INSERT and every
/// apply write share a connection, so a crash between the claim and the
/// apply's side effects rolls back the claim too — Stripe's redelivery then
/// re-processes the event instead of seeing a committed-but-uneffected claim
/// and acking it as a permanent no-op (the adversarial finding this closes).
/// The in-transaction dedup of the audit row is preserved because the claim
/// row and the `plan_transitions` row commit together.
pub async fn claim_webhook_event_on_conn(
    conn: &mut PgConnection,
    event_id: &str,
    event_type: &str,
) -> Result<bool, CloudError> {
    let inserted = sqlx::query(
        "INSERT INTO processed_webhook_events (event_id, event_type) \
         VALUES ($1, $2) ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(event_type)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("claiming webhook event"))?
    .rows_affected();
    Ok(inserted > 0)
}

/// [`claim_webhook_event_on_conn`] against a pooled connection — the direct
/// (non-transactional) form the tests drive. The webhook itself uses the
/// `_on_conn` form inside its transaction.
pub async fn claim_webhook_event(
    control: &ControlPlane,
    event_id: &str,
    event_type: &str,
) -> Result<bool, CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to claim webhook event",
    ))?;
    claim_webhook_event_on_conn(&mut conn, event_id, event_type).await
}

/// Release a previously-[`claim_webhook_event`]ed id, deleting its row. Called
/// only when applying the event FAILED, so Stripe's retry re-processes it
/// instead of being deduped into a permanent no-op (the side effects never
/// landed). Idempotent — a missing row is success.
pub async fn release_webhook_event(
    control: &ControlPlane,
    event_id: &str,
) -> Result<(), CloudError> {
    sqlx::query("DELETE FROM processed_webhook_events WHERE event_id = $1")
        .bind(event_id)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("releasing webhook event"))?;
    Ok(())
}

/// Resolve the account id a webhook event pertains to, via its Stripe
/// customer id, on the caller's connection. `None` when no `stripe_customers`
/// row maps it (an event for an account we don't know — e.g. a customer
/// created out-of-band).
pub async fn account_for_customer_on_conn(
    conn: &mut PgConnection,
    stripe_customer_id: &str,
) -> Result<Option<String>, CloudError> {
    sqlx::query_scalar("SELECT account_id FROM stripe_customers WHERE stripe_customer_id = $1")
        .bind(stripe_customer_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(CloudError::db("looking up account by Stripe customer"))
}

/// [`account_for_customer_on_conn`] against a pooled connection.
pub async fn account_for_customer(
    control: &ControlPlane,
    stripe_customer_id: &str,
) -> Result<Option<String>, CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to resolve Stripe customer",
    ))?;
    account_for_customer_on_conn(&mut conn, stripe_customer_id).await
}

/// UPSERT the `stripe_customers` linkage on the caller's connection. Called
/// when a checkout completes (the first time we learn an account's Stripe
/// customer id) and idempotent on retry.
pub async fn link_stripe_customer_on_conn(
    conn: &mut PgConnection,
    account_id: &str,
    stripe_customer_id: &str,
) -> Result<(), CloudError> {
    sqlx::query(
        "INSERT INTO stripe_customers (account_id, stripe_customer_id) \
         VALUES ($1, $2) \
         ON CONFLICT (account_id) DO UPDATE SET stripe_customer_id = EXCLUDED.stripe_customer_id",
    )
    .bind(account_id)
    .bind(stripe_customer_id)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("linking Stripe customer"))?;
    Ok(())
}

/// [`link_stripe_customer_on_conn`] against a pooled connection — the form the
/// tests drive directly.
pub async fn link_stripe_customer(
    control: &ControlPlane,
    account_id: &str,
    stripe_customer_id: &str,
) -> Result<(), CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to link Stripe customer",
    ))?;
    link_stripe_customer_on_conn(&mut conn, account_id, stripe_customer_id).await
}

/// Apply a `customer.subscription.{created,updated,deleted}` projection to an
/// account: persist the subscription row, set `accounts.plan_id`, and move
/// the billing state. This is the plan's "Plan transitions" table made
/// executable:
///
/// - **created / active**: plan updated, quotas widen, billing state cleared
///   to `active`.
/// - **past_due** (Stripe's own dunning status): mark `past_due` and stamp
///   `past_due_since` if not already past due — the time machine takes it
///   from there.
/// - **deleted / canceled**: drop to the free plan; over-limit data is
///   retained (the quota guard blocks writes until under), billing state
///   cleared to `active` (the *subscription* is gone, not a payment problem).
///
/// `deleted` is signaled by `state.status == "canceled"` or the dedicated
/// [`apply_subscription_deleted`]; this handles the create/update arm.
///
/// # Ordering
///
/// [`claim_webhook_event`] makes every *redelivery* of one event a no-op, so
/// a verbatim replay can't re-apply a plan/state change or duplicate the
/// audit row. Stripe does not, however, guarantee *ordering* between distinct
/// events: a stale `subscription.updated` (its own event id) delivered after a
/// `subscription.deleted` would transiently re-widen the plan. This is
/// accepted (the plan's webhook is the source of truth and the next
/// authoritative event self-heals it; billing money rides Stripe-side credits,
/// not this projection). A strict newer-wins guard would need the event's
/// `created` timestamp persisted and compared here — deferred until reorder is
/// observed in practice.
pub async fn apply_subscription_event(
    control: &ControlPlane,
    account_id: &str,
    state: &SubscriptionState,
) -> Result<(), CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to apply subscription event",
    ))?;
    apply_subscription_event_on_conn(&mut conn, account_id, state).await
}

/// [`apply_subscription_event`] on the caller's connection — the form the
/// webhook calls inside its claim+apply transaction.
pub async fn apply_subscription_event_on_conn(
    conn: &mut PgConnection,
    account_id: &str,
    state: &SubscriptionState,
) -> Result<(), CloudError> {
    // An admin-pinned plan holds against the projection: comped accounts
    // must not be re-planned by a stray/legacy subscription event. The
    // subscription row itself is still persisted below (billing history
    // stays truthful) — only the plan/billing-state stamps are skipped.
    if plan_pinned(conn, account_id).await? {
        tracing::warn!(
            account_id,
            status = %state.status,
            "subscription event for an admin-pinned account; \
             persisting the subscription row but leaving the pinned plan untouched"
        );
        return persist_subscription_row(conn, account_id, state).await;
    }
    persist_subscription_row(conn, account_id, state).await?;

    let from_plan = current_plan(conn, account_id).await?;

    match state.status.as_str() {
        // A live subscription: stamp the plan, clear any dunning.
        "active" | "trialing" => {
            set_plan(conn, account_id, &state.plan_id).await?;
            clear_billing_state(conn, account_id).await?;
            record_transition(
                conn,
                account_id,
                from_plan.as_deref(),
                Some(&state.plan_id),
                "checkout",
                Some(&state.status),
            )
            .await?;
        }
        // Stripe is dunning this subscription: enter past_due (idempotent —
        // only stamps past_due_since on the first transition into it).
        "past_due" | "unpaid" => {
            enter_past_due(conn, account_id).await?;
            record_transition(
                conn,
                account_id,
                from_plan.as_deref(),
                from_plan.as_deref(),
                "payment_failed",
                Some(&state.status),
            )
            .await?;
        }
        // Canceled subscription: drop to free, keep data.
        "canceled" | "incomplete_expired" => {
            apply_subscription_deleted_on_conn(conn, account_id).await?;
        }
        // Any other Stripe status (incomplete, paused, …): persist the row
        // (done above) but make no serving-state change — the next
        // authoritative event resolves it.
        _ => {}
    }
    Ok(())
}

/// Persist the subscription projection row — the billing-history half of
/// [`apply_subscription_event_on_conn`], shared with the admin-pinned early
/// return (which records the event but never re-plans the account).
async fn persist_subscription_row(
    conn: &mut PgConnection,
    account_id: &str,
    state: &SubscriptionState,
) -> Result<(), CloudError> {
    sqlx::query(
        "INSERT INTO stripe_subscriptions \
             (account_id, stripe_subscription_id, plan_id, status, \
              current_period_start, current_period_end, cancel_at_period_end, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) \
         ON CONFLICT (account_id) DO UPDATE SET \
             stripe_subscription_id = EXCLUDED.stripe_subscription_id, \
             plan_id = EXCLUDED.plan_id, \
             status = EXCLUDED.status, \
             current_period_start = EXCLUDED.current_period_start, \
             current_period_end = EXCLUDED.current_period_end, \
             cancel_at_period_end = EXCLUDED.cancel_at_period_end, \
             updated_at = NOW()",
    )
    .bind(account_id)
    .bind(&state.stripe_subscription_id)
    .bind(&state.plan_id)
    .bind(&state.status)
    .bind(state.current_period_start)
    .bind(state.current_period_end)
    .bind(state.cancel_at_period_end)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("persisting Stripe subscription"))?;
    Ok(())
}

/// Whether the account's plan is admin-pinned (see migration 020): pinned
/// plans hold against every automated writer — the trial-expiry sweep and
/// the Stripe subscription projection.
pub(crate) async fn plan_pinned(
    conn: &mut PgConnection,
    account_id: &str,
) -> Result<bool, CloudError> {
    sqlx::query_scalar("SELECT plan_pinned FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(CloudError::db("reading plan pin"))
        .map(|row| row.unwrap_or(false))
}

/// `customer.subscription.deleted`: drop the account to the free plan and
/// clear the subscription row. Over-limit data is **retained** — the quota
/// guard blocks new writes until the user is back under the free limits (no
/// auto-deletion, ever). Billing state returns to `active`: there is no
/// payment problem, just no paid subscription.
pub async fn apply_subscription_deleted(
    control: &ControlPlane,
    account_id: &str,
) -> Result<(), CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to apply subscription deletion",
    ))?;
    apply_subscription_deleted_on_conn(&mut conn, account_id).await
}

/// [`apply_subscription_deleted`] on the caller's connection — the form the
/// webhook calls inside its claim+apply transaction.
pub async fn apply_subscription_deleted_on_conn(
    conn: &mut PgConnection,
    account_id: &str,
) -> Result<(), CloudError> {
    // Admin-pinned plans hold here too: the subscription row is cleared
    // (billing history stays truthful) but a comped account is never
    // dropped to free by its old subscription winding down.
    if plan_pinned(conn, account_id).await? {
        tracing::warn!(
            account_id,
            "subscription deleted for an admin-pinned account; \
             clearing the subscription row, keeping the pinned plan"
        );
        sqlx::query("DELETE FROM stripe_subscriptions WHERE account_id = $1")
            .bind(account_id)
            .execute(&mut *conn)
            .await
            .map_err(CloudError::db("clearing deleted subscription"))?;
        return Ok(());
    }
    let from_plan = current_plan(conn, account_id).await?;
    set_plan(conn, account_id, DEFAULT_PLAN_ID).await?;
    clear_billing_state(conn, account_id).await?;
    sqlx::query("DELETE FROM stripe_subscriptions WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *conn)
        .await
        .map_err(CloudError::db("clearing deleted subscription"))?;
    record_transition(
        conn,
        account_id,
        from_plan.as_deref(),
        Some(DEFAULT_PLAN_ID),
        "subscription_deleted",
        None,
    )
    .await?;
    Ok(())
}

/// `invoice.payment_succeeded`: a payment cleared, so any dunning lifts and
/// the account returns to full access. Idempotent — a no-op when already
/// `active`.
pub async fn apply_payment_succeeded(
    control: &ControlPlane,
    account_id: &str,
) -> Result<(), CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to apply payment success",
    ))?;
    apply_payment_succeeded_on_conn(&mut conn, account_id).await
}

/// [`apply_payment_succeeded`] on the caller's connection — the form the
/// webhook calls inside its claim+apply transaction.
pub async fn apply_payment_succeeded_on_conn(
    conn: &mut PgConnection,
    account_id: &str,
) -> Result<(), CloudError> {
    let changed = clear_billing_state(conn, account_id).await?;
    if changed {
        let plan = current_plan(conn, account_id).await?;
        record_transition(
            conn,
            account_id,
            plan.as_deref(),
            plan.as_deref(),
            "payment_succeeded",
            None,
        )
        .await?;
    }
    Ok(())
}

/// `invoice.payment_failed`: enter `past_due` and stamp `past_due_since` if
/// not already past due (plan: "Payment fail (Stripe dunning x3 over 1 week)
/// → Status → past_due"). The time machine ([`advance_dunning`]) escalates
/// from there.
pub async fn apply_payment_failed(
    control: &ControlPlane,
    account_id: &str,
) -> Result<(), CloudError> {
    let mut conn = control.pool().acquire().await.map_err(CloudError::db(
        "acquiring connection to apply payment failure",
    ))?;
    apply_payment_failed_on_conn(&mut conn, account_id).await
}

/// [`apply_payment_failed`] on the caller's connection — the form the webhook
/// calls inside its claim+apply transaction.
pub async fn apply_payment_failed_on_conn(
    conn: &mut PgConnection,
    account_id: &str,
) -> Result<(), CloudError> {
    let changed = enter_past_due(conn, account_id).await?;
    if changed {
        let plan = current_plan(conn, account_id).await?;
        record_transition(
            conn,
            account_id,
            plan.as_deref(),
            plan.as_deref(),
            "payment_failed",
            None,
        )
        .await?;
    }
    Ok(())
}

/// Set `accounts.plan_id`. Used by the subscription/checkout arms.
///
/// This moves only the control-plane plan FK; the account's managed runtime
/// key must be re-sized to the new plan's allowance separately, *after* the
/// webhook transaction commits, via [`reconcile_managed_key_limit`] (the
/// reconcile issues an outbound provider call that must not ride inside the
/// claim+apply transaction — see its docs).
pub(crate) async fn set_plan(
    conn: &mut PgConnection,
    account_id: &str,
    plan_id: &str,
) -> Result<(), CloudError> {
    sqlx::query("UPDATE accounts SET plan_id = $2 WHERE id = $1")
        .bind(account_id)
        .bind(plan_id)
        .execute(&mut *conn)
        .await
        .map_err(CloudError::db("setting account plan"))?;
    Ok(())
}

/// Re-size the account's managed runtime key to its *current* plan's AI
/// allowance (`plans.ai_credits_monthly_cents`; free=50, pro=2000 per
/// migration 010), best-effort. The companion to the plan-changing
/// transitions: a managed key is minted at signup with the free allowance,
/// so without re-applying the cap on every transition a paid/trial account's
/// key stays pinned at 50¢ and its AI dies the moment it spends the free
/// tier (the MAI-1 finding). Call this *after* a transition lands
/// (`set_plan` via the webhook, [`start_trial`], [`finish_expired_trial`]),
/// reading the now-current `plan_id`.
///
/// **Deliberately outside any webhook transaction.** This issues an outbound
/// provider HTTP PATCH; running it inside the webhook's claim+apply
/// transaction would let a Stripe-side timeout roll back the committed plan
/// change (and re-trigger redelivery). The plan transition is authoritative
/// on its own; the key cap is reconciled separately and best-effort
/// ([`ManagedKeys::reconcile_key_limit`] logs-and-continues on a provider
/// error). A control-plane *read* failure (resolving the plan) is the only
/// error surfaced.
///
/// Resolves the allowance via the account's live `plan_id` FK; a NULL/absent
/// plan (a pre-migration-010 row) is logged and skipped rather than guessed.
/// `Disabled` mode and keyless/BYOK accounts are silent no-ops.
pub async fn reconcile_managed_key_limit(
    control: &ControlPlane,
    managed: &ManagedKeys,
    account_id: &str,
) -> Result<(), CloudError> {
    let allowance_cents: Option<i32> = sqlx::query_scalar(
        "SELECT p.ai_credits_monthly_cents \
           FROM accounts a JOIN plans p ON p.id = a.plan_id \
          WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("resolving plan AI-credit allowance"))?;
    let Some(allowance_cents) = allowance_cents else {
        tracing::warn!(
            account_id,
            "no plan AI-credit allowance resolved (NULL/absent plan_id); \
             leaving managed key limit unchanged"
        );
        return Ok(());
    };
    managed
        .reconcile_key_limit(control, account_id, allowance_cents.max(0) as u32)
        .await
}

/// The account's current `plan_id`, for the audit-log `from_plan` field.
pub(crate) async fn current_plan(
    conn: &mut PgConnection,
    account_id: &str,
) -> Result<Option<String>, CloudError> {
    sqlx::query_scalar("SELECT plan_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&mut *conn)
        .await
        .map(Option::flatten)
        .map_err(CloudError::db("reading current plan"))
}

/// Move to `past_due`, stamping `past_due_since = NOW()` only on the FIRST
/// transition into a past-due-family state — re-running while already
/// past_due/read_only/suspended must not reset the dunning clock and rescue
/// the account. Returns whether the state actually changed.
async fn enter_past_due(conn: &mut PgConnection, account_id: &str) -> Result<bool, CloudError> {
    let updated = sqlx::query(
        "UPDATE accounts \
            SET billing_state = 'past_due', past_due_since = NOW() \
          WHERE id = $1 AND billing_state = 'active'",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("entering past_due"))?
    .rows_affected();
    Ok(updated > 0)
}

/// Clear back to `active`, wiping `past_due_since`. Returns whether the state
/// changed (so a payment-succeeded webhook that arrives while already active
/// records no spurious transition).
async fn clear_billing_state(
    conn: &mut PgConnection,
    account_id: &str,
) -> Result<bool, CloudError> {
    let updated = sqlx::query(
        "UPDATE accounts \
            SET billing_state = 'active', past_due_since = NULL \
          WHERE id = $1 AND billing_state <> 'active'",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("clearing billing state"))?
    .rows_affected();
    Ok(updated > 0)
}

/// One advance of the time-driven dunning ladder, at `now` (plan: 3 days →
/// read_only, 14 days → suspended). Reaper-style: a single set of conditional
/// UPDATEs over every past-due account, each guarded so it only ever advances
/// (never rescues), and each comparing `past_due_since` against `now` minus
/// the threshold. Returns how many accounts moved into each state.
///
/// `now` is explicit so a test can drive the ladder by manufacturing a past
/// `past_due_since` via SQL and calling with the real clock — or by passing a
/// future `now` — with no real waits. Suspended advances first so a deeply
/// overdue account lands in `suspended` in one pass rather than stopping at
/// `read_only`.
pub async fn advance_dunning(
    control: &ControlPlane,
    now: DateTime<Utc>,
) -> Result<DunningAdvance, CloudError> {
    advance_dunning_with(control, now, DunningThresholds::default()).await
}

/// [`advance_dunning`] with explicit [`DunningThresholds`] — the form `serve`
/// calls so its `--dunning-read-only-days` / `--dunning-suspended-days` flags
/// take effect. The two functions share one body; `advance_dunning` is the
/// default-thresholds convenience the unit tests use.
pub async fn advance_dunning_with(
    control: &ControlPlane,
    now: DateTime<Utc>,
    thresholds: DunningThresholds,
) -> Result<DunningAdvance, CloudError> {
    let suspend_horizon = now - chrono::Duration::days(thresholds.suspended_after_days);
    let read_only_horizon = now - chrono::Duration::days(thresholds.read_only_after_days);

    // 14+ days: suspend (from past_due or read_only). Data retained.
    let suspended = sqlx::query(
        "UPDATE accounts SET billing_state = 'suspended' \
          WHERE billing_state IN ('past_due', 'read_only') \
            AND past_due_since IS NOT NULL \
            AND past_due_since <= $1",
    )
    .bind(suspend_horizon)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("advancing dunning to suspended"))?
    .rows_affected();

    // 3-14 days: read_only (from past_due only — never downgrade suspended).
    let read_only = sqlx::query(
        "UPDATE accounts SET billing_state = 'read_only' \
          WHERE billing_state = 'past_due' \
            AND past_due_since IS NOT NULL \
            AND past_due_since <= $1",
    )
    .bind(read_only_horizon)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("advancing dunning to read_only"))?
    .rows_affected();

    if suspended > 0 || read_only > 0 {
        tracing::info!(suspended, read_only, "dunning advance");
    }
    Ok(DunningAdvance {
        moved_to_read_only: read_only,
        moved_to_suspended: suspended,
    })
}

/// What one [`advance_dunning`] pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DunningAdvance {
    pub moved_to_read_only: u64,
    pub moved_to_suspended: u64,
}

impl DunningAdvance {
    /// Whether the pass changed anything (for quiet logging).
    pub fn is_quiet(self) -> bool {
        self.moved_to_read_only == 0 && self.moved_to_suspended == 0
    }
}

/// Default trial length: 14 days of the paid tier on signup, no card (plan:
/// "Trials"). Exposed so the wiring (and the trial-start config) share one
/// constant; the actual duration is configurable on the account plane.
pub const DEFAULT_TRIAL_DAYS: i64 = 14;

/// Start a free trial for an account — the plan's "14 days of paid tier on
/// signup, no card required" made executable (plan: "Trials"). Sets
/// `billing_state = 'trialing'`, `trial_ends_at = now + duration`, and
/// `plan_id = trial_plan_id` (the paid tier the trial grants).
///
/// **First-time only, and idempotent.** The UPDATE is guarded to fire only
/// for a pristine account — one still `billing_state = 'active'`, on the free
/// plan, that has never started a trial (`trial_ends_at IS NULL`). That makes
/// it safe to call unconditionally from signup completion, which can re-run
/// on resume (`crate::provision::provision_account` is idempotent, and so is
/// this): a second call after the trial is running, converted, or expired
/// matches zero rows and changes nothing. It also means an account that
/// already paid (a subscription webhook moved it off `free`) is never
/// silently reset into a trial. Returns whether a trial was actually started.
///
/// Trial start lives at **signup completion** (`crate::account_plane`), not in
/// `provision_account`: a trial is a product/onboarding decision (it can be
/// turned off, its length tuned), while provisioning is the low-level tenant
/// bring-up that the reaper also re-runs. Keeping the trial out of
/// `provision_account` leaves every resume/recovery path free of trial side
/// effects. Auto-downgrade is the time-driven [`advance_expired_trials`] arm.
///
/// This promotes the account's plan but does not touch its managed runtime
/// key; the caller (signup completion) must follow a `true` return with
/// [`reconcile_managed_key_limit`] so the key cap moves from the free
/// allowance to the trial tier's — otherwise the trial account's AI dies at
/// the free 50¢ cap (the MAI-1 finding).
pub async fn start_trial(
    control: &ControlPlane,
    account_id: &str,
    trial_plan_id: &str,
    duration: chrono::Duration,
) -> Result<bool, CloudError> {
    let mut conn = control
        .pool()
        .acquire()
        .await
        .map_err(CloudError::db("acquiring connection to start trial"))?;
    let from_plan = current_plan(&mut conn, account_id).await?;
    let started = sqlx::query(
        "UPDATE accounts \
            SET billing_state = 'trialing', \
                trial_ends_at = NOW() + $2, \
                plan_id = $3 \
          WHERE id = $1 \
            AND billing_state = 'active' \
            AND plan_id = $4 \
            AND trial_ends_at IS NULL",
    )
    .bind(account_id)
    .bind(duration)
    .bind(trial_plan_id)
    .bind(DEFAULT_PLAN_ID)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("starting trial"))?
    .rows_affected();
    if started > 0 {
        record_transition(
            &mut conn,
            account_id,
            from_plan.as_deref(),
            Some(trial_plan_id),
            "trial_started",
            None,
        )
        .await?;
    }
    Ok(started > 0)
}

/// Every account whose trial has expired at `now`: still `trialing`, with a
/// `trial_ends_at` in the past. Returned for the sweep to resolve each one's
/// over-limit status against its tenant database before downgrading
/// ([`finish_expired_trial`]) — the control plane alone can't read the atom/KB
/// count, so the tenant-aware decision is made by the caller and fed back in.
pub async fn expired_trials(
    control: &ControlPlane,
    now: DateTime<Utc>,
) -> Result<Vec<String>, CloudError> {
    // `NOT plan_pinned`: an admin-pinned plan holds — the sweep never even
    // considers a comped account, so no downgrade and no key resize.
    sqlx::query_scalar(
        "SELECT id FROM accounts \
          WHERE billing_state = 'trialing' \
            AND trial_ends_at IS NOT NULL \
            AND trial_ends_at <= $1 \
            AND NOT plan_pinned",
    )
    .bind(now)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing expired trials"))
}

/// Finish one expired trial: drop to the free plan and clear the trialing
/// state (plan: "Auto-downgrade to free after"). The post-trial serving state
/// depends on whether the now-free account is over the free plan's limits:
///
/// - **under limits** → `billing_state = 'active'`: full access on free.
/// - **over limits** → `billing_state = 'read_only'`: writes blocked until the
///   user deletes data or upgrades, exactly the subscription-deleted
///   over-limit rule (plan: "Plan transitions" → "Drops to free plan; if over
///   free limits, read-only until under"). The over-limit data is **retained**
///   — never deleted (the cardinal rule).
///
/// `over_free_limits` is computed by the caller from the tenant database (the
/// live atom/KB count vs the free plan's limits). Guarded to only act on a
/// row still `trialing` (so a converted/already-finished trial is a no-op),
/// which keeps it idempotent under a concurrent sweep on another pod.
///
/// This downgrades the plan but does not touch the managed runtime key; the
/// sweep ([`advance_expired_trials`]) follows a `true` return with
/// [`reconcile_managed_key_limit`] so the key cap drops back to the free
/// allowance (sized correctly for a now-free account).
pub async fn finish_expired_trial(
    control: &ControlPlane,
    account_id: &str,
    over_free_limits: bool,
) -> Result<bool, CloudError> {
    let target_state = if over_free_limits {
        BillingState::ReadOnly
    } else {
        BillingState::Active
    };
    let mut conn = control
        .pool()
        .acquire()
        .await
        .map_err(CloudError::db("acquiring connection to finish trial"))?;
    let from_plan = current_plan(&mut conn, account_id).await?;
    let downgraded = sqlx::query(
        "UPDATE accounts \
            SET plan_id = $2, \
                billing_state = $3, \
                trial_ends_at = NULL \
          WHERE id = $1 AND billing_state = 'trialing'",
    )
    .bind(account_id)
    .bind(DEFAULT_PLAN_ID)
    .bind(target_state.as_str())
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("finishing expired trial"))?
    .rows_affected();
    if downgraded > 0 {
        record_transition(
            &mut conn,
            account_id,
            from_plan.as_deref(),
            Some(DEFAULT_PLAN_ID),
            "trial_expired",
            Some(target_state.as_str()),
        )
        .await?;
    }
    Ok(downgraded > 0)
}

/// One pass of trial auto-downgrade at `now`: find every expired trial and
/// downgrade each to the free plan, going `read_only` when the now-free
/// account is over the free limits (plan: "Auto-downgrade to free after";
/// "Drops to free plan; if over free limits, read-only until under"). Data is
/// **never deleted**.
///
/// `over_free_limits` is an async predicate the caller supplies — it reads the
/// account's tenant database (live atom/KB count) against the free plan, work
/// the control plane can't do alone. Threading it as a closure keeps this
/// function's control-plane logic unit-testable ([`finish_expired_trial`] is
/// driven directly with an explicit `over_free_limits` in tests) while the
/// `serve` wiring supplies the real tenant-aware check. A predicate error for
/// one account is logged and that account is left `trialing` for the next
/// sweep — better than fail-open (downgrading without knowing the limit) or
/// fail-closed (read_only-ing a possibly-under-limit account).
///
/// `managed` lets the sweep re-size each downgraded account's managed
/// runtime key back to the free allowance ([`reconcile_managed_key_limit`],
/// best-effort) the moment its plan drops — without it the key would stay
/// pinned at the (larger) trial-tier cap. Reconciliation runs only after the
/// `finish_expired_trial` UPDATE actually moved a row, and a provider error
/// is logged-and-swallowed inside the reconcile, so a reconcile failure can
/// never abort the rest of the pass.
pub async fn advance_expired_trials<F, Fut>(
    control: &ControlPlane,
    managed: &ManagedKeys,
    now: DateTime<Utc>,
    mut over_free_limits: F,
) -> Result<TrialAdvance, CloudError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<bool, CloudError>>,
{
    let mut advance = TrialAdvance::default();
    for account_id in expired_trials(control, now).await? {
        let over = match over_free_limits(account_id.clone()).await {
            Ok(over) => over,
            Err(e) => {
                tracing::error!(account_id, error = %e, "trial over-limit check failed; deferring downgrade");
                continue;
            }
        };
        if finish_expired_trial(control, &account_id, over).await? {
            // The plan just dropped to free; re-size the managed key to the
            // free allowance (best-effort — see reconcile_managed_key_limit).
            reconcile_managed_key_limit(control, managed, &account_id).await?;
            if over {
                advance.downgraded_to_read_only += 1;
            } else {
                advance.downgraded_to_active += 1;
            }
        }
    }
    if !advance.is_quiet() {
        tracing::info!(?advance, "trial auto-downgrade");
    }
    Ok(advance)
}

/// What one [`advance_expired_trials`] pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrialAdvance {
    /// Expired trials dropped to free, under the free limits (full access).
    pub downgraded_to_active: u64,
    /// Expired trials dropped to free but over the free limits (writes
    /// blocked until under; data retained).
    pub downgraded_to_read_only: u64,
}

impl TrialAdvance {
    /// Whether the pass changed anything (for quiet logging).
    pub fn is_quiet(self) -> bool {
        self.downgraded_to_active == 0 && self.downgraded_to_read_only == 0
    }
}

/// Default cadence for the dunning sweep loop in `serve` — hourly is ample
/// for day-granularity thresholds. Exposed so the wiring (and any future CLI
/// knob) shares one constant.
pub const DEFAULT_DUNNING_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_state_round_trips_and_gates() {
        for state in [
            BillingState::Active,
            BillingState::Trialing,
            BillingState::PastDue,
            BillingState::ReadOnly,
            BillingState::Suspended,
        ] {
            assert_eq!(state.as_str().parse::<BillingState>().unwrap(), state);
        }
        assert!(BillingState::Suspended.blocks_serving());
        assert!(!BillingState::ReadOnly.blocks_serving());
        assert!(!BillingState::Trialing.blocks_serving());
        assert!(BillingState::ReadOnly.blocks_writes());
        assert!(!BillingState::Suspended.blocks_writes()); // serving-blocked instead
        assert!(!BillingState::PastDue.blocks_writes());
        assert!(!BillingState::Active.blocks_writes());
        // A trial is full access — neither serving nor writes are blocked.
        assert!(!BillingState::Trialing.blocks_writes());

        // Unknown column degrades to active (never block over corruption).
        assert_eq!(billing_state_from_column("garbage"), BillingState::Active);
        assert_eq!(
            billing_state_from_column("read_only"),
            BillingState::ReadOnly
        );
    }
}
