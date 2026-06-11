//! The composed multi-tenant cloud application.
//!
//! [`configure_cloud_app`] assembles atomic-server's granular route pieces
//! under cloud middleware, producing the service tree the `atomic-cloud serve`
//! binary listens on. The composition is deliberately narrower than the
//! self-hosted server's [`configure_app`](atomic_server::app::configure_app):
//!
//! - `GET /health` — public, no auth on any host.
//! - **The account plane** ([`crate::account_plane`]) — `POST
//!   /signup/request-link`, `POST /login/request-link`, `GET
//!   /signup/complete`, and `GET /login/complete`, served only on the
//!   **app host** (the bare base domain or `app.<base>`). No CloudAuth,
//!   no tenant state: these routes exist for people without accounts (or
//!   without a live session). The
//!   host split fails closed in both directions — the routes carry an
//!   app-host guard (404 on tenant subdomains), and tenant routes 404 on
//!   the app host because `CloudAuth` extracts no subdomain from the bare
//!   base and `app` is blocklisted from ever resolving to an account. See
//!   the account_plane module docs; the e2e suite pins both directions.
//! - **The tenant plane's cloud-owned routes** ([`crate::tenant_plane`]) —
//!   currently `DELETE /api/account`, the authenticated account-deletion
//!   route. Registered *before* atomic-server's `api_scope()` so the
//!   exact-path resource wins the route match; behind [`CloudAuth`] like
//!   everything else on a tenant subdomain.
//! - `GET /ws` — a **cloud-owned** WebSocket route under [`CloudAuth`].
//!   Self-hosted's public `/ws` route authenticates a `?token=` query
//!   parameter against the tenant's own `api_tokens` table — exactly the
//!   identity plane cloud replaces — so cloud routes its own handler straight
//!   to [`ws::start_event_session`], streaming the per-account event channel
//!   the middleware injected.
//! - `/api/*` — atomic-server's full route table
//!   ([`api_scope`](atomic_server::app::api_scope)) wrapped in [`CloudAuth`]
//!   (in place of self-hosted's `BearerAuth`) plus [`cloud_plane_guard`].
//!
//! Deliberately **not** registered, with their replacements landing in later
//! slices (plan: `docs/plans/atomic-cloud.md`):
//!
//! - `configure_public_routes` — its OAuth discovery/flow, instance setup,
//!   self-hosted `/ws`, API docs, and export download all assume the
//!   single-tenant identity model. Cloud OAuth is per-account and lives in
//!   atomic-cloud when the OAuth/MCP slice arrives.
//! - `mcp_scope` — the MCP transport binds one `DatabaseManager` and one
//!   event channel at construction, which cannot express per-request tenant
//!   resolution. Cloud MCP arrives with the OAuth/MCP slice.
//! - `/api/auth/*` (inside `api_scope`) is the self-hosted token plane; it
//!   operates on the composition-time [`AppState`] manager rather than the
//!   request's tenant. Cloud tokens live in the control plane
//!   ([`crate::tokens`]), so [`cloud_plane_guard`] unroutes the family
//!   entirely (404).
//! - The **export-job family** (inside `api_scope`:
//!   `POST /api/databases/{id}/exports/markdown`, `GET|DELETE
//!   /api/exports/{id}`) reads `state.export_jobs` — under cloud, the single
//!   inert fallback's `ExportJobManager`, one process-global namespace of
//!   job ids and artifacts shared by every tenant. Any authenticated tenant
//!   could fetch or delete another tenant's export by id, so
//!   [`cloud_plane_guard`] unroutes the family (404) until a per-tenant
//!   export story exists.
//! - **`GET /api/logs`** (inside `api_scope`) reads `state.log_buffer` —
//!   likewise a single process-wide ring buffer, not a per-tenant log
//!   stream. Unrouted (404) for the same reason.
//!
//! # The fallback `AppState` decision
//!
//! atomic-server's handlers and extractors require a `web::Data<AppState>`
//! in app data even when every request carries the
//! [`RequestDatabaseManager`] / [`RequestEventChannel`] extensions: the `Db`
//! extractor takes the state for its fallback unconditionally (see the
//! `FromRequest` doc in `atomic-server/src/db_extractor.rs`), and several
//! handlers extract `web::Data<AppState>` for `log_buffer` / `export_jobs`
//! (those handlers' routes are unrouted, per the list above).
//! The state's `manager` field cannot be absent, so the cloud composition
//! registers a **dedicated inert fallback** ([`FallbackAppState`]): an empty
//! SQLite scratch store in a process-lifetime temp directory. It is a
//! type-level placeholder, never a serving database:
//!
//! - [`CloudAuth`] installs the tenant extensions on every request it lets
//!   through, so the extractor fallback never fires.
//! - [`cloud_plane_guard`] makes that an invariant rather than a happy-path
//!   property: a request that somehow reaches the route table *without* the
//!   tenant extension is failed closed (500), not served from the fallback.
//! - The route families that bind composition-time state directly —
//!   `/api/auth/*` (manager), the export-job family (`export_jobs`),
//!   `/api/logs` (`log_buffer`), and the public/MCP planes — are unrouted
//!   or 404'd, per the list above.
//!
//! The alternative — teaching atomic-server to serve without an `AppState` —
//! would mean making the state's fields optional across ~78 routes for the
//! benefit of exactly one composition; the inert fallback plus a fail-closed
//! guard gets the same isolation guarantee without touching atomic-server.

use std::sync::Arc;

use actix_web::body::BoxBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::{from_fn, Next};
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use atomic_core::DatabaseManager;
use atomic_server::app::{api_scope, health};
use atomic_server::db_extractor::RequestDatabaseManager;
use atomic_server::event_channel::EventChannel;
use atomic_server::export_jobs::ExportJobManager;
use atomic_server::log_buffer::LogBuffer;
use atomic_server::state::{AppState, SetupClaimLimiter};
use atomic_server::ws;
use tokio::sync::broadcast;

use crate::account_plane::AccountPlane;
use crate::auth::CloudAuth;
use crate::error::CloudError;
use crate::tenant_plane::TenantPlane;

/// The inert [`AppState`] registered as app data in the cloud composition.
///
/// See the module docs for why it must exist and why it is safe: every
/// serving request resolves its tenant through the request extensions
/// installed by [`CloudAuth`], and [`cloud_plane_guard`] fails closed any
/// request that lacks them. The backing store is an empty SQLite scratch
/// database in a temp directory owned by this struct — keep the struct alive
/// for the life of the server (the directory is removed on drop).
pub struct FallbackAppState {
    data: web::Data<AppState>,
    _scratch: tempfile::TempDir,
}

impl FallbackAppState {
    /// Create the scratch store and wrap it in an [`AppState`].
    ///
    /// Nothing is seeded: no API tokens (so nothing could ever verify
    /// against it), no settings, no atoms. `event_tx` is a fresh channel
    /// with no subscribers — the cloud `/ws` route streams the per-account
    /// channel from the request extensions, never this one.
    pub fn build() -> Result<Self, CloudError> {
        let scratch = tempfile::tempdir().map_err(|source| CloudError::Io {
            context: "creating fallback scratch directory".to_string(),
            source,
        })?;
        let manager = DatabaseManager::new(scratch.path())
            .map_err(CloudError::core("opening fallback scratch database"))?;
        let export_jobs = ExportJobManager::new(scratch.path().join("exports"))
            .map_err(CloudError::core("initializing export job manager"))?;
        let (event_tx, _) = broadcast::channel(16);
        let data = web::Data::new(AppState {
            manager: Arc::new(manager),
            event_tx,
            public_url: None,
            log_buffer: LogBuffer::new(16),
            export_jobs,
            setup_token: None,
            dangerously_skip_setup_token: false,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: SetupClaimLimiter::new(),
        });
        Ok(Self {
            data,
            _scratch: scratch,
        })
    }

    /// The app-data handle to register on each worker's `App` (cheap clone).
    pub fn data(&self) -> web::Data<AppState> {
        self.data.clone()
    }
}

/// Whether `path` belongs to a route family whose handlers operate on
/// composition-time [`AppState`] fields — under cloud, the single inert
/// fallback shared by every tenant — rather than the request's resolved
/// tenant. These planes are unrouted (404) in the cloud composition; the
/// module docs enumerate each family and why:
///
/// - `/api/auth/*` — self-hosted's token plane (`state.manager`).
/// - `/api/exports/{id}` and `/api/databases/{id}/exports/*` — the
///   export-job plane (`state.export_jobs`): one process-global namespace of
///   job ids and artifacts, so any tenant could read or delete another
///   tenant's export by id.
/// - `/api/logs` — the process-wide log ring buffer (`state.log_buffer`).
fn fallback_bound_plane(path: &str) -> bool {
    if path.starts_with("/api/auth/") || path.starts_with("/api/exports/") || path == "/api/logs" {
        return true;
    }
    // `/api/databases/{id}/exports/...` — the export-start route lives under
    // the databases prefix; match the whole exports subtree so a future
    // export format added to atomic-server stays unrouted here by default.
    path.strip_prefix("/api/databases/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, tail)| tail == "exports" || tail.starts_with("exports/"))
}

/// Composition-level guard between [`CloudAuth`] and atomic-server's routes.
///
/// Two rules, both enforcing the boundary documented in the module docs:
///
/// 1. **Fallback-bound planes are unrouted (404).** The self-hosted token
///    plane (`/api/auth/*`), the export-job family, and `/api/logs` all
///    operate on composition-time [`AppState`] fields — in cloud, the inert
///    fallback, one process-global namespace shared across tenants — rather
///    than the request's tenant (see [`fallback_bound_plane`]). Cloud tokens
///    are control-plane rows managed via the CLI (and, in later slices,
///    cloud-owned routes); per-tenant export and log planes arrive in later
///    slices.
/// 2. **No tenant extension → fail closed (500).** [`CloudAuth`] installs
///    [`RequestDatabaseManager`] on every request it passes through, so this
///    can only fire on a composition bug — and when it does, the request
///    must error rather than fall back to the scratch [`AppState`] store.
///
/// Public so the e2e suite can prove rule 2 against the exact middleware the
/// composition uses; wire it with `actix_web::middleware::from_fn`.
pub async fn cloud_plane_guard(
    req: ServiceRequest,
    next: Next<impl actix_web::body::MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    if fallback_bound_plane(req.path()) {
        let denial = HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" }));
        return Ok(req.into_response(denial));
    }
    if req.extensions().get::<RequestDatabaseManager>().is_none() {
        tracing::error!(
            path = req.path(),
            "request reached the route table without a resolved tenant; failing closed"
        );
        let denial = HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "tenant_not_resolved",
            "message": "The request was not resolved to an account.",
        }));
        return Ok(req.into_response(denial));
    }
    next.call(req).await.map(|res| res.map_into_boxed_body())
}

/// Cloud WebSocket handler: stream the authenticated tenant's event channel.
///
/// Runs strictly behind [`CloudAuth`] + [`cloud_plane_guard`], so the
/// [`EventChannel`] extractor always resolves the [`RequestEventChannel`]
/// extension — the same per-account channel the tenant's `/api` handlers
/// publish into — and never the fallback state's inert channel.
///
/// [`RequestEventChannel`]: atomic_server::event_channel::RequestEventChannel
async fn cloud_ws(
    req: HttpRequest,
    stream: web::Payload,
    events: EventChannel,
) -> Result<HttpResponse, actix_web::Error> {
    ws::start_event_session(&req, stream, events.0)
}

/// Build the cloud application's route table as a [`web::ServiceConfig`]
/// closure, suitable for `App::new().configure(...)` — the multi-tenant
/// counterpart of atomic-server's `configure_app`. See the module docs for
/// the exact composition and what is deliberately absent.
///
/// Takes everything the route table depends on:
/// - `state` — the inert fallback app data from [`FallbackAppState::data`];
///   the owning [`FallbackAppState`] must outlive the server.
/// - `auth` — the [`CloudAuth`] middleware, carrying the control plane and
///   the account cache. Cheap to clone per worker.
/// - `account_plane` — the app-host plane ([`AccountPlane`]): signup/login
///   request-link routes, each guarded to the app host so they don't exist
///   on tenant subdomains.
/// - `tenant_plane` — the cloud-owned tenant routes ([`TenantPlane`]):
///   account deletion, behind the same `auth`.
///
/// Returns `impl FnOnce` rather than taking `&mut ServiceConfig` directly
/// because the registration captures per-caller values; the server factory
/// clones the arguments into each worker's call.
pub fn configure_cloud_app(
    state: web::Data<AppState>,
    auth: CloudAuth,
    account_plane: AccountPlane,
    tenant_plane: TenantPlane,
) -> impl FnOnce(&mut web::ServiceConfig) {
    move |cfg: &mut web::ServiceConfig| {
        cfg.app_data(state).route("/health", web::get().to(health));
        account_plane.configure(cfg);
        // Before api_scope: its exact-path /api/account resource must win
        // the route match over the /api scope.
        tenant_plane.configure(cfg, auth.clone());
        cfg.service(
            web::resource("/ws")
                .route(web::get().to(cloud_ws))
                // Later-registered wrap runs first: auth resolves the
                // tenant, then the guard verifies the extensions exist.
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        )
        .service(api_scope().wrap(from_fn(cloud_plane_guard)).wrap(auth));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_bound_planes_are_matched() {
        for unrouted in [
            "/api/auth/tokens",
            "/api/auth/tokens/some-id",
            "/api/exports/job-1",
            "/api/exports/job-1/download",
            "/api/databases/default/exports/markdown",
            "/api/databases/abc-123/exports",
            "/api/logs",
        ] {
            assert!(fallback_bound_plane(unrouted), "{unrouted} must be 404'd");
        }
    }

    #[test]
    fn tenant_routes_are_not_matched() {
        for routed in [
            "/api/atoms",
            "/api/databases",
            "/api/databases/default/stats",
            "/api/databases/default/activate",
            // Only the exact /api/logs path is the log plane.
            "/api/logsearch",
            // An atom id that happens to contain "exports" is not the plane.
            "/api/atoms/exports",
        ] {
            assert!(!fallback_bound_plane(routed), "{routed} must stay routed");
        }
    }
}
