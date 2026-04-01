//! WebSocket endpoint for real-time event streaming

use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use tokio::sync::broadcast;

/// WebSocket upgrade handler
/// Auth via query param: /ws?token=xxx
pub async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
    query: web::Query<WsQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    // Reject early if server is not yet initialized (fresh/locked)
    if !state.manager.is_initialized() {
        return Ok(HttpResponse::ServiceUnavailable()
            .json(serde_json::json!({"error": "server_not_initialized"})));
    }

    // Authenticate via query param
    let core = state.manager.active_core()
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to get database"))?;
    match core.verify_api_token(&query.token) {
        Ok(Some(_)) => {}
        _ => return Ok(HttpResponse::Unauthorized().finish()),
    }

    let (response, mut session, _msg_stream) = actix_ws::handle(&req, stream)?;

    // Subscribe to broadcast channel
    let mut rx = state.event_tx.subscribe();

    // Spawn task to forward broadcast events to this WebSocket client
    actix_web::rt::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&event) {
                        if session.text(json).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "WebSocket client lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct WsQuery {
    pub token: String,
}
