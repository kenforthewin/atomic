//! Event-channel resolution extractor for actix-web.
//!
//! `EventChannel` is a `FromRequest` extractor that resolves the broadcast
//! sender a request's handler publishes [`ServerEvent`]s into. By default it
//! is the process-wide channel in [`AppState`] — the one the `/ws` endpoint
//! streams to clients; callers composing atomic-server's routes under their
//! own middleware can override it per request via [`RequestEventChannel`].

use crate::state::{AppState, ServerEvent};
use actix_web::{web, FromRequest, HttpMessage, HttpRequest};
use std::future::{ready, Ready};
use tokio::sync::broadcast;

/// Per-request override for the broadcast channel the [`EventChannel`]
/// extractor resolves to.
///
/// Callers composing atomic-server's routes under their own middleware can
/// insert this into a request's extensions to direct that request's events
/// at a channel other than the one in [`AppState`]. Both producers and
/// consumers honor it: route handlers publish into the injected sender, and
/// the WebSocket handler subscribes to it — so a composition that fans
/// requests out over per-request channels keeps each WS client on the same
/// channel its events are sent to.
///
/// Scope boundary: only *request-driven* events are affected. The background
/// loops in the binary (startup pipeline resume, the feed-polling sweep, the
/// scheduled-task and report runners in `main.rs`) are not tied to any
/// request, so there is no extension to consult — they always publish into
/// the process-wide channel held by [`AppState`].
///
/// When absent (the standalone server installs no middleware that sets it),
/// the extractor falls back to [`AppState`]'s channel, so compositions that
/// don't need the override pay no per-request cost.
///
/// A second boundary, shared with
/// [`RequestDatabaseManager`](crate::db_extractor::RequestDatabaseManager):
/// the `/mcp` scope and the auth/registry plane (`BearerAuth`, `McpAuth`,
/// WebSocket token verification, token CRUD, the OAuth flow, instance
/// setup) bind to [`AppState`] at composition time — the MCP transport in
/// particular captures its event channel in
/// [`AtomicMcpTransport::new`](crate::mcp::AtomicMcpTransport::new) — and
/// never consult this extension. A caller that needs those planes bound
/// differently composes the granular pieces in [`crate::app`] with its own
/// middleware and transport instead.
#[derive(Clone)]
pub struct RequestEventChannel(pub broadcast::Sender<ServerEvent>);

/// Extractor that resolves the event channel for the current request.
pub struct EventChannel(pub broadcast::Sender<ServerEvent>);

impl FromRequest for EventChannel {
    type Error = actix_web::Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _payload: &mut actix_web::dev::Payload) -> Self::Future {
        // A composing layer may have installed a per-request channel;
        // otherwise fall back to the process-wide one in AppState.
        if let Some(injected) = req.extensions().get::<RequestEventChannel>() {
            return ready(Ok(EventChannel(injected.0.clone())));
        }
        ready(
            req.app_data::<web::Data<AppState>>()
                .map(|state| EventChannel(state.event_tx.clone()))
                .ok_or_else(|| {
                    actix_web::error::ErrorInternalServerError("AppState not configured")
                }),
        )
    }
}
