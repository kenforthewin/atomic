//! Data-plane write-guard for the two "writes blocked, data retained" states:
//! the dunning `read_only` billing state (plan: "Billing" → "3 days past_due
//! → Read-only mode"; "Subscription deleted … read-only until under") and the
//! storage-bytes `restricted` state (plan: "Quotas" → enforcement table:
//! "Periodic reaper | Storage bytes recompute | Week 1 warn; week 2 restrict
//! writes; **no auto-delete**").
//!
//! `suspended` is gated up front in [`CloudAuth`](crate::auth) — the request
//! never reaches a handler. `read_only` and storage-`restricted` are subtler:
//! reads and exports must keep working (the user can still see and retrieve
//! their data; **nothing is deleted**), only *writes* are blocked. That
//! distinction needs the request method, so it lives here as a data-plane
//! middleware that 402s mutating methods while passing reads through. Wired
//! inside CloudAuth and the plane guard (so [`ResolvedTenant`] is installed)
//! and *outside* the dispatch-hint writer, so a blocked write never marks a
//! hint.
//!
//! The two causes are **orthogonal** (a tenant can be over its storage
//! ceiling while current on payment, or vice versa) and recover
//! independently, so each rides its own column on [`ResolvedTenant`]. This
//! guard blocks a write when *either* says so, and reports which: a billing
//! delinquency returns `account_read_only`, a storage overage returns
//! `account_storage_restricted` (so the frontend routes the user to the right
//! remedy — pay vs. delete data/upgrade). Billing is checked first: it is the
//! more time-sensitive remedy.
//!
//! The same pattern the quota and out-of-credits guards use: read the state
//! off [`ResolvedTenant`] (CloudAuth already loaded it from the accounts row
//! this request paid for), so the guard itself does no I/O.

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::{HttpMessage, HttpResponse};

use crate::auth::ResolvedTenant;

/// Whether `method` mutates. The write-guard blocks exactly these while the
/// tenant is `read_only`; GET/HEAD/OPTIONS reads pass.
fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Mutations that stay open even while writes are blocked. A returning
/// `read_only` (or storage-`restricted`) tenant still has to be able to mark
/// onboarding complete — that flag is a per-tenant settings write
/// (`PUT /api/settings/onboarding_completed`), and blocking it traps the user
/// re-looping the first-run wizard with no way out (the remedy — paying or
/// deleting data — lives *past* onboarding). It writes a single, fixed
/// settings key and creates no metered resource, so exempting it costs the
/// write-block nothing while unsticking the recovery flow.
fn is_write_block_exempt(method: &Method, path: &str) -> bool {
    *method == Method::PUT && path == "/api/settings/onboarding_completed"
}

/// Data-plane middleware: while the tenant is blocked for writes by EITHER
/// the dunning `read_only` billing state or the storage `restricted` state,
/// return a structured 402 for mutating requests (module docs). A missing
/// [`ResolvedTenant`] is skipped defensively (the plane guard fails such
/// requests closed already).
pub async fn billing_write_guard(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    // Billing first (the more time-sensitive remedy), then storage. Each
    // rides its own column; either blocks, with a distinct error code.
    let block = req.extensions().get::<ResolvedTenant>().and_then(|t| {
        if t.billing_state.blocks_writes() {
            Some(WriteBlock::Billing)
        } else if t.storage_state.blocks_writes() {
            Some(WriteBlock::Storage)
        } else {
            None
        }
    });

    if let Some(block) = block {
        if is_mutating(req.method()) && !is_write_block_exempt(req.method(), req.path()) {
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .or_else(|| req.uri().host())
                .unwrap_or_default();
            let denial = HttpResponse::PaymentRequired().json(block.body(host));
            return Ok(req.into_response(denial));
        }
    }
    next.call(req).await.map(|res| res.map_into_boxed_body())
}

/// Why a write is blocked, and the structured 402 body each cause returns.
#[derive(Debug, Clone, Copy)]
enum WriteBlock {
    /// Dunning read-only (non-payment): the remedy is updating billing.
    Billing,
    /// Storage over the plan ceiling past the grace window: the remedy is
    /// deleting data or upgrading. Data is retained, never deleted.
    Storage,
}

impl WriteBlock {
    fn body(self, host: &str) -> serde_json::Value {
        match self {
            WriteBlock::Billing => serde_json::json!({
                "error": "account_read_only",
                "message": "This account is read-only for non-payment. Your data is \
                            retained and readable; update your billing to resume editing.",
                "upgrade_url": upgrade_url(host),
            }),
            WriteBlock::Storage => serde_json::json!({
                "error": "account_storage_restricted",
                "message": "This account is over its storage limit. Your data is \
                            retained and readable; delete some data or upgrade your \
                            plan to resume editing.",
                "upgrade_url": upgrade_url(host),
            }),
        }
    }
}

/// `<sub>.<base>` → `https://app.<base>/account/billing` (the dashboard billing
/// route; the app-host billing page a suspended/blocked tenant can still reach).
fn upgrade_url(host: &str) -> String {
    let host = host.split(':').next().unwrap_or(host);
    let base = host.split_once('.').map(|(_, base)| base).unwrap_or(host);
    format!("https://app.{base}/account/billing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mutating_methods_blocked() {
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PUT));
        assert!(is_mutating(&Method::PATCH));
        assert!(is_mutating(&Method::DELETE));
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
        assert!(!is_mutating(&Method::OPTIONS));
    }

    #[test]
    fn onboarding_completion_write_is_exempt() {
        // The returning read_only tenant must be able to finish onboarding,
        // so its settings write passes the write-block…
        assert!(is_write_block_exempt(
            &Method::PUT,
            "/api/settings/onboarding_completed"
        ));
        // …but nothing else: a different settings key, a different method, or
        // any other route stays blocked.
        assert!(!is_write_block_exempt(
            &Method::PUT,
            "/api/settings/ai_provider"
        ));
        assert!(!is_write_block_exempt(
            &Method::POST,
            "/api/settings/onboarding_completed"
        ));
        assert!(!is_write_block_exempt(&Method::POST, "/api/atoms"));
    }

    #[test]
    fn upgrade_url_derives_app_host() {
        assert_eq!(
            upgrade_url("kenny.atomicapp.ai"),
            "https://app.atomicapp.ai/account/billing"
        );
    }
}
