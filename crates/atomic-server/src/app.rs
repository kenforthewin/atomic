//! Whole-application route composition.
//!
//! [`configure_app`] registers the complete service tree the standalone
//! binary serves. It is assembled from three granular pieces — exposed so a
//! caller composing these routes into a larger application can pick which
//! planes to reuse and which to replace:
//!
//! - [`configure_public_routes`] — the unauthenticated block (health, API
//!   docs, WebSocket, OAuth discovery + flow, instance setup, export
//!   download). Also registers the [`AppState`] app data every other piece's
//!   handlers and middleware rely on.
//! - [`mcp_scope`] — the `/mcp` service tree, *without* auth middleware.
//! - [`api_scope`] — the `/api` route table, *without* auth middleware.
//!
//! The auth/registry plane is deliberately bound at composition time:
//! `configure_app` wraps [`mcp_scope`] in [`McpAuth`] and [`api_scope`] in
//! [`BearerAuth`], both holding the [`AppState`] they were constructed with.
//! A caller that needs a different identity scheme composes the granular
//! pieces and wraps its own middleware in their place — the auth middleware
//! is not baked into the scopes precisely so it can be swapped without
//! forking the route tables.
//!
//! Boundary: middleware that reflects the *deployment* rather than the API
//! contract — CORS, compression, request logging — is deliberately not
//! registered here. Actix middleware wraps the whole `App`, so callers
//! layer those around the composed routes as needed: `main.rs` adds CORS +
//! compression, the test harness adds nothing, and both still serve an
//! identical route table.

use actix_web::{web, HttpResponse, Responder, Scope};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::auth::BearerAuth;
use crate::mcp::AtomicMcpTransport;
use crate::mcp_auth::McpAuth;
use crate::state::AppState;
use crate::{openapi_spec, routes, ws, ApiDoc};

/// `GET /health` — public liveness probe reporting the server version.
pub async fn health() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Register the public (unauthenticated) route block: health, API docs,
/// the WebSocket endpoint, OAuth discovery + flow, instance setup, and
/// export download. Also registers `state` as app data, which the
/// extractors and auth middleware of every other piece resolve at request
/// time — compositions that skip this piece must register
/// `web::Data<AppState>` themselves.
///
/// These routes carry their own authorization where they need it (setup is
/// guarded by the zero-token check, export download by a one-time query
/// token, `/ws` by token verification inside the handler), so no middleware
/// belongs in front of them in the standalone composition. A caller may
/// still wrap the enclosing `App` with deployment middleware (CORS,
/// logging) as usual.
pub fn configure_public_routes(state: web::Data<AppState>) -> impl FnOnce(&mut web::ServiceConfig) {
    move |cfg: &mut web::ServiceConfig| {
        cfg.app_data(state)
            .route("/health", web::get().to(health))
            .route("/api/docs/openapi.json", web::get().to(openapi_spec))
            .service(Scalar::with_url("/api/docs", ApiDoc::openapi()))
            .route("/ws", web::get().to(ws::ws_handler))
            // OAuth discovery (public, no auth)
            .route(
                "/.well-known/oauth-authorization-server",
                web::get().to(routes::oauth::metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                web::get().to(routes::oauth::resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                web::get().to(routes::oauth::resource_metadata),
            )
            // Instance setup (public, no auth — guarded by zero-token check)
            .route(
                "/api/setup/status",
                web::get().to(routes::setup::setup_status),
            )
            .route(
                "/api/setup/claim",
                web::post().to(routes::setup::claim_instance),
            )
            // OAuth flow (public, no auth)
            .route("/oauth/register", web::post().to(routes::oauth::register))
            .route(
                "/oauth/authorize",
                web::get().to(routes::oauth::authorize_page),
            )
            .route(
                "/oauth/authorize",
                web::post().to(routes::oauth::authorize_approve),
            )
            .route("/oauth/token", web::post().to(routes::oauth::token))
            // Export download (public — authorized by one-time token in query)
            .route(
                "/api/exports/{id}/download",
                web::get().to(routes::exports::download_export),
            );
    }
}

/// The `/mcp` service tree, parameterized by the MCP Streamable HTTP
/// transport and carrying **no auth middleware**. `configure_app` wraps it
/// in [`McpAuth`]; a caller composing it directly wraps whatever gate fits
/// its deployment (`.service(mcp_scope(transport).wrap(my_auth))`), reusing
/// the transport wiring without inheriting the token scheme.
///
/// `mcp_transport` must be constructed once per process and cloned per
/// worker (see [`AtomicMcpTransport::new`]) so every actix worker shares one
/// MCP session manager. Note the transport binds its [`DatabaseManager`]
/// and event channel at construction — the MCP plane does not consult the
/// per-request extensions honored by the `/api` handlers.
pub fn mcp_scope(mcp_transport: AtomicMcpTransport) -> Scope {
    web::scope("/mcp").service(mcp_transport.scope())
}

/// The `/api` route table ([`routes::configure_routes`]) under its scope,
/// carrying **no auth middleware**. `configure_app` wraps it in
/// [`BearerAuth`]; a caller composing it directly chooses the wrapping
/// middleware itself (`.service(api_scope().wrap(my_auth))`) and gets the
/// full route table without the bearer-token contract.
///
/// Handlers resolve [`AppState`] from app data — register it via
/// [`configure_public_routes`] or `App::app_data` before serving this scope.
pub fn api_scope() -> Scope {
    web::scope("/api").configure(routes::configure_routes)
}

/// Build the full application route table as a [`web::ServiceConfig`]
/// closure, suitable for `App::new().configure(...)` — the all-in-one
/// composition the standalone binary serves: [`configure_public_routes`],
/// plus [`mcp_scope`] behind [`McpAuth`] and [`api_scope`] behind
/// [`BearerAuth`], both bound to `state`.
///
/// Takes everything the route table depends on:
/// - `state` — shared [`AppState`]; registered as app data so extractors
///   and middleware resolve it.
/// - `mcp_transport` — the MCP Streamable HTTP transport. Constructed by
///   the caller (once per process, cloned per worker) so every actix
///   worker shares one MCP session manager.
///
/// Returns `impl FnOnce` rather than taking `&mut ServiceConfig` directly
/// because the registration captures per-caller values; this keeps the
/// call site down to a single `.configure(...)`.
pub fn configure_app(
    state: web::Data<AppState>,
    mcp_transport: AtomicMcpTransport,
) -> impl FnOnce(&mut web::ServiceConfig) {
    move |cfg: &mut web::ServiceConfig| {
        cfg.configure(configure_public_routes(state.clone()))
            // MCP endpoint with MCP-aware auth
            .service(mcp_scope(mcp_transport).wrap(McpAuth {
                state: state.clone(),
            }))
            // Authenticated API routes
            .service(api_scope().wrap(BearerAuth { state }));
    }
}
