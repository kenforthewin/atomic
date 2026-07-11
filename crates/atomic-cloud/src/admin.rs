//! The admin plane: operator routes on the **app host** (`/admin/api/*`)
//! plus the plan-override machinery they and the CLI share.
//!
//! # Authorization
//!
//! Admin identity is `accounts.is_admin` (migration 020) — set only via the
//! CLI (`account promote`), never through a tenant-facing route. Requests
//! authenticate by the ordinary web session cookie (the admin logs in like
//! any user); [`require_admin`] resolves the session **without** a subdomain
//! (the app host names no account) and checks the flag. Everything short of
//! a valid admin session answers **404, not 403** — the plane does not
//! confirm its own existence to non-admins.
//!
//! # Plan overrides and the pin
//!
//! [`set_plan_override`] is the one writer for admin plan changes: it stamps
//! `accounts.plan_id` + `plan_pinned`, records the existing
//! `plan_transitions` ledger with `trigger='admin'`, re-sizes the managed
//! OpenRouter key to the new plan's allowance (the same
//! [`reconcile_managed_key_limit`] the billing transitions use), and writes
//! the `admin_actions` audit row. The pin is honored by every automated
//! plan writer (trial sweep, subscription projection/deletion — see
//! [`crate::billing::dunning`]), so a comped account holds until an admin
//! changes it back.
//!
//! Comp tiers are ordinary `plans` catalogue rows: the portal's picker
//! renders from `GET /admin/api/plans`, so adding a tier is a migration,
//! never a code change.

use std::sync::Arc;

use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::account_cache::AccountCache;
use crate::account_plane::app_host_guard;
use crate::auth::SESSION_COOKIE;
use crate::billing::dunning::{current_plan, record_transition, reconcile_managed_key_limit};
use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::managed_keys::ManagedKeys;
use crate::tokens::resolve_session;

/// The admin plane's composition state.
#[derive(Clone)]
pub struct AdminPlane {
    control: ControlPlane,
    cache: Arc<AccountCache>,
    managed: ManagedKeys,
    base_domain: String,
}

impl AdminPlane {
    pub fn new(
        control: ControlPlane,
        cache: Arc<AccountCache>,
        managed: ManagedKeys,
        base_domain: &str,
    ) -> Self {
        Self {
            control,
            cache,
            managed,
            base_domain: base_domain.to_string(),
        }
    }

    /// Register the admin routes, guarded to the **app host** — the admin
    /// plane never exists on a tenant subdomain.
    pub(crate) fn configure(&self, cfg: &mut web::ServiceConfig) {
        let state = web::Data::new(self.clone());
        cfg.service(
            web::scope("/admin/api")
                .guard(app_host_guard(self.base_domain.clone()))
                .app_data(state)
                .route("/accounts", web::get().to(list_accounts_route))
                .route("/accounts/{id}", web::get().to(account_detail_route))
                .route("/accounts/{id}/plan", web::put().to(set_plan_route))
                .route("/accounts/{id}/evict", web::post().to(evict_route))
                .route("/plans", web::get().to(list_plans_route)),
        );
    }
}

/// A verified admin caller.
struct AdminActor {
    account_id: String,
}

/// The uniform non-admin answer: the plane does not exist for you.
fn admin_not_found() -> HttpResponse {
    HttpResponse::NotFound().json(json!({ "error": "not_found" }))
}

/// Resolve the request's session cookie to an admin account, or the 404
/// denial. Fail-closed: no cookie, dead session, unknown account, or a
/// lookup error all read identically as not-found.
async fn require_admin(
    req: &HttpRequest,
    control: &ControlPlane,
) -> Result<AdminActor, HttpResponse> {
    let Some(secret) = req.cookie(SESSION_COOKIE).map(|c| c.value().to_string()) else {
        return Err(admin_not_found());
    };
    let session = match resolve_session(control, &secret).await {
        Ok(Some(session)) => session,
        Ok(None) => return Err(admin_not_found()),
        Err(e) => {
            tracing::error!(error = %e, "admin session resolution failed");
            return Err(admin_not_found());
        }
    };
    let is_admin: Option<bool> = sqlx::query_scalar(
        "SELECT is_admin FROM accounts WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(&session.account_id)
    .fetch_optional(control.pool())
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "admin flag lookup failed");
        None
    });
    if is_admin != Some(true) {
        return Err(admin_not_found());
    }
    Ok(AdminActor {
        account_id: session.account_id,
    })
}

/// Write one `admin_actions` audit row. Best-effort by design: an audit
/// failure is loud in the log but never blocks the action it describes.
pub async fn audit(
    control: &ControlPlane,
    actor: &str,
    action: &str,
    target_account_id: Option<&str>,
    detail: serde_json::Value,
) {
    let result = sqlx::query(
        "INSERT INTO admin_actions (actor, action, target_account_id, detail) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(action)
    .bind(target_account_id)
    .bind(detail)
    .execute(control.pool())
    .await;
    if let Err(e) = result {
        tracing::error!(actor, action, error = %e, "writing admin audit row failed");
    }
}

/// Set (or clear) an admin plan override: validate the plan against the
/// catalogue, stamp `plan_id` + `plan_pinned`, ledger the transition as
/// `trigger='admin'`, and re-size the managed key to the new allowance.
/// The key resize is best-effort like every allowance reconcile — a
/// provisioning-API hiccup logs and leaves the plan change standing.
pub async fn set_plan_override(
    control: &ControlPlane,
    managed: &ManagedKeys,
    actor: &str,
    account_id: &str,
    plan_id: &str,
    pinned: bool,
) -> Result<(), CloudError> {
    let plan_exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM plans WHERE id = $1")
        .bind(plan_id)
        .fetch_optional(control.pool())
        .await
        .map_err(CloudError::db("validating plan id"))?;
    if plan_exists.is_none() {
        return Err(CloudError::Invariant(format!(
            "unknown plan id {plan_id:?}"
        )));
    }

    let mut conn = control
        .pool()
        .acquire()
        .await
        .map_err(CloudError::db("acquiring connection for plan override"))?;
    let from_plan = current_plan(&mut conn, account_id).await?;
    let updated = sqlx::query(
        "UPDATE accounts SET plan_id = $2, plan_pinned = $3 \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(account_id)
    .bind(plan_id)
    .bind(pinned)
    .execute(&mut *conn)
    .await
    .map_err(CloudError::db("applying plan override"))?;
    if updated.rows_affected() == 0 {
        return Err(CloudError::Invariant(format!(
            "no active account {account_id:?}"
        )));
    }
    record_transition(
        &mut conn,
        account_id,
        from_plan.as_deref(),
        Some(plan_id),
        "admin",
        Some(if pinned { "pinned" } else { "unpinned" }),
    )
    .await?;
    drop(conn);

    if let Err(e) = reconcile_managed_key_limit(control, managed, account_id).await {
        tracing::warn!(
            account_id,
            plan_id,
            error = %e,
            "managed-key resize after admin plan override failed; \
             plan change stands, allowance reconciles on the next transition"
        );
    }

    audit(
        control,
        actor,
        "set_plan",
        Some(account_id),
        json!({ "plan_id": plan_id, "pinned": pinned, "from": from_plan }),
    )
    .await;
    Ok(())
}

/// Set or clear `accounts.is_admin` for a subdomain (CLI `account promote`).
pub async fn set_admin_flag(
    control: &ControlPlane,
    subdomain: &str,
    is_admin: bool,
) -> Result<(), CloudError> {
    let updated = sqlx::query(
        "UPDATE accounts SET is_admin = $2 \
         WHERE subdomain = $1 AND deleted_at IS NULL",
    )
    .bind(subdomain)
    .bind(is_admin)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("setting admin flag"))?;
    if updated.rows_affected() == 0 {
        return Err(CloudError::Invariant(format!(
            "no active account with subdomain {subdomain:?}"
        )));
    }
    audit(
        control,
        "cli",
        if is_admin { "promote" } else { "demote" },
        None,
        json!({ "subdomain": subdomain }),
    )
    .await;
    Ok(())
}

// --- Route handlers ----------------------------------------------------------

/// `GET /admin/api/accounts`: every active account with its plan, pin,
/// billing state, and backup freshness — the portal's landing table.
async fn list_accounts_route(req: HttpRequest, state: web::Data<AdminPlane>) -> HttpResponse {
    if let Err(denial) = require_admin(&req, &state.control).await {
        return denial;
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        subdomain: String,
        email: String,
        status: String,
        plan_id: Option<String>,
        plan_pinned: bool,
        is_admin: bool,
        billing_state: String,
        trial_ends_at: Option<chrono::DateTime<chrono::Utc>>,
        created_at: chrono::DateTime<chrono::Utc>,
        last_backup_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows: Result<Vec<Row>, _> = sqlx::query_as(
        "SELECT a.id, a.subdomain, a.email, a.status, a.plan_id, a.plan_pinned, \
                a.is_admin, a.billing_state, a.trial_ends_at, a.created_at, \
                d.last_backup_at \
         FROM accounts a \
         LEFT JOIN account_databases d ON d.account_id = a.id \
         WHERE a.deleted_at IS NULL \
         ORDER BY a.created_at",
    )
    .fetch_all(state.control.pool())
    .await;
    match rows {
        Ok(rows) => HttpResponse::Ok().json(
            rows.into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "subdomain": r.subdomain,
                        "email": r.email,
                        "status": r.status,
                        "plan_id": r.plan_id,
                        "plan_pinned": r.plan_pinned,
                        "is_admin": r.is_admin,
                        "billing_state": r.billing_state,
                        "trial_ends_at": r.trial_ends_at.map(|t| t.to_rfc3339()),
                        "created_at": r.created_at.to_rfc3339(),
                        "last_backup_at": r.last_backup_at.map(|t| t.to_rfc3339()),
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => {
            tracing::error!(error = %e, "admin account listing failed");
            HttpResponse::InternalServerError().json(json!({ "error": "internal_error" }))
        }
    }
}

/// `GET /admin/api/accounts/{id}`: one account plus its recent plan
/// transitions and admin actions — the detail drawer's read.
async fn account_detail_route(
    req: HttpRequest,
    state: web::Data<AdminPlane>,
    path: web::Path<String>,
) -> HttpResponse {
    if let Err(denial) = require_admin(&req, &state.control).await {
        return denial;
    }
    let account_id = path.into_inner();
    let account: Result<Option<serde_json::Value>, CloudError> = async {
        let row: Option<(String, String, String, Option<String>, bool, String)> =
            sqlx::query_as(
                "SELECT subdomain, email, status, plan_id, plan_pinned, billing_state \
                 FROM accounts WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(&account_id)
            .fetch_optional(state.control.pool())
            .await
            .map_err(CloudError::db("reading account"))?;
        let Some((subdomain, email, status, plan_id, plan_pinned, billing_state)) = row else {
            return Ok(None);
        };
        let transitions: Vec<(Option<String>, Option<String>, String, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as(
                "SELECT from_plan_id, to_plan_id, trigger, created_at \
                 FROM plan_transitions WHERE account_id = $1 \
                 ORDER BY created_at DESC LIMIT 10",
            )
            .bind(&account_id)
            .fetch_all(state.control.pool())
            .await
            .map_err(CloudError::db("reading plan transitions"))?;
        Ok(Some(json!({
            "id": account_id,
            "subdomain": subdomain,
            "email": email,
            "status": status,
            "plan_id": plan_id,
            "plan_pinned": plan_pinned,
            "billing_state": billing_state,
            "recent_transitions": transitions.into_iter().map(|(from, to, trigger, at)| json!({
                "from": from, "to": to, "trigger": trigger, "at": at.to_rfc3339(),
            })).collect::<Vec<_>>(),
        })))
    }
    .await;
    match account {
        Ok(Some(body)) => HttpResponse::Ok().json(body),
        Ok(None) => HttpResponse::NotFound().json(json!({ "error": "not_found" })),
        Err(e) => {
            tracing::error!(error = %e, "admin account detail failed");
            HttpResponse::InternalServerError().json(json!({ "error": "internal_error" }))
        }
    }
}

/// `GET /admin/api/plans`: the catalogue, for the portal's plan picker —
/// new (comp) tiers appear here with zero UI changes.
async fn list_plans_route(req: HttpRequest, state: web::Data<AdminPlane>) -> HttpResponse {
    if let Err(denial) = require_admin(&req, &state.control).await {
        return denial;
    }
    let rows: Result<
        Vec<(String, String, i32, Option<i32>, Option<i32>, Option<i64>, i32)>,
        _,
    > = sqlx::query_as(
        "SELECT id, name, monthly_price_cents, atom_limit, kb_limit, \
                storage_bytes_limit, ai_credits_monthly_cents \
         FROM plans ORDER BY monthly_price_cents, id",
    )
    .fetch_all(state.control.pool())
    .await;
    match rows {
        Ok(rows) => HttpResponse::Ok().json(
            rows.into_iter()
                .map(|(id, name, price, atoms, kbs, storage, ai)| {
                    json!({
                        "id": id,
                        "name": name,
                        "monthly_price_cents": price,
                        "atom_limit": atoms,
                        "kb_limit": kbs,
                        "storage_bytes_limit": storage,
                        "ai_credits_monthly_cents": ai,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => {
            tracing::error!(error = %e, "admin plan listing failed");
            HttpResponse::InternalServerError().json(json!({ "error": "internal_error" }))
        }
    }
}

#[derive(Deserialize)]
struct SetPlanBody {
    plan_id: String,
    /// Pin the plan against the automated writers. Defaults to `true`: an
    /// admin assigning a plan almost always means "and keep it there".
    #[serde(default = "default_pin")]
    pinned: bool,
}

fn default_pin() -> bool {
    true
}

/// `PUT /admin/api/accounts/{id}/plan`: the plan override.
async fn set_plan_route(
    req: HttpRequest,
    state: web::Data<AdminPlane>,
    path: web::Path<String>,
    body: web::Json<SetPlanBody>,
) -> HttpResponse {
    let actor = match require_admin(&req, &state.control).await {
        Ok(actor) => actor,
        Err(denial) => return denial,
    };
    let account_id = path.into_inner();
    let body = body.into_inner();
    match set_plan_override(
        &state.control,
        &state.managed,
        &actor.account_id,
        &account_id,
        &body.plan_id,
        body.pinned,
    )
    .await
    {
        Ok(()) => HttpResponse::Ok().json(json!({
            "plan_id": body.plan_id,
            "pinned": body.pinned,
        })),
        Err(CloudError::Invariant(msg)) => {
            HttpResponse::BadRequest().json(json!({ "error": "invalid_request", "message": msg }))
        }
        Err(e) => {
            tracing::error!(error = %e, "admin plan override failed");
            HttpResponse::InternalServerError().json(json!({ "error": "internal_error" }))
        }
    }
}

/// `POST /admin/api/accounts/{id}/evict`: drop the account's serving-cache
/// entry — the restore runbook's step 3, finally reachable without a pod
/// restart.
async fn evict_route(
    req: HttpRequest,
    state: web::Data<AdminPlane>,
    path: web::Path<String>,
) -> HttpResponse {
    let actor = match require_admin(&req, &state.control).await {
        Ok(actor) => actor,
        Err(denial) => return denial,
    };
    let account_id = path.into_inner();
    state.cache.evict(&account_id).await;
    audit(
        &state.control,
        &actor.account_id,
        "evict",
        Some(&account_id),
        json!({}),
    )
    .await;
    HttpResponse::Ok().json(json!({ "evicted": true }))
}
