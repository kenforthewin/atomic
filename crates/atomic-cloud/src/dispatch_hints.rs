//! Dispatch hints — the control-plane "pending work" bit for the
//! cross-tenant ledger scan (plan: "Worker fairness & job queue" →
//! "Cross-tenant ledger scan").
//!
//! The durable work ledgers (`atom_pipeline_jobs`, `task_runs`) live inside
//! each tenant database, so a dispatcher that polled every tenant on every
//! tick would pay N connections per pass for mostly-idle tenants. The hint
//! table inverts that: `dispatch_hints` holds one control-plane row per
//! account that *may* have pending ledger work, and the dispatcher's fast
//! path polls only hinted tenants ([`list_hinted_accounts`]), clearing the
//! hint when a tenant's ledgers come back empty.
//!
//! # Who writes hints
//!
//! [`mark_hint_on_mutation`], a middleware on the tenant plane's data-plane
//! scope (`/api/*` under CloudAuth), UPSERTs the hint **after** any
//! mutating-method request (POST/PUT/PATCH/DELETE) completes. Deliberately
//! coarse-grained: not every mutation enqueues ledger work, but a
//! false-positive hint costs exactly one empty poll (and is cleared by it),
//! while teaching the middleware which of ~78 routes enqueue — and keeping
//! that list in sync with atomic-server — would be a standing correctness
//! liability. The mark runs after the handler so the ledger write is
//! already durable when the hint lands (see the loss bound below); it is
//! unconditional on the response status, since a handler can fail *after*
//! enqueueing.
//!
//! Background enqueuers that don't pass through the data plane (the
//! dispatcher's own task executions, e.g. a feed poll creating atoms) are
//! the dispatcher's own writes — it marks the hint itself when it enqueues
//! follow-on work (next phase).
//!
//! # The dual-write loss bound
//!
//! The ledger row (tenant database) and the hint (control plane) are two
//! unrelated writes — no transaction spans them. Two failure shapes, both
//! bounded:
//!
//! - **Hint write fails** (control plane hiccup): the work sits in the
//!   ledger unhinted until the next mutation for that tenant — or, at the
//!   latest, the dispatcher's **slow-path full scan**, which sweeps ALL
//!   active accounts ([`list_active_account_ids`]) regardless of hints on a
//!   configured interval (default 5 minutes; the dispatcher phase owns the
//!   loop).
//! - **Hint written during a scan**: the dispatcher read the hints at scan
//!   start, found the tenant's ledger empty, and wants to clear — but a
//!   request enqueued new work in between. [`clear_hint_if_older`] only
//!   deletes a hint whose `last_enqueued_at` is no newer than the value the
//!   scan read, so the mid-scan hint survives and the next pass picks the
//!   work up. This is why the mark must follow the ledger write: marking
//!   first would let a scan observe the hint, find the ledger still empty,
//!   and clear it before the work lands.

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::Method;
use actix_web::middleware::Next;
use actix_web::{web, HttpMessage};
use chrono::{DateTime, Utc};

use crate::auth::ResolvedTenant;
use crate::control_plane::ControlPlane;
use crate::error::CloudError;

/// One `dispatch_hints` row: an account that may have pending ledger work,
/// stamped with the control-plane time of its most recent enqueue-adjacent
/// mutation. The stamp is what [`clear_hint_if_older`] compares against.
#[derive(Debug, Clone)]
pub struct DispatchHint {
    pub account_id: String,
    pub last_enqueued_at: DateTime<Utc>,
}

/// UPSERT the account's hint, bumping `last_enqueued_at` to the control
/// plane's `NOW()` — one clock domain for both the stamp and the scan
/// reads, so [`clear_hint_if_older`]'s comparison never mixes clocks.
///
/// A foreign-key violation (the account was deleted between the caller's
/// auth and this write) is success: a hint for a dead account is
/// meaningless, and the deletion's CASCADE would have swept it anyway.
pub async fn mark_hint(control: &ControlPlane, account_id: &str) -> Result<(), CloudError> {
    let result = sqlx::query(
        "INSERT INTO dispatch_hints (account_id, last_enqueued_at) VALUES ($1, NOW()) \
         ON CONFLICT (account_id) DO UPDATE SET last_enqueued_at = NOW()",
    )
    .bind(account_id)
    .execute(control.pool())
    .await;
    match result {
        Ok(_) => Ok(()),
        // 23503 foreign_key_violation: the account vanished mid-request.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => {
            tracing::debug!(account_id, "dispatch hint skipped: account deleted");
            Ok(())
        }
        Err(e) => Err(CloudError::db("marking dispatch hint")(e)),
    }
}

/// Every hinted account, oldest stamp first — the dispatcher's fast-path
/// scan set. Each entry carries the `last_enqueued_at` the caller must hand
/// back to [`clear_hint_if_older`] when that tenant's ledgers turn out
/// empty.
pub async fn list_hinted_accounts(control: &ControlPlane) -> Result<Vec<DispatchHint>, CloudError> {
    let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT account_id, last_enqueued_at FROM dispatch_hints ORDER BY last_enqueued_at",
    )
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing dispatch hints"))?;
    Ok(rows
        .into_iter()
        .map(|(account_id, last_enqueued_at)| DispatchHint {
            account_id,
            last_enqueued_at,
        })
        .collect())
}

/// Clear the account's hint, but only if no enqueue has bumped it past
/// `as_of` — the `last_enqueued_at` the caller read when its scan started.
/// Returns whether a row was deleted; `false` means either no hint existed
/// or a newer enqueue arrived mid-scan and the hint must survive (module
/// docs: the dual-write loss bound).
pub async fn clear_hint_if_older(
    control: &ControlPlane,
    account_id: &str,
    as_of: DateTime<Utc>,
) -> Result<bool, CloudError> {
    let result =
        sqlx::query("DELETE FROM dispatch_hints WHERE account_id = $1 AND last_enqueued_at <= $2")
            .bind(account_id)
            .bind(as_of)
            .execute(control.pool())
            .await
            .map_err(CloudError::db("clearing dispatch hint"))?;
    Ok(result.rows_affected() > 0)
}

/// Every active account id, in stable order — the dispatcher's **slow-path
/// full scan** set. Swept on a configured interval (default 5 minutes)
/// regardless of hints, this is the bound on hint loss: ledger work whose
/// hint write failed waits at most one full-scan interval. Non-`active`
/// accounts (provisioning, mid-deletion) are excluded — their tenant
/// databases aren't servable.
pub async fn list_active_account_ids(control: &ControlPlane) -> Result<Vec<String>, CloudError> {
    sqlx::query_scalar("SELECT id FROM accounts WHERE status = 'active' ORDER BY id")
        .fetch_all(control.pool())
        .await
        .map_err(CloudError::db("listing active accounts"))
}

/// State for [`mark_hint_on_mutation`], registered as app data on the
/// data-plane scope by `configure_cloud_app`.
#[derive(Clone)]
pub struct DispatchHintWriter {
    control: ControlPlane,
}

impl DispatchHintWriter {
    pub fn new(control: ControlPlane) -> Self {
        Self { control }
    }
}

/// Data-plane middleware: after a mutating-method request completes, mark
/// the authenticated account's dispatch hint (module docs: "Who writes
/// hints"). Wired inside CloudAuth and the plane guard, so it only ever
/// sees authenticated, routable tenant requests with [`ResolvedTenant`]
/// installed; a missing extension is skipped defensively rather than
/// failed (the guard already fails such requests closed).
///
/// The mark is awaited inline — one UPSERT against the control plane,
/// comparable to the auth lookups every request already makes — so a
/// completed mutation's hint is durably visible to the dispatcher before
/// the response returns. A mark failure is logged and swallowed: the
/// response already reflects the handler's outcome, and the slow-path full
/// scan bounds the hint loss.
pub async fn mark_hint_on_mutation(
    writer: web::Data<DispatchHintWriter>,
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    let mutating = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let account_id = req
        .extensions()
        .get::<ResolvedTenant>()
        .map(|tenant| tenant.principal.account_id.clone());

    let res = next.call(req).await?;

    if mutating {
        if let Some(account_id) = account_id {
            if let Err(e) = mark_hint(&writer.control, &account_id).await {
                tracing::warn!(
                    account_id,
                    error = %e,
                    "failed to mark dispatch hint; the slow-path scan will cover it"
                );
            }
        }
    }
    Ok(res)
}
