//! CloudAuth middleware — tenant routing + authentication in one layer
//! (plan: "Auth & tenant routing" → "CloudAuth middleware").
//!
//! This middleware is the entire authorization layer for the cloud
//! composition. Per request, in order:
//!
//! 1. `Host` header → strip the configured base domain → subdomain.
//!    Malformed, missing, or foreign hosts → 404.
//! 2. `accounts WHERE subdomain = ?` → 404 when absent. A non-serveable
//!    account is blocked before credentials are even read: `provisioning`
//!    returns a structured 503 (the plan's hold-message pattern), an active
//!    account whose tenant database lags the binary's compiled schema
//!    target gets the sibling 503 `account_upgrading` (the deploy-gating
//!    straggler gate — see below), anything else (`failed`, …) is
//!    indistinguishable from absent — 404.
//! 3. Credentials: `Authorization: Bearer` first, the session cookie
//!    ([`SESSION_COOKIE`]) as fallback. Neither → 401.
//! 4. Verification is **scoped by account**: `WHERE account_id = ? AND
//!    hash = ?` (see [`crate::tokens`]). That predicate is the cross-tenant
//!    chokepoint — account A's perfectly valid credential presented on
//!    account B's subdomain verifies nothing and 401s. The shared
//!    `.atomic.cloud` session cookie crosses subdomains by design, so this
//!    check is what actually enforces browser-level tenant isolation.
//! 5. Build an [`AuthPrincipal`]. Route handlers never see a raw token.
//! 6. Enforce the principal's database scope (see below).
//! 7. Resolve the account's [`TenantHandle`] via [`AccountCache`] and
//!    install the request extensions atomic-server's handlers honor:
//!    [`RequestDatabaseManager`], [`RequestEventChannel`], and
//!    [`ResolvedTenant`].
//!
//! # The `allowed_db_id` chokepoint
//!
//! Database-scoped credentials pin the request to one knowledge base inside
//! the tenant. Enforcement happens here, before any handler runs:
//!
//! - An explicit `X-Atomic-Database` header or `?db=` parameter naming a
//!   *different* database → 403. (Without this, a database-scoped MCP token
//!   could read another KB via header override.)
//! - No explicit selection → the middleware **injects** an
//!   `X-Atomic-Database` header carrying `allowed_db_id` before forwarding.
//!   atomic-server's `resolve_core` reads that header first, so the request
//!   resolves to the credential's database rather than the tenant's active
//!   one — no handler or extractor changes needed, and the selection rules
//!   stay defined in exactly one place (`db_extractor::resolve_core`).
//!
//! Account-scoped credentials (including sessions) are unrestricted:
//! explicit selections pass through and the default falls to the tenant's
//! active database. Reading `accounts.last_active_db_id` as that default is
//! deferred to the slice that also writes it on explicit switches; until
//! then the tenant-internal active database (the manager's default KB)
//! keeps today's behavior.
//!
//! # The straggler gate (plan: "Schema migration on deploy" → "Stragglers")
//!
//! The per-request account lookup also reads the account's
//! `account_databases.last_migrated_version` (the same already-paid-for
//! query the provider generation and circuit-breaker pause ride on). When
//! it lags the binary's compiled tenant schema target
//! ([`crate::fleet_migration::tenant_schema_target`]) — a failed or
//! not-yet-reached tenant during a fleet migration — the request gets the
//! structured 503 `account_upgrading` with `Retry-After`, before
//! credentials are read, exactly like the `account_provisioning` sibling.
//! Serving a request against a behind-schema tenant would let new code
//! query columns/tables that don't exist there yet.
//!
//! **WebSocket connects are gated identically**: the cloud `/ws` route sits
//! behind this middleware, so an upgrading account's upgrade request
//! receives the same 503 instead of a socket. That is deliberate — a WS
//! session is a long-lived data-plane consumer; admitting one mid-upgrade
//! would hold a live subscription to a tenant the HTTP plane is refusing to
//! touch. Clients reconnect (with backoff) exactly as they do for any other
//! 503. Health (`/health`) and the account plane (signup/login on the app
//! host) never pass through this middleware and are unaffected.

use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use actix_web::body::EitherBody;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::{self, HeaderName, HeaderValue};
use actix_web::{Error, HttpMessage, HttpResponse};
use atomic_server::db_extractor::{RequestDatabaseManager, RequestJobScope};
use atomic_server::event_channel::RequestEventChannel;
use futures::future::{ok, LocalBoxFuture, Ready};

use crate::account_cache::AccountCache;
use crate::control_plane::ControlPlane;
use crate::tokens::{self, TokenScope};

/// Name of the session cookie carrying the opaque session secret. Set by
/// the login flow (signup slice) with `Domain=.{base}; Secure; HttpOnly;
/// SameSite=Lax` so one session works across every subdomain the user
/// visits.
pub const SESSION_COOKIE: &str = "atomic_session";

/// The database-selection header atomic-server's `resolve_core` reads
/// first. Lowercase because [`HeaderName::from_static`] requires it; header
/// lookup is case-insensitive.
const DB_HEADER: &str = "x-atomic-database";

/// Which kind of credential authenticated the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// `Authorization: Bearer atm_…` verified against `cloud_tokens`.
    Token,
    /// [`SESSION_COOKIE`] verified against `sessions`.
    Session,
}

impl CredentialSource {
    /// Stable text form, for logs and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialSource::Token => "token",
            CredentialSource::Session => "session",
        }
    }
}

/// The authenticated identity of a request, built by [`CloudAuth`] in step
/// 5. Handlers consume this (via [`ResolvedTenant`]) and never see the raw
/// credential.
#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub account_id: String,
    /// Sessions always carry [`TokenScope::Account`] — a logged-in browser
    /// has full account access.
    pub scope: TokenScope,
    /// When `Some`, the one knowledge base this credential may touch;
    /// enforced by the chokepoint (module docs) before any handler runs.
    pub allowed_db_id: Option<String>,
    pub source: CredentialSource,
}

/// Request extension installed after successful authentication: the
/// authenticated principal plus the subdomain the request arrived on.
/// The tenant's manager and event channel travel in the extensions
/// atomic-server already honors ([`RequestDatabaseManager`],
/// [`RequestEventChannel`]) so its handlers work unmodified.
#[derive(Debug, Clone)]
pub struct ResolvedTenant {
    pub principal: AuthPrincipal,
    pub subdomain: String,
    /// The account's provider circuit-breaker pause, read off the same
    /// accounts row the auth lookup already pays for. `Some` whenever the
    /// pause columns are set — consumers check
    /// [`active_at`](crate::backpressure::ProviderPause::active_at)
    /// themselves (an expired pause is inert, not an error). Only the
    /// `credits` kind affects request handling (the `out_of_ai_credits`
    /// guard); `rate_limit` pauses govern background dispatch alone.
    pub provider_pause: Option<crate::backpressure::ProviderPause>,
    /// The account's billing serving state, read off the same accounts row
    /// the auth lookup already pays for (plan: "Billing" → dunning). A
    /// `suspended` account is blocked before this struct is ever built (the
    /// gate in [`authenticate`]); `read_only` rides here so the data-plane
    /// write-guard ([`crate::billing_guard::billing_write_guard`]) can 402
    /// mutations while still serving reads. `active`/`past_due` impose no
    /// restriction.
    pub billing_state: crate::billing::dunning::BillingState,
    /// The account's storage serving state, read off the same accounts row
    /// the auth lookup already pays for (plan: "Quotas" → enforcement table:
    /// "Periodic reaper | Storage bytes recompute | Week 1 warn; week 2
    /// restrict writes; no auto-delete"). Orthogonal to
    /// [`billing_state`](Self::billing_state) — a tenant can be over its
    /// storage ceiling while perfectly current on payment, and vice versa. A
    /// `restricted` value rides here so the data-plane write-guard 402s
    /// mutations while still serving reads (data is RETAINED, never deleted);
    /// `active`/`warn` impose no restriction. Set by the storage-recompute
    /// arm ([`crate::quota_usage::recompute_storage`]).
    pub storage_state: crate::quota_usage::StorageState,
}

/// Everything a request needs to be authenticated, shared across workers.
struct AuthCtx {
    control: ControlPlane,
    cache: Arc<AccountCache>,
    /// Normalized (lowercase, no leading dot) base domain, e.g.
    /// `atomic.cloud` — or `localhost` for local/test deployments, where
    /// hosts look like `kenny.localhost:8080`.
    base_domain: String,
    /// Scheme of the tenant's public origin (`https` in production, `http`
    /// for local/dev). Used only to build the `WWW-Authenticate`
    /// `resource_metadata` URL on an unauthenticated `/mcp` request — the
    /// same scheme [`crate::oauth_routes::OAuthPlane`] builds its discovery
    /// URLs from, so a client following the challenge reaches the tenant's
    /// own OAuth metadata.
    public_scheme: String,
}

/// The middleware factory. Cheap to clone; construct once and hand to every
/// worker's `App`.
#[derive(Clone)]
pub struct CloudAuth {
    ctx: Arc<AuthCtx>,
}

impl CloudAuth {
    /// Build the middleware for accounts hosted under `base_domain`
    /// (`<subdomain>.<base_domain>`). A leading dot and any port in
    /// incoming `Host` values are tolerated; matching is case-insensitive.
    ///
    /// The public scheme defaults to `https`; local/dev deployments served
    /// over plain HTTP set it with [`with_public_scheme`](Self::with_public_scheme)
    /// so the MCP `WWW-Authenticate` challenge points at a reachable URL.
    pub fn new(
        control: ControlPlane,
        cache: Arc<AccountCache>,
        base_domain: impl Into<String>,
    ) -> Self {
        let base_domain = base_domain
            .into()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        Self {
            ctx: Arc::new(AuthCtx {
                control,
                cache,
                base_domain,
                public_scheme: "https".to_string(),
            }),
        }
    }

    /// Override the scheme used to build the MCP `WWW-Authenticate`
    /// `resource_metadata` URL (default `https`). Wire it from the same
    /// app-public-URL scheme [`crate::oauth_routes::OAuthPlane`] uses, so the
    /// challenge a `/mcp` client follows resolves to the tenant's own OAuth
    /// discovery over the right scheme (`http` for local/dev).
    pub fn with_public_scheme(mut self, scheme: impl Into<String>) -> Self {
        // The Arc is freshly constructed and not yet shared, so this is the
        // single owner — mutate in place rather than reallocating.
        Arc::get_mut(&mut self.ctx)
            .expect("CloudAuth not yet cloned when configuring the public scheme")
            .public_scheme = scheme.into();
        self
    }

    /// The normalized base domain (`atomic.cloud`, `localhost`) this
    /// middleware routes under. `pub(crate)` so the composition can build the
    /// account-dashboard session gate ([`crate::spa::AccountGate`]) from the
    /// same base domain `CloudAuth` already resolved — one source of truth for
    /// the host split, no extra `configure_cloud_app` argument.
    pub(crate) fn base_domain(&self) -> &str {
        &self.ctx.base_domain
    }

    /// The public-origin scheme (`https` in prod, `http` for local/dev). Used
    /// alongside [`base_domain`](Self::base_domain) to build the account
    /// gate's login-redirect URL, matching the scheme the MCP challenge and
    /// OAuth discovery URLs use.
    pub(crate) fn public_scheme(&self) -> &str {
        &self.ctx.public_scheme
    }
}

impl<S, B> Transform<S, ServiceRequest> for CloudAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    // EitherBody, as in atomic-server's McpAuth: denials are Ok(response)
    // rather than Err, so they flow back through outer middleware (CORS)
    // and carry structured JSON bodies.
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = CloudAuthMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(CloudAuthMiddleware {
            service: Rc::new(service),
            ctx: Arc::clone(&self.ctx),
        })
    }
}

pub struct CloudAuthMiddleware<S> {
    service: Rc<S>,
    ctx: Arc<AuthCtx>,
}

impl<S, B> Service<ServiceRequest> for CloudAuthMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let ctx = Arc::clone(&self.ctx);
        let svc = Rc::clone(&self.service);
        Box::pin(async move {
            let mut req = req;
            match authenticate(&ctx, &mut req).await {
                Ok(()) => svc.call(req).await.map(|res| res.map_into_left_body()),
                Err(denial) => {
                    let denial = decorate_mcp_unauthorized(&ctx, &req, denial);
                    Ok(req.into_response(denial).map_into_right_body())
                }
            }
        })
    }
}

/// Run the full middleware sequence against `req`, mutating it (injected
/// database header, request extensions) on success. A denial is the exact
/// `HttpResponse` to return.
async fn authenticate(ctx: &AuthCtx, req: &mut ServiceRequest) -> Result<(), HttpResponse> {
    // 1 — host → subdomain.
    let subdomain = request_host(req)
        .and_then(|host| subdomain_from_host(host, &ctx.base_domain))
        .ok_or_else(not_found)?;

    // 2 — subdomain → account. The provider generation, the circuit-breaker
    // pause, and the tenant's migration progress all ride along on the
    // lookup this middleware already makes per request (no auth caching),
    // making the cache's rotation-convergence check in step 7, the
    // out-of-credits guard, and the straggler gate free. The migration
    // progress is the MIN over the account's active mapping rows (exactly
    // one in v1): if ANY serving database lags, the account is mid-upgrade.
    type AccountRow = (
        String,
        String,
        i64,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        String,
        String,
        Option<i32>,
    );
    let account: Option<AccountRow> = sqlx::query_as(
        "SELECT id, status, provider_generation, provider_paused_until, provider_pause_kind, \
                billing_state, storage_state, \
                (SELECT MIN(last_migrated_version) FROM account_databases \
                 WHERE account_id = accounts.id AND status = 'active') \
         FROM accounts WHERE subdomain = $1",
    )
    .bind(&subdomain)
    .fetch_optional(ctx.control.pool())
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "account lookup failed");
        internal_error()
    })?;
    let (
        account_id,
        status,
        provider_generation,
        paused_until,
        pause_kind,
        billing_state_raw,
        storage_state_raw,
        migrated_version,
    ) = account.ok_or_else(not_found)?;
    let provider_pause =
        crate::backpressure::ProviderPause::from_columns(paused_until, pause_kind.as_deref());
    let billing_state = crate::billing::dunning::billing_state_from_column(&billing_state_raw);
    let storage_state = crate::quota_usage::storage_state_from_column(&storage_state_raw);
    match status.as_str() {
        "active" => {}
        "provisioning" => return Err(account_provisioning()),
        // Anything unrecognized: not servable, and not worth distinguishing
        // from "no such account" to the outside. (No status beyond
        // 'provisioning'/'active' exists today — failed provisions are
        // hard-deleted by the reaper, never tombstoned.)
        _ => return Err(not_found()),
    }

    // Billing gate (plan: "Billing" → dunning): a suspended account is
    // blocked before credentials are even read — login and serving both stop
    // (data is retained, never deleted). read_only and past_due still serve;
    // read_only's write block is enforced later by the data-plane
    // write-guard (it needs the request method/path, which this pre-auth gate
    // doesn't gate on). The structured 402 carries the upgrade link so the
    // frontend can route the user to billing.
    if billing_state.blocks_serving() {
        return Err(account_suspended(request_host(req).unwrap_or_default()));
    }

    // 2½ — the straggler gate (module docs): an otherwise-serveable account
    // whose tenant database lags the compiled schema target holds with the
    // plan's 503 until the fleet runner or the reaper brings it current.
    // `None` means no active mapping row at all — an interrupted deletion,
    // not a straggler: fall through and let the tenant load fail on its own
    // terms (the reaper's recovery arm owns that state).
    if let Some(version) = migrated_version {
        if version < crate::fleet_migration::tenant_schema_target() {
            return Err(account_upgrading());
        }
    }

    // 3 + 4 + 5 — credentials → verified principal. Bearer wins when both
    // are present: an API client deliberately sent it, while the session
    // cookie rides along on every browser request.
    let principal = if let Some(token) = bearer_token(req) {
        let record = tokens::verify_token(&ctx.control, &account_id, &token)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "token verification failed");
                internal_error()
            })?
            .ok_or_else(unauthorized)?;
        AuthPrincipal {
            account_id,
            scope: record.scope,
            allowed_db_id: record.allowed_db_id,
            source: CredentialSource::Token,
        }
    } else if let Some(session) = session_secret(req) {
        tokens::verify_session(&ctx.control, &account_id, &session)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "session verification failed");
                internal_error()
            })?
            .ok_or_else(unauthorized)?;
        AuthPrincipal {
            account_id,
            scope: TokenScope::Account,
            allowed_db_id: None,
            source: CredentialSource::Session,
        }
    } else {
        return Err(unauthorized());
    };

    // 6 — database-scope chokepoint, before the (potentially expensive)
    // tenant load: a forbidden request never warms the cache.
    if let Some(allowed) = principal.allowed_db_id.clone() {
        enforce_allowed_db(req, &allowed)?;
    }

    // 7 — tenant resources + request extensions. The generation observed in
    // step 2 lets the cache detect (and heal) a provider config that lags a
    // rotation written by another pod or a racing entry build.
    let handle = ctx
        .cache
        .get_or_load_with_generation(&principal.account_id, provider_generation)
        .await
        .map_err(|e| {
            tracing::error!(
                account_id = %principal.account_id,
                error = %e,
                "tenant resolution failed"
            );
            account_unavailable()
        })?;
    req.extensions_mut()
        .insert(RequestDatabaseManager(handle.manager));
    req.extensions_mut()
        .insert(RequestEventChannel(handle.event_tx));
    // Background-job ownership: jobs created by this request (migration
    // imports) are stamped with the account id, and lookups from another
    // account read as not-found. See `RequestJobScope`'s docs.
    req.extensions_mut()
        .insert(RequestJobScope(principal.account_id.clone()));
    req.extensions_mut().insert(ResolvedTenant {
        principal,
        subdomain,
        provider_pause,
        billing_state,
        storage_state,
    });
    Ok(())
}

/// The MCP Streamable HTTP endpoint path, mounted by the cloud composition
/// at `/mcp` (see [`crate::server::configure_cloud_app`]). The transport's
/// `NormalizePath` collapses trailing-slash variants, but the challenge is
/// attached in CloudAuth *before* that runs — so a client that connects to
/// `/mcp/` (or any sub-path the MCP scope serves) must still receive the
/// challenge. [`is_mcp_path`] matches the bare path and anything beneath it.
const MCP_PATH: &str = "/mcp";

/// Whether `path` addresses the MCP scope: the bare [`MCP_PATH`] or anything
/// under it (`/mcp/`, `/mcp/<sub>`). `NormalizePath` runs inside the transport
/// scope, after CloudAuth, so the challenge decoration can't rely on the path
/// already being canonical — it must recognize the trailing-slash form itself.
fn is_mcp_path(path: &str) -> bool {
    path == MCP_PATH || path.starts_with("/mcp/")
}

/// Add the MCP-compliant `WWW-Authenticate` challenge to a 401 on the `/mcp`
/// path, so an MCP client (Claude Desktop, the MCP Inspector) that hits the
/// tenant's MCP endpoint without a token discovers the OAuth flow.
///
/// This is the cloud counterpart of self-hosted's
/// [`McpAuth`](atomic_server::mcp_auth) `WWW-Authenticate` header: under cloud
/// the MCP scope authenticates through [`CloudAuth`] (the bearer MCP token the
/// OAuth flow mints), not `McpAuth`, so the challenge that points clients at
/// the OAuth discovery document has to be produced here. `resource_metadata`
/// points at the tenant's *own* `/.well-known/oauth-protected-resource` —
/// `{public_scheme}://{request Host}/...`, the same origin
/// [`crate::oauth_routes`] serves its per-tenant discovery from — so the
/// client walks straight into this account's OAuth flow.
///
/// Only a 401 on the MCP path is decorated; every other denial (including
/// 401s on `/api/*`, where API clients already hold a token) is returned
/// verbatim, so self-hosted-style discovery noise never leaks onto the data
/// plane.
fn decorate_mcp_unauthorized(
    ctx: &AuthCtx,
    req: &ServiceRequest,
    denial: HttpResponse,
) -> HttpResponse {
    if denial.status() != actix_web::http::StatusCode::UNAUTHORIZED || !is_mcp_path(req.path()) {
        return denial;
    }
    let Some(host) = request_host(req) else {
        return denial;
    };
    let challenge = format!(
        "Bearer resource_metadata=\"{}://{}/.well-known/oauth-protected-resource\"",
        ctx.public_scheme, host
    );
    let Ok(value) = HeaderValue::from_str(&challenge) else {
        // A `Host` with bytes illegal in a header value can't form a valid
        // challenge; return the plain 401 rather than a malformed header.
        return denial;
    };
    let mut denial = denial;
    denial.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    denial
}

/// The host the client addressed: `Host` header, falling back to the URI
/// authority (HTTP/2 requests carry `:authority` instead of a header).
fn request_host(req: &ServiceRequest) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
}

/// Extract the subdomain from `host`, given the normalized base domain.
/// Exactly one label below the base is accepted; ports are ignored.
///
/// `pub(crate)` so the OAuth plane ([`crate::oauth_routes`]) resolves the
/// tenant from `Host` with exactly the same rules CloudAuth uses — its
/// discovery/register/token endpoints are public (no credential to verify)
/// but still account-scoped by host, so they can't route through CloudAuth
/// itself (which 401s an un-credentialed request).
pub(crate) fn subdomain_from_host(host: &str, base_domain: &str) -> Option<String> {
    // Strip any port. IPv6 literals contain colons too, but they can never
    // match `<label>.<base_domain>`, so mangling them is harmless.
    let host = host.split(':').next()?.to_ascii_lowercase();
    let subdomain = host
        .strip_suffix(base_domain)?
        .strip_suffix('.')
        .filter(|s| !s.is_empty() && !s.contains('.'))?;
    Some(subdomain.to_string())
}

/// The Bearer token from the `Authorization` header, if present.
fn bearer_token(req: &ServiceRequest) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(String::from)
}

/// The session secret from [`SESSION_COOKIE`], if present.
fn session_secret(req: &ServiceRequest) -> Option<String> {
    req.cookie(SESSION_COOKIE).map(|c| c.value().to_string())
}

/// Step 6 (module docs): explicit selections of a different database are
/// rejected; an unselective request is pinned to `allowed` by injecting the
/// header `resolve_core` reads first.
fn enforce_allowed_db(req: &mut ServiceRequest, allowed: &str) -> Result<(), HttpResponse> {
    if let Some(value) = req.headers().get(DB_HEADER) {
        // A header that isn't valid UTF-8 can't equal any db id.
        if value.to_str().ok() != Some(allowed) {
            return Err(database_forbidden());
        }
        return Ok(());
    }
    if let Some(requested) = query_db_param(req.query_string()) {
        if requested != allowed {
            return Err(database_forbidden());
        }
        return Ok(());
    }
    let value = HeaderValue::from_str(allowed).map_err(|_| {
        tracing::error!("allowed_db_id is not a valid header value");
        internal_error()
    })?;
    req.headers_mut()
        .insert(HeaderName::from_static(DB_HEADER), value);
    Ok(())
}

/// The raw `?db=` value, parsed exactly as atomic-server's `resolve_core`
/// parses it (no URL decoding) so the chokepoint and the extractor can
/// never disagree about which database a request selects.
fn query_db_param(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        if parts.next()? == "db" {
            parts.next()
        } else {
            None
        }
    })
}

// --- Denial responses -----------------------------------------------------
//
// Structured JSON throughout; 404s are uniform so the outside can't
// distinguish "no such subdomain" from "account in a dead state".

fn not_found() -> HttpResponse {
    HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" }))
}

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({ "error": "unauthorized" }))
}

fn database_forbidden() -> HttpResponse {
    HttpResponse::Forbidden().json(serde_json::json!({
        "error": "database_forbidden",
        "message": "This credential is scoped to a different database.",
    }))
}

/// The hold-message pattern from the plan ("Stragglers"): the account isn't
/// servable *yet*; clients should retry rather than give up.
fn account_provisioning() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .insert_header((header::RETRY_AFTER, "60"))
        .json(serde_json::json!({
            "error": "account_provisioning",
            "message": "Your account is being set up. Try again shortly.",
            "retry_after_seconds": 60,
        }))
}

/// The plan's straggler response, verbatim ("Schema migration on deploy" →
/// "Stragglers"): the account's tenant database is mid-fleet-migration.
/// The frontend renders an upgrade screen; MCP clients back off and retry.
fn account_upgrading() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .insert_header((header::RETRY_AFTER, "60"))
        .json(serde_json::json!({
            "error": "account_upgrading",
            "message": "Your account is being upgraded. Try again shortly.",
            "retry_after_seconds": 60,
        }))
}

/// The account is `billing_state = 'suspended'` (14+ days past_due): serving
/// and login are blocked, data is retained (plan: "Billing" → dunning, "Never
/// auto-delete"). A structured 402 with the upgrade link so the frontend can
/// route to billing; the user clears it by paying (which lifts the dunning
/// state) — nothing in their account is deleted.
fn account_suspended(host: &str) -> HttpResponse {
    HttpResponse::PaymentRequired().json(serde_json::json!({
        "error": "account_suspended",
        "message": "This account is suspended for non-payment. Your data is \
                    retained; update your billing to restore access.",
        "upgrade_url": app_billing_url(host),
    }))
}

/// `<sub>.<base>` → `https://app.<base>/account/billing` (the dashboard billing
/// route). Same derivation the out-of-credits and quota guards use.
fn app_billing_url(host: &str) -> String {
    let host = host.split(':').next().unwrap_or(host);
    let base = host.split_once('.').map(|(_, base)| base).unwrap_or(host);
    format!("https://app.{base}/account/billing")
}

/// The account exists and the credential verified, but its tenant database
/// couldn't be opened — an operational fault, not a client error.
fn account_unavailable() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(serde_json::json!({
        "error": "account_unavailable",
        "message": "Your account is temporarily unavailable. Try again shortly.",
    }))
}

fn internal_error() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({ "error": "internal_error" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomain_extraction() {
        let base = "atomic.cloud";
        assert_eq!(
            subdomain_from_host("kenny.atomic.cloud", base).as_deref(),
            Some("kenny")
        );
        // Ports and case are tolerated.
        assert_eq!(
            subdomain_from_host("Kenny.Atomic.Cloud:8080", base).as_deref(),
            Some("kenny")
        );
        // localhost-style base for tests/dev.
        assert_eq!(
            subdomain_from_host("kenny.localhost:8080", "localhost").as_deref(),
            Some("kenny")
        );

        // The bare base domain, nested labels, foreign domains, lookalike
        // suffixes, and garbage all fail.
        for bad in [
            "atomic.cloud",
            "a.b.atomic.cloud",
            ".atomic.cloud",
            "kenny.example.com",
            "kennyatomic.cloud",
            "evil-atomic.cloud",
            "",
            "[::1]:8080",
        ] {
            assert_eq!(subdomain_from_host(bad, base), None, "{bad:?} must fail");
        }
    }

    #[test]
    fn query_db_param_mirrors_resolve_core() {
        assert_eq!(query_db_param("db=work"), Some("work"));
        assert_eq!(query_db_param("a=1&db=work&b=2"), Some("work"));
        assert_eq!(query_db_param("db="), Some(""));
        assert_eq!(query_db_param(""), None);
        assert_eq!(query_db_param("dbx=work"), None);
        assert_eq!(query_db_param("a=db"), None);
    }
}
