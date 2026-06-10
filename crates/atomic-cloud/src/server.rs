//! The composed multi-tenant cloud application.
//!
//! [`configure_cloud_app`] assembles atomic-server's granular route pieces
//! under cloud middleware, producing the service tree the `atomic-cloud serve`
//! binary listens on. The composition is deliberately narrower than the
//! self-hosted server's [`configure_app`](atomic_server::app::configure_app):
//!
//! - `GET /health` — public, no auth. The only unauthenticated route.
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
//!
//! # The fallback `AppState` decision
//!
//! atomic-server's handlers and extractors require a `web::Data<AppState>`
//! in app data even when every request carries the
//! [`RequestDatabaseManager`] / [`RequestEventChannel`] extensions: the `Db`
//! extractor takes the state for its fallback unconditionally (see the
//! `FromRequest` doc in `atomic-server/src/db_extractor.rs`), and several
//! handlers extract `web::Data<AppState>` for `log_buffer` / `export_jobs`.
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
//! - The route families that bind the composition-time manager directly
//!   (`/api/auth/*`, the public/MCP planes) are unrouted or 404'd, per the
//!   list above.
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

use crate::auth::CloudAuth;
use crate::error::CloudError;

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

/// Composition-level guard between [`CloudAuth`] and atomic-server's routes.
///
/// Two rules, both enforcing the boundary documented in the module docs:
///
/// 1. **`/api/auth/*` is unrouted (404).** That family is self-hosted's
///    token plane; its handlers operate on the composition-time
///    [`AppState`] manager — in cloud, the inert fallback — rather than the
///    request's tenant. Cloud tokens are control-plane rows managed via the
///    CLI (and, in later slices, cloud-owned routes).
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
    if req.path().starts_with("/api/auth/") {
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
///
/// Returns `impl FnOnce` rather than taking `&mut ServiceConfig` directly
/// because the registration captures per-caller values; the server factory
/// clones `state` and `auth` into each worker's call.
pub fn configure_cloud_app(
    state: web::Data<AppState>,
    auth: CloudAuth,
) -> impl FnOnce(&mut web::ServiceConfig) {
    move |cfg: &mut web::ServiceConfig| {
        cfg.app_data(state)
            .route("/health", web::get().to(health))
            .service(
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
