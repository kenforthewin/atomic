//! CloudAuth middleware — tenant routing + authentication in one layer
//! (plan: "Auth & tenant routing" → "CloudAuth middleware").
//!
//! This middleware is the entire authorization layer for the cloud
//! composition. Per request, in order:
//!
//! 1. `Host` header → strip the configured base domain → subdomain.
//!    Malformed, missing, or foreign hosts → 404.
//! 2. `accounts WHERE subdomain = ?` → 404 when absent. A non-`active`
//!    account is blocked before credentials are even read: `provisioning`
//!    returns a structured 503 (the plan's hold-message pattern; the
//!    deploy-gating `account_upgrading` variant joins it in a later slice),
//!    anything else (`failed`, …) is indistinguishable from absent — 404.
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

use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use actix_web::body::EitherBody;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::{self, HeaderName, HeaderValue};
use actix_web::{Error, HttpMessage, HttpResponse};
use atomic_server::db_extractor::RequestDatabaseManager;
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
}

/// Everything a request needs to be authenticated, shared across workers.
struct AuthCtx {
    control: ControlPlane,
    cache: Arc<AccountCache>,
    /// Normalized (lowercase, no leading dot) base domain, e.g.
    /// `atomic.cloud` — or `localhost` for local/test deployments, where
    /// hosts look like `kenny.localhost:8080`.
    base_domain: String,
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
            }),
        }
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
                Err(denial) => Ok(req.into_response(denial).map_into_right_body()),
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

    // 2 — subdomain → account. The provider generation rides along on the
    // lookup this middleware already makes per request (no auth caching),
    // making the cache's rotation-convergence check in step 7 free.
    let account: Option<(String, String, i64)> =
        sqlx::query_as("SELECT id, status, provider_generation FROM accounts WHERE subdomain = $1")
            .bind(&subdomain)
            .fetch_optional(ctx.control.pool())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "account lookup failed");
                internal_error()
            })?;
    let (account_id, status, provider_generation) = account.ok_or_else(not_found)?;
    match status.as_str() {
        "active" => {}
        "provisioning" => return Err(account_provisioning()),
        // Anything unrecognized: not servable, and not worth distinguishing
        // from "no such account" to the outside. (No status beyond
        // 'provisioning'/'active' exists today — failed provisions are
        // hard-deleted by the reaper, never tombstoned.)
        _ => return Err(not_found()),
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
    req.extensions_mut().insert(ResolvedTenant {
        principal,
        subdomain,
    });
    Ok(())
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
fn subdomain_from_host(host: &str, base_domain: &str) -> Option<String> {
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
