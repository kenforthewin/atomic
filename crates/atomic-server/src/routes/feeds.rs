//! Feed CRUD and polling routes

use crate::db_extractor::Db;
use crate::error::{ok_or_error, ApiErrorResponse};
use crate::event_bridge::{embedding_event_callback, ingestion_event_callback};
use crate::event_channel::EventChannel;
use actix_web::{web, HttpResponse};

#[utoipa::path(get, path = "/api/feeds", responses((status = 200, description = "All feeds", body = Vec<atomic_core::Feed>)), tag = "feeds")]
pub async fn list_feeds(db: Db) -> HttpResponse {
    ok_or_error(db.0.list_feeds().await)
}

#[utoipa::path(get, path = "/api/feeds/{id}", params(("id" = String, Path, description = "Feed ID")), responses((status = 200, description = "Feed details", body = atomic_core::Feed), (status = 404, description = "Feed not found", body = ApiErrorResponse)), tag = "feeds")]
pub async fn get_feed(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    ok_or_error(db.0.get_feed(&id).await)
}

#[utoipa::path(post, path = "/api/feeds", request_body = atomic_core::CreateFeedRequest, responses((status = 201, description = "Feed created", body = atomic_core::Feed)), tag = "feeds")]
pub async fn create_feed(
    events: EventChannel,
    db: Db,
    body: web::Json<atomic_core::CreateFeedRequest>,
) -> HttpResponse {
    let on_ingest = ingestion_event_callback(events.0.clone());
    let on_embed = embedding_event_callback(events.0.clone());

    match db
        .0
        .create_feed(body.into_inner(), on_ingest, on_embed)
        .await
    {
        Ok(feed) => HttpResponse::Created().json(feed),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(put, path = "/api/feeds/{id}", params(("id" = String, Path, description = "Feed ID")), request_body = atomic_core::UpdateFeedRequest, responses((status = 200, description = "Feed updated", body = atomic_core::Feed)), tag = "feeds")]
pub async fn update_feed(
    db: Db,
    path: web::Path<String>,
    body: web::Json<atomic_core::UpdateFeedRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    ok_or_error(db.0.update_feed(&id, body.into_inner()).await)
}

#[utoipa::path(delete, path = "/api/feeds/{id}", params(("id" = String, Path, description = "Feed ID")), responses((status = 200, description = "Feed deleted")), tag = "feeds")]
pub async fn delete_feed(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match db.0.delete_feed(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"deleted": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/feeds/{id}/poll", params(("id" = String, Path, description = "Feed ID")), responses((status = 200, description = "Poll results"), (status = 409, description = "A poll for this feed is already in flight or backing off after a failure", body = ApiErrorResponse)), tag = "feeds")]
pub async fn poll_feed(events: EventChannel, db: Db, path: web::Path<String>) -> HttpResponse {
    let feed_id = path.into_inner();
    let on_ingest = ingestion_event_callback(events.0.clone());
    let on_embed = embedding_event_callback(events.0.clone());

    // Manual polls ride the same `task_runs` ledger as the 60s sweep, so
    // a poll already in flight (live lease) or a failed poll inside its
    // backoff window comes back as `Skipped` instead of double-polling.
    match db
        .0
        .poll_feed(
            &feed_id,
            atomic_core::TaskRunTrigger::Manual,
            on_ingest,
            on_embed,
        )
        .await
    {
        Ok(atomic_core::PollOutcome::Polled(result)) => HttpResponse::Ok().json(result),
        Ok(atomic_core::PollOutcome::Failed { error }) => {
            crate::error::error_response(atomic_core::AtomicCoreError::Ingestion(error))
        }
        Ok(atomic_core::PollOutcome::Skipped) => HttpResponse::Conflict().json(ApiErrorResponse {
            error: "a poll for this feed is already in flight or backing off after a failure"
                .to_string(),
        }),
        Err(e) => crate::error::error_response(e),
    }
}
