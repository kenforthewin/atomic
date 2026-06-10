//! End-to-end WebSocket event delivery across both storage backends.
//!
//! Boots a real `HttpServer` on an ephemeral port, connects a tokio-tungstenite
//! WebSocket client to `/ws?token=...`, then POSTs an atom over HTTP and waits
//! for the embedding + tagging pipeline events to land on the WS. Validates
//! the full event flow:
//!
//!   route handler → on_event closure → broadcast::Sender → ws_handler → JSON
//!
//! Anything below this layer (the pipeline emitting events at the right
//! moments) is covered by atomic-core's pipeline tests; the WS-specific gaps
//! are auth-via-query-param and the JSON-over-wire serialization.

mod support;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use support::{spawn_live_server, Backend, TestCtx};
use tokio_tungstenite::tungstenite::Message;

#[actix_web::test]
async fn ws_delivers_pipeline_events_sqlite() {
    run_ws_delivers_pipeline_events(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ws_delivers_pipeline_events_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "ws_delivers_pipeline_events_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_ws_delivers_pipeline_events(Backend::Postgres).await;
}

async fn run_ws_delivers_pipeline_events(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;

    // Connect the WS first so we're definitely subscribed before the POST
    // fires events. Auth goes through `?token=...` per ws_handler's contract.
    let ws_url = format!(
        "{}/ws?token={}",
        server.base_url.replace("http://", "ws://"),
        ctx.token
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("ws upgrade should succeed");

    // POST an atom. The route fires Started / EmbeddingComplete / TaggingComplete
    // / PipelineQueueCompleted events as the pipeline progresses.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/atoms", server.base_url))
        .bearer_auth(&ctx.token)
        .json(&json!({ "content": "quantum particles atomic waves" }))
        .send()
        .await
        .expect("POST /api/atoms");
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.expect("parse atom response");
    let atom_id = body["id"].as_str().expect("atom id").to_string();

    // Collect WS events until we see both EmbeddingComplete and TaggingComplete
    // for the atom we created. The mock provider responds instantly, but the
    // pipeline runs on a background task — bound the wait so a hung pipeline
    // surfaces as a clear failure instead of a hang.
    let mut saw_embedding = false;
    let mut saw_tagging = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(saw_embedding && saw_tagging) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "did not observe both EmbeddingComplete and TaggingComplete for {atom_id} \
                 within 15s; saw_embedding={saw_embedding}, saw_tagging={saw_tagging}"
            );
        }
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .expect("ws recv timeout")
            .expect("ws stream ended")
            .expect("ws frame");
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                continue
            }
            Message::Close(_) => panic!("server closed the ws connection mid-test"),
        };
        let event: Value = serde_json::from_str(&text).expect("ws frame is JSON");
        let event_type = event["type"].as_str().unwrap_or("");
        let event_atom = event["atom_id"].as_str().unwrap_or("");
        if event_atom != atom_id {
            continue;
        }
        match event_type {
            "EmbeddingComplete" => saw_embedding = true,
            "TaggingComplete" => saw_tagging = true,
            _ => {}
        }
    }

    ws.send(Message::Close(None)).await.ok();
    server.stop().await;
}

#[actix_web::test]
async fn ws_rejects_invalid_token_sqlite() {
    run_ws_rejects_invalid_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ws_rejects_invalid_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "ws_rejects_invalid_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_ws_rejects_invalid_token(Backend::Postgres).await;
}

async fn run_ws_rejects_invalid_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;

    let ws_url = format!(
        "{}/ws?token=not-a-real-token",
        server.base_url.replace("http://", "ws://")
    );
    let result = tokio_tungstenite::connect_async(&ws_url).await;
    assert!(
        result.is_err(),
        "ws upgrade with a bogus token must be refused; got {:?}",
        result.map(|_| "Ok")
    );

    server.stop().await;
}
