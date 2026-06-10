//! Whole-application route composition.
//!
//! [`configure_app`] registers the complete service tree the standalone
//! binary serves: the public routes (health, API docs, WebSocket, OAuth
//! discovery + flow, instance setup, export download), the `/mcp` scope
//! behind [`McpAuth`], and the `/api` scope behind [`BearerAuth`] +
//! [`routes::configure_routes`]. Extracting the wiring as a
//! `ServiceConfig` function makes it reusable: the binary, the e2e test
//! harness, and any other embedder compose the exact same route table
//! instead of hand-mirroring it (and silently drifting).
//!
//! Boundary: middleware that reflects the *deployment* rather than the API
//! contract — CORS, compression, request logging — is deliberately not
//! registered here. Actix middleware wraps the whole `App`, so callers
//! layer those around the composed routes as needed: `main.rs` adds CORS +
//! compression, the test harness adds nothing, and both still serve an
//! identical route table.

use actix_web::{web, HttpResponse, Responder};
use utoipa::OpenApi;

use crate::auth::BearerAuth;
use crate::mcp::AtomicMcpTransport;
use crate::mcp_auth::McpAuth;
use crate::state::AppState;
use crate::{openapi_spec, routes, ws, ApiDoc, Scalar, Servable};

/// `GET /health` — public liveness probe reporting the server version.
pub async fn health() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Build the full application route table as a [`web::ServiceConfig`]
/// closure, suitable for `App::new().configure(...)`.
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
        cfg.app_data(state.clone())
            // Public routes (no auth)
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
            )
            // MCP endpoint with MCP-aware auth
            .service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .service(mcp_transport.scope()),
            )
            // Authenticated API routes
            .service(
                web::scope("/api")
                    .wrap(BearerAuth { state })
                    .configure(routes::configure_routes),
            );
    }
}
