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

use crate::billing::SubscriptionState;
use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::plans::DEFAULT_PLAN_ID;

/// Days past_due before writes are blocked (`past_due → read_only`).
pub const READ_ONLY_AFTER_DAYS: i64 = 3;

/// Days past_due before serving is blocked (`read_only → suspended`).
pub const SUSPENDED_AFTER_DAYS: i64 = 14;

/// The serving restriction a billing problem imposes. Stored as text in
/// `accounts.billing_state`; read by CloudAuth (suspended → block) and the
/// billing write-guard (read_only → block writes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingState {
    /// Normal — no restriction.
    Active,
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
async fn record_transition(
    control: &ControlPlane,
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
    .execute(control.pool())
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => Ok(()),
        Err(e) => Err(CloudError::db("recording plan transition")(e)),
    }
}

/// Claim a Stripe webhook event id for one-time processing. Inserts the id
/// into `processed_webhook_events`; returns `true` when THIS call won the
/// insert (a first delivery, so the caller should apply the event) and
/// `false` when the id was already present (a redelivery — Stripe retries
/// until it sees a 2xx and does not guarantee at-most-once delivery, so the
/// caller acks without re-running the side effects).
///
/// This is the idempotency boundary: claiming before applying collapses every
/// redelivery of the same event to a no-op, including the unconditional
/// `checkout`-arm audit row that would otherwise duplicate in
/// `plan_transitions` (plan: "The webhook is the source of truth").
pub async fn claim_webhook_event(
    control: &ControlPlane,
    event_id: &str,
    event_type: &str,
) -> Result<bool, CloudError> {
    let inserted = sqlx::query(
        "INSERT INTO processed_webhook_events (event_id, event_type) \
         VALUES ($1, $2) ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(event_type)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("claiming webhook event"))?
    .rows_affected();
    Ok(inserted > 0)
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
/// customer id. `None` when no `stripe_customers` row maps it (an event for
/// an account we don't know — e.g. a customer created out-of-band).
pub async fn account_for_customer(
    control: &ControlPlane,
    stripe_customer_id: &str,
) -> Result<Option<String>, CloudError> {
    sqlx::query_scalar("SELECT account_id FROM stripe_customers WHERE stripe_customer_id = $1")
        .bind(stripe_customer_id)
        .fetch_optional(control.pool())
        .await
        .map_err(CloudError::db("looking up account by Stripe customer"))
}

/// UPSERT the `stripe_customers` linkage. Called when a checkout completes (the
/// first time we learn an account's Stripe customer id) and idempotent on
/// retry.
pub async fn link_stripe_customer(
    control: &ControlPlane,
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
    .execute(control.pool())
    .await
    .map_err(CloudError::db("linking Stripe customer"))?;
    Ok(())
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
    // Persist the subscription projection.
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
    .execute(control.pool())
    .await
    .map_err(CloudError::db("persisting Stripe subscription"))?;

    let from_plan = current_plan(control, account_id).await?;

    match state.status.as_str() {
        // A live subscription: stamp the plan, clear any dunning.
        "active" | "trialing" => {
            set_plan(control, account_id, &state.plan_id).await?;
            clear_billing_state(control, account_id).await?;
            record_transition(
                control,
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
            enter_past_due(control, account_id).await?;
            record_transition(
                control,
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
            apply_subscription_deleted(control, account_id).await?;
        }
        // Any other Stripe status (incomplete, paused, …): persist the row
        // (done above) but make no serving-state change — the next
        // authoritative event resolves it.
        _ => {}
    }
    Ok(())
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
    let from_plan = current_plan(control, account_id).await?;
    set_plan(control, account_id, DEFAULT_PLAN_ID).await?;
    clear_billing_state(control, account_id).await?;
    sqlx::query("DELETE FROM stripe_subscriptions WHERE account_id = $1")
        .bind(account_id)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("clearing deleted subscription"))?;
    record_transition(
        control,
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
    let changed = clear_billing_state(control, account_id).await?;
    if changed {
        let plan = current_plan(control, account_id).await?;
        record_transition(
            control,
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
    let changed = enter_past_due(control, account_id).await?;
    if changed {
        let plan = current_plan(control, account_id).await?;
        record_transition(
            control,
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
async fn set_plan(
    control: &ControlPlane,
    account_id: &str,
    plan_id: &str,
) -> Result<(), CloudError> {
    sqlx::query("UPDATE accounts SET plan_id = $2 WHERE id = $1")
        .bind(account_id)
        .bind(plan_id)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("setting account plan"))?;
    Ok(())
}

/// The account's current `plan_id`, for the audit-log `from_plan` field.
async fn current_plan(
    control: &ControlPlane,
    account_id: &str,
) -> Result<Option<String>, CloudError> {
    sqlx::query_scalar("SELECT plan_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(control.pool())
        .await
        .map(Option::flatten)
        .map_err(CloudError::db("reading current plan"))
}

/// Move to `past_due`, stamping `past_due_since = NOW()` only on the FIRST
/// transition into a past-due-family state — re-running while already
/// past_due/read_only/suspended must not reset the dunning clock and rescue
/// the account. Returns whether the state actually changed.
async fn enter_past_due(control: &ControlPlane, account_id: &str) -> Result<bool, CloudError> {
    let updated = sqlx::query(
        "UPDATE accounts \
            SET billing_state = 'past_due', past_due_since = NOW() \
          WHERE id = $1 AND billing_state = 'active'",
    )
    .bind(account_id)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("entering past_due"))?
    .rows_affected();
    Ok(updated > 0)
}

/// Clear back to `active`, wiping `past_due_since`. Returns whether the state
/// changed (so a payment-succeeded webhook that arrives while already active
/// records no spurious transition).
async fn clear_billing_state(control: &ControlPlane, account_id: &str) -> Result<bool, CloudError> {
    let updated = sqlx::query(
        "UPDATE accounts \
            SET billing_state = 'active', past_due_since = NULL \
          WHERE id = $1 AND billing_state <> 'active'",
    )
    .bind(account_id)
    .execute(control.pool())
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
    let suspend_horizon = now - chrono::Duration::days(SUSPENDED_AFTER_DAYS);
    let read_only_horizon = now - chrono::Duration::days(READ_ONLY_AFTER_DAYS);

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
            BillingState::PastDue,
            BillingState::ReadOnly,
            BillingState::Suspended,
        ] {
            assert_eq!(state.as_str().parse::<BillingState>().unwrap(), state);
        }
        assert!(BillingState::Suspended.blocks_serving());
        assert!(!BillingState::ReadOnly.blocks_serving());
        assert!(BillingState::ReadOnly.blocks_writes());
        assert!(!BillingState::Suspended.blocks_writes()); // serving-blocked instead
        assert!(!BillingState::PastDue.blocks_writes());
        assert!(!BillingState::Active.blocks_writes());

        // Unknown column degrades to active (never block over corruption).
        assert_eq!(billing_state_from_column("garbage"), BillingState::Active);
        assert_eq!(
            billing_state_from_column("read_only"),
            BillingState::ReadOnly
        );
    }
}
