//! End-to-end chat tests across both storage backends.
//!
//! The chat HTTP endpoint (`POST /api/conversations/{id}/messages`) is
//! synchronous — it returns the assistant message after the agent loop
//! completes. Streaming deltas, tool starts, tool completes, and the
//! terminal `ChatComplete` arrive over the existing WebSocket broadcast bus
//! (`chat_event_callback` bridges `ChatEvent` → `ServerEvent`).
//!
//! The mock (in `atomic_test_support::mock_ai`) speaks the streaming
//! OpenAI-compat protocol. On the first request it emits a `tool_calls` SSE
//! frame for `search_atoms`; on the second (which now contains tool-result
//! messages) it emits a content frame and finishes. That two-pass shape
//! mirrors a real provider, so the agent loop runs end-to-end without state
//! cheats in the mock.

mod support;

use futures_util::SinkExt;
use serde_json::{json, Value};
use std::time::Duration;
use support::{collect_ws_event_until, spawn_live_server, Backend, TestCtx};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ==================== Test helpers ====================

/// Connect a WebSocket subscriber to the live server. Auth flows via the
/// `?token=...` query param, same shape the chat e2e shares with the
/// pipeline-events suite.
async fn connect_ws(
    base_url: &str,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let ws_url = format!("{}/ws?token={}", base_url.replace("http://", "ws://"), token);
    let (ws, _resp) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("ws upgrade should succeed");
    ws
}

/// Create an empty conversation scoped to the supplied tag ids and return
/// its id. The chat route expects a pre-existing conversation row.
async fn create_conversation(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    tag_ids: &[&str],
) -> String {
    let resp = client
        .post(format!("{}/api/conversations", base_url))
        .bearer_auth(token)
        .json(&json!({ "tag_ids": tag_ids, "title": "test conversation" }))
        .send()
        .await
        .expect("POST /api/conversations");
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.expect("parse conversation");
    body["id"]
        .as_str()
        .expect("conversation id")
        .to_string()
}

/// POST an atom and wait until its pipeline reaches a terminal state. The
/// chat agent's search tool depends on chunks + embeddings being persisted,
/// so without waiting the search would race the pipeline.
async fn seed_atom_live(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    content: &str,
    tag_ids: &[&str],
) -> String {
    let resp = client
        .post(format!("{}/api/atoms", base_url))
        .bearer_auth(token)
        .json(&json!({ "content": content, "tag_ids": tag_ids }))
        .send()
        .await
        .expect("POST /api/atoms");
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.expect("parse atom");
    let id = body["id"].as_str().expect("id").to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let resp = client
            .get(format!("{}/api/atoms/{}", base_url, id))
            .bearer_auth(token)
            .send()
            .await
            .expect("GET /api/atoms/{id}");
        let body: Value = resp.json().await.expect("parse atom");
        let status = body["embedding_status"].as_str().unwrap_or("");
        if status == "complete" || status == "failed" {
            return id;
        }
        if std::time::Instant::now() >= deadline {
            panic!("atom {id} did not reach terminal embedding state within 15s");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn create_tag_live(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    name: &str,
) -> String {
    let resp = client
        .post(format!("{}/api/tags", base_url))
        .bearer_auth(token)
        .json(&json!({ "name": name, "parent_id": null }))
        .send()
        .await
        .expect("POST /api/tags");
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.expect("parse tag");
    body["id"].as_str().unwrap().to_string()
}

// ==================== 1. Streams content over WS ====================

#[actix_web::test]
async fn chat_message_streams_response_chunks_sqlite() {
    run_chat_message_streams_response_chunks(Backend::Sqlite).await;
}

#[actix_web::test]
async fn chat_message_streams_response_chunks_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "chat_message_streams_response_chunks_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_chat_message_streams_response_chunks(Backend::Postgres).await;
}

async fn run_chat_message_streams_response_chunks(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;
    let client = reqwest::Client::new();
    let mut ws = connect_ws(&server.base_url, &ctx.token).await;

    // Seed an atom so the mock's search tool finds at least one citation.
    seed_atom_live(
        &client,
        &server.base_url,
        &ctx.token,
        "quantum particles atomic waves momentum",
        &[],
    )
    .await;

    let conv_id = create_conversation(&client, &server.base_url, &ctx.token, &[]).await;

    // Drive the agent on a background task so we can collect WS frames in
    // parallel with the synchronous HTTP request.
    let send_handle = {
        let base_url = server.base_url.clone();
        let token = ctx.token.clone();
        let conv_id = conv_id.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let resp = client
                .post(format!(
                    "{}/api/conversations/{}/messages",
                    base_url, conv_id
                ))
                .bearer_auth(&token)
                .json(&json!({ "content": "what do you know about quantum particles?" }))
                .send()
                .await
                .expect("POST /api/conversations/{id}/messages");
            assert!(
                resp.status().is_success(),
                "chat send must succeed, got {}",
                resp.status()
            );
            let body: Value = resp.json().await.expect("parse chat reply");
            body
        })
    };

    let conv_id_for_pred = conv_id.clone();
    let mut saw_delta = false;
    let event = collect_ws_event_until(&mut ws, Duration::from_secs(20), move |event| {
        let event_conv = event["conversation_id"].as_str().unwrap_or("");
        if event_conv != conv_id_for_pred {
            return false;
        }
        match event["type"].as_str().unwrap_or("") {
            "ChatStreamDelta" => {
                saw_delta = true;
                false
            }
            "ChatComplete" => saw_delta,
            _ => false,
        }
    })
    .await;
    assert_eq!(event["type"], "ChatComplete");

    let reply = send_handle.await.expect("send task joined");
    assert!(
        reply["content"]
            .as_str()
            .unwrap_or("")
            .contains("Mock assistant reply"),
        "final reply should contain mock content; got {reply}"
    );

    ws.send(WsMessage::Close(None)).await.ok();
    server.stop().await;
}

// ==================== 2. Agent uses search tool ====================

#[actix_web::test]
async fn chat_agent_uses_search_tool_sqlite() {
    run_chat_agent_uses_search_tool(Backend::Sqlite).await;
}

#[actix_web::test]
async fn chat_agent_uses_search_tool_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "chat_agent_uses_search_tool_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_chat_agent_uses_search_tool(Backend::Postgres).await;
}

async fn run_chat_agent_uses_search_tool(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;
    let client = reqwest::Client::new();
    let mut ws = connect_ws(&server.base_url, &ctx.token).await;

    // Seed two physics atoms — the search tool query is lifted verbatim
    // from the user message so the bag-of-words mock embedder matches.
    seed_atom_live(
        &client,
        &server.base_url,
        &ctx.token,
        "quantum particles atomic waves momentum",
        &[],
    )
    .await;
    seed_atom_live(
        &client,
        &server.base_url,
        &ctx.token,
        "quantum field theory particles spin",
        &[],
    )
    .await;

    let conv_id = create_conversation(&client, &server.base_url, &ctx.token, &[]).await;

    let send_handle = {
        let base_url = server.base_url.clone();
        let token = ctx.token.clone();
        let conv_id = conv_id.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            client
                .post(format!(
                    "{}/api/conversations/{}/messages",
                    base_url, conv_id
                ))
                .bearer_auth(&token)
                .json(&json!({ "content": "quantum particles atomic waves" }))
                .send()
                .await
                .expect("send chat")
                .json::<Value>()
                .await
                .expect("parse")
        })
    };

    // Track ChatToolStart + ChatToolComplete observations through a shared
    // mutex so the FnMut closure can mutate without being `move` over local
    // mutables (the closure crosses an async await boundary, so capturing
    // by &mut to a stack int would borrow-check against the future).
    let observations = std::sync::Arc::new(std::sync::Mutex::new((false, 0i32)));
    let observations_for_pred = std::sync::Arc::clone(&observations);
    let conv_id_for_pred = conv_id.clone();
    let event = collect_ws_event_until(&mut ws, Duration::from_secs(20), move |event| {
        if event["conversation_id"].as_str().unwrap_or("") != conv_id_for_pred {
            return false;
        }
        match event["type"].as_str().unwrap_or("") {
            "ChatToolStart" => {
                if event["tool_name"].as_str() == Some("search_atoms") {
                    observations_for_pred.lock().unwrap().0 = true;
                }
                false
            }
            "ChatToolComplete" => {
                observations_for_pred.lock().unwrap().1 =
                    event["results_count"].as_i64().unwrap_or(0) as i32;
                false
            }
            "ChatComplete" => observations_for_pred.lock().unwrap().0,
            _ => false,
        }
    })
    .await;
    assert_eq!(event["type"], "ChatComplete");
    let (saw_tool_start, tool_complete_count) = *observations.lock().unwrap();
    assert!(saw_tool_start, "expected ChatToolStart for search_atoms");

    let reply = send_handle.await.expect("send task joined");
    let tool_calls = reply["tool_calls"].as_array().cloned().unwrap_or_default();
    assert!(
        tool_calls
            .iter()
            .any(|tc| tc["tool_name"].as_str() == Some("search_atoms")),
        "reply should record a search_atoms tool call; got tool_calls={tool_calls:?}"
    );
    assert!(
        tool_complete_count >= 1,
        "search tool should report at least one result (seeded 2 physics atoms); got {tool_complete_count}"
    );

    ws.send(WsMessage::Close(None)).await.ok();
    server.stop().await;
}

// ==================== 3. Scope filter excludes other tags ====================

#[actix_web::test]
async fn conversation_scoped_to_tag_filters_search_sqlite() {
    run_conversation_scoped_to_tag_filters_search(Backend::Sqlite).await;
}

#[actix_web::test]
async fn conversation_scoped_to_tag_filters_search_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "conversation_scoped_to_tag_filters_search_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_conversation_scoped_to_tag_filters_search(Backend::Postgres).await;
}

async fn run_conversation_scoped_to_tag_filters_search(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;
    let client = reqwest::Client::new();
    let mut ws = connect_ws(&server.base_url, &ctx.token).await;

    // Use a name the mock auto-tagger doesn't emit so it doesn't sneak the
    // same tag onto the unscoped atom.
    let physics_tag = create_tag_live(&client, &server.base_url, &ctx.token, "ChatScope").await;
    let in_scope = seed_atom_live(
        &client,
        &server.base_url,
        &ctx.token,
        "quantum particles atomic waves",
        &[physics_tag.as_str()],
    )
    .await;
    let out_of_scope = seed_atom_live(
        &client,
        &server.base_url,
        &ctx.token,
        "quantum field theory particles spin",
        &[],
    )
    .await;

    let conv_id =
        create_conversation(&client, &server.base_url, &ctx.token, &[physics_tag.as_str()]).await;

    let send_handle = {
        let base_url = server.base_url.clone();
        let token = ctx.token.clone();
        let conv_id = conv_id.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            client
                .post(format!(
                    "{}/api/conversations/{}/messages",
                    base_url, conv_id
                ))
                .bearer_auth(&token)
                .json(&json!({ "content": "quantum particles atomic waves" }))
                .send()
                .await
                .expect("send chat")
                .json::<Value>()
                .await
                .expect("parse")
        })
    };

    // Wait for the conversation to complete so the reply payload is final.
    let conv_id_for_pred = conv_id.clone();
    collect_ws_event_until(&mut ws, Duration::from_secs(20), move |event| {
        event["conversation_id"].as_str().unwrap_or("") == conv_id_for_pred
            && event["type"].as_str().unwrap_or("") == "ChatComplete"
    })
    .await;

    let reply = send_handle.await.expect("send task joined");
    let cited_atoms: Vec<String> = reply["citations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c["atom_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        cited_atoms.iter().any(|a| a == &in_scope),
        "in-scope atom must be cited; got {cited_atoms:?}"
    );
    assert!(
        !cited_atoms.iter().any(|a| a == &out_of_scope),
        "out-of-scope atom must not appear; got {cited_atoms:?}"
    );

    ws.send(WsMessage::Close(None)).await.ok();
    server.stop().await;
}

// ==================== 4. Persists message to storage ====================

#[actix_web::test]
async fn chat_message_persists_to_storage_sqlite() {
    run_chat_message_persists_to_storage(Backend::Sqlite).await;
}

#[actix_web::test]
async fn chat_message_persists_to_storage_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "chat_message_persists_to_storage_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_chat_message_persists_to_storage(Backend::Postgres).await;
}

async fn run_chat_message_persists_to_storage(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;
    let client = reqwest::Client::new();

    seed_atom_live(
        &client,
        &server.base_url,
        &ctx.token,
        "quantum particles atomic waves",
        &[],
    )
    .await;

    let conv_id = create_conversation(&client, &server.base_url, &ctx.token, &[]).await;
    let resp = client
        .post(format!(
            "{}/api/conversations/{}/messages",
            server.base_url, conv_id
        ))
        .bearer_auth(&ctx.token)
        .json(&json!({ "content": "tell me about quantum particles" }))
        .send()
        .await
        .expect("send chat");
    assert!(resp.status().is_success());

    // Read the conversation back; it should contain both turns in order.
    let resp = client
        .get(format!(
            "{}/api/conversations/{}",
            server.base_url, conv_id
        ))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .expect("get conversation");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("parse");
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    assert!(
        messages.len() >= 2,
        "expected user + assistant turns; got {}",
        messages.len()
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");

    server.stop().await;
}

// ==================== 5. Auth required ====================

#[actix_web::test]
async fn chat_requires_auth_sqlite() {
    run_chat_requires_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn chat_requires_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("chat_requires_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_chat_requires_auth(Backend::Postgres).await;
}

async fn run_chat_requires_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/conversations/00000000-0000-0000-0000-000000000000/messages",
            server.base_url
        ))
        .json(&json!({ "content": "hi" }))
        .send()
        .await
        .expect("send chat without auth");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "unauthenticated chat must be rejected"
    );

    server.stop().await;
}
