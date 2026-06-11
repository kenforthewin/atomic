//! Cloud-owned routes on the **tenant plane** (per-account subdomains).
//!
//! Most of the tenant plane *is* atomic-server's `api_scope()` under
//! [`CloudAuth`](crate::auth::CloudAuth); this module holds the routes that
//! have no self-hosted counterpart because they act on the **account** —
//! control-plane state — rather than on knowledge-base data. One route so
//! far:
//!
//! # `DELETE /api/account` (plan: "Provisioning lifecycle" → "Account deletion")
//!
//! Hard-deletes the authenticated account. It lives on the tenant subdomain
//! — deletion is an action on *this account* — and CloudAuth's host routing
//! is what binds the request to it: the route is unreachable from the app
//! host (no subdomain label → CloudAuth 404s before any handler) and
//! unreachable for other tenants (their credentials don't verify here — the
//! cross-tenant chokepoint). The e2e suite pins both directions anyway.
//!
//! Two refusals run before anything is touched, in order:
//!
//! - **Scope** — only account-scope credentials may destroy the account:
//!   account-scope tokens and web sessions (sessions always carry account
//!   scope; see [`crate::auth`]). Database- and MCP-scoped tokens get 403 —
//!   a KB-pinned integration must never be able to take the whole account
//!   down with it.
//! - **Confirmation** — the body must be `{"confirm": "<subdomain>"}`
//!   naming the account's own subdomain, so a stray DELETE (wrong host in a
//!   script, the shared base-domain cookie riding along) can't fire. A
//!   missing, malformed, or mismatched body is a 400.
//!
//! Then the sequence: [`delete_account`] → [`AccountCache::evict`] → the
//! WebSocket severing falls out of the eviction (below). A repeat DELETE
//! after success never reaches the handler: the accounts row is gone, so
//! CloudAuth answers 404 at step 2.
//!
//! # Cancellation-proofing: the work runs in a spawned task
//!
//! actix drops a handler's future the moment the client disconnects. The
//! deletion sequence destroys the account's credentials *first* (token
//! revocation, session deletion — deliberately, to close the
//! still-authenticated crash window) and then does multi-second work
//! (terminating backends, dropping the tenant database). A future dropped
//! between those would strand a zombie: `status = 'active'`, credentials
//! revoked, tenant database possibly gone — an account its owner can no
//! longer drive to completion with the credential they just used. So the
//! handler spawns the delete + evict sequence as a detached task
//! (`actix_web::rt::spawn` onto the worker's LocalSet) and awaits its
//! `JoinHandle`: a disconnect cancels only the *await*, while the spawned
//! task runs to completion regardless. Only a process death can interrupt
//! the sequence now — and that residue is exactly what the reaper's
//! interrupted-deletion arm detects (an active account with no
//! `account_databases` row) and completes.
//!
//! # Ordering: delete first, evict second
//!
//! The plan's deletion sequence lists cache eviction (step 5) before the
//! database drop (step 7); in-process the safe order is the reverse, and
//! [`delete_account`] makes it equivalent:
//!
//! - Evict-then-delete leaves a window where a concurrent authenticated
//!   request (tokens still verify until `delete_account`'s first step)
//!   re-warms the cache mid-drop — resurrecting a pool, and a fresh event
//!   channel nobody will ever sever, for an account that's about to be gone.
//! - Delete-then-evict closes that window: once `delete_account` returns,
//!   the accounts row is gone (CloudAuth 404s) and the `account_databases`
//!   rows are gone (a cache rebuild fails typed), so the eviction is final.
//!   The pooled connections an un-evicted entry held during the drop are
//!   handled inside `delete_account` — `pg_terminate_backend` plus
//!   `DROP DATABASE … WITH (FORCE)`.
//!
//! # WebSocket severing
//!
//! Eviction drops the cache entry, and with it the entry's broadcast
//! `Sender`. When the last remaining clone goes — typically this DELETE
//! request's own `RequestEventChannel` extension, dropped with the response
//! — every `/ws` session's `Receiver` yields `RecvError::Closed`;
//! `ws::start_event_session`'s forwarding loop breaks on exactly that,
//! drops its `actix_ws::Session`, and the connection terminates (the
//! session channel closing ends the response's streaming body). No
//! cloud-side reaping of sessions is needed. Deletion deliberately bypasses
//! the live-receiver eviction pinning — pinning protects *idle* entries
//! from TTL eviction; it must never protect a deleted account's resources
//! ([`AccountCache::evict`] is unconditional by contract, and the e2e suite
//! pins the combination).
//!
//! [`delete_account`]: crate::provision::delete_account
//! [`AccountCache::evict`]: crate::account_cache::AccountCache::evict

use std::sync::Arc;

use actix_web::middleware::from_fn;
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::Deserialize;

use crate::account_cache::AccountCache;
use crate::auth::{CloudAuth, ResolvedTenant};
use crate::control_plane::ControlPlane;
use crate::provision::{delete_account, ClusterConfig};
use crate::server::cloud_plane_guard;
use crate::tokens::TokenScope;

/// Everything the tenant-plane handlers need, shared across workers.
struct PlaneState {
    control: ControlPlane,
    /// The shared tenant cluster, where `DELETE /api/account` drops the
    /// tenant database.
    cluster: ClusterConfig,
    /// The serving cache this process resolves tenants through; deletion
    /// must evict from *this* cache or the dropped account's entry (and the
    /// WebSocket channel it owns) would linger until the idle TTL.
    cache: Arc<AccountCache>,
}

/// The cloud-owned tenant-plane routes as a registrable unit: construct
/// once, hand a clone to every worker's `configure_cloud_app` call. Cheap to
/// clone.
#[derive(Clone)]
pub struct TenantPlane {
    state: web::Data<PlaneState>,
}

impl TenantPlane {
    /// Build the plane over the same control plane, cluster, and cache the
    /// rest of the composition serves from.
    pub fn new(control: ControlPlane, cluster: ClusterConfig, cache: Arc<AccountCache>) -> Self {
        Self {
            state: web::Data::new(PlaneState {
                control,
                cluster,
                cache,
            }),
        }
    }

    /// Register the tenant-plane routes on `cfg`, each behind `auth` (and
    /// the plane guard, mirroring the cloud `/ws` route). Called by
    /// `configure_cloud_app` **before** atomic-server's `api_scope()` so the
    /// exact-path resources here win the route match.
    pub(crate) fn configure(&self, cfg: &mut web::ServiceConfig, auth: CloudAuth) {
        cfg.service(
            web::resource("/api/account")
                .app_data(self.state.clone())
                .route(web::delete().to(delete_account_route))
                // Later-registered wrap runs first: auth resolves the
                // tenant, then the guard verifies the extensions exist.
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth),
        );
    }
}

/// Confirmation body for `DELETE /api/account`. Extracted as
/// `Option<web::Json<…>>` so a missing or malformed body produces this
/// module's structured 400 instead of actix's default deserialization
/// error.
#[derive(Deserialize)]
struct DeleteAccountRequest {
    /// Must equal the subdomain the request arrived on.
    confirm: String,
}

/// `DELETE /api/account` (tenant subdomain only). See the module docs for
/// the refusal order and the delete→evict sequencing rationale.
async fn delete_account_route(
    req: HttpRequest,
    state: web::Data<PlaneState>,
    body: Option<web::Json<DeleteAccountRequest>>,
) -> HttpResponse {
    // CloudAuth installs the extension on every request it passes; like
    // cloud_plane_guard, treat its absence as a composition bug and fail
    // closed rather than guess at an identity.
    let Some(tenant) = req.extensions().get::<ResolvedTenant>().cloned() else {
        tracing::error!("account deletion reached the handler without a resolved tenant");
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "tenant_not_resolved",
            "message": "The request was not resolved to an account.",
        }));
    };

    if tenant.principal.scope != TokenScope::Account {
        return account_scope_required();
    }

    let confirmed = body
        .map(web::Json::into_inner)
        .is_some_and(|b| b.confirm == tenant.subdomain);
    if !confirmed {
        return confirmation_required(&tenant.subdomain);
    }

    // Spawn the destructive sequence and await the handle (module docs:
    // "Cancellation-proofing"): a client disconnect drops this handler
    // future, but the spawned task — which has already revoked the
    // account's credentials by its first step — keeps running to
    // completion. Evict happens inside the same task, after the delete
    // (module docs): the account rows are gone by then, so nothing can
    // rebuild the entry behind it; dropping the entry drops its event
    // channel's Sender, which is what severs the account's live WebSocket
    // sessions once the request-scoped clones unwind.
    // `actix_web::rt::spawn` (a `spawn_local` onto this worker's LocalSet)
    // rather than `tokio::spawn`: the detachment semantics are identical —
    // dropping this handler's future does not cancel the spawned task — and
    // it sidesteps rustc's overly-conservative `Send` analysis of sqlx
    // futures (the same limitation noted in tests/provisioning.rs).
    let task_state = state.clone();
    let account_id = tenant.principal.account_id.clone();
    let outcome = actix_web::rt::spawn(async move {
        delete_account(&task_state.control, &task_state.cluster, &account_id).await?;
        task_state.cache.evict(&account_id).await;
        Ok::<(), crate::error::CloudError>(())
    })
    .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(join_error) => {
            tracing::error!(
                account_id = tenant.principal.account_id,
                error = %join_error,
                "account deletion task panicked"
            );
            return deletion_failed();
        }
    };
    if let Err(e) = outcome {
        tracing::error!(
            account_id = tenant.principal.account_id,
            subdomain = tenant.subdomain,
            error = %e,
            "account deletion failed"
        );
        return deletion_failed();
    }

    tracing::info!(
        account_id = tenant.principal.account_id,
        subdomain = tenant.subdomain,
        source = tenant.principal.source.as_str(),
        "account deleted via HTTP route"
    );
    HttpResponse::Ok().json(serde_json::json!({
        "status": "deleted",
        "subdomain": tenant.subdomain,
    }))
}

// --- Denial responses -------------------------------------------------------

/// The credential is real but not allowed to destroy the account: database-
/// and MCP-scoped tokens are pinned to a knowledge base, and account
/// deletion is strictly above their station.
fn account_scope_required() -> HttpResponse {
    HttpResponse::Forbidden().json(serde_json::json!({
        "error": "account_scope_required",
        "message": "Account deletion requires an account-scope token or a web session.",
    }))
}

/// Missing, malformed, or mismatched confirmation body. The message names
/// the expected value — the caller already proved they control the account,
/// so this leaks nothing; it just makes a deliberate retry easy and an
/// accidental one impossible.
fn confirmation_required(subdomain: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({
        "error": "confirmation_mismatch",
        "message": format!(
            "Deleting this account is permanent. \
             Send {{\"confirm\": {subdomain:?}}} to proceed."
        ),
    }))
}

/// `delete_account` failed partway. The credential that authenticated this
/// request is likely already revoked (revocation is deletion's first step),
/// so the body must NOT advise retrying with it. The honest message:
/// recovery is automatic — anything left half-deleted is reaper territory
/// (the interrupted-deletion arm; see [`crate::reaper`]) — and if the
/// account is still reachable, a fresh login link mints a fresh credential
/// to retry with.
fn deletion_failed() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({
        "error": "deletion_failed",
        "message": "Something went wrong deleting the account. Cleanup \
                    completes automatically in the background; if the \
                    account is still reachable, request a fresh login link \
                    to sign in and try again.",
    }))
}
