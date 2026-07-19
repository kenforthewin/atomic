//! WebSocket endpoint for real-time event streaming

use crate::event_channel::EventChannel;
use crate::state::{AppState, ServerEvent};
use actix_web::{web, HttpRequest, HttpResponse};
use futures::StreamExt;
use tokio::sync::broadcast;

/// How often the forwarding task pings the client, and how long it waits
/// past the last inbound frame before declaring the client dead. Browsers
/// answer pings with pongs automatically, so a healthy-but-quiet client
/// refreshes `last_seen` every ping interval; a client that vanished
/// without closing the TCP stream stops answering and gets reaped within
/// one timeout.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// WebSocket upgrade handler
/// Auth via query param: /ws?token=xxx
pub async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<AppState>,
    events: EventChannel,
    query: web::Query<WsQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    // Authenticate via query param. Token verification is part of the
    // auth/registry plane: it resolves against AppState's manager at
    // composition time, like BearerAuth — not the per-request extensions
    // honored by the data-plane handlers. A composition with its own
    // identity scheme routes its own handler to `start_event_session`
    // instead of layering on this one.
    let core = state
        .manager
        .active_core()
        .await
        .map_err(|_| actix_web::error::ErrorInternalServerError("Failed to get database"))?;
    match core.verify_api_token(&query.token).await {
        Ok(Some(_)) => {}
        _ => return Ok(HttpResponse::Unauthorized().finish()),
    }

    // Subscribe to the request's event channel — AppState's process-wide
    // channel unless a composing layer injected one, in which case this
    // client streams the same channel that request-driven events publish to.
    start_event_session(&req, stream, events.0)
}

/// Start an event-streaming WebSocket session on an already-authorized
/// request: perform the upgrade, subscribe to `events`, and forward every
/// broadcast [`ServerEvent`] to the client as a JSON text frame (sending
/// [`ServerEvent::EventsLagged`] when the client falls behind the channel).
///
/// This is the post-auth half of [`ws_handler`], exposed so a caller
/// composing its own authenticated WS route — a different token scheme, an
/// identity established by middleware — reuses the session machinery
/// instead of reimplementing the forwarding loop. The caller picks the
/// channel: typically the sender resolved by the
/// [`EventChannel`](crate::event_channel::EventChannel) extractor, so the
/// WS client streams the same channel its request-driven events publish to.
///
/// **The caller is responsible for authentication** — this function starts
/// streaming for whoever holds the request.
pub fn start_event_session(
    req: &HttpRequest,
    stream: web::Payload,
    events: broadcast::Sender<ServerEvent>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(req, stream)?;

    let mut rx = events.subscribe();

    // Forward broadcast events to this WebSocket client.
    //
    // The inbound stream MUST be polled alongside the event channel: it is
    // the only place a client's Close frame or TCP EOF is observable. A
    // task that waits solely on the (possibly quiet) event channel outlives
    // its client indefinitely — retaining the broadcast receiver (which a
    // composing layer may use as a liveness signal, e.g. cache pinning)
    // and, when the peer sent a FIN nobody read, a CLOSE_WAIT socket. The
    // heartbeat covers the third exit path: a peer that vanished without
    // closing the stream stops answering pings and times out.
    actix_web::rt::spawn(async move {
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_seen = tokio::time::Instant::now();

        loop {
            tokio::select! {
                broadcast = rx.recv() => match broadcast {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if session.text(json).await.is_err() {
                                break; // Client disconnected
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("WebSocket client lagged, skipped {} events", n);
                        let event = ServerEvent::EventsLagged { skipped: n };
                        if let Ok(json) = serde_json::to_string(&event) {
                            if session.text(json).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                inbound = msg_stream.next() => match inbound {
                    Some(Ok(actix_ws::Message::Close(_))) | None => break,
                    Some(Ok(actix_ws::Message::Ping(bytes))) => {
                        last_seen = tokio::time::Instant::now();
                        if session.pong(&bytes).await.is_err() {
                            break;
                        }
                    }
                    // Pongs (heartbeat replies) and any client chatter both
                    // count as liveness; this endpoint is send-only so
                    // inbound payloads are otherwise ignored.
                    Some(Ok(_)) => last_seen = tokio::time::Instant::now(),
                    Some(Err(_)) => break,
                },
                _ = heartbeat.tick() => {
                    if last_seen.elapsed() > CLIENT_TIMEOUT {
                        let _ = session.close(None).await;
                        return;
                    }
                    if session.ping(b"").await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = session.close(None).await;
    });

    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct WsQuery {
    pub token: String,
}
