//! Database resolution extractor for actix-web.
//!
//! `Db` is a `FromRequest` extractor that resolves the correct `AtomicCore`
//! from the request (via `X-Atomic-Database` header, `?db=` param, or active
//! db). By default it resolves against the [`DatabaseManager`] in
//! [`AppState`]; callers composing atomic-server's routes under their own
//! middleware can override that per request via [`RequestDatabaseManager`].

use crate::state::AppState;
use actix_web::{web, FromRequest, HttpMessage, HttpRequest};
use atomic_core::{AtomicCore, DatabaseManager};
use std::sync::Arc;

/// Per-request override for which [`DatabaseManager`] the [`Db`] extractor
/// resolves against.
///
/// Callers composing atomic-server's routes under their own middleware can
/// insert this into a request's extensions to direct that request at a
/// manager other than the one in [`AppState`]. The extension carries the
/// *manager* rather than a pre-resolved [`AtomicCore`] so the per-request
/// database selection rules (`X-Atomic-Database` header, `?db=` query
/// parameter, active-database fallback) stay defined in exactly one place —
/// [`resolve_core`] — regardless of where the manager came from.
///
/// When absent (the standalone server installs no middleware that sets it),
/// the extractor falls back to [`AppState`]'s manager, so compositions that
/// don't need the override pay no per-request cost.
///
/// Scope boundary: only the data plane honors this extension — handlers that
/// resolve a database through the [`Db`] extractor or operate on the manager
/// itself via [`request_manager`]. The `/mcp` scope and the auth/registry
/// plane (`BearerAuth`, `McpAuth`, WebSocket token verification, token CRUD,
/// the OAuth flow, instance setup) bind to [`AppState`] when the app is
/// composed and never consult request extensions. A caller that needs a
/// different identity story or MCP binding should compose the granular
/// pieces in [`crate::app`] with its own middleware in their place, rather
/// than expecting this extension to redirect those planes.
#[derive(Clone)]
pub struct RequestDatabaseManager(pub Arc<DatabaseManager>);

/// Resolve the [`DatabaseManager`] governing `req`: the
/// [`RequestDatabaseManager`] extension when a composing layer installed
/// one, otherwise the manager in [`AppState`].
///
/// This is the single extension-lookup site. The [`Db`] extractor routes
/// through it, and so must every handler that operates on the manager
/// itself — database CRUD, cross-database listings, export jobs — rather
/// than on one resolved core. Reading `state.manager` directly in a handler
/// silently opts that route out of the composition contract.
pub fn request_manager(req: &HttpRequest, state: &AppState) -> Arc<DatabaseManager> {
    req.extensions()
        .get::<RequestDatabaseManager>()
        .map(|m| Arc::clone(&m.0))
        .unwrap_or_else(|| Arc::clone(&state.manager))
}

/// Extractor that resolves the correct AtomicCore for the current request.
pub struct Db(pub AtomicCore);

/// Resolve which database core `req` addresses within `manager`.
/// Checks the `X-Atomic-Database` header, then the `?db=` query parameter,
/// then falls back to the manager's active database.
///
/// This is the single definition of per-request database selection: the
/// [`Db`] extractor applies it to whichever manager governs the request
/// (injected or [`AppState`]'s), and [`AppState::resolve_core`] delegates
/// here for its own manager.
pub async fn resolve_core(
    manager: &DatabaseManager,
    req: &HttpRequest,
) -> Result<AtomicCore, atomic_core::AtomicCoreError> {
    // Check X-Atomic-Database header
    if let Some(db_id) = req
        .headers()
        .get("X-Atomic-Database")
        .and_then(|v| v.to_str().ok())
    {
        return manager.get_core(db_id).await;
    }

    // Check ?db= query parameter
    if let Some(db_id) = req.query_string().split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        if parts.next()? == "db" {
            parts.next()
        } else {
            None
        }
    }) {
        return manager.get_core(db_id).await;
    }

    // Default to active database
    manager.active_core().await
}

impl FromRequest for Db {
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut actix_web::dev::Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let state = req.app_data::<web::Data<AppState>>().ok_or_else(|| {
                actix_web::error::ErrorInternalServerError("AppState not configured")
            })?;
            let manager = request_manager(&req, state);
            resolve_core(&manager, &req).await.map(Db).map_err(|e| {
                actix_web::error::ErrorBadRequest(format!("Database not found: {}", e))
            })
        })
    }
}
