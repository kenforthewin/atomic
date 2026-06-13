//! The composed multi-tenant cloud application.
//!
//! [`configure_cloud_app`] assembles atomic-server's granular route pieces
//! under cloud middleware, producing the service tree the `atomic-cloud serve`
//! binary listens on. The composition is deliberately narrower than the
//! self-hosted server's [`configure_app`](atomic_server::app::configure_app):
//!
//! - `GET /health` — public, no auth on any host: liveness, always up.
//! - `GET /ready` — public, no auth on any host: deploy-gated readiness
//!   ([`crate::deploy::Readiness`]). 503 `{"status":"migrating", ...}`
//!   while the boot fleet migration runs, 503 with the holding status when
//!   the failure-rate policy held the pod back, 200 once it admits (or an
//!   operator runs `deploy advance`). Liveness and readiness are split
//!   exactly so orchestrators keep migrating pods alive but unrouted.
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
//! - **Cloud OAuth** ([`crate::oauth_routes`]) — the per-account discovery /
//!   DCR / authorize / token endpoints. PUBLIC (no [`CloudAuth`]): an MCP
//!   client bootstraps before any token exists, so each handler resolves the
//!   account from `Host` itself and is account-scoped through the control
//!   plane. The authorize-approve step verifies the session cookie. On the
//!   app host they resolve no subdomain and 404, like every tenant route.
//! - `/mcp` — atomic-server's MCP Streamable HTTP transport
//!   ([`mcp_scope`](atomic_server::app::mcp_scope)) under [`CloudAuth`] +
//!   [`cloud_plane_guard`]. The bearer MCP token the OAuth flow mints
//!   authenticates the request; CloudAuth injects the tenant's
//!   [`RequestDatabaseManager`], which the transport resolves per request
//!   (its cloud-unaware `RequestManager` override), so every tenant's `/mcp`
//!   call hits its own knowledge base — never the inert baked-in manager.
//! - `/api/*` — atomic-server's full route table
//!   ([`api_scope`](atomic_server::app::api_scope)) wrapped in [`CloudAuth`]
//!   (in place of self-hosted's `BearerAuth`) plus [`cloud_plane_guard`].
//!
//! Deliberately **not** registered, with their replacements landing in later
//! slices (plan: `docs/plans/atomic-cloud.md`):
//!
//! - `configure_public_routes` — its instance setup, self-hosted `/ws`, API
//!   docs, and export download all assume the single-tenant identity model.
//!   (Cloud OAuth, formerly listed here, now ships as the per-account
//!   [`crate::oauth_routes`] plane above.)
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
use atomic_server::app::{api_scope, health, mcp_scope};
use atomic_server::db_extractor::RequestDatabaseManager;
use atomic_server::event_channel::EventChannel;
use atomic_server::export_jobs::ExportJobManager;
use atomic_server::log_buffer::LogBuffer;
use atomic_server::mcp::AtomicMcpTransport;
use atomic_server::state::{AppState, SetupClaimLimiter};
use atomic_server::ws;
use std::time::Duration;
use tokio::sync::broadcast;

use crate::account_plane::AccountPlane;
use crate::auth::CloudAuth;
use crate::backpressure::out_of_credits_guard;
use crate::billing_guard::billing_write_guard;
use crate::billing_routes::Billing;
use crate::chat_streams::{chat_stream_guard, ChatStreamLimiter};
use crate::control_plane::ControlPlane;
use crate::deploy::Readiness;
use crate::dispatch_hints::{mark_hint_on_mutation, DispatchHintWriter};
use crate::error::CloudError;
use crate::oauth_routes::OAuthPlane;
use crate::plans::PlanRegistry;
use crate::quota::quota_guard;
use crate::rate_limit::{data_plane_rate_limit_guard, DataPlaneRateLimiter};
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

    /// Build the MCP Streamable HTTP transport for the cloud `/mcp` route.
    ///
    /// The transport bakes in *this fallback's* inert manager and a fresh
    /// event channel — the same type-level-placeholder rationale as the
    /// fallback [`AppState`] itself (module docs): [`CloudAuth`] injects the
    /// real per-request [`RequestDatabaseManager`] on every authenticated
    /// `/mcp` request, and atomic-server's transport resolves *that* manager
    /// per request (its `RequestManager` override), so the baked-in one is
    /// never reached. [`cloud_plane_guard`] makes that an invariant: a `/mcp`
    /// request without the tenant extension fails closed rather than serving
    /// from the inert manager.
    ///
    /// Build it **once per process** and clone it into each worker's
    /// `configure_cloud_app` call, so every actix worker shares one MCP
    /// session manager (the transport is cheap to clone — it's `Arc`-backed).
    /// `sse_keep_alive` is the interval between SSE `:ping` frames on a live
    /// stream.
    ///
    /// Worker-event scope: MCP tool calls that create atoms broadcast onto the
    /// transport's baked-in channel, not the per-account one a WS client
    /// subscribes to. That matches the existing v1 limitation (MCP tool events
    /// aren't relayed to WS subscribers); durable state is always correct.
    pub fn mcp_transport(&self, sse_keep_alive: Duration) -> AtomicMcpTransport {
        let (event_tx, _) = broadcast::channel(16);
        AtomicMcpTransport::new(Arc::clone(&self.data.manager), event_tx, sse_keep_alive)
    }
}

/// Default interval between SSE `:ping` keep-alive frames on a live MCP
/// stream (matches atomic-server's standalone default).
pub const DEFAULT_MCP_SSE_KEEP_ALIVE: Duration = Duration::from_secs(30);

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

/// The public readiness probe (module docs): delegates to
/// [`Readiness::probe`], which owns the state machine and the
/// awaiting-review advance re-check.
async fn ready(readiness: web::Data<Readiness>) -> HttpResponse {
    readiness.probe().await
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
/// - `control` — the control plane, for the dispatch-hint middleware on the
///   data plane ([`mark_hint_on_mutation`]): mutating `/api/*` requests mark
///   the account's `dispatch_hints` row so the dispatcher's cross-tenant
///   scan knows which tenants may hold pending ledger work. Only the
///   `api_scope` plane carries it — the cloud-owned `/api/account*` routes
///   mutate control-plane state, never the tenant's work ledgers, and `/ws`
///   is read-only.
/// - `chat_streams` — the per-account streaming-chat semaphore
///   ([`crate::chat_streams`]). MUST be one process-wide instance cloned
///   into every worker's call (it counts in memory; a per-worker instance
///   would multiply the cap by the worker count).
/// - `readiness` — this process's deploy-gated readiness handle
///   ([`crate::deploy::Readiness`]), served publicly at `/ready`. Like the
///   chat limiter, it must be the one process-wide instance: `serve`'s
///   fleet gate flips exactly this handle.
///
/// Returns `impl FnOnce` rather than taking `&mut ServiceConfig` directly
/// because the registration captures per-caller values; the server factory
/// clones the arguments into each worker's call.
/// The plans/quota/billing composition inputs, bundled so the (already
/// long) `configure_cloud_app` signature grows by one argument rather than
/// three, and so a test harness can build the lot with one call
/// ([`QuotaBilling::for_tests`]).
///
/// - `plan_registry` — the in-memory plan catalogue the quota guard reads
///   (one shared instance; `web::Data` clones the `Arc`).
/// - `rate_limiter` — the per-account data-plane sliding windows (one
///   process-wide instance, cloned into every worker; a per-worker instance
///   would multiply every limit by the worker count).
/// - `billing` — the billing plane (portal/checkout routes + the webhook),
///   `provider: None` when Stripe isn't configured (routes degrade to 503).
#[derive(Clone)]
pub struct QuotaBilling {
    pub plan_registry: web::Data<PlanRegistry>,
    pub rate_limiter: DataPlaneRateLimiter,
    pub billing: Billing,
}

impl QuotaBilling {
    /// A test/disabled-billing bundle: an **unlimited** plan registry,
    /// default data-plane rate limits, and a Stripe-disabled billing plane.
    ///
    /// The seeded `free` plan is widened to unlimited (NULL atom/kb limits)
    /// before the registry loads, so the many integration harnesses that
    /// aren't *about* quotas (they create several atoms and KBs as fixtures)
    /// are never tripped by the free-tier ceiling. The dedicated quota suite
    /// (`tests/quota_billing.rs`) builds its own `QuotaBilling` with explicit
    /// limits to exercise enforcement. Per-test control DB, so this widening
    /// only ever touches test data.
    pub async fn for_tests(control: ControlPlane, base_domain: &str) -> Result<Self, CloudError> {
        sqlx::query("UPDATE plans SET atom_limit = NULL, kb_limit = NULL")
            .execute(control.pool())
            .await
            .map_err(CloudError::db("widening test plan limits"))?;
        Ok(Self {
            plan_registry: web::Data::new(PlanRegistry::load(control.clone()).await?),
            rate_limiter: DataPlaneRateLimiter::new(
                crate::rate_limit::DataPlaneRateLimits::default(),
            ),
            billing: Billing::with_provider(
                control,
                None,
                "",
                std::collections::HashMap::new(),
                format!("https://app.{base_domain}"),
                base_domain,
            ),
        })
    }
}

#[allow(clippy::too_many_arguments)] // Composition assembly; each argument is a distinct plane/guard input.
pub fn configure_cloud_app(
    state: web::Data<AppState>,
    auth: CloudAuth,
    account_plane: AccountPlane,
    tenant_plane: TenantPlane,
    oauth_plane: OAuthPlane,
    mcp_transport: AtomicMcpTransport,
    control: ControlPlane,
    chat_streams: ChatStreamLimiter,
    readiness: Readiness,
    quota_billing: QuotaBilling,
) -> impl FnOnce(&mut web::ServiceConfig) {
    let QuotaBilling {
        plan_registry,
        rate_limiter,
        billing,
    } = quota_billing;
    move |cfg: &mut web::ServiceConfig| {
        cfg.app_data(state)
            .route("/health", web::get().to(health))
            .service(
                web::resource("/ready")
                    .app_data(web::Data::new(readiness))
                    .route(web::get().to(ready)),
            );
        account_plane.configure(cfg);
        // The signed Stripe webhook on the app host (unauthenticated — the
        // signature is the auth; guarded to the app host like the account
        // plane). Registered with the app plane, never on tenant subdomains.
        billing.configure_app(cfg);
        // Cloud's per-account OAuth flow (plan: "OAuth"). PUBLIC — no
        // CloudAuth — because the discovery/register/token endpoints are how
        // an MCP client bootstraps before any token exists; each handler
        // resolves the account from Host itself and is account-scoped through
        // the control-plane store. On the app host they resolve no subdomain
        // and 404, like every tenant route. The authorize-approve step
        // verifies the session cookie. See `crate::oauth_routes`.
        oauth_plane.configure(cfg);
        // Before api_scope: its exact-path /api/account resource must win
        // the route match over the /api scope.
        tenant_plane.configure(cfg, auth.clone());
        // Per-tenant MCP Streamable HTTP (plan: "MCP token UX"). Behind
        // CloudAuth: the bearer MCP token this flow mints authenticates the
        // request and CloudAuth injects the tenant's `RequestDatabaseManager`,
        // which atomic-server's MCP transport resolves per-request (the
        // cloud-unaware `RequestManager` override) — so every tenant's `/mcp`
        // call hits its own knowledge base, never the inert baked-in manager.
        // The plane guard fails closed if the extension is somehow absent.
        cfg.service(
            mcp_scope(mcp_transport)
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        );
        cfg.service(
            web::resource("/ws")
                .route(web::get().to(cloud_ws))
                // Later-registered wrap runs first: auth resolves the
                // tenant, then the guard verifies the extensions exist.
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth.clone()),
        )
        .service({
            let mut scope = api_scope()
                .app_data(web::Data::new(DispatchHintWriter::new(control)))
                .app_data(web::Data::new(chat_streams))
                .app_data(plan_registry)
                .app_data(web::Data::new(rate_limiter));
            // The authenticated tenant-plane billing routes (portal,
            // checkout) live inside the /api scope so they share its
            // CloudAuth wrap and resolved tenant.
            scope = scope.configure(|c| billing.configure_tenant(c));
            scope
                // Execution order is outermost-last-registered: auth
                // resolves the tenant, the plane guard fails closed /
                // unroutes the fallback-bound planes, the rate-limit guard
                // 429s an over-quota account before any work, the billing
                // write-guard 402s mutations on a read_only (dunning)
                // account, the quota guard 402s a create that would exceed
                // the plan's resource limit, the credits guard 402s the
                // AI-interactive routes under a credits pause, the
                // chat-stream guard 429s an over-cap chat send, and only
                // then does the hint writer see the request — so unrouted
                // planes, unauthenticated requests, and every denial
                // (402/429 — none reach a handler) never mark hints.
                .wrap(from_fn(mark_hint_on_mutation))
                .wrap(from_fn(chat_stream_guard))
                .wrap(from_fn(out_of_credits_guard))
                .wrap(from_fn(quota_guard))
                .wrap(from_fn(billing_write_guard))
                .wrap(from_fn(data_plane_rate_limit_guard))
                .wrap(from_fn(cloud_plane_guard))
                .wrap(auth)
        });
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
